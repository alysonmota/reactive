//! Capability TLS contains no runtime registry. Runtime contexts remain explicit.
//!
//! Tracking, batch and flush guards keep reentrant scopes explicit and cumulative.
#![allow(dead_code)]

use std::cell::Cell;

use crate::{RuntimeError, arena::SlotId, handle::RuntimeId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Read,
    Write,
    Create,
    Dispose,
    Flush,
}

impl Operation {
    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallbackKind {
    Host,
    Memo,
    Effect,
    Updating,
    Comparator,
    Cleanup,
    Hook,
    Destructor,
    BorrowedRead,
}

const ALL: u8 = 0b1_1111;

impl CallbackKind {
    const fn permissions(self) -> u8 {
        match self {
            Self::Host => ALL,
            Self::Memo => Operation::Read.bit(),
            Self::Effect | Self::Updating => ALL & !Operation::Flush.bit(),
            Self::Cleanup => {
                Operation::Read.bit() | Operation::Write.bit() | Operation::Dispose.bit()
            }
            Self::Comparator | Self::Hook | Self::Destructor | Self::BorrowedRead => 0,
        }
    }
}

#[derive(Clone, Copy)]
struct Capabilities {
    permissions: u8,
    pure: bool,
}

thread_local! {
    static CAPABILITIES: Cell<Capabilities> = const { Cell::new(Capabilities {
        permissions: ALL,
        pure: false,
    }) };
    // Identity only, not an implicit runtime or handle lookup registry.
    static READER_RUNTIME: Cell<Option<RuntimeId>> = const { Cell::new(None) };
}

pub(crate) fn check_read_runtime(runtime: RuntimeId) -> Result<(), RuntimeError> {
    if READER_RUNTIME.get().is_some_and(|reader| reader != runtime) {
        Err(RuntimeError::WrongRuntime)
    } else {
        Ok(())
    }
}

pub(crate) struct ReaderRuntimeGuard(Option<RuntimeId>);

impl ReaderRuntimeGuard {
    pub(crate) fn enter(runtime: Option<RuntimeId>) -> Self {
        Self(READER_RUNTIME.replace(runtime))
    }

    pub(crate) fn set(&self, runtime: Option<RuntimeId>) {
        READER_RUNTIME.set(runtime);
    }
}

impl Drop for ReaderRuntimeGuard {
    fn drop(&mut self) {
        READER_RUNTIME.set(self.0);
    }
}

pub(crate) fn check(operation: Operation) -> Result<(), RuntimeError> {
    let capabilities = CAPABILITIES.get();
    if capabilities.permissions & operation.bit() != 0 {
        return Ok(());
    }
    Err(match (capabilities.pure, operation) {
        (true, Operation::Write) => RuntimeError::WriteForbidden,
        (true, Operation::Create | Operation::Dispose) => RuntimeError::MutationForbidden,
        _ => RuntimeError::Reentrancy,
    })
}

/// Intersects permissions rather than replacing them, including across runtimes.
pub(crate) struct CapabilityGuard(Capabilities);

impl CapabilityGuard {
    pub(crate) fn enter(kind: CallbackKind) -> Self {
        let previous = CAPABILITIES.get();
        CAPABILITIES.set(Capabilities {
            permissions: previous.permissions & kind.permissions(),
            pure: previous.pure || matches!(kind, CallbackKind::Memo | CallbackKind::Comparator),
        });
        Self(previous)
    }
}

impl Drop for CapabilityGuard {
    fn drop(&mut self) {
        CAPABILITIES.set(self.0);
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) owner: Option<SlotId>,
    pub(crate) reader: Option<SlotId>,
    pub(crate) batch_depth: usize,
    pub(crate) flushing: bool,
    pub(crate) disposal_depth: usize,
    pub(crate) evaluation_depth: usize,
}

#[derive(Default)]
pub(crate) struct Context(Cell<Snapshot>);

impl Context {
    pub(crate) fn modify(&self, modify: impl FnOnce(&mut Snapshot)) {
        let mut snapshot = self.0.get();
        modify(&mut snapshot);
        self.0.set(snapshot);
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.0.get()
    }

    pub(crate) fn enter(&self, modify: impl FnOnce(&mut Snapshot)) -> ContextGuard<'_> {
        let previous = self.0.get();
        let mut next = previous;
        modify(&mut next);
        self.0.set(next);
        ContextGuard {
            context: self,
            previous,
        }
    }

