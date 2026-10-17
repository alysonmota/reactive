use std::{cell::RefCell, rc::Rc};

use super::{EvaluationGuard, PanicPayload, Runtime};
use crate::{
    EvaluationPhase, Memo, RuntimeError, RuntimeStatus, State,
    arena::SlotId,
    context::{CallbackKind, CapabilityGuard, Operation, ReaderRuntimeGuard},
    handle::HandleId,
    node::{Comparator, MemoCompute, NodeKind, Payload, ReadRecord, Value, ValueType},
};

struct NewMemo<F, C> {
    compute: Option<F>,
    comparator: Option<C>,
}

struct Execution {
    id: SlotId,
    value_type: &'static ValueType,
    compute: Option<MemoCompute>,
    comparator: Option<Comparator>,
    previous: Option<Value>,
    candidate: Option<Value>,
}

impl Execution {
    fn release(mut self, runtime: &Runtime) -> Option<PanicPayload> {
        // The closure stays outside the catch closure, so compute/compare panic
        // cannot drop it during unwind or silently remove it from a live node.
        {
            let mut storage = runtime.inner.storage.borrow_mut();
            if let Some(node) = storage.nodes.get_mut(self.id)
                && let Payload::Memo { compute, value, .. } = &mut node.payload
            {
                if compute.is_none() {
                    *compute = self.compute.take();
                }
                // On failure, preserve the old cache for eventual guarded
                // teardown. Never install it into a disposed/reused generation.
                if value.is_none() {
                    *value = self.previous.take();
                }
            }
        }
        let mut first = None;
        runtime.merge_panic(&mut first, runtime.release_option(self.compute.take()));
        runtime.merge_panic(
            &mut first,
            runtime.release_value_slot(self.candidate.take(), self.value_type),
        );
        runtime.merge_panic(
            &mut first,
            runtime.release_value_slot(self.previous.take(), self.value_type),
        );
        runtime.merge_panic(&mut first, runtime.release_shared(self.comparator.take()));
        if runtime.status() != RuntimeStatus::Healthy {
            let failure = runtime.first_failure();
            if let Some(node) = runtime.inner.storage.borrow_mut().nodes.get_mut(self.id) {
                node.metadata.state = State::Dirty;
                node.metadata.failure = failure;
                node.reads.clear();
            }
        }
        first
    }
}

