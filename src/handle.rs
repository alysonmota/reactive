use std::{fmt, marker::PhantomData, rc::Rc};

use crate::{Runtime, RuntimeError, arena::SlotId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RuntimeId(pub(crate) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct HandleId {
    pub(crate) runtime: RuntimeId,
    pub(crate) slot: SlotId,
}

macro_rules! typed_handle {
    ($name:ident, $validate:ident, $description:literal) => {
        #[doc = $description]
        pub struct $name<T: 'static> {
            pub(crate) id: HandleId,
            marker: PhantomData<Rc<T>>,
        }

        impl<T> $name<T> {
            // Runtime-owned constructors supply checked identities.
            #[allow(dead_code)]
            pub(crate) fn new(id: HandleId) -> Self {
                Self {
                    id,
                    marker: PhantomData,
                }
            }

            /// Checks runtime, generation, life, node kind and stored value type.
            /// Does not read a value, evaluate a computation or create an edge.
            pub fn validate(self, runtime: &Runtime) -> Result<(), RuntimeError> {
                runtime.$validate::<T>(self.id)
            }
        }

        impl<T> Copy for $name<T> {}

        impl<T> Clone for $name<T> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<T> PartialEq for $name<T> {
            fn eq(&self, other: &Self) -> bool {
                self.id == other.id
            }
        }

        impl<T> Eq for $name<T> {}

        impl<T> std::hash::Hash for $name<T> {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.id.hash(state);
            }
        }

        impl<T> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.id).finish()
            }
        }
    };
}

typed_handle!(Signal, validate_signal, "Typed non-owning signal handle.");
typed_handle!(Memo, validate_memo, "Typed non-owning memo handle.");

impl<T> Signal<T> {
    /// Publishes immediately if the configured comparator considers it different.
    /// Comparisons and destructors run outside graph borrows. No effect executes.
    pub fn try_set(self, runtime: &Runtime, value: T) -> Result<(), RuntimeError> {
        runtime.write_signal(self.id, value)
    }
}

impl<T: Clone> Signal<T> {
    /// Clones a working copy outside graph borrows with reactive operations
    /// forbidden during Clone, then edits and publishes it under Updating permissions.
    /// A reentrant change returns UpdateConflict; disposal returns InvalidHandle.
    /// The callback is never retried and its external effects are not rolled back.
    /// Internal snapshot capture is untracked; explicit callback reads may track.
    pub fn try_update_cloned(
        self,
        runtime: &Runtime,
        callback: impl FnOnce(&mut T),
    ) -> Result<(), RuntimeError> {
        runtime.update_signal(self.id, callback)
    }
}

macro_rules! value_reads {
    ($handle:ident, $read:ident, $copy:ident) => {
        impl<T: 'static> $handle<T> {
            /// Tracked temporary read. Reactive operations are forbidden during
            /// the callback, and the borrowed reference cannot escape it.
            pub fn try_with<R>(
                self,
                runtime: &Runtime,
                callback: impl FnOnce(&T) -> R,
            ) -> Result<R, RuntimeError> {
                runtime.with_read(|| runtime.$read(self.id, true), callback)
            }
            /// Fresh temporary read without creating an external tracking edge.
            pub fn try_peek_with<R>(
                self,
                runtime: &Runtime,
                callback: impl FnOnce(&T) -> R,
            ) -> Result<R, RuntimeError> {
                runtime.with_read(|| runtime.$read(self.id, false), callback)
            }
            /// Explicitly copies a small value. No owning snapshot is created.
            pub fn try_get_copy(self, runtime: &Runtime) -> Result<T, RuntimeError>
            where
                T: Copy,
            {
                runtime.$copy(self.id, true)
            }
            pub fn try_peek_copy(self, runtime: &Runtime) -> Result<T, RuntimeError>
            where
                T: Copy,
            {
                runtime.$copy(self.id, false)
            }
            /// Explicitly invokes T::clone under the borrowed-read restrictions.
            pub fn try_get_cloned(self, runtime: &Runtime) -> Result<T, RuntimeError>
            where
                T: Clone,
            {
                self.try_with(runtime, T::clone)
            }
            pub fn try_peek_cloned(self, runtime: &Runtime) -> Result<T, RuntimeError>
            where
                T: Clone,
            {
                self.try_peek_with(runtime, T::clone)
            }
        }
    };
}
value_reads!(Signal, read_signal, read_signal_copy);
value_reads!(Memo, read_memo, read_memo_copy);

