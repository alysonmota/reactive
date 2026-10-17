use std::{any::Any, cell::RefCell, rc::Rc};

use super::{PanicPayload, Runtime, Storage};
use crate::{
    RuntimeError, Signal,
    arena::SlotId,
    context::{CallbackKind, CapabilityGuard, Operation, ReaderRuntimeGuard},
    handle::HandleId,
    node::{Comparator, NodeKind, Payload, ReadRecord},
};

struct NewSignal<T, F> {
    value: Option<T>,
    comparator: Option<F>,
}

impl<T, F> NewSignal<T, F> {
    fn release(self, runtime: &Runtime) -> Option<PanicPayload> {
        let mut first = None;
        runtime.merge_panic(&mut first, runtime.release_option(self.comparator));
        runtime.merge_panic(&mut first, runtime.release_option(self.value));
        first
    }
}

struct WriteInputs<T, F = ()> {
    candidate: Option<T>,
    callback: Option<F>,
    previous: Option<Rc<RefCell<Option<T>>>>,
    comparator: Option<Comparator>,
    retired: Option<T>,
}

type CapturedSignal<T> = (Rc<RefCell<Option<T>>>, Comparator, u64);

impl<T: 'static, F> WriteInputs<T, F> {
    fn release(self, runtime: &Runtime) -> Option<PanicPayload> {
        let mut first = None;
        runtime.merge_panic(&mut first, runtime.release_option(self.callback));
        runtime.merge_panic(&mut first, runtime.release_option(self.candidate));
        runtime.merge_panic(&mut first, runtime.release_option(self.retired));
        runtime.merge_panic(&mut first, runtime.release_shared(self.previous));
        runtime.merge_panic(&mut first, runtime.release_shared(self.comparator));
        first
    }
}