impl Runtime {
    /// Creates a pure, lazy memo in the current explicit owner. No computation
    /// runs until a read; FnMut permits private derivation caches, not writes.
    pub fn memo<T: PartialEq + 'static>(
        &self,
        compute: impl FnMut(&Runtime) -> Result<T, RuntimeError> + 'static,
    ) -> Result<Memo<T>, RuntimeError> {
        self.memo_with(compute, T::eq)
    }

    /// A lazy memo with an explicit pure equality policy. Equal results retain
    /// the previous value slot but still reconcile dynamic dependencies.
    pub fn memo_with<T: 'static>(
        &self,
        compute: impl FnMut(&Runtime) -> Result<T, RuntimeError> + 'static,
        comparator: impl Fn(&T, &T) -> bool + 'static,
    ) -> Result<Memo<T>, RuntimeError> {
        self.resource_boundary(
            NewMemo {
                compute: Some(compute),
                comparator: Some(comparator),
            },
            |inputs| {
                self.check(Operation::Create)?;
                let owner = self
                    .inner
                    .context
                    .snapshot()
                    .owner
                    .ok_or(RuntimeError::MissingOwner)?;
                if self.inner.storage.borrow().owners.get(owner).is_none() {
                    return Err(RuntimeError::OwnerDisposed);
                }
                let mut compute = inputs.compute.take().expect("compute");
                // Two reusable typed slots: current and provisional candidate.
                // No per-evaluation Rc allocation, and no T crosses erasure by value.
                let slots: [Value; 2] = [
                    Rc::new(RefCell::new(None::<T>)),
                    Rc::new(RefCell::new(None::<T>)),
                ];
                self.insert_node(Payload::Memo {
                    value_type: &const { ValueType::of::<T>() },
                    value: None,
                    compute: Some(Box::new(move |runtime, previous| {
                        let index =
                            usize::from(previous.is_some_and(|value| Rc::ptr_eq(value, &slots[0])));
                        let candidate = compute(runtime)?;
                        let cell = slots[index]
                            .downcast_ref::<RefCell<Option<T>>>()
                            .expect("typed candidate slot");
                        debug_assert!(cell.borrow().is_none());
                        *cell.borrow_mut() = Some(candidate);
                        Ok(slots[index].clone())
                    })),
                    equals: Payload::comparator(inputs.comparator.take().expect("comparator")),
                })
                .map(Memo::new)
            },
            |inputs| {
                let mut first = None;
                self.merge_panic(&mut first, self.release_one(inputs.compute));
                self.merge_panic(&mut first, self.release_one(inputs.comparator));
                first
            },
        )
    }

    pub fn memo_infallible<T: PartialEq + 'static>(
        &self,
        mut compute: impl FnMut(&Runtime) -> T + 'static,
    ) -> Result<Memo<T>, RuntimeError> {
        self.memo(move |runtime| Ok(compute(runtime)))
    }

    pub fn memo_infallible_with<T: 'static>(
        &self,
        mut compute: impl FnMut(&Runtime) -> T + 'static,
        comparator: impl Fn(&T, &T) -> bool + 'static,
    ) -> Result<Memo<T>, RuntimeError> {
        self.memo_with(move |runtime| Ok(compute(runtime)), comparator)
    }

    /// Temporarily suspends only the external reader. Nested lazy memos still
    /// install their own tracking contexts; capabilities never become broader.
    pub fn untrack<R>(&self, callback: impl FnOnce(&Runtime) -> R) -> R {
        self.resource_boundary(
            (),
            |_| {
                let _context = self.inner.context.enter(|snapshot| snapshot.reader = None);
                let _reader_runtime = ReaderRuntimeGuard::enter(None);
                callback(self)
            },
            |_| None,
        )
    }

    pub(crate) fn read_memo_copy<T: Copy + 'static>(
        &self,
        id: HandleId,
        tracked: bool,
    ) -> Result<T, RuntimeError> {
        let reader = self.reader_for_read(id, tracked)?;
        if let Some(value) = self.cached_memo_copy::<T>(id.slot, reader)? {
            return Ok(value);
        }
        self.resource_boundary(
            (),
            |_| {
                self.update_if_necessary(id.slot)?;
                self.cached_memo_copy(id.slot, reader)?
                    .ok_or(RuntimeError::InvariantViolation(
                        "memo did not finish clean",
                    ))
            },
            |_| None,
        )
    }

    fn cached_memo_copy<T: Copy + 'static>(
        &self,
        id: SlotId,
        reader: Option<SlotId>,
    ) -> Result<Option<T>, RuntimeError> {
        fn copy<T: Copy + 'static>(node: &crate::node::Node) -> Result<Option<T>, RuntimeError> {
            if node.metadata.phase == EvaluationPhase::Idle
                && node.metadata.state == State::Clean
                && node.metadata.initialized
            {
                node.copy_value::<T>(NodeKind::Memo).map(Some)
            } else {
                node.validate_type::<T>(NodeKind::Memo)?;
                Ok(None)
            }
        }
        if let Some(reader) = reader {
            let mut storage = self.inner.storage.borrow_mut();
            let node = storage.nodes.get(id).ok_or(RuntimeError::InvalidHandle)?;
            let Some(value) = copy(node)? else {
                return Ok(None);
            };
            let record = ReadRecord {
                source: id,
                freshness_revision: node.metadata.freshness_revision,
                value_revision: node.metadata.value_revision,
            };
            Self::record_read(&mut storage, reader, record);
            Ok(Some(value))
        } else {
            self.inner
                .storage
                .borrow()
                .nodes
                .get(id)
                .ok_or(RuntimeError::InvalidHandle)
                .and_then(copy)
        }
    }

    pub(crate) fn read_memo<T: 'static>(
        &self,
        id: HandleId,
        tracked: bool,
    ) -> Result<Rc<RefCell<Option<T>>>, RuntimeError> {
        let reader = self.reader_for_read(id, tracked)?;
        // A clean read resolves storage once, with no evaluation guard, source
        // traversal or duplicate erased type check. Dirty reads validate their
        // declared/cached type before running a user closure.
        if let Some(snapshot) = self.cached_memo::<T>(id.slot, reader)? {
            return Ok(snapshot);
        }
        self.resource_boundary(
            (),
            |_| {
                self.update_if_necessary(id.slot)?;
                self.cached_memo(id.slot, reader)?
                    .ok_or(RuntimeError::InvariantViolation(
                        "memo did not finish clean",
                    ))
            },
            |_| None,
        )
    }

    fn cached_memo<T: 'static>(
        &self,
        id: SlotId,
        reader: Option<SlotId>,
    ) -> Result<Option<Rc<RefCell<Option<T>>>>, RuntimeError> {
        if let Some(reader) = reader {
            let mut storage = self.inner.storage.borrow_mut();
            let node = storage.nodes.get(id).ok_or(RuntimeError::InvalidHandle)?;
            let Some(snapshot) = clean_value_cell::<T>(node)? else {
                return Ok(None);
            };
            let read = ReadRecord {
                source: id,
                freshness_revision: node.metadata.freshness_revision,
                value_revision: node.metadata.value_revision,
            };
            Self::record_read(&mut storage, reader, read);
            Ok(Some(snapshot))
        } else {
            self.inner
                .storage
                .borrow()
                .nodes
                .get(id)
                .ok_or(RuntimeError::InvalidHandle)
                .and_then(clean_value_cell::<T>)
        }
    }

    pub(super) fn fail_evaluation(&self, id: SlotId, error: RuntimeError) {
        self.record_failure(error.clone());
        if let Some(node) = self.inner.storage.borrow_mut().nodes.get_mut(id) {
            node.metadata.state = State::Dirty;
            node.metadata.failure = Some(Rc::new(error));
        }
    }

    pub(super) fn update_if_necessary(&self, id: SlotId) -> Result<(), RuntimeError> {
        self.check(Operation::Read)?;
        let Some(mut guard) = self.enter_evaluation_inner::<true>(id, EvaluationPhase::Checking)?
        else {
            return Ok(());
        };
        let result = self.update_memo(id, &mut guard);
        if let Err(error) = &result {
            self.fail_evaluation(id, error.clone());
        }
        result
    }

    pub(super) fn check_computation_sources(&self, id: SlotId) -> Result<bool, RuntimeError> {
        // The fast path below runs no user code, so this entry check remains
        // valid until a slow source evaluation; that path rechecks afterward.
        self.check(Operation::Read)?;
        let mut storage = self.inner.storage.borrow_mut();
        let mut cursor = 0;
        loop {
            let edge = {
                let node = storage.nodes.get(id).ok_or(RuntimeError::InvalidHandle)?;
                if node.metadata.state != State::Check || cursor == node.sources.len() {
                    let dirty = node.metadata.state == State::Dirty || !node.metadata.initialized;
                    if !dirty && node.metadata.state != State::Clean {
                        storage
                            .nodes
                            .get_mut(id)
                            .expect("validated reader")
                            .metadata
                            .state = State::Clean;
                    }
                    return Ok(dirty);
                }
                let edge = node.sources[cursor];
                #[cfg(feature = "diagnostics")]
                {
                    storage.diagnostics.counters.source_checks =
                        storage.diagnostics.counters.source_checks.saturating_add(1);
                }
                let source = storage
                    .nodes
                    .get(edge.source)
                    .ok_or(RuntimeError::InvalidHandle)?;
                if matches!(source.payload.kind(), NodeKind::Signal | NodeKind::Memo)
                    && source.metadata.state == State::Clean
                    && source.metadata.phase == EvaluationPhase::Idle
                    && source.metadata.initialized
                {
                    let revision = source.metadata.value_revision;
                    if revision == edge.value_revision {
                        // Reader is still Check: no user code has run since
                        // its validation. Matching revisions need no mutable
                        // reader resolution or redundant metadata write.
                        cursor += 1;
                        continue;
                    }
                    let node = storage
                        .nodes
                        .get_mut(id)
                        .ok_or(RuntimeError::InvalidHandle)?;
                    if observe_source_revision(node, edge.value_revision, revision)? {
                        return Ok(true);
                    }
                    cursor += 1;
                    continue;
                }
                // Only an ID/revision copy escapes. Drop the graph borrow
                // before updating a source: compute/cleanup may reenter it.
                edge
            };
            drop(storage);
            self.update_if_necessary(edge.source)?;
            self.check(Operation::Read)?;
            storage = self.inner.storage.borrow_mut();
            let revision = storage
                .nodes
                .get(edge.source)
                .ok_or(RuntimeError::InvalidHandle)?
                .metadata
                .value_revision;
            let node = storage
                .nodes
                .get_mut(id)
                .ok_or(RuntimeError::InvalidHandle)?;
            if observe_source_revision(node, edge.value_revision, revision)? {
                return Ok(true);
            }
            cursor += 1;
        }
    }

    fn update_memo(&self, id: SlotId, guard: &mut EvaluationGuard<'_>) -> Result<(), RuntimeError> {
        if !self.check_computation_sources(id)? {
            return Ok(());
        }
        guard.set_phase(EvaluationPhase::Cleaning)?;
        if !self.prepare_execution(id)? {
            return Err(RuntimeError::InvalidHandle);
        }
        // set_phase performs the post-cleanup runtime/capability check; no user
        // code intervenes, so an immediately preceding duplicate check is unnecessary.
        guard.set_phase(EvaluationPhase::Executing)?;
        let inputs = {
            let mut storage = self.inner.storage.borrow_mut();
            let node = storage
                .nodes
                .get_mut(id)
                .ok_or(RuntimeError::InvalidHandle)?;
            let Payload::Memo {
                value_type,
                value,
                compute,
                equals,
                ..
            } = &mut node.payload
            else {
                unreachable!()
            };
            Execution {
                id,
                value_type,
                compute: Some(
                    compute
                        .take()
                        .ok_or(RuntimeError::InvariantViolation("missing memo closure"))?,
                ),
                // Active memo reads reject cycles. Transfer the current slot
                // to execution resources until commit or failure restoration.
                previous: value.take(),
                comparator: Some(equals.clone()),
                candidate: None,
            }
        };
        self.resource_boundary(
            inputs,
            |inputs| {
                let result = self.execute_memo(inputs);
                if let Err(error) = &result {
                    self.fail_evaluation(id, error.clone());
                }
                result
            },
            |inputs| inputs.release(self),
        )
    }

    fn execute_memo(&self, inputs: &mut Execution) -> Result<(), RuntimeError> {
        #[cfg(feature = "diagnostics")]
        {
            let mut storage = self.inner.storage.borrow_mut();
            storage.diagnostics.counters.memo_executions = storage
                .diagnostics
                .counters
                .memo_executions
                .saturating_add(1);
        }
        let candidate = inputs.compute.as_mut().expect("compute")(self, inputs.previous.as_ref())?;
        inputs.candidate = Some(candidate);
        self.check(Operation::Read)?;
        let expected = self
            .inner
            .storage
            .borrow()
            .nodes
            .get(inputs.id)
            .ok_or(RuntimeError::InvalidHandle)?
            .payload
            .value_type()
            .expect("memo type");
        if inputs
            .candidate
            .as_ref()
            .expect("candidate")
            .as_ref()
            .type_id()
            != inputs.value_type.storage_id
            || expected != inputs.value_type.id
        {
            return Err(RuntimeError::TypeMismatch);
        }
        let changed = if let Some(previous) = &inputs.previous {
            let _context = self.inner.context.enter(|snapshot| snapshot.reader = None);
            let _reader_runtime = ReaderRuntimeGuard::enter(None);
            let _capabilities = CapabilityGuard::enter(CallbackKind::Comparator);
            !inputs.comparator.as_ref().expect("comparator")(
                previous.as_ref(),
                inputs.candidate.as_ref().expect("candidate").as_ref(),
            )?
        } else {
            true
        };
        self.check(Operation::Read)?;
        {
            let mut storage = self.inner.storage.borrow_mut();
            let node = storage
                .nodes
                .get(inputs.id)
                .ok_or(RuntimeError::InvalidHandle)?;
            let revisions = if changed {
                Some((
                    node.metadata
                        .value_revision
                        .checked_add(1)
                        .ok_or(RuntimeError::CounterExhausted)?,
                    node.metadata
                        .freshness_revision
                        .checked_add(1)
                        .ok_or(RuntimeError::CounterExhausted)?,
                ))
            } else {
                None
            };
            Self::reconcile_sources(&mut storage, inputs.id)?;
            let node = storage.nodes.get_mut(inputs.id).expect("reader validated");
            if let Some((value_revision, freshness_revision)) = revisions {
                let Payload::Memo { value, .. } = &mut node.payload else {
                    unreachable!()
                };
                *value = inputs.candidate.take();
                node.metadata.value_revision = value_revision;
                node.metadata.freshness_revision = freshness_revision;
            } else {
                let Payload::Memo { value, .. } = &mut node.payload else {
                    unreachable!()
                };
                *value = inputs.previous.take();
            }
            node.metadata.initialized = true;
            node.metadata.state = State::Clean;
            if changed {
                Self::mark_observers(&mut storage, inputs.id)
            } else {
                Ok(())
            }
        }
    }
}