    /// Evaluation modifies only owner, reader and evaluation depth. Other
    /// nested scopes retain their own full guards (batch/flush/disposal/owner).
    pub(crate) fn enter_evaluation(
        &self,
        reader: Option<SlotId>,
        depth: usize,
    ) -> EvaluationContextGuard<'_> {
        let previous = self.0.get();
        let next = Snapshot {
            reader,
            evaluation_depth: depth,
            ..previous
        };
        self.0.set(next);
        EvaluationContextGuard {
            context: self,
            owner: previous.owner,
            reader: previous.reader,
            depth: previous.evaluation_depth,
        }
    }
}

pub(crate) struct EvaluationContextGuard<'a> {
    context: &'a Context,
    owner: Option<SlotId>,
    reader: Option<SlotId>,
    depth: usize,
}
impl Drop for EvaluationContextGuard<'_> {
    fn drop(&mut self) {
        self.context.modify(|snapshot| {
            snapshot.owner = self.owner;
            snapshot.reader = self.reader;
            snapshot.evaluation_depth = self.depth;
        });
    }
}

/// Drop only restores metadata; it cannot invoke user callbacks.
pub(crate) struct ContextGuard<'a> {
    context: &'a Context,
    previous: Snapshot,
}

impl Drop for ContextGuard<'_> {
    fn drop(&mut self) {
        self.context.0.set(self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluation_guard_is_smaller_and_nested_full_scopes_restore_on_unwind() {
        assert!(
            std::mem::size_of::<EvaluationContextGuard<'_>>()
                < std::mem::size_of::<ContextGuard<'_>>()
        );
        let context = Context::default();
        let id = SlotId::new(7, 2).unwrap();
        context.modify(|s| {
            s.owner = Some(id);
            s.batch_depth = 4;
            s.flushing = true;
            s.disposal_depth = 3;
        });
        let before = context.snapshot();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _evaluation = context.enter_evaluation(Some(id), 1);
            let _nested = context.enter(|s| {
                s.owner = None;
                s.batch_depth += 1;
                s.disposal_depth += 1;
                s.reader = None;
            });
            panic!("nested scope");
        }));
        assert!(outcome.is_err());
        assert_eq!(context.snapshot(), before);
    }

    #[test]
    fn nested_capabilities_never_expand_permissions() {
        let memo = CapabilityGuard::enter(CallbackKind::Memo);
        assert_eq!(check(Operation::Read), Ok(()));
        assert_eq!(check(Operation::Write), Err(RuntimeError::WriteForbidden));
        {
            let _effect = CapabilityGuard::enter(CallbackKind::Effect);
            assert_eq!(
                check(Operation::Create),
                Err(RuntimeError::MutationForbidden)
            );
            assert_eq!(check(Operation::Write), Err(RuntimeError::WriteForbidden));
        }
        drop(memo);
        assert_eq!(check(Operation::Create), Ok(()));
    }

    #[test]
    fn callback_permission_matrix() {
        for kind in [
            CallbackKind::Host,
            CallbackKind::Memo,
            CallbackKind::Effect,
            CallbackKind::Updating,
            CallbackKind::Comparator,
            CallbackKind::Cleanup,
            CallbackKind::Hook,
            CallbackKind::Destructor,
        ] {
            let _guard = CapabilityGuard::enter(kind);
            for operation in [
                Operation::Read,
                Operation::Write,
                Operation::Create,
                Operation::Dispose,
                Operation::Flush,
            ] {
                assert_eq!(
                    check(operation).is_ok(),
                    kind.permissions() & operation.bit() != 0
                );
            }
        }
    }

    #[test]
    fn contexts_restore_all_fields_on_early_return() {
        let context = Context::default();
        let id = SlotId::new(1, 2).unwrap();
        let callback = || {
            let _guard = context.enter(|snapshot| {
                snapshot.owner = Some(id);
                snapshot.reader = Some(id);
                snapshot.batch_depth = 3;
                snapshot.flushing = true;
                snapshot.disposal_depth = 2;
            });
            {
                let _nested = context.enter(|snapshot| snapshot.reader = None);
                assert_eq!(context.snapshot().reader, None);
            }
            assert_eq!(context.snapshot().reader, Some(id));
            Err(())
        };
        let result: Result<(), ()> = callback();
        assert_eq!(result, Err(()));
        assert_eq!(context.snapshot(), Snapshot::default());
    }

    #[test]
    fn context_and_capabilities_restore_on_unwind() {
        let context = Context::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _context = context.enter(|snapshot| {
                snapshot.reader = SlotId::new(0, 1);
                snapshot.batch_depth = 1;
                snapshot.flushing = true;
            });
            let _capabilities = CapabilityGuard::enter(CallbackKind::Comparator);
            panic!("context unwind");
        }));
        assert!(result.is_err());
        assert_eq!(context.snapshot(), Snapshot::default());
        assert_eq!(check(Operation::Write), Ok(()));
    }
}