#[cfg(test)]
mod read_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn explicit_clone_read_panics_poison_and_restore_guards() {
        struct PanicClone;
        impl Clone for PanicClone {
            fn clone(&self) -> Self {
                panic!("clone read panic");
            }
        }
        for peek in [false, true] {
            let rt = Runtime::new();
            let root = rt.create_root().unwrap();
            let signal = rt
                .with_owner(root, |rt| rt.signal_with(PanicClone, |_, _| true))
                .unwrap();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if peek {
                    signal.try_peek_cloned(&rt)
                } else {
                    signal.try_get_cloned(&rt)
                }
            }));
            assert!(outcome.is_err());
            assert_eq!(rt.status(), crate::RuntimeStatus::Poisoned);
            assert_eq!(
                crate::context::check(crate::context::Operation::Write),
                Ok(())
            );
            rt.shutdown().unwrap();
        }
    }

    #[test]
    fn rejected_borrowed_read_releases_callback_captures_under_destructor_guard() {
        struct Capture(crate::WeakRuntime, Rc<Cell<usize>>);
        impl Drop for Capture {
            fn drop(&mut self) {
                self.1.set(self.1.get() + 1);
                let rt = self.0.upgrade().unwrap();
                assert_eq!(rt.create_root(), Err(RuntimeError::Reentrancy));
                panic!("capture drop");
            }
        }
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let signal = rt.with_owner(root, |rt| rt.signal(7_u64)).unwrap();
        rt.dispose(root).unwrap();
        let drops = Rc::new(Cell::new(0));
        let capture = Capture(rt.downgrade(), drops.clone());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            signal.try_with(&rt, move |_| {
                drop(capture);
                panic!("callback must not run");
            })
        }));
        assert!(outcome.is_err());
        assert_eq!(drops.get(), 1);
        assert_eq!(rt.status(), crate::RuntimeStatus::Poisoned);
        assert_eq!(
            crate::context::check(crate::context::Operation::Write),
            Ok(())
        );
        rt.shutdown().unwrap();
    }

    #[test]
    fn borrowed_read_blocks_all_reactive_operations_including_foreign_runtime() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let signal = rt.with_owner(root, |rt| rt.signal(7_u32)).unwrap();
        let other = Runtime::new();
        let other_root = other.create_root().unwrap();
        let foreign = other.with_owner(other_root, |rt| rt.signal(1)).unwrap();
        signal
            .try_with(&rt, |value| {
                assert_eq!(*value, 7);
                assert_eq!(signal.try_get_copy(&rt), Err(RuntimeError::Reentrancy));
                assert_eq!(foreign.try_get_copy(&other), Err(RuntimeError::Reentrancy));
                assert_eq!(signal.try_set(&rt, 9), Err(RuntimeError::Reentrancy));
                assert_eq!(rt.create_root(), Err(RuntimeError::Reentrancy));
                assert_eq!(rt.dispose(root), Err(RuntimeError::Reentrancy));
                assert_eq!(
                    rt.flush(Default::default()).err(),
                    Some(RuntimeError::Reentrancy)
                );
                assert_eq!(rt.shutdown(), Err(RuntimeError::Reentrancy));
            })
            .unwrap();
        signal.try_set(&rt, 8).unwrap();
        assert_eq!(signal.try_get_copy(&rt).unwrap(), 8);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn non_clone_signal_and_memo_can_be_borrowed_and_peek_does_not_track() {
        struct Value(u32);
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt
            .with_owner(root, |rt| rt.signal_with(Value(2), |a, b| a.0 == b.0))
            .unwrap();
        let peek = rt.with_owner(root, |rt| rt.signal(10)).unwrap();
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo_with(
                    move |rt| {
                        captured.set(captured.get() + 1);
                        let value = input.try_with(rt, |value| value.0)?;
                        peek.try_peek_with(rt, |_| ())?;
                        Ok(Value(value * 2))
                    },
                    |a, b| a.0 == b.0,
                )
            })
            .unwrap();
        assert_eq!(memo.try_with(&rt, |value| value.0).unwrap(), 4);
        peek.try_set(&rt, 11).unwrap();
        assert_eq!(memo.try_peek_with(&rt, |value| value.0).unwrap(), 4);
        assert_eq!(calls.get(), 1);
        input.try_set(&rt, Value(3)).unwrap();
        assert_eq!(memo.try_with(&rt, |value| value.0).unwrap(), 6);
        assert_eq!(calls.get(), 2);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn explicit_shared_values_remain_alive_after_publication_and_disposal() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let initial = Rc::new(String::from("old"));
        let weak = Rc::downgrade(&initial);
        let signal = rt.with_owner(root, |rt| rt.signal(initial)).unwrap();
        let owned = signal.try_get_cloned(&rt).unwrap();
        signal.try_set(&rt, Rc::new(String::from("new"))).unwrap();
        rt.dispose(root).unwrap();
        assert_eq!(owned.as_str(), "old");
        assert!(weak.upgrade().is_some());
        drop(owned);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn borrowed_callback_panic_releases_value_borrow_and_restores_capabilities() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let signal = rt.with_owner(root, |rt| rt.signal(7)).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || signal.try_with(&rt, |_| panic!("read panic"))
            ))
            .is_err()
        );
        assert_eq!(
            crate::context::check(crate::context::Operation::Write),
            Ok(())
        );
        assert_eq!(rt.status(), crate::RuntimeStatus::Poisoned);
        // Teardown can acquire the value and release it after unwind.
        rt.shutdown().unwrap();
    }
}

macro_rules! untyped_handle {
    ($name:ident, $validate:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name {
            pub(crate) id: HandleId,
            marker: PhantomData<Rc<()>>,
        }

        impl $name {
            #[allow(dead_code)]
            pub(crate) fn new(id: HandleId) -> Self {
                Self {
                    id,
                    marker: PhantomData,
                }
            }

            /// Validates this non-owning handle without executing callbacks.
            pub fn validate(self, runtime: &Runtime) -> Result<(), RuntimeError> {
                runtime.$validate(self.id)
            }
        }
    };
}

untyped_handle!(EffectHandle, validate_effect, "Non-owning effect handle.");
untyped_handle!(
    OwnerHandle,
    validate_owner,
    "Non-owning explicit lifetime scope handle."
);