/// Shared by fast validation and post-callback revalidation. Stop at the first
/// changed source; effects do not publish a memo freshness revision.
fn observe_source_revision(
    node: &mut crate::node::Node,
    expected: u64,
    actual: u64,
) -> Result<bool, RuntimeError> {
    if actual != expected && node.metadata.state != State::Dirty {
        if node.payload.kind() == NodeKind::Memo {
            node.metadata.freshness_revision = node
                .metadata
                .freshness_revision
                .checked_add(1)
                .ok_or(RuntimeError::CounterExhausted)?;
        }
        node.metadata.state = State::Dirty;
    }
    Ok(node.metadata.state == State::Dirty)
}

fn clean_value_cell<T: 'static>(
    node: &crate::node::Node,
) -> Result<Option<Rc<RefCell<Option<T>>>>, RuntimeError> {
    if node.metadata.phase == EvaluationPhase::Idle
        && node.metadata.state == State::Clean
        && node.metadata.initialized
    {
        node.value_cell::<T>(NodeKind::Memo).map(Some)
    } else {
        node.validate_type::<T>(NodeKind::Memo)?;
        Ok(None)
    }
}

impl EvaluationGuard<'_> {
    pub(super) fn set_phase(&mut self, phase: EvaluationPhase) -> Result<(), RuntimeError> {
        self.runtime.check(Operation::Read)?;
        let mut storage = self.runtime.inner.storage.borrow_mut();
        if phase == EvaluationPhase::Executing {
            Runtime::start_tracking(&mut storage, self.id)?;
            self.tracking = true;
        }
        let node = storage
            .nodes
            .get_mut(self.id)
            .ok_or(RuntimeError::InvalidHandle)?;
        node.metadata.phase = phase;
        let owner = node.execution_owner.unwrap_or(node.owner);
        #[cfg(feature = "diagnostics")]
        if let Some(frame) = storage.diagnostics.active.last_mut() {
            frame.1 = phase;
        }
        drop(storage);
        let executing = phase == EvaluationPhase::Executing;
        self.runtime.inner.context.modify(|snapshot| {
            snapshot.reader = executing.then_some(self.id);
            if executing {
                snapshot.owner = Some(owner);
            }
        });
        self._reader_runtime
            .set(executing.then_some(self.runtime.inner.id));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context;

    #[test]
    fn ascent_panic_restores_suspended_context_capabilities_and_reader_runtime() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u32)).unwrap();
        let mut tail = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    assert_eq!(input.try_get_copy(rt)?, 0, "intentional upstream panic");
                    Ok(0_u32)
                })
            })
            .unwrap();
        let mut chain = vec![tail];
        for _ in 0..16 {
            let previous = tail;
            tail = rt
                .with_owner(root, |rt| {
                    rt.memo(move |rt| Ok(previous.try_get_copy(rt)? + 1))
                })
                .unwrap();
            chain.push(tail);
        }
        assert_eq!(tail.try_get_copy(&rt), Ok(16));
        input.try_set(&rt, 1).unwrap();
        let before = rt.inner.context.snapshot();
        let panic =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tail.try_get_copy(&rt)));
        assert!(panic.is_err());
        assert_eq!(rt.inner.context.snapshot(), before);
        assert_eq!(rt.status(), RuntimeStatus::Poisoned);
        let storage = rt.inner.storage.borrow();
        for memo in chain {
            let node = storage.nodes.get(memo.id.slot).unwrap();
            assert_eq!(node.metadata.phase, EvaluationPhase::Idle);
            assert_eq!(node.metadata.state, State::Dirty);
        }
        #[cfg(feature = "diagnostics")]
        assert!(storage.diagnostics.active.is_empty());
        drop(storage);
        let foreign = Runtime::new();
        let owner = foreign.create_root().unwrap();
        let value = foreign.with_owner(owner, |rt| rt.signal(11_u32)).unwrap();
        assert_eq!(value.try_get_copy(&foreign), Ok(11));
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn check_entry_skips_clean_values_before_depth_limit_and_rejects_invalid_kinds() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let (signal, memo, effect) = rt
            .with_owner(root, |rt| {
                Ok((
                    rt.signal(7_u32)?,
                    rt.memo(|_| Ok(8_u32))?,
                    rt.effect(|_| Ok(()))?,
                ))
            })
            .unwrap();
        memo.try_get_copy(&rt).unwrap();
        let snapshot = rt.inner.context.snapshot();
        {
            let _depth = rt.inner.context.enter(|s| s.evaluation_depth = 512);
            assert_eq!(rt.update_if_necessary(signal.id.slot), Ok(()));
            assert_eq!(rt.update_if_necessary(memo.id.slot), Ok(()));
            assert_eq!(
                rt.update_if_necessary(effect.id.slot),
                Err(RuntimeError::TypeMismatch)
            );
        }
        rt.dispose(root).unwrap();
        assert_eq!(
            rt.update_if_necessary(memo.id.slot),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(rt.status(), crate::RuntimeStatus::Healthy);
        assert_eq!(rt.inner.context.snapshot(), snapshot);
        rt.flush(Default::default()).unwrap();
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn check_entry_cycle_poison_restores_existing_guard_and_depth_failure_leaves_idle() {
        for cycle in [false, true] {
            let rt = Runtime::new();
            let root = rt.create_root().unwrap();
            let memo = rt.with_owner(root, |rt| rt.memo(|_| Ok(7_u32))).unwrap();
            let snapshot = rt.inner.context.snapshot();
            if cycle {
                let _guard = rt
                    .enter_evaluation(memo.id, EvaluationPhase::Checking)
                    .unwrap();
                assert_eq!(
                    rt.update_if_necessary(memo.id.slot),
                    Err(RuntimeError::Cycle)
                );
            } else {
                let _depth = rt.inner.context.enter(|s| s.evaluation_depth = 512);
                assert_eq!(
                    rt.update_if_necessary(memo.id.slot),
                    Err(RuntimeError::DepthLimit)
                );
            }
            assert_eq!(rt.inner.context.snapshot(), snapshot);
            assert_eq!(rt.status(), crate::RuntimeStatus::Poisoned);
            let storage = rt.inner.storage.borrow();
            assert_eq!(
                storage.nodes.get(memo.id.slot).unwrap().metadata.phase,
                EvaluationPhase::Idle
            );
            #[cfg(feature = "diagnostics")]
            assert!(storage.diagnostics.active.is_empty());
            drop(storage);
            rt.verify_invariants().unwrap();
        }
    }

    #[test]
    fn check_entry_equality_cut_restores_nested_context_without_recomputing_tail() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let runs = Rc::new(Cell::new(0));
        let calls = runs.clone();
        let (input, tail) = rt
            .with_owner(root, |rt| {
                let input = rt.signal(0_u32)?;
                let head = rt.memo(move |rt| {
                    input.try_get_copy(rt)?;
                    Ok(7_u32)
                })?;
                let tail = rt.memo(move |rt| {
                    calls.set(calls.get() + 1);
                    Ok(head.try_get_copy(rt)? + 1)
                })?;
                Ok((input, tail))
            })
            .unwrap();
        assert_eq!(tail.try_get_copy(&rt), Ok(8));
        for value in 1..=4 {
            input.try_set(&rt, value).unwrap();
            let _owner = rt.inner.context.enter(|s| {
                s.owner = Some(root.id.slot);
                s.batch_depth = 2;
            });
            let before = rt.inner.context.snapshot();
            assert_eq!(tail.try_get_copy(&rt), Ok(8));
            assert_eq!(rt.inner.context.snapshot(), before);
            assert_eq!(runs.get(), 1);
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(tail.id.slot).unwrap();
            assert_eq!(
                (node.metadata.phase, node.metadata.state),
                (EvaluationPhase::Idle, State::Clean)
            );
            #[cfg(feature = "diagnostics")]
            assert!(storage.diagnostics.active.is_empty());
        }
        rt.verify_invariants().unwrap();
    }
    use crate::{OwnerHandle, Signal};
    use std::cell::Cell;

    #[test]
    fn memo_reuses_two_slots_without_retaining_retired_values() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let memo = rt
            .with_owner(root, |rt| rt.memo(move |rt| input.try_get_copy(rt)))
            .unwrap();
        assert_eq!(memo.try_get_copy(&rt).unwrap(), 0);
        let first = rt
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .value_cell::<u64>(NodeKind::Memo)
            .unwrap();
        input.try_set(&rt, 1).unwrap();
        assert_eq!(memo.try_get_copy(&rt).unwrap(), 1);
        assert!(first.borrow().is_none());
        input.try_set(&rt, 2).unwrap();
        assert_eq!(memo.try_get_copy(&rt).unwrap(), 2);
        let third = rt
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .value_cell::<u64>(NodeKind::Memo)
            .unwrap();
        assert!(Rc::ptr_eq(&first, &third));
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn moved_cache_is_restored_after_comparator_panic_and_equal_snapshot_keeps_identity() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let panic = Rc::new(Cell::new(false));
        let captured = panic.clone();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo_with(
                    move |rt| Ok(input.try_get_copy(rt)? / 2),
                    move |a, b| {
                        assert!(!captured.get(), "comparator failure");
                        a == b
                    },
                )
            })
            .unwrap();
        let old = memo.try_get_copy(&rt).unwrap();
        let old_cell = rt
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .value_cell::<u64>(NodeKind::Memo)
            .unwrap();
        input.try_set(&rt, 1).unwrap();
        assert_eq!(old, memo.try_get_copy(&rt).unwrap());
        assert!(Rc::ptr_eq(
            &old_cell,
            &rt.inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .value_cell::<u64>(NodeKind::Memo)
                .unwrap()
        ));
        panic.set(true);
        input.try_set(&rt, 2).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| memo.try_get_copy(&rt)))
                .is_err()
        );
        let storage = rt.inner.storage.borrow();
        let node = storage.nodes.get(memo.id.slot).unwrap();
        assert!(Rc::ptr_eq(
            &old_cell,
            &node.value_cell::<u64>(NodeKind::Memo).unwrap()
        ));
        assert_eq!(old, *node.snapshot::<u64>(NodeKind::Memo).unwrap());
        assert_eq!(node.metadata.phase, EvaluationPhase::Idle);
        assert_eq!(node.metadata.state, State::Dirty);
        drop(storage);
        rt.verify_invariants().unwrap();
    }

    fn setup() -> (Runtime, OwnerHandle) {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        (runtime, root)
    }

    #[test]
    fn executing_transition_still_checks_poison_before_starting_tracking() {
        let (rt, root) = setup();
        let value = memo(&rt, root, |_| Ok(1_u64));
        value.try_get_copy(&rt).unwrap();
        let mut guard = rt
            .enter_evaluation(value.id, EvaluationPhase::Cleaning)
            .unwrap();
        let epoch = rt.inner.storage.borrow().next_execution_epoch;
        rt.record_failure(RuntimeError::InvariantViolation("failure during cleanup"));
        assert_eq!(
            guard.set_phase(EvaluationPhase::Executing),
            Err(RuntimeError::Poisoned)
        );
        assert_eq!(rt.inner.storage.borrow().next_execution_epoch, epoch);
        assert!(rt.inner.storage.borrow().tracking_stack.is_empty());
        assert_eq!(
            rt.inner
                .storage
                .borrow()
                .nodes
                .get(value.id.slot)
                .unwrap()
                .metadata
                .phase,
            EvaluationPhase::Cleaning
        );
        drop(guard);
        rt.verify_invariants().unwrap();
    }

    fn signal<T: PartialEq + 'static>(runtime: &Runtime, root: OwnerHandle, value: T) -> Signal<T> {
        runtime
            .with_owner(root, |runtime| runtime.signal(value))
            .unwrap()
    }

    fn memo<T: PartialEq + 'static>(
        runtime: &Runtime,
        root: OwnerHandle,
        compute: impl FnMut(&Runtime) -> Result<T, RuntimeError> + 'static,
    ) -> Memo<T> {
        runtime
            .with_owner(root, |runtime| runtime.memo(compute))
            .unwrap()
    }

    fn sources<T>(runtime: &Runtime, memo: Memo<T>) -> Vec<SlotId> {
        runtime
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .sources
            .iter()
            .filter(|edge| edge.live)
            .map(|edge| edge.source)
            .collect()
    }

    #[test]
    fn clean_idle_sources_validate_without_reexecuting_equal_callbacks() {
        let (rt, root) = setup();
        let input = signal(&rt, root, 0_u64);
        let leaf_calls = Rc::new(Cell::new(0));
        let calls = leaf_calls.clone();
        let leaf = memo(&rt, root, move |rt| {
            calls.set(calls.get() + 1);
            Ok(input.try_get_copy(rt)? % 2)
        });
        let terminal_calls = Rc::new(Cell::new(0));
        let calls = terminal_calls.clone();
        let terminal = memo(&rt, root, move |rt| {
            calls.set(calls.get() + 1);
            leaf.try_get_copy(rt)
        });
        assert_eq!(terminal.try_get_copy(&rt).unwrap(), 0);
        input.try_set(&rt, 2).unwrap();
        assert_eq!(leaf.try_peek_copy(&rt).unwrap(), 0); // source is already Clean
        let before = rt
            .inner
            .storage
            .borrow()
            .nodes
            .get(terminal.id.slot)
            .unwrap()
            .metadata
            .freshness_revision;
        assert_eq!(terminal.try_get_copy(&rt).unwrap(), 0);
        assert_eq!(leaf_calls.get(), 2);
        assert_eq!(terminal_calls.get(), 1);
        assert_eq!(
            rt.inner
                .storage
                .borrow()
                .nodes
                .get(terminal.id.slot)
                .unwrap()
                .metadata
                .freshness_revision,
            before
        );
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn clean_revision_mismatch_stops_before_unused_dirty_source() {
        let (rt, root) = setup();
        let input = signal(&rt, root, 0_u64);
        let unused_input = signal(&rt, root, 0_u64);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let unused = memo(&rt, root, move |rt| {
            captured.set(captured.get() + 1);
            unused_input.try_get_copy(rt)
        });
        let terminal = memo(&rt, root, move |rt| {
            Ok(input.try_get_copy(rt)? + unused.try_get_copy(rt)?)
        });
        terminal.try_get_copy(&rt).unwrap();
        input.try_set(&rt, 1).unwrap();
        unused_input.try_set(&rt, 1).unwrap();
        let before = {
            let mut storage = rt.inner.storage.borrow_mut();
            let node = storage.nodes.get_mut(terminal.id.slot).unwrap();
            node.metadata.state = State::Check; // exercise revision checking directly
            node.metadata.freshness_revision
        };
        assert!(rt.check_computation_sources(terminal.id.slot).unwrap());
        assert_eq!(calls.get(), 1);
        let storage = rt.inner.storage.borrow();
        let node = storage.nodes.get(terminal.id.slot).unwrap();
        assert_eq!(node.metadata.state, State::Dirty);
        assert_eq!(node.metadata.freshness_revision, before + 1);
        drop(storage);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn clean_source_with_active_phase_keeps_cycle_and_poison_path() {
        for phase in [
            EvaluationPhase::Checking,
            EvaluationPhase::Cleaning,
            EvaluationPhase::Executing,
        ] {
            let (rt, root) = setup();
            let leaf = memo(&rt, root, |_| Ok(1_u64));
            let terminal = memo(&rt, root, move |rt| leaf.try_get_copy(rt));
            terminal.try_get_copy(&rt).unwrap();
            {
                let mut storage = rt.inner.storage.borrow_mut();
                storage.nodes.get_mut(leaf.id.slot).unwrap().metadata.phase = phase;
                storage
                    .nodes
                    .get_mut(terminal.id.slot)
                    .unwrap()
                    .metadata
                    .state = State::Check;
            }
            assert_eq!(terminal.try_get_copy(&rt), Err(RuntimeError::Cycle));
            assert_eq!(rt.status(), RuntimeStatus::Poisoned);
            assert_eq!(rt.first_failure().as_deref(), Some(&RuntimeError::Cycle));
        }
    }

    #[test]
    fn clean_but_uninitialized_source_takes_full_evaluation_path_without_borrow() {
        let (rt, root) = setup();
        let input = signal(&rt, root, 1_u64);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let leaf = memo(&rt, root, move |rt| {
            captured.set(captured.get() + 1);
            // Both read and untrack touch runtime storage/context during compute.
            rt.untrack(|rt| input.try_get_copy(rt))
        });
        let terminal = memo(&rt, root, move |rt| leaf.try_get_copy(rt));
        terminal.try_get_copy(&rt).unwrap();
        {
            let mut storage = rt.inner.storage.borrow_mut();
            storage
                .nodes
                .get_mut(leaf.id.slot)
                .unwrap()
                .metadata
                .initialized = false;
            storage
                .nodes
                .get_mut(terminal.id.slot)
                .unwrap()
                .metadata
                .state = State::Check;
        }
        assert_eq!(terminal.try_get_copy(&rt).unwrap(), 1);
        assert_eq!(calls.get(), 2);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn clean_source_revision_overflow_and_poison_are_not_bypassed() {
        let (rt, root) = setup();
        let input = signal(&rt, root, 0_u64);
        let terminal = memo(&rt, root, move |rt| input.try_get_copy(rt));
        terminal.try_get_copy(&rt).unwrap();
        input.try_set(&rt, 1).unwrap();
        {
            let mut storage = rt.inner.storage.borrow_mut();
            let node = storage.nodes.get_mut(terminal.id.slot).unwrap();
            node.metadata.state = State::Check;
            node.metadata.freshness_revision = u64::MAX;
        }
        assert_eq!(
            terminal.try_get_copy(&rt),
            Err(RuntimeError::CounterExhausted)
        );
        assert_eq!(rt.status(), RuntimeStatus::Poisoned);
        assert_eq!(
            rt.check_computation_sources(terminal.id.slot),
            Err(RuntimeError::Poisoned)
        );
    }

    #[test]
    fn initialized_clean_effect_is_not_accepted_as_value_source() {
        let (rt, root) = setup();
        let effect = rt.with_owner(root, |rt| rt.effect(|_| Ok(()))).unwrap();
        rt.flush(Default::default()).unwrap();
        let terminal = memo(&rt, root, |_| Ok(1_u64));
        terminal.try_get_copy(&rt).unwrap();
        {
            // Deliberately malformed committed edge to exercise the kind guard;
            // public tracked reads cannot produce an effect-as-source edge.
            let mut storage = rt.inner.storage.borrow_mut();
            Runtime::add_source(&mut storage, terminal.id.slot, effect.id.slot, 0);
            storage
                .nodes
                .get_mut(terminal.id.slot)
                .unwrap()
                .metadata
                .state = State::Check;
        }
        assert_eq!(terminal.try_get_copy(&rt), Err(RuntimeError::TypeMismatch));
        assert_eq!(rt.status(), RuntimeStatus::Poisoned);
    }

    #[test]
    fn lazy_initialization_clean_fast_path_and_immediate_freshness() {
        let (runtime, root) = setup();
        let count = signal(&runtime, root, 2);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let memo = memo(&runtime, root, move |runtime| {
            assert!(runtime.inner.storage.try_borrow_mut().is_ok());
            let scope = runtime.current_owner()?.unwrap();
            assert_ne!(scope, root);
            assert_eq!(runtime.owner_depth(scope)?, 1);
            captured.set(captured.get() + 1);
            Ok(count.try_get_copy(runtime)? * 3)
        });
        assert_eq!(calls.get(), 0);
        assert!(
            !runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .metadata
                .initialized
        );
        let first = memo.try_get_copy(&runtime).unwrap();
        assert_eq!(first, 6);
        let epoch = runtime.inner.storage.borrow().next_execution_epoch;
        for _ in 0..100 {
            assert_eq!(first, memo.try_peek_copy(&runtime).unwrap());
        }
        assert_eq!(runtime.inner.storage.borrow().next_execution_epoch, epoch);
        assert_eq!(calls.get(), 1);
        count.try_set(&runtime, 3).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(memo.try_peek_copy(&runtime).unwrap(), 9);
        count.try_set(&runtime, 4).unwrap();
        assert_eq!(memo.try_get_copy(&runtime).unwrap(), 12);
        assert_eq!(first, 6);
        assert_eq!(calls.get(), 3);
        assert!(runtime.inner.storage.borrow().tracking_stack.is_empty());
        runtime.verify_graph();
    }

    #[test]
    fn diamond_has_no_glitches_and_recomputes_each_node_once() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let calls = Rc::new(Cell::new(0));
        let left_calls = calls.clone();
        let left = memo(&runtime, root, move |runtime| {
            left_calls.set(left_calls.get() + 1);
            Ok(input.try_get_copy(runtime)? + 1)
        });
        let right_calls = calls.clone();
        let right = memo(&runtime, root, move |runtime| {
            right_calls.set(right_calls.get() + 1);
            Ok(input.try_get_copy(runtime)? * 2)
        });
        let sum = memo(&runtime, root, move |runtime| {
            Ok(left.try_get_copy(runtime)? + right.try_get_copy(runtime)?)
        });
        assert_eq!(sum.try_get_copy(&runtime).unwrap(), 4);
        assert_eq!(sources(&runtime, sum), [left.id.slot, right.id.slot]);
        input.try_set(&runtime, 2).unwrap();
        assert_eq!(sum.try_get_copy(&runtime).unwrap(), 7);
        assert_eq!(calls.get(), 4);
        runtime.verify_graph();
    }

    #[test]
    fn equality_cut_stops_downstream_and_retains_snapshot() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let parity = memo(&runtime, root, move |runtime| {
            Ok(input.try_get_copy(runtime)? % 2)
        });
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let downstream = memo(&runtime, root, move |runtime| {
            captured.set(captured.get() + 1);
            Ok(parity.try_get_copy(runtime)? + 10)
        });
        let first = downstream.try_get_copy(&runtime).unwrap();
        let parity_first = parity.try_get_copy(&runtime).unwrap();
        input.try_set(&runtime, 3).unwrap();
        assert_eq!(first, downstream.try_get_copy(&runtime).unwrap());
        assert_eq!(parity_first, parity.try_get_copy(&runtime).unwrap());
        assert_eq!(calls.get(), 1);
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(parity.id.slot)
                .unwrap()
                .metadata
                .value_revision,
            1
        );
        runtime.verify_graph();
    }

    #[test]
    fn check_detects_source_value_changed_by_an_earlier_independent_lazy_read() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let inner = memo(&runtime, root, move |runtime| {
            Ok(input.try_get_copy(runtime)? * 2)
        });
        let outer = memo(&runtime, root, move |runtime| {
            Ok(inner.try_get_copy(runtime)? + 1)
        });
        assert_eq!(outer.try_get_copy(&runtime).unwrap(), 3);
        input.try_set(&runtime, 5).unwrap();
        assert_eq!(inner.try_peek_copy(&runtime).unwrap(), 10);
        assert_eq!(outer.try_get_copy(&runtime).unwrap(), 11);
        runtime.verify_graph();
    }

    #[test]
    fn conditional_equal_result_reconciles_edges_and_keeps_stable_prefix() {
        let (runtime, root) = setup();
        let branch = signal(&runtime, root, true);
        let left = signal(&runtime, root, 1);
        let right = signal(&runtime, root, 1);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let chosen = memo(&runtime, root, move |runtime| {
            captured.set(captured.get() + 1);
            if branch.try_get_copy(runtime)? {
                Ok(left.try_get_copy(runtime)?)
            } else {
                Ok(right.try_get_copy(runtime)?)
            }
        });
        let first = chosen.try_get_copy(&runtime).unwrap();
        let prefix_index = runtime
            .inner
            .storage
            .borrow()
            .nodes
            .get(chosen.id.slot)
            .unwrap()
            .sources[0]
            .observer_index;
        branch.try_set(&runtime, false).unwrap();
        assert_eq!(first, chosen.try_get_copy(&runtime).unwrap());
        assert_eq!(sources(&runtime, chosen), [branch.id.slot, right.id.slot]);
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(chosen.id.slot)
                .unwrap()
                .sources[0]
                .observer_index,
            prefix_index
        );
        left.try_set(&runtime, 99).unwrap();
        chosen.try_get_copy(&runtime).unwrap();
        assert_eq!(calls.get(), 2);
        right.try_set(&runtime, 5).unwrap();
        assert_eq!(chosen.try_get_copy(&runtime).unwrap(), 5);
        runtime.verify_graph();
    }

    #[test]
    fn repeated_reads_do_not_advance_prefix_and_reordering_rebuilds_only_suffix() {
        let (runtime, root) = setup();
        let reverse = signal(&runtime, root, false);
        let a = signal(&runtime, root, 10);
        let b = signal(&runtime, root, 20);
        let value = memo(&runtime, root, move |runtime| {
            let reverse = reverse.try_get_copy(runtime)?;
            if reverse {
                b.try_get_copy(runtime)?;
                a.try_get_copy(runtime)?;
                a.try_get_copy(runtime)?;
            } else {
                a.try_get_copy(runtime)?;
                a.try_get_copy(runtime)?;
                b.try_get_copy(runtime)?;
            }
            Ok(0)
        });
        value.try_get_copy(&runtime).unwrap();
        assert_eq!(
            sources(&runtime, value),
            [reverse.id.slot, a.id.slot, b.id.slot]
        );
        a.try_set(&runtime, 11).unwrap();
        value.try_get_copy(&runtime).unwrap();
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(value.id.slot)
                .unwrap()
                .tracking
                .stable_prefix_cursor,
            3
        );
        reverse.try_set(&runtime, true).unwrap();
        value.try_get_copy(&runtime).unwrap();
        assert_eq!(
            sources(&runtime, value),
            [reverse.id.slot, b.id.slot, a.id.slot]
        );
        runtime.verify_graph();
    }

    #[test]
    fn high_fan_in_dedup_and_nested_readers_preserve_suspended_context() {
        let (runtime, root) = setup();
        let inputs: Vec<_> = (0..100)
            .map(|value| signal(&runtime, root, value))
            .collect();
        let inputs = Rc::new(inputs);
        let captured = inputs.clone();
        let inner = memo(&runtime, root, move |runtime| {
            let mut sum = 0;
            for input in captured.iter() {
                sum += input.try_get_copy(runtime)?;
                input.try_get_copy(runtime)?;
            }
            Ok(sum)
        });
        let captured = inputs.clone();
        let outer = memo(&runtime, root, move |runtime| {
            let mut sum = captured[0].try_get_copy(runtime)?;
            sum += inner.try_get_copy(runtime)?;
            for input in captured.iter() {
                sum += input.try_get_copy(runtime)?;
            }
            Ok(sum)
        });
        assert_eq!(outer.try_get_copy(&runtime).unwrap(), 9900);
        assert_eq!(sources(&runtime, inner).len(), 100);
        assert_eq!(sources(&runtime, outer).len(), 101);
        inputs[0].try_set(&runtime, 2).unwrap();
        assert_eq!(outer.try_get_copy(&runtime).unwrap(), 9906);
        runtime.verify_graph();
    }

    #[test]
    fn untrack_and_peek_suspend_only_external_tracking() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let trigger = signal(&runtime, root, 1);
        let inner = memo(&runtime, root, move |runtime| input.try_get_copy(runtime));
        let outer = memo(&runtime, root, move |runtime| {
            runtime.untrack(|runtime| inner.try_get_copy(runtime))?;
            inner.try_peek_copy(runtime)?;
            trigger.try_get_copy(runtime)
        });
        outer.try_get_copy(&runtime).unwrap();
        assert_eq!(sources(&runtime, inner), [input.id.slot]);
        assert_eq!(sources(&runtime, outer), [trigger.id.slot]);
        input.try_set(&runtime, 5).unwrap();
        assert_eq!(outer.try_get_copy(&runtime).unwrap(), 1);
        assert_eq!(inner.try_peek_copy(&runtime).unwrap(), 5);
        runtime.verify_graph();
    }

    #[test]
    fn source_disposal_repairs_both_directions_and_invalidates_surviving_readers() {
        let (runtime, root) = setup();
        let other_root = runtime.create_root().unwrap();
        let a = signal(&runtime, root, 1);
        let b = signal(&runtime, other_root, 2);
        let chosen = memo(&runtime, other_root, move |runtime| {
            Ok(a.try_get_copy(runtime)? + b.try_get_copy(runtime)?)
        });
        chosen.try_get_copy(&runtime).unwrap();
        runtime.dispose(root).unwrap();
        assert_eq!(sources(&runtime, chosen), [b.id.slot]);
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(chosen.id.slot)
                .unwrap()
                .metadata
                .state,
            State::Dirty
        );
        runtime.verify_graph();
        assert_eq!(
            chosen.try_get_copy(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn disposing_reader_repairs_swap_removed_observer_back_index() {
        let (runtime, root) = setup();
        let other_root = runtime.create_root().unwrap();
        let input = signal(&runtime, root, 1);
        let first = memo(&runtime, other_root, move |runtime| {
            input.try_get_copy(runtime)
        });
        let second = memo(&runtime, root, move |runtime| {
            Ok(input.try_get_copy(runtime)? + 1)
        });
        first.try_get_copy(&runtime).unwrap();
        second.try_get_copy(&runtime).unwrap();
        runtime.dispose(other_root).unwrap();
        runtime.verify_graph();
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(second.id.slot)
                .unwrap()
                .sources[0]
                .observer_index,
            0
        );
        input.try_set(&runtime, 3).unwrap();
        assert_eq!(second.try_get_copy(&runtime).unwrap(), 4);
    }

    #[test]
    fn many_disposed_sources_leave_only_id_tombstones_until_reader_reconciles() {
        let (runtime, root) = setup();
        let other_root = runtime.create_root().unwrap();
        let inputs: Vec<_> = (0..100)
            .map(|value| signal(&runtime, root, value))
            .collect();
        let survivor = signal(&runtime, other_root, 1);
        let memo = memo(&runtime, other_root, move |runtime| {
            let mut sum = 0;
            for input in &inputs {
                sum += input.try_get_copy(runtime)?;
            }
            Ok(sum + survivor.try_get_copy(runtime)?)
        });
        memo.try_get_copy(&runtime).unwrap();
        runtime.dispose(root).unwrap();
        assert_eq!(sources(&runtime, memo), [survivor.id.slot]);
        let storage = runtime.inner.storage.borrow();
        let node = storage.nodes.get(memo.id.slot).unwrap();
        assert_eq!(node.sources.len(), 101);
        assert!(node.source_holes);
        assert_eq!(node.sources.iter().filter(|edge| !edge.live).count(), 100);
        assert_eq!(
            storage.nodes.get(survivor.id.slot).unwrap().observers[0].source_index,
            100
        );
        drop(storage);
        runtime.verify_graph();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn dynamic_dependency_can_abandon_a_disposed_source() {
        let (runtime, root) = setup();
        let other_root = runtime.create_root().unwrap();
        let input = signal(&runtime, root, 1);
        let enabled = signal(&runtime, other_root, true);
        let chosen = memo(&runtime, other_root, move |runtime| {
            if enabled.try_get_copy(runtime)? {
                Ok(input.try_get_copy(runtime)?)
            } else {
                Ok(0)
            }
        });
        chosen.try_get_copy(&runtime).unwrap();
        enabled.try_set(&runtime, false).unwrap();
        runtime.dispose(root).unwrap();
        assert_eq!(chosen.try_get_copy(&runtime).unwrap(), 0);
        assert_eq!(sources(&runtime, chosen), [enabled.id.slot]);
        runtime.verify_graph();
    }

    #[test]
    fn comparator_failure_does_not_publish_value_or_new_edges() {
        let (runtime, root) = setup();
        let branch = signal(&runtime, root, true);
        let a = signal(&runtime, root, 1);
        let b = signal(&runtime, root, 2);
        let weak = runtime.downgrade();
        let chosen = runtime
            .with_owner(root, |runtime| {
                runtime.memo_with(
                    move |runtime| {
                        if branch.try_get_copy(runtime)? {
                            Ok(a.try_get_copy(runtime)?)
                        } else {
                            Ok(b.try_get_copy(runtime)?)
                        }
                    },
                    move |_: &i32, _: &i32| {
                        let runtime = weak.upgrade().unwrap();
                        assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                        assert_eq!(a.try_get_copy(&runtime), Err(RuntimeError::Reentrancy));
                        assert_eq!(
                            branch.try_set(&runtime, true),
                            Err(RuntimeError::WriteForbidden)
                        );
                        panic!("comparator");
                    },
                )
            })
            .unwrap();
        let old = chosen.try_get_copy(&runtime).unwrap();
        branch.try_set(&runtime, false).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || chosen.try_get_copy(&runtime)
            ))
            .is_err()
        );
        assert_eq!(old, 1);
        assert_eq!(sources(&runtime, chosen), [branch.id.slot, a.id.slot]);
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(chosen.id.slot)
                .unwrap()
                .metadata
                .phase,
            EvaluationPhase::Idle
        );
        runtime.verify_graph();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn compute_error_and_panic_poison_without_publishing_initial_value() {
        for panics in [false, true] {
            let (runtime, root) = setup();
            let input = signal(&runtime, root, 1);
            let failed = memo::<i32>(&runtime, root, move |runtime| {
                input.try_get_copy(runtime)?;
                if panics {
                    panic!("compute")
                }
                Err(RuntimeError::CallbackFailed("compute"))
            });
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                failed.try_get_copy(&runtime)
            }));
            if panics {
                assert!(outcome.is_err());
            } else {
                assert_eq!(
                    outcome.unwrap(),
                    Err(RuntimeError::CallbackFailed("compute"))
                );
            }
            assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
            let storage = runtime.inner.storage.borrow();
            let node = storage.nodes.get(failed.id.slot).unwrap();
            assert!(!node.metadata.initialized);
            assert_eq!(node.metadata.state, State::Dirty);
            assert_eq!(node.metadata.phase, EvaluationPhase::Idle);
            assert!(node.sources.is_empty());
            assert!(node.metadata.failure.is_some());
            assert!(storage.tracking_stack.is_empty());
            drop(storage);
            runtime.verify_graph();
            runtime.shutdown().unwrap();
        }
    }

    #[test]
    fn direct_and_indirect_cycles_are_explicit_and_poison_even_if_caught() {
        for indirect in [false, true] {
            let (runtime, root) = setup();
            let holder = Rc::new(Cell::new(None::<Memo<i32>>));
            let captured = holder.clone();
            let first = memo(&runtime, root, move |runtime| {
                let result = captured.get().unwrap().try_get_copy(runtime);
                if !indirect {
                    assert_eq!(result, Err(RuntimeError::Cycle));
                    return Ok(99);
                }
                result
            });
            let target = if indirect {
                memo(&runtime, root, move |runtime| first.try_get_copy(runtime))
            } else {
                first
            };
            holder.set(Some(target));
            let result = first.try_get_copy(&runtime);
            assert_eq!(
                result,
                Err(if indirect {
                    RuntimeError::Cycle
                } else {
                    RuntimeError::Poisoned
                })
            );
            assert_eq!(
                runtime.first_failure().as_deref(),
                Some(&RuntimeError::Cycle)
            );
            runtime.verify_graph();
            runtime.shutdown().unwrap();
        }
    }

    #[test]
    fn pure_compute_rejects_writes_creation_disposal_including_other_runtimes() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let (other, other_root) = setup();
        let external = signal(&other, other_root, 2);
        let weak = other.downgrade();
        let pure = memo(&runtime, root, move |runtime| {
            assert_eq!(input.try_set(runtime, 5), Err(RuntimeError::WriteForbidden));
            assert_eq!(runtime.signal(1), Err(RuntimeError::MutationForbidden));
            assert_eq!(runtime.dispose(root), Err(RuntimeError::MutationForbidden));
            let other = weak.upgrade().unwrap();
            assert_eq!(
                external.try_set(&other, 3),
                Err(RuntimeError::WriteForbidden)
            );
            assert_eq!(
                external.try_get_copy(&other),
                Err(RuntimeError::WrongRuntime)
            );
            input.try_get_copy(runtime)
        });
        assert_eq!(pure.try_get_copy(&runtime).unwrap(), 1);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        assert_eq!(other.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn wrong_runtime_type_and_dead_handles_never_execute_compute() {
        let (runtime, root) = setup();
        let (other, _) = setup();
        let value = memo(&runtime, root, |_| -> Result<u32, RuntimeError> {
            panic!("must not execute")
        });
        assert_eq!(value.try_get_copy(&other), Err(RuntimeError::WrongRuntime));
        assert_eq!(
            Memo::<String>::new(value.id).try_get_cloned(&runtime),
            Err(RuntimeError::TypeMismatch)
        );
        runtime.dispose(root).unwrap();
        assert_eq!(
            value.try_get_copy(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn infallible_adapter_supports_private_fn_mut_state_and_non_partial_eq_policy() {
        struct NoEq(u32);
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let value = runtime
            .with_owner(root, |runtime| {
                let mut cache = None;
                runtime.memo_infallible_with(
                    move |runtime| {
                        let input = input.try_get_copy(runtime).unwrap();
                        if cache.is_none_or(|(previous, _)| previous != input) {
                            cache = Some((input, input + 1));
                        }
                        NoEq(cache.unwrap().1)
                    },
                    |left, right| left.0 == right.0,
                )
            })
            .unwrap();
        assert_eq!(value.try_peek_with(&runtime, |value| value.0).unwrap(), 2);
        input.try_set(&runtime, 2).unwrap();
        assert_eq!(value.try_peek_with(&runtime, |value| value.0).unwrap(), 3);
        runtime.verify_graph();
    }

    #[test]
    fn epoch_and_publication_overflow_never_wrap() {
        for epoch in [false, true] {
            let (runtime, root) = setup();
            let value = memo(&runtime, root, |_| Ok(1));
            if epoch {
                runtime.inner.storage.borrow_mut().next_execution_epoch = u64::MAX;
            } else {
                runtime
                    .inner
                    .storage
                    .borrow_mut()
                    .nodes
                    .get_mut(value.id.slot)
                    .unwrap()
                    .metadata
                    .value_revision = u64::MAX;
            }
            assert_eq!(
                value.try_get_copy(&runtime),
                Err(RuntimeError::CounterExhausted)
            );
            assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
            assert!(
                !runtime
                    .inner
                    .storage
                    .borrow()
                    .nodes
                    .get(value.id.slot)
                    .unwrap()
                    .metadata
                    .initialized
            );
            runtime.shutdown().unwrap();
        }
    }

    #[test]
    fn checking_stops_at_first_changed_source_without_eagerly_updating_unused_branch() {
        let (runtime, root) = setup();
        let switch = signal(&runtime, root, true);
        let input = signal(&runtime, root, 1);
        let switch_memo = memo(&runtime, root, move |runtime| switch.try_get_copy(runtime));
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let expensive = memo(&runtime, root, move |runtime| {
            captured.set(captured.get() + 1);
            input.try_get_copy(runtime)
        });
        let outer = memo(&runtime, root, move |runtime| {
            if switch_memo.try_get_copy(runtime)? {
                Ok(expensive.try_get_copy(runtime)?)
            } else {
                Ok(0)
            }
        });
        outer.try_get_copy(&runtime).unwrap();
        switch.try_set(&runtime, false).unwrap();
        input.try_set(&runtime, 2).unwrap();
        assert_eq!(outer.try_get_copy(&runtime).unwrap(), 0);
        assert_eq!(calls.get(), 1);
        assert_eq!(sources(&runtime, outer), [switch_memo.id.slot]);
        runtime.verify_graph();
    }

    #[test]
    fn evaluation_depth_limit_is_distinct_from_cycle_and_unwinds_guards() {
        // A generous host stack lets this test exercise the explicit runtime
        // limit rather than the platform-specific thread stack size.
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let (runtime, root) = setup();
                let mut previous = memo(&runtime, root, |_| Ok(0));
                for _ in 0..520 {
                    previous = memo(&runtime, root, move |runtime| {
                        Ok(previous.try_get_copy(runtime)? + 1)
                    });
                }
                assert_eq!(
                    previous.try_get_copy(&runtime),
                    Err(RuntimeError::DepthLimit)
                );
                assert_eq!(
                    runtime.first_failure().as_deref(),
                    Some(&RuntimeError::DepthLimit)
                );
                assert!(runtime.inner.storage.borrow().tracking_stack.is_empty());
                assert_eq!(runtime.inner.context.snapshot().evaluation_depth, 0);
                runtime.shutdown().unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn unexpected_stale_first_read_fails_without_committing_provisional_edges() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let value = memo(&runtime, root, move |runtime| {
            let value = input.try_get_copy(runtime)?;
            // Fault injection: public pure callbacks cannot do this. A second
            // read must not replace the revision of the first value consumed.
            runtime
                .inner
                .storage
                .borrow_mut()
                .nodes
                .get_mut(input.id.slot)
                .unwrap()
                .metadata
                .freshness_revision += 1;
            input.try_get_copy(runtime)?;
            Ok(value)
        });
        assert_eq!(
            value.try_get_copy(&runtime),
            Err(RuntimeError::InvariantViolation(
                "memo source changed after first read"
            ))
        );
        assert!(sources(&runtime, value).is_empty());
        assert!(
            !runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(value.id.slot)
                .unwrap()
                .metadata
                .initialized
        );
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        runtime.verify_graph();
    }

    #[test]
    fn lazy_memo_updates_preserve_effect_queue_tickets() {
        use crate::QueueMembership;
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let value = memo(&runtime, root, move |runtime| input.try_get_copy(runtime));
        value.try_get_copy(&runtime).unwrap();
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let effect = runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    captured.set(captured.get() + 1);
                    value.try_get_copy(runtime)?;
                    Ok(())
                })
            })
            .unwrap();
        runtime.flush(Default::default()).unwrap();
        input.try_set(&runtime, 2).unwrap();
        assert_eq!(value.try_peek_copy(&runtime).unwrap(), 2);
        let storage = runtime.inner.storage.borrow();
        let metadata = &storage.nodes.get(effect.id.slot).unwrap().metadata;
        assert_eq!(metadata.state, State::Dirty);
        assert_eq!(metadata.queue, QueueMembership::Pending);
        assert_eq!(calls.get(), 1);
        drop(storage);
        runtime.verify_graph();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn randomized_dynamic_dag_matches_full_evaluation_reference() {
        let (runtime, root) = setup();
        let inputs: Vec<_> = (0..8).map(|value| signal(&runtime, root, value)).collect();
        let selectors: Vec<_> = (0..32).map(|_| signal(&runtime, root, false)).collect();
        let mut memos = Vec::new();
        let mut parents = Vec::new();
        let mut seed = 1234567_u64;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        for (index, &selector) in selectors.iter().enumerate() {
            let a = random() % (inputs.len() + index);
            let b = random() % (inputs.len() + index);
            parents.push((a, b));
            let captured_memos: Vec<Memo<i32>> = memos.clone();
            let captured_inputs = inputs.clone();
            memos.push(memo(&runtime, root, move |runtime| {
                let parent = if selector.try_get_copy(runtime)? {
                    a
                } else {
                    b
                };
                let value = if parent < captured_inputs.len() {
                    captured_inputs[parent].try_get_copy(runtime)?
                } else {
                    captured_memos[parent - captured_inputs.len()].try_get_copy(runtime)?
                };
                Ok((value + index as i32) % 11)
            }));
        }
        for _ in 0..1000 {
            if random() % 2 == 0 {
                inputs[random() % inputs.len()]
                    .try_set(&runtime, (random() % 20) as i32)
                    .unwrap();
            } else {
                selectors[random() % selectors.len()]
                    .try_set(&runtime, random() % 2 == 0)
                    .unwrap();
            }
            let mut expected: Vec<_> = inputs
                .iter()
                .map(|input| input.try_peek_copy(&runtime).unwrap())
                .collect();
            for (index, &(a, b)) in parents.iter().enumerate() {
                let parent = if selectors[index].try_peek_copy(&runtime).unwrap() {
                    a
                } else {
                    b
                };
                expected.push((expected[parent] + index as i32) % 11);
            }
            // Read in arbitrary order: cached Clean sources and partially lazy
            // branches must agree with a naive complete topological evaluation.
            for _ in 0..4 {
                let index = random() % memos.len();
                assert_eq!(
                    memos[index].try_get_copy(&runtime).unwrap(),
                    expected[inputs.len() + index]
                );
            }
            runtime.verify_graph();
        }
    }

    #[test]
    fn stable_high_fan_in_reuses_tracking_and_edge_allocations() {
        let (runtime, root) = setup();
        let inputs: Vec<_> = (0..64).map(|value| signal(&runtime, root, value)).collect();
        let trigger = inputs[0];
        let value = memo(&runtime, root, move |runtime| {
            let mut sum = 0;
            for input in &inputs {
                sum += input.try_get_copy(runtime)?;
            }
            Ok(sum)
        });
        value.try_get_copy(&runtime).unwrap();
        let allocations = || {
            let storage = runtime.inner.storage.borrow();
            let node = storage.nodes.get(value.id.slot).unwrap();
            (
                node.sources.as_ptr(),
                node.reads.as_ptr(),
                node.tracking.seen_sources.capacity(),
            )
        };
        let initial = allocations();
        for i in 1..100 {
            trigger.try_set(&runtime, i).unwrap();
            value.try_get_copy(&runtime).unwrap();
            assert_eq!(allocations(), initial);
        }
        runtime.verify_graph();
    }

    #[test]
    fn equal_candidate_destructor_panic_poison_preserves_previous_memo_snapshot() {
        struct DropProbe {
            value: i32,
            runtime: crate::WeakRuntime,
            panics: bool,
        }
        impl Drop for DropProbe {
            fn drop(&mut self) {
                if let Some(runtime) = self.runtime.upgrade() {
                    assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                    assert_eq!(
                        context::check(Operation::Read),
                        Err(RuntimeError::Reentrancy)
                    );
                    assert!(matches!(
                        runtime.create_root(),
                        Err(RuntimeError::MutationForbidden
                            | RuntimeError::Reentrancy
                            | RuntimeError::Poisoned)
                    ));
                }
                if self.panics {
                    panic!("equal candidate drop")
                }
            }
        }
        let (runtime, root) = setup();
        let input = signal(&runtime, root, false);
        let value = runtime
            .with_owner(root, |runtime| {
                runtime.memo_with(
                    move |runtime| {
                        Ok(DropProbe {
                            value: 1,
                            runtime: runtime.downgrade(),
                            panics: input.try_get_copy(runtime)?,
                        })
                    },
                    |a, b| a.value == b.value,
                )
            })
            .unwrap();
        let first = value
            .try_with(&runtime, |value| value as *const DropProbe as usize)
            .unwrap();
        input.try_set(&runtime, true).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || value.try_with(&runtime, |_| ())
            ))
            .is_err()
        );
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        let storage = runtime.inner.storage.borrow();
        let node = storage.nodes.get(value.id.slot).unwrap();
        assert_eq!(node.metadata.value_revision, 1);
        assert_eq!(node.metadata.state, State::Dirty);
        let cell = node.value_cell::<DropProbe>(NodeKind::Memo).unwrap();
        assert_eq!(
            first,
            cell.borrow().as_ref().unwrap() as *const DropProbe as usize
        );
        drop(cell);
        drop(storage);
        runtime.verify_graph();
        runtime.shutdown().unwrap();
        std::mem::drop(runtime);
    }
}