impl Runtime {
    /// Creates a signal in the current explicit owner, using `PartialEq`.
    /// Values need not implement Clone. NaN follows standard float PartialEq.
    pub fn signal<T: PartialEq + 'static>(&self, initial: T) -> Result<Signal<T>, RuntimeError> {
        self.signal_with(initial, T::eq)
    }

    /// Creates a signal with a pure, untracked comparator over immutable values.
    /// Equality retains the existing snapshot and does not advance revisions.
    /// Use identity/revision comparisons for large immutable snapshots; bitwise
    /// or approximate float comparisons must be selected explicitly by the host.
    /// Interior mutation without publishing a reactive revision is unsupported.
    pub fn signal_with<T: 'static>(
        &self,
        initial: T,
        comparator: impl Fn(&T, &T) -> bool + 'static,
    ) -> Result<Signal<T>, RuntimeError> {
        self.resource_boundary(
            NewSignal {
                value: Some(initial),
                comparator: Some(comparator),
            },
            |inputs| {
                // Reject invalid creation before allocating graph snapshots.
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
                let payload = Payload::signal(
                    inputs.value.take().expect("initial value"),
                    inputs.comparator.take().expect("signal comparator"),
                );
                self.insert_node(payload).map(Signal::new)
            },
            |inputs| inputs.release(self),
        )
    }

    /// Validate, copy and record in a single callback-free borrow window.
    pub(crate) fn read_signal_copy<T: Copy + 'static>(
        &self,
        id: HandleId,
        tracked: bool,
    ) -> Result<T, RuntimeError> {
        let reader = self.reader_for_read(id, tracked)?;
        if let Some(reader) = reader {
            let mut storage = self.inner.storage.borrow_mut();
            let source = storage
                .nodes
                .get(id.slot)
                .ok_or(RuntimeError::InvalidHandle)?;
            let value = source.copy_value::<T>(NodeKind::Signal)?;
            let record = ReadRecord {
                source: id.slot,
                freshness_revision: source.metadata.freshness_revision,
                value_revision: source.metadata.value_revision,
            };
            Self::record_read(&mut storage, reader, record);
            Ok(value)
        } else {
            self.inner
                .storage
                .borrow()
                .nodes
                .get(id.slot)
                .ok_or(RuntimeError::InvalidHandle)?
                .copy_value::<T>(NodeKind::Signal)
        }
    }

    pub(crate) fn read_signal<T: 'static>(
        &self,
        id: HandleId,
        tracked: bool,
    ) -> Result<Rc<RefCell<Option<T>>>, RuntimeError> {
        let reader = self.reader_for_read(id, tracked)?;
        if let Some(reader) = reader {
            let mut storage = self.inner.storage.borrow_mut();
            let source = storage
                .nodes
                .get(id.slot)
                .ok_or(RuntimeError::InvalidHandle)?;
            let snapshot = source.value_cell::<T>(NodeKind::Signal)?;
            let record = ReadRecord {
                source: id.slot,
                freshness_revision: source.metadata.freshness_revision,
                value_revision: source.metadata.value_revision,
            };
            // A reader disposed during its callback cannot acquire new edges,
            // but may still read a source belonging to another live owner.
            Self::record_read(&mut storage, reader, record);
            Ok(snapshot)
        } else {
            self.inner
                .storage
                .borrow()
                .nodes
                .get(id.slot)
                .ok_or(RuntimeError::InvalidHandle)?
                .value_cell::<T>(NodeKind::Signal)
        }
    }

    fn capture_signal<T: 'static>(&self, id: HandleId) -> Result<CapturedSignal<T>, RuntimeError> {
        self.check(Operation::Write)?;
        self.check_identity(id)?;
        let storage = self.inner.storage.borrow();
        let node = storage
            .nodes
            .get(id.slot)
            .ok_or(RuntimeError::InvalidHandle)?;
        let previous = node.value_cell::<T>(NodeKind::Signal)?;
        let Payload::Signal(data) = &node.payload else {
            unreachable!("validated signal kind")
        };
        Ok((previous, data.equals.clone(), node.metadata.value_revision))
    }

    pub(crate) fn write_signal<T: 'static>(
        &self,
        id: HandleId,
        value: T,
    ) -> Result<(), RuntimeError> {
        self.resource_boundary(
            WriteInputs::<T> {
                candidate: Some(value),
                callback: None,
                previous: None,
                comparator: None,
                retired: None,
            },
            |inputs| {
                let (previous, comparator, revision) = self.capture_signal::<T>(id)?;
                inputs.previous = Some(previous);
                inputs.comparator = Some(comparator);
                self.commit_signal(id, revision, inputs)
            },
            |inputs| inputs.release(self),
        )
    }

    pub(crate) fn update_signal<T: Clone + 'static>(
        &self,
        id: HandleId,
        callback: impl FnOnce(&mut T),
    ) -> Result<(), RuntimeError> {
        self.resource_boundary(
            WriteInputs {
                candidate: None,
                callback: Some(callback),
                previous: None,
                comparator: None,
                retired: None,
            },
            |inputs| {
                let (previous, comparator, revision) = self.capture_signal::<T>(id)?;
                inputs.previous = Some(previous);
                inputs.comparator = Some(comparator);
                {
                    let _capabilities = CapabilityGuard::enter(CallbackKind::BorrowedRead);
                    let previous = inputs.previous.as_ref().expect("previous value").borrow();
                    inputs.candidate = Some(previous.as_ref().expect("initialized signal").clone());
                }
                {
                    let _capabilities = CapabilityGuard::enter(CallbackKind::Updating);
                    let callback = inputs.callback.take().expect("update callback");
                    callback(inputs.candidate.as_mut().expect("working copy"));
                }
                // Check conflicts/disposal before comparing the working copy.
                self.check_signal_revision::<T>(id, revision)?;
                self.commit_signal(id, revision, inputs)
            },
            |inputs| inputs.release(self),
        )
    }

    fn check_signal_revision<T: 'static>(
        &self,
        id: HandleId,
        revision: u64,
    ) -> Result<(), RuntimeError> {
        self.check(Operation::Write)?;
        self.check_identity(id)?;
        let storage = self.inner.storage.borrow();
        let node = storage
            .nodes
            .get(id.slot)
            .ok_or(RuntimeError::InvalidHandle)?;
        node.validate_type::<T>(NodeKind::Signal)?;
        if node.metadata.value_revision != revision {
            return Err(RuntimeError::UpdateConflict);
        }
        Ok(())
    }

    fn commit_signal<T: 'static, F>(
        &self,
        id: HandleId,
        revision: u64,
        inputs: &mut WriteInputs<T, F>,
    ) -> Result<(), RuntimeError> {
        let equal = {
            let _context = self.inner.context.enter(|snapshot| snapshot.reader = None);
            let _reader_runtime = ReaderRuntimeGuard::enter(None);
            let _capabilities = CapabilityGuard::enter(CallbackKind::Comparator);
            let previous = inputs
                .previous
                .as_ref()
                .expect("previous snapshot")
                .as_ref();
            let candidate = inputs.candidate.as_ref().expect("candidate value");
            inputs.comparator.as_ref().expect("signal comparator")(
                previous as &dyn Any,
                candidate as &dyn Any,
            )?
        };
        self.check(Operation::Write)?;
        self.check_identity(id)?;
        let result = {
            let mut storage = self.inner.storage.borrow_mut();
            let node = storage
                .nodes
                .get_mut(id.slot)
                .ok_or(RuntimeError::InvalidHandle)?;
            node.validate_type::<T>(NodeKind::Signal)?;
            if node.metadata.value_revision != revision {
                return Err(RuntimeError::UpdateConflict);
            }
            if equal {
                // No candidate Rc allocation, marking or revision change.
                return Ok(());
            }
            let Some(value_revision) = node.metadata.value_revision.checked_add(1) else {
                drop(storage);
                self.record_failure(RuntimeError::CounterExhausted);
                return Err(RuntimeError::CounterExhausted);
            };
            let Some(freshness_revision) = node.metadata.freshness_revision.checked_add(1) else {
                drop(storage);
                self.record_failure(RuntimeError::CounterExhausted);
                return Err(RuntimeError::CounterExhausted);
            };
            let Payload::Signal(data) = &mut node.payload else {
                unreachable!("validated signal kind")
            };
            let cell = data
                .value
                .downcast_ref::<RefCell<Option<T>>>()
                .expect("validated cell");
            inputs.retired = cell
                .borrow_mut()
                .replace(inputs.candidate.take().expect("candidate value"));
            node.metadata.value_revision = value_revision;
            node.metadata.freshness_revision = freshness_revision;
            Self::invalidate_signal_observers(&mut storage, id.slot)
        };
        if let Err(error) = &result {
            self.record_failure(error.clone());
        }
        result
    }

    /// Forward invalidation only: no computation, user callback or arena scan.
    /// Graph marking shares the memo path and schedules effects without executing them.
    pub(super) fn invalidate_signal_observers(
        storage: &mut Storage,
        source: SlotId,
    ) -> Result<(), RuntimeError> {
        Self::mark_observers(storage, source)
    }
}

#[cfg(test)]
mod tests {
    use crate::context;
    use std::cell::Cell;

    use super::*;
    use crate::{EvaluationPhase, OwnerHandle, RuntimeStatus, State, WeakRuntime};

    fn setup<T: PartialEq + 'static>(value: T) -> (Runtime, OwnerHandle, Signal<T>) {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let signal = runtime
            .with_owner(root, |runtime| runtime.signal(value))
            .unwrap();
        (runtime, root, signal)
    }

    fn revisions<T>(runtime: &Runtime, signal: Signal<T>) -> (u64, u64) {
        let storage = runtime.inner.storage.borrow();
        let metadata = &storage.nodes.get(signal.id.slot).unwrap().metadata;
        assert_eq!(metadata.state, State::Clean);
        (metadata.value_revision, metadata.freshness_revision)
    }

    fn computation(runtime: &Runtime, owner: OwnerHandle, kind: NodeKind) -> HandleId {
        let payload = match kind {
            NodeKind::Memo => Payload::Memo {
                value_type: &const { crate::node::ValueType::of::<u32>() },
                value: Some(Rc::new(RefCell::new(Some(0_u32)))),
                compute: Some(Box::new(|_, _| panic!("set must not run a memo"))),
                equals: Payload::comparator::<u32>(u32::eq),
            },
            NodeKind::Effect => Payload::effect(Box::new(|_| panic!("set must not run an effect"))),
            NodeKind::Signal => unreachable!(),
        };
        let id = runtime
            .with_owner(owner, |runtime| runtime.insert_node(payload))
            .unwrap();
        runtime
            .inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(id.slot)
            .unwrap()
            .metadata
            .state = State::Clean;
        id
    }

    fn observe(runtime: &Runtime, source: HandleId, observer: HandleId) {
        let mut storage = runtime.inner.storage.borrow_mut();
        let revision = storage
            .nodes
            .get(source.slot)
            .unwrap()
            .metadata
            .value_revision;
        Runtime::add_source(&mut storage, observer.slot, source.slot, revision);
    }

    #[test]
    fn creation_requires_explicit_live_owner() {
        let runtime = Runtime::new();
        assert_eq!(runtime.signal(1), Err(RuntimeError::MissingOwner));
        assert_eq!(
            runtime.signal_with(1, |_, _| false),
            Err(RuntimeError::MissingOwner)
        );
        assert_eq!(runtime.node_count(), 0);
        let root = runtime.create_root().unwrap();
        runtime
            .with_owner(root, |runtime| {
                runtime.dispose(root)?;
                assert_eq!(runtime.signal(1), Err(RuntimeError::OwnerDisposed));
                Ok(())
            })
            .unwrap();
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn cloned_values_survive_later_writes_and_disposal() {
        let (runtime, root, signal) = setup(String::from("initial"));
        let initial = signal.try_get_cloned(&runtime).unwrap();
        assert_eq!(initial, signal.try_peek_cloned(&runtime).unwrap());
        signal.try_set(&runtime, String::from("new")).unwrap();
        let current = signal.try_get_cloned(&runtime).unwrap();
        assert_eq!(current.as_str(), "new");
        assert_eq!(initial.as_str(), "initial");
        assert_ne!(initial, current);
        assert_eq!(revisions(&runtime, signal), (1, 1));
        runtime.dispose(root).unwrap();
        assert_eq!(current.as_str(), "new");
        assert_eq!(
            signal.try_get_cloned(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
    }

    #[test]
    fn equality_cut_retains_pointer_and_revision_without_marking_observers() {
        let (runtime, root, signal) = setup(7_u32);
        let observer = computation(&runtime, root, NodeKind::Memo);
        observe(&runtime, signal.id, observer);
        let initial = signal.try_get_copy(&runtime).unwrap();
        for _ in 0..100 {
            signal.try_set(&runtime, 7).unwrap();
        }
        assert_eq!(initial, signal.try_get_copy(&runtime).unwrap());
        assert_eq!(revisions(&runtime, signal), (0, 0));
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(observer.slot)
                .unwrap()
                .metadata
                .state,
            State::Clean
        );
        assert!(runtime.inner.storage.borrow().mark_stack.is_empty());
    }

    #[test]
    fn borrowed_reads_do_not_clone_but_clone_and_update_are_explicit() {
        struct Counted {
            value: u32,
            clones: Rc<Cell<usize>>,
        }
        impl Clone for Counted {
            fn clone(&self) -> Self {
                self.clones.set(self.clones.get() + 1);
                Self {
                    value: self.value,
                    clones: self.clones.clone(),
                }
            }
        }
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let clones = Rc::new(Cell::new(0));
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    Counted {
                        value: 1,
                        clones: clones.clone(),
                    },
                    |left, right| left.value == right.value,
                )
            })
            .unwrap();
        let previous = signal.try_with(&runtime, |value| value.value).unwrap();
        signal.try_peek_with(&runtime, |_| ()).unwrap();
        signal.try_with(&runtime, |_| ()).unwrap();
        assert_eq!(clones.get(), 0);
        signal
            .try_update_cloned(&runtime, |value| value.value += 1)
            .unwrap();
        assert_eq!(clones.get(), 1);
        assert_eq!(signal.try_with(&runtime, |value| value.value).unwrap(), 2);
        assert_eq!(previous, 1);
        assert_eq!(signal.try_get_cloned(&runtime).unwrap().value, 2);
        assert_eq!(clones.get(), 2);
    }

    #[test]
    fn non_clone_non_partial_eq_values_support_explicit_policy_and_identity() {
        struct Document {
            revision: u64,
            points: Vec<u32>,
        }
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    Document {
                        revision: 0,
                        points: vec![1, 2],
                    },
                    |left, right| left.revision == right.revision,
                )
            })
            .unwrap();
        signal
            .try_set(
                &runtime,
                Document {
                    revision: 1,
                    points: vec![3, 4],
                },
            )
            .unwrap();
        assert!(
            signal
                .try_with(&runtime, |value| value.points == [3, 4])
                .unwrap()
        );
        assert_eq!(revisions(&runtime, signal), (1, 1));

        let document = Rc::new(Document {
            revision: 0,
            points: vec![],
        });
        let identity = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(document.clone(), Rc::ptr_eq)
            })
            .unwrap();
        let initial = identity.try_get_cloned(&runtime).unwrap();
        identity.try_set(&runtime, document).unwrap();
        assert!(Rc::ptr_eq(
            &initial,
            &identity.try_get_cloned(&runtime).unwrap()
        ));
        identity
            .try_set(
                &runtime,
                Rc::new(Document {
                    revision: 0,
                    points: vec![],
                }),
            )
            .unwrap();
        assert_eq!(revisions(&runtime, identity), (1, 1));
    }

    #[test]
    fn float_policies_are_explicit_nan_signed_zero_and_tolerance() {
        let (runtime, root, nan) = setup(f64::NAN);
        nan.try_set(&runtime, f64::NAN).unwrap();
        assert_eq!(revisions(&runtime, nan), (1, 1));
        let zero = runtime
            .with_owner(root, |runtime| runtime.signal(0.0_f64))
            .unwrap();
        zero.try_set(&runtime, -0.0).unwrap();
        assert_eq!(revisions(&runtime, zero), (0, 0));
        assert_eq!(
            zero.try_get_copy(&runtime).unwrap().to_bits(),
            0.0_f64.to_bits()
        );

        let bits = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(f64::NAN, |left, right| left.to_bits() == right.to_bits())
            })
            .unwrap();
        bits.try_set(&runtime, f64::NAN).unwrap();
        assert_eq!(revisions(&runtime, bits), (0, 0));
        bits.try_set(&runtime, 0.0).unwrap();
        bits.try_set(&runtime, -0.0).unwrap();
        assert_eq!(revisions(&runtime, bits), (2, 2));

        let tolerance = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(1.0_f64, |left, right| (left - right).abs() < 0.1)
            })
            .unwrap();
        tolerance.try_set(&runtime, 1.05).unwrap();
        assert_eq!(tolerance.try_get_copy(&runtime).unwrap(), 1.0);
        let exact = runtime
            .with_owner(root, |runtime| runtime.signal(1.0_f64))
            .unwrap();
        exact.try_set(&runtime, 1.05).unwrap();
        assert_eq!(exact.try_get_copy(&runtime).unwrap(), 1.05);
    }

    #[test]
    fn wrong_runtime_and_disposed_handles_reject_all_access_without_callbacks() {
        let (runtime, root, signal) = setup(1_u32);
        let other = Runtime::new();
        assert_eq!(signal.try_get_copy(&other), Err(RuntimeError::WrongRuntime));
        assert_eq!(
            signal.try_peek_copy(&other),
            Err(RuntimeError::WrongRuntime)
        );
        assert_eq!(signal.try_set(&other, 2), Err(RuntimeError::WrongRuntime));
        assert_eq!(
            signal.try_update_cloned(&other, |_| panic!("must not run")),
            Err(RuntimeError::WrongRuntime)
        );
        runtime.dispose(root).unwrap();
        let new_root = runtime.create_root().unwrap();
        let new = runtime
            .with_owner(new_root, |runtime| runtime.signal(9_u32))
            .unwrap();
        assert_eq!(signal.id.slot.index, new.id.slot.index);
        assert_eq!(
            signal.try_get_copy(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(
            signal.try_peek_copy(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(
            signal.try_set(&runtime, 2),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(
            signal.try_update_cloned(&runtime, |_| panic!("must not run")),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(new.try_get_copy(&runtime).unwrap(), 9);
        assert_eq!(other.status(), RuntimeStatus::Healthy);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn forged_type_or_node_kind_never_reinterprets_a_snapshot_or_runs_update() {
        let (runtime, root, signal) = setup(1_u32);
        let forged = Signal::<String>::new(signal.id);
        assert_eq!(
            forged.try_get_cloned(&runtime),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(
            forged.try_set(&runtime, String::from("wrong")),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(
            forged.try_update_cloned(&runtime, |_| panic!("must not run")),
            Err(RuntimeError::TypeMismatch)
        );
        let memo = computation(&runtime, root, NodeKind::Memo);
        assert_eq!(
            Signal::<u32>::new(memo).try_peek_copy(&runtime),
            Err(RuntimeError::TypeMismatch)
        );
        assert_eq!(signal.try_get_copy(&runtime).unwrap(), 1);
    }

    #[test]
    fn comparator_runs_without_borrows_tracking_or_mutation_authority() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let other = Runtime::new();
        let other_root = other.create_root().unwrap();
        let other_signal = other
            .with_owner(other_root, |runtime| runtime.signal(1_u32))
            .unwrap();
        let weak = runtime.downgrade();
        let other_weak = other.downgrade();
        let holder = Rc::new(Cell::new(None::<Signal<u32>>));
        let captured = holder.clone();
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(1_u32, move |a, b| {
                    let runtime = weak.upgrade().unwrap();
                    let other = other_weak.upgrade().unwrap();
                    let signal = captured.get().unwrap();
                    assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                    assert_eq!(runtime.inner.context.snapshot().reader, None);
                    assert_eq!(context::check_read_runtime(other.inner.id), Ok(()));
                    assert_eq!(signal.try_get_copy(&runtime), Err(RuntimeError::Reentrancy));
                    assert_eq!(
                        signal.try_peek_copy(&runtime),
                        Err(RuntimeError::Reentrancy)
                    );
                    assert_eq!(
                        signal.try_set(&runtime, 3),
                        Err(RuntimeError::WriteForbidden)
                    );
                    assert_eq!(runtime.create_root(), Err(RuntimeError::MutationForbidden));
                    assert_eq!(runtime.dispose(root), Err(RuntimeError::MutationForbidden));
                    assert_eq!(
                        other_signal.try_set(&other, 3),
                        Err(RuntimeError::WriteForbidden)
                    );
                    assert_eq!(
                        other_signal.try_peek_copy(&other),
                        Err(RuntimeError::Reentrancy)
                    );
                    a == b
                })
            })
            .unwrap();
        holder.set(Some(signal));
        let reader = computation(&runtime, root, NodeKind::Effect);
        let guard = runtime
            .enter_evaluation(reader, EvaluationPhase::Executing)
            .unwrap();
        signal.try_set(&runtime, 2).unwrap();
        assert_eq!(runtime.inner.context.snapshot().reader, Some(reader.slot));
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        assert_eq!(other.status(), RuntimeStatus::Healthy);
        assert!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(reader.slot)
                .unwrap()
                .reads
                .is_empty()
        );
        drop(guard);
        assert_eq!(signal.try_get_copy(&runtime).unwrap(), 2);
    }

    #[test]
    fn tracking_deduplicates_nested_readers_and_preserves_first_read_revision() {
        let (runtime, root, signal) = setup(1_u32);
        let effect = computation(&runtime, root, NodeKind::Effect);
        let memo = computation(&runtime, root, NodeKind::Memo);
        let outer = runtime
            .enter_evaluation(effect, EvaluationPhase::Executing)
            .unwrap();
        signal.try_get_copy(&runtime).unwrap();
        {
            let _nested = runtime
                .enter_evaluation(memo, EvaluationPhase::Executing)
                .unwrap();
            signal.try_get_copy(&runtime).unwrap();
            signal.try_get_copy(&runtime).unwrap();
        }
        signal.try_set(&runtime, 2).unwrap();
        assert_eq!(signal.try_get_copy(&runtime).unwrap(), 2);
        let storage = runtime.inner.storage.borrow();
        let effect_reads = &storage.nodes.get(effect.slot).unwrap().reads;
        let memo_reads = &storage.nodes.get(memo.slot).unwrap().reads;
        assert_eq!(effect_reads.len(), 1);
        assert_eq!(memo_reads.len(), 1);
        assert_eq!(effect_reads[0].value_revision, 0);
        assert_eq!(effect_reads[0].freshness_revision, 0);
        assert_eq!(revisions_after_borrow(&storage, signal), (1, 1));
        drop(storage);
        drop(outer);
    }

    fn revisions_after_borrow<T>(storage: &Storage, signal: Signal<T>) -> (u64, u64) {
        let metadata = &storage.nodes.get(signal.id.slot).unwrap().metadata;
        (metadata.value_revision, metadata.freshness_revision)
    }

    #[test]
    fn peek_and_internal_update_snapshot_do_not_track_but_callback_reads_do() {
        let (runtime, root, signal) = setup(1_u32);
        let other = runtime
            .with_owner(root, |runtime| runtime.signal(10_u32))
            .unwrap();
        let reader = computation(&runtime, root, NodeKind::Effect);
        let _guard = runtime
            .enter_evaluation(reader, EvaluationPhase::Executing)
            .unwrap();
        signal.try_peek_copy(&runtime).unwrap();
        signal
            .try_update_cloned(&runtime, |value| {
                *value += other.try_get_copy(&runtime).unwrap();
            })
            .unwrap();
        let storage = runtime.inner.storage.borrow();
        let reads = &storage.nodes.get(reader.slot).unwrap().reads;
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].source, other.id.slot);
        assert_eq!(
            *storage
                .nodes
                .get(signal.id.slot)
                .unwrap()
                .snapshot::<u32>(NodeKind::Signal)
                .unwrap(),
            11
        );
    }

    #[test]
    fn cross_runtime_tracked_reads_are_rejected_while_explicit_peek_is_allowed() {
        let (runtime, root, _) = setup(1_u32);
        let (other, _, signal) = setup(2_u32);
        let reader = computation(&runtime, root, NodeKind::Memo);
        let _guard = runtime
            .enter_evaluation(reader, EvaluationPhase::Executing)
            .unwrap();
        assert_eq!(signal.try_get_copy(&other), Err(RuntimeError::WrongRuntime));
        assert_eq!(signal.try_peek_copy(&other).unwrap(), 2);
        assert!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(reader.slot)
                .unwrap()
                .reads
                .is_empty()
        );
    }

    #[test]
    fn reads_after_disposing_current_reader_do_not_restore_a_dead_dependency() {
        let (runtime, root, signal) = setup(1_u32);
        let reader = computation(&runtime, root, NodeKind::Effect);
        let _guard = runtime
            .enter_evaluation(reader, EvaluationPhase::Executing)
            .unwrap();
        runtime.dispose(crate::EffectHandle::new(reader)).unwrap();
        assert_eq!(signal.try_get_copy(&runtime).unwrap(), 1);
        assert_eq!(runtime.node_count(), 1);
    }

    #[test]
    fn changed_write_marks_direct_observers_dirty_and_descendants_check_without_execution() {
        let (runtime, root, signal) = setup(1_u32);
        let memo = computation(&runtime, root, NodeKind::Memo);
        let effect = computation(&runtime, root, NodeKind::Effect);
        let unaffected = computation(&runtime, root, NodeKind::Memo);
        observe(&runtime, signal.id, memo);
        observe(&runtime, memo, effect);
        signal.try_set(&runtime, 2).unwrap();
        let storage = runtime.inner.storage.borrow();
        assert_eq!(
            storage.nodes.get(memo.slot).unwrap().metadata.state,
            State::Dirty
        );
        assert_eq!(
            storage.nodes.get(effect.slot).unwrap().metadata.state,
            State::Check
        );
        assert_eq!(
            storage.nodes.get(unaffected.slot).unwrap().metadata.state,
            State::Clean
        );
        drop(storage);
        signal.try_set(&runtime, 3).unwrap();
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.slot)
                .unwrap()
                .metadata
                .freshness_revision,
            1
        );
    }

    #[test]
    fn repeated_writes_record_invalidation_even_if_running_observer_is_already_dirty() {
        let (runtime, root, signal) = setup(1_u32);
        let observer = computation(&runtime, root, NodeKind::Effect);
        observe(&runtime, signal.id, observer);
        let _guard = runtime
            .enter_evaluation(observer, EvaluationPhase::Executing)
            .unwrap();
        signal.try_set(&runtime, 2).unwrap();
        signal.try_set(&runtime, 3).unwrap();
        let storage = runtime.inner.storage.borrow();
        let metadata = &storage.nodes.get(observer.slot).unwrap().metadata;
        assert_eq!(metadata.state, State::Dirty);
        assert_eq!(metadata.invalidation_revision, 2);
    }

    #[test]
    fn disposed_observer_ids_are_ignored_even_after_slot_reuse() {
        let (runtime, root, signal) = setup(1_u32);
        let old = computation(&runtime, root, NodeKind::Effect);
        observe(&runtime, signal.id, old);
        runtime.dispose(crate::EffectHandle::new(old)).unwrap();
        let new = computation(&runtime, root, NodeKind::Effect);
        assert_eq!(old.slot.index, new.slot.index);
        signal.try_set(&runtime, 2).unwrap();
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(new.slot)
                .unwrap()
                .metadata
                .state,
            State::Clean
        );
    }

    #[test]
    fn update_works_with_equality_cut_and_reentrant_equal_write_is_not_a_conflict() {
        let (runtime, _, signal) = setup(7_u32);
        let snapshot = signal.try_get_copy(&runtime).unwrap();
        signal.try_update_cloned(&runtime, |_| {}).unwrap();
        assert_eq!(snapshot, signal.try_get_copy(&runtime).unwrap());
        signal
            .try_update_cloned(&runtime, |value| {
                signal.try_set(&runtime, 7).unwrap();
                *value += 1;
            })
            .unwrap();
        assert_eq!(signal.try_get_copy(&runtime).unwrap(), 8);
        assert_eq!(revisions(&runtime, signal), (1, 1));
    }

    #[test]
    fn update_conflict_does_not_overwrite_retry_or_compare_stale_candidate() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let comparisons = Rc::new(Cell::new(0));
        let calls = comparisons.clone();
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(0_u32, move |a, b| {
                    calls.set(calls.get() + 1);
                    a == b
                })
            })
            .unwrap();
        let callbacks = Cell::new(0);
        let result = signal.try_update_cloned(&runtime, |value| {
            callbacks.set(callbacks.get() + 1);
            signal.try_set(&runtime, 10).unwrap();
            *value = 1;
        });
        assert_eq!(result, Err(RuntimeError::UpdateConflict));
        assert_eq!(signal.try_get_copy(&runtime).unwrap(), 10);
        assert_eq!(callbacks.get(), 1);
        assert_eq!(comparisons.get(), 1); // Only the nested set was compared.
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn update_detects_change_back_to_initial_value_by_revision_not_value() {
        let (runtime, _, signal) = setup(0_u32);
        let result = signal.try_update_cloned(&runtime, |value| {
            signal.try_set(&runtime, 1).unwrap();
            signal.try_set(&runtime, 0).unwrap();
            *value = 2;
        });
        assert_eq!(result, Err(RuntimeError::UpdateConflict));
        assert_eq!(signal.try_get_copy(&runtime).unwrap(), 0);
        assert_eq!(revisions(&runtime, signal), (2, 2));
    }

    #[test]
    fn update_disposal_and_reuse_cannot_commit_to_the_new_occupant() {
        let (runtime, root, signal) = setup(1_u32);
        let replacement = Cell::new(None);
        let result = signal.try_update_cloned(&runtime, |value| {
            runtime.dispose(root).unwrap();
            let new_root = runtime.create_root().unwrap();
            let new = runtime
                .with_owner(new_root, |runtime| runtime.signal(9_u32))
                .unwrap();
            replacement.set(Some(new));
            *value = 2;
        });
        assert_eq!(result, Err(RuntimeError::InvalidHandle));
        let new = replacement.get().unwrap();
        assert_eq!(signal.id.slot.index, new.id.slot.index);
        assert_eq!(new.try_get_copy(&runtime).unwrap(), 9);
    }

    #[test]
    fn memo_purity_covers_set_update_and_other_runtime_writes() {
        let (runtime, root, signal) = setup(1_u32);
        let (other, _, other_signal) = setup(2_u32);
        let memo = computation(&runtime, root, NodeKind::Memo);
        let _guard = runtime
            .enter_evaluation(memo, EvaluationPhase::Executing)
            .unwrap();
        assert_eq!(
            signal.try_set(&runtime, 3),
            Err(RuntimeError::WriteForbidden)
        );
        assert_eq!(
            signal.try_update_cloned(&runtime, |_| panic!("must not run")),
            Err(RuntimeError::WriteForbidden)
        );
        assert_eq!(
            other_signal.try_set(&other, 3),
            Err(RuntimeError::WriteForbidden)
        );
        assert_eq!(revisions(&runtime, signal), (0, 0));
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn revision_overflow_poison_never_wraps_or_publishes_candidate() {
        for freshness in [false, true] {
            let (runtime, _, signal) = setup(1_u32);
            {
                let mut storage = runtime.inner.storage.borrow_mut();
                let metadata = &mut storage.nodes.get_mut(signal.id.slot).unwrap().metadata;
                if freshness {
                    metadata.freshness_revision = u64::MAX;
                } else {
                    metadata.value_revision = u64::MAX;
                }
            }
            assert_eq!(
                signal.try_set(&runtime, 2),
                Err(RuntimeError::CounterExhausted)
            );
            assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
            let storage = runtime.inner.storage.borrow();
            let node = storage.nodes.get(signal.id.slot).unwrap();
            assert_eq!(*node.snapshot::<u32>(NodeKind::Signal).unwrap(), 1);
            assert_eq!(node.metadata.state, State::Clean);
        }
    }

    #[test]
    fn equal_write_at_max_revision_does_not_need_counter_increment() {
        let (runtime, _, signal) = setup(1_u32);
        {
            let mut storage = runtime.inner.storage.borrow_mut();
            let metadata = &mut storage.nodes.get_mut(signal.id.slot).unwrap().metadata;
            metadata.value_revision = u64::MAX;
            metadata.freshness_revision = u64::MAX;
        }
        signal.try_set(&runtime, 1).unwrap();
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        assert_eq!(revisions(&runtime, signal), (u64::MAX, u64::MAX));
    }

    #[test]
    fn poisoned_or_closed_runtime_rejects_reads_and_writes_before_user_callbacks() {
        let (runtime, _, signal) = setup(1_u32);
        runtime.record_failure(RuntimeError::CallbackFailed("failure"));
        assert_eq!(signal.try_get_copy(&runtime), Err(RuntimeError::Poisoned));
        assert_eq!(signal.try_peek_copy(&runtime), Err(RuntimeError::Poisoned));
        assert_eq!(signal.try_set(&runtime, 2), Err(RuntimeError::Poisoned));
        assert_eq!(
            signal.try_update_cloned(&runtime, |_| panic!("must not run")),
            Err(RuntimeError::Poisoned)
        );
        let (closed, _, closed_signal) = setup(1_u32);
        closed.shutdown().unwrap();
        assert_eq!(
            closed_signal.try_peek_copy(&closed),
            Err(RuntimeError::RuntimeClosed)
        );
        assert_eq!(
            closed_signal.try_set(&closed, 2),
            Err(RuntimeError::RuntimeClosed)
        );
    }

    struct CloneProbe {
        value: u32,
        runtime: WeakRuntime,
        clones: Rc<Cell<usize>>,
        on_clone: Option<Rc<dyn Fn()>>,
    }

    impl Clone for CloneProbe {
        fn clone(&self) -> Self {
            self.clones.set(self.clones.get() + 1);
            if let Some(runtime) = self.runtime.upgrade() {
                assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                assert_eq!(
                    context::check(Operation::Write),
                    Err(RuntimeError::Reentrancy)
                );
                assert_eq!(
                    context::check(Operation::Flush),
                    Err(RuntimeError::Reentrancy)
                );
            }
            if let Some(callback) = &self.on_clone {
                callback()
            }
            Self {
                value: self.value,
                runtime: self.runtime.clone(),
                clones: self.clones.clone(),
                on_clone: self.on_clone.clone(),
            }
        }
    }

    #[test]
    fn clone_write_reentrancy_is_rejected_without_preventing_working_copy_commit() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let clones = Rc::new(Cell::new(0));
        let holder = Rc::new(Cell::new(None::<Signal<CloneProbe>>));
        let captured = holder.clone();
        let weak = runtime.downgrade();
        let clone_count = clones.clone();
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    CloneProbe {
                        value: 0,
                        runtime: runtime.downgrade(),
                        clones: clones.clone(),
                        on_clone: Some(Rc::new(move || {
                            let runtime = weak.upgrade().unwrap();
                            assert_eq!(
                                captured.get().unwrap().try_set(
                                    &runtime,
                                    CloneProbe {
                                        value: 10,
                                        runtime: runtime.downgrade(),
                                        clones: clone_count.clone(),
                                        on_clone: None,
                                    },
                                ),
                                Err(RuntimeError::Reentrancy)
                            );
                        })),
                    },
                    |a, b| a.value == b.value,
                )
            })
            .unwrap();
        holder.set(Some(signal));
        let callbacks = Cell::new(0);
        let result = signal.try_update_cloned(&runtime, |value| {
            assert!(runtime.inner.storage.try_borrow_mut().is_ok());
            callbacks.set(callbacks.get() + 1);
            value.value = 1;
        });
        assert_eq!(result, Ok(()));
        assert_eq!(signal.try_with(&runtime, |value| value.value).unwrap(), 1);
        assert_eq!(clones.get(), 1);
        assert_eq!(callbacks.get(), 1);
    }

    #[test]
    fn clone_cannot_dispose_target_and_working_copy_commits_after_borrow_ends() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let weak = runtime.downgrade();
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    CloneProbe {
                        value: 0,
                        runtime: runtime.downgrade(),
                        clones: Rc::new(Cell::new(0)),
                        on_clone: Some(Rc::new(move || {
                            assert_eq!(
                                weak.upgrade().unwrap().dispose(root),
                                Err(RuntimeError::Reentrancy)
                            );
                        })),
                    },
                    |a, b| a.value == b.value,
                )
            })
            .unwrap();
        assert_eq!(
            signal.try_update_cloned(&runtime, |value| value.value = 1),
            Ok(())
        );
        assert_eq!(runtime.node_count(), 1);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn clone_panic_poison_does_not_run_callback_or_invalidate_previous_snapshot() {
        #[derive(PartialEq)]
        struct PanicClone(u32);
        impl Clone for PanicClone {
            fn clone(&self) -> Self {
                panic!("clone panic")
            }
        }
        let (runtime, _, signal) = setup(PanicClone(7));
        let previous = signal.try_peek_with(&runtime, |value| value.0).unwrap();
        let callbacks = Cell::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            signal.try_update_cloned(&runtime, |_| callbacks.set(callbacks.get() + 1))
        }));
        assert!(result.is_err());
        assert_eq!(callbacks.get(), 0);
        assert_eq!(previous, 7);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(context::check(Operation::Write), Ok(()));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn update_panic_preserves_immediate_nested_writes_without_rollback() {
        let (runtime, _, signal) = setup(0_u32);
        let previous = signal.try_peek_copy(&runtime).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            signal.try_update_cloned(&runtime, |value| {
                signal.try_set(&runtime, 10).unwrap();
                *value = 1;
                panic!("update panic");
            })
        }));
        assert!(result.is_err());
        assert_eq!(previous, 0);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(
            *runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(signal.id.slot)
                .unwrap()
                .snapshot::<u32>(NodeKind::Signal)
                .unwrap(),
            10
        );
        assert_eq!(context::check(Operation::Write), Ok(()));
    }

    #[test]
    fn comparator_panic_restores_outer_tracking_and_never_publishes_candidate() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(1_u32, |_, _| panic!("comparison panic"))
            })
            .unwrap();
        let previous = signal.try_peek_copy(&runtime).unwrap();
        let reader = computation(&runtime, root, NodeKind::Effect);
        let guard = runtime
            .enter_evaluation(reader, EvaluationPhase::Executing)
            .unwrap();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| signal.try_set(&runtime, 2)));
        assert!(result.is_err());
        assert_eq!(previous, 1);
        assert_eq!(revisions(&runtime, signal), (0, 0));
        assert_eq!(runtime.inner.context.snapshot().reader, Some(reader.slot));
        assert_eq!(context::check(Operation::Write), Ok(()));
        assert_eq!(context::check_read_runtime(runtime.inner.id), Ok(()));
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        drop(guard);
        assert_eq!(runtime.inner.context.snapshot().reader, None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn partial_eq_is_user_code_with_the_same_pure_guard_as_custom_comparators() {
        struct GuardedEq(WeakRuntime, u32);
        impl PartialEq for GuardedEq {
            fn eq(&self, other: &Self) -> bool {
                let runtime = self.0.upgrade().unwrap();
                assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                assert_eq!(runtime.create_root(), Err(RuntimeError::MutationForbidden));
                self.1 == other.1
            }
        }
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal(GuardedEq(runtime.downgrade(), 1))
            })
            .unwrap();
        signal
            .try_set(&runtime, GuardedEq(runtime.downgrade(), 2))
            .unwrap();
        assert_eq!(signal.try_with(&runtime, |value| value.1).unwrap(), 2);
    }

    struct DropFlag {
        value: u32,
        drops: Rc<Cell<usize>>,
        callback: Option<Box<dyn Fn()>>,
        panic: Option<&'static str>,
    }

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            if let Some(callback) = &self.callback {
                callback()
            }
            if let Some(message) = self.panic {
                std::panic::panic_any(message)
            }
        }
    }

    #[test]
    fn retired_value_drops_after_publication_and_observer_marking_outside_borrows() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let observer = computation(&runtime, root, NodeKind::Memo);
        let weak = runtime.downgrade();
        let holder = Rc::new(Cell::new(None::<Signal<DropFlag>>));
        let captured = holder.clone();
        let drops = Rc::new(Cell::new(0));
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    DropFlag {
                        value: 1,
                        drops: drops.clone(),
                        panic: None,
                        callback: Some(Box::new(move || {
                            let runtime = weak.upgrade().unwrap();
                            assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                            assert_eq!(
                                captured
                                    .get()
                                    .unwrap()
                                    .try_peek_with(&runtime, |_| ())
                                    .err(),
                                Some(RuntimeError::Reentrancy)
                            );
                            assert_eq!(runtime.create_root(), Err(RuntimeError::Reentrancy));
                            let storage = runtime.inner.storage.borrow();
                            assert_eq!(
                                storage.nodes.get(observer.slot).unwrap().metadata.state,
                                State::Dirty
                            );
                            let snapshot = storage
                                .nodes
                                .get(captured.get().unwrap().id.slot)
                                .unwrap()
                                .value_cell::<DropFlag>(NodeKind::Signal)
                                .unwrap();
                            assert_eq!(snapshot.borrow().as_ref().unwrap().value, 2);
                        })),
                    },
                    |a, b| a.value == b.value,
                )
            })
            .unwrap();
        holder.set(Some(signal));
        observe(&runtime, signal.id, observer);
        signal
            .try_set(
                &runtime,
                DropFlag {
                    value: 2,
                    drops: drops.clone(),
                    callback: None,
                    panic: None,
                },
            )
            .unwrap();
        assert_eq!(drops.get(), 1);
        runtime.shutdown().unwrap();
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn equal_candidate_drops_once_without_replacing_snapshot() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let drops = Rc::new(Cell::new(0));
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    DropFlag {
                        value: 1,
                        drops: drops.clone(),
                        callback: None,
                        panic: None,
                    },
                    |a, b| a.value == b.value,
                )
            })
            .unwrap();
        let previous = signal
            .try_with(&runtime, |value| value as *const DropFlag as usize)
            .unwrap();
        signal
            .try_set(
                &runtime,
                DropFlag {
                    value: 1,
                    drops: drops.clone(),
                    callback: None,
                    panic: None,
                },
            )
            .unwrap();
        assert_eq!(drops.get(), 1);
        assert_eq!(
            previous,
            signal
                .try_with(&runtime, |value| value as *const DropFlag as usize)
                .unwrap()
        );
        runtime.shutdown().unwrap();
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn comparator_panic_and_candidate_destructor_panic_preserve_first_panic() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let drops = Rc::new(Cell::new(0));
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    DropFlag {
                        value: 1,
                        drops: drops.clone(),
                        callback: None,
                        panic: None,
                    },
                    |_, _| std::panic::panic_any("comparator"),
                )
            })
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            signal.try_set(
                &runtime,
                DropFlag {
                    value: 2,
                    drops: drops.clone(),
                    callback: None,
                    panic: Some("candidate drop"),
                },
            )
        }));
        let payload = result.unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"comparator"));
        assert_eq!(drops.get(), 1);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        runtime.shutdown().unwrap();
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn rejected_creation_drains_both_value_and_comparator_even_if_both_panic() {
        let runtime = Runtime::new();
        let drops = Rc::new(Cell::new(0));
        let capture = DropFlag {
            value: 0,
            drops: drops.clone(),
            callback: None,
            panic: Some("comparator drop"),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.signal_with(
                DropFlag {
                    value: 1,
                    drops: drops.clone(),
                    callback: None,
                    panic: Some("value drop"),
                },
                move |_: &DropFlag, _: &DropFlag| {
                    let _capture = &capture;
                    false
                },
            )
        }));
        let payload = result.unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"comparator drop"));
        assert_eq!(drops.get(), 2);
        assert_eq!(runtime.node_count(), 0);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
    }

    #[test]
    fn destructor_panic_after_commit_does_not_undo_publication_or_marking() {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        let drops = Rc::new(Cell::new(0));
        let signal = runtime
            .with_owner(root, |runtime| {
                runtime.signal_with(
                    DropFlag {
                        value: 1,
                        drops: drops.clone(),
                        callback: None,
                        panic: Some("retired drop"),
                    },
                    |a, b| a.value == b.value,
                )
            })
            .unwrap();
        let observer = computation(&runtime, root, NodeKind::Memo);
        observe(&runtime, signal.id, observer);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            signal.try_set(
                &runtime,
                DropFlag {
                    value: 2,
                    drops: drops.clone(),
                    callback: None,
                    panic: None,
                },
            )
        }));
        assert!(result.is_err());
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(revisions(&runtime, signal), (1, 1));
        let storage = runtime.inner.storage.borrow();
        assert_eq!(
            storage.nodes.get(observer.slot).unwrap().metadata.state,
            State::Dirty
        );
        assert_eq!(
            storage
                .nodes
                .get(signal.id.slot)
                .unwrap()
                .value_cell::<DropFlag>(NodeKind::Signal)
                .unwrap()
                .borrow()
                .as_ref()
                .unwrap()
                .value,
            2
        );
        drop(storage);
        runtime.shutdown().unwrap();
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn observer_revision_overflow_poison_is_explicit_after_immediate_write() {
        let (runtime, root, signal) = setup(1_u32);
        let observer = computation(&runtime, root, NodeKind::Memo);
        observe(&runtime, signal.id, observer);
        runtime
            .inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(observer.slot)
            .unwrap()
            .metadata
            .freshness_revision = u64::MAX;
        assert_eq!(
            signal.try_set(&runtime, 2),
            Err(RuntimeError::CounterExhausted)
        );
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        assert_eq!(
            *runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(signal.id.slot)
                .unwrap()
                .snapshot::<u32>(NodeKind::Signal)
                .unwrap(),
            2
        );
    }
}
