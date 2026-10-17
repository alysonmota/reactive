use super::{Owner, PanicPayload, Runtime, RuntimeStatus, Storage};
use crate::{
    RuntimeError,
    arena::SlotId,
    context::{CallbackKind, CapabilityGuard, Operation, ReaderRuntimeGuard},
    node::Node,
};
use std::rc::Rc;

// Stored as FnMut so the closure and its captures remain outside catch_unwind.
// Registration is removed before invocation; each callback is attempted once.
pub(super) type Cleanup = Box<dyn FnMut(&Runtime) -> Result<(), RuntimeError>>;

pub(super) struct RetiredOwner {
    id: SlotId,
    owner: Owner,
    nodes: Vec<Node>,
}

#[derive(Default)]
pub(super) struct Teardown {
    pub(super) owners: Vec<RetiredOwner>,
    pub(super) nodes: Vec<Node>,
}

impl Runtime {
    /// Registers a one-shot cleanup on the current explicit live owner. Called
    /// in reverse order on disposal or before reexecuting its owning computation.
    /// FnMut keeps captured resources outside unwind; callbacks are not retried.
    pub fn on_cleanup(
        &self,
        callback: impl FnMut(&Runtime) -> Result<(), RuntimeError> + 'static,
    ) -> Result<(), RuntimeError> {
        self.resource_boundary(
            Some(callback),
            |callback| {
                self.check(Operation::Create)?;
                let owner = self
                    .inner
                    .context
                    .snapshot()
                    .owner
                    .ok_or(RuntimeError::MissingOwner)?;
                let mut storage = self.inner.storage.borrow_mut();
                let owner = storage
                    .owners
                    .get_mut(owner)
                    .ok_or(RuntimeError::OwnerDisposed)?;
                owner
                    .cleanups
                    .push(Box::new(callback.take().expect("cleanup")));
                Ok(())
            },
            |callback| self.release_one(callback),
        )
    }

    pub fn on_cleanup_infallible(
        &self,
        mut callback: impl FnMut(&Runtime) + 'static,
    ) -> Result<(), RuntimeError> {
        self.on_cleanup(move |runtime| {
            callback(runtime);
            Ok(())
        })
    }

    /// Secondary teardown failures, in attempt order. No recovery/reset is implied.
    pub fn secondary_failures(&self) -> Vec<Rc<RuntimeError>> {
        self.inner.secondary_failures.borrow().clone()
    }

    pub(super) fn record_teardown_failure(&self, failure: RuntimeError) {
        if self.first_failure().is_none() {
            self.record_failure(failure);
        } else {
            self.inner
                .secondary_failures
                .borrow_mut()
                .push(Rc::new(failure));
        }
    }

    pub(super) fn retain_active_tracking(storage: &mut Storage, id: SlotId, node: &mut Node) {
        if storage.tracking_stack.contains(&id) {
            storage.retired_tracking.push((
                id,
                std::mem::take(&mut node.reads),
                std::mem::take(&mut node.tracking),
            ));
        }
    }

    /// All nodes/owners in this subtree become inaccessible before removal and
    /// before any user cleanup/destructor. Resources are returned to the boundary.
    pub(super) fn detach_subtree(&self, storage: &mut Storage, root: SlotId) -> Teardown {
        let owner = storage.owners.get(root).expect("live subtree root");
        let parent = owner.parent;
        let index = owner.parent_index;
        let siblings = if let Some(parent) = parent {
            &mut storage
                .owners
                .get_mut(parent)
                .expect("live parent")
                .children
        } else {
            &mut storage.roots
        };
        siblings.swap_remove(index);
        if let Some(moved) = siblings.get(index).copied() {
            storage
                .owners
                .get_mut(moved)
                .expect("live sibling")
                .parent_index = index;
        }
        let mut order = Vec::with_capacity(1);
        Self::mark_subtree_disposing(storage, root, &mut order);
        self.detach_disposing_owners(storage, order)
    }

    fn mark_subtree_disposing(storage: &mut Storage, root: SlotId, order: &mut Vec<SlotId>) {
        let mut cursor = order.len();
        order.push(root);
        while cursor < order.len() {
            let id = order[cursor];
            let owner = storage.owners.get(id).expect("live subtree owner");
            order.extend_from_slice(&owner.children);
            for &node in &owner.nodes {
                storage.nodes.mark_disposing(node);
            }
            storage.owners.mark_disposing(id);
            cursor += 1;
        }
    }

    /// Mark the entire forest before detaching any edge. Append each root's
    /// breadth-first traversal in current root order, then reverse the whole
    /// sequence: exactly the former last-root-first, reverse-BFS teardown order.
    /// No cross-root reader survives, so no invalidation/repair is required.
    pub(super) fn detach_all_roots(&self, storage: &mut Storage) -> Teardown {
        let mut order = Vec::new();
        for index in 0..storage.roots.len() {
            let root = storage.roots[index];
            Self::mark_subtree_disposing(storage, root, &mut order);
        }
        storage.roots.clear();
        self.detach_disposing_owners(storage, order)
    }

    fn detach_disposing_owners(&self, storage: &mut Storage, order: Vec<SlotId>) -> Teardown {
        let mut teardown = Teardown::default();
        for id in order.into_iter().rev() {
            let mut owner = storage.owners.remove(id).expect("disposing owner");
            let mut nodes = Vec::new();
            for node_id in owner.nodes.drain(..) {
                self.detach_edges(storage, node_id);
                let mut node = storage.nodes.remove(node_id).expect("disposing node");
                Self::retain_active_tracking(storage, node_id, &mut node);
                nodes.push(node);
            }
            owner.children.clear();
            teardown.owners.push(RetiredOwner { id, owner, nodes });
        }
        teardown
    }

    fn drain_teardown(
        &self,
        teardown: &mut Teardown,
    ) -> (Option<RuntimeError>, Option<PanicPayload>) {
        let mut first_error = None;
        let mut first_panic = None;
        for entry in &mut teardown.owners {
            let _context = self.inner.context.enter(|snapshot| {
                snapshot.reader = None;
                snapshot.owner = Some(entry.id);
            });
            let _reader_runtime = ReaderRuntimeGuard::enter(None);
            while let Some(mut callback) = entry.owner.cleanups.pop() {
                let outcome = {
                    let _capabilities = CapabilityGuard::enter(CallbackKind::Cleanup);
                    // Only invariants are restored: after unwind the callback
                    // is discarded, never reused as a recovered computation.
                    #[cfg(panic = "unwind")]
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(self)));
                    #[cfg(not(panic = "unwind"))]
                    let result: Result<Result<(), RuntimeError>, PanicPayload> = Ok(callback(self));
                    result
                };
                match outcome {
                    Ok(Ok(())) => (),
                    Ok(Err(error)) => {
                        self.record_teardown_failure(error.clone());
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                    Err(payload) => {
                        self.record_teardown_failure(RuntimeError::Panic);
                        self.merge_panic(&mut first_panic, Some(payload));
                    }
                }
                self.merge_panic(&mut first_panic, self.release_one(callback));
            }
            for node in entry.nodes.drain(..) {
                self.merge_panic(&mut first_panic, self.release_payload(node.payload));
            }
        }
        for node in teardown.nodes.drain(..) {
            self.merge_panic(&mut first_panic, self.release_payload(node.payload));
        }
        (first_error, first_panic)
    }

    pub(super) fn release_teardown(&self, mut teardown: Teardown) -> Result<(), RuntimeError> {
        let (error, panic) = self.drain_teardown(&mut teardown);
        if let Some(panic) = panic {
            std::panic::resume_unwind(panic);
        }
        error.map_or(Ok(()), Err)
    }

    /// Removes the previous execution content, then installs a new generation.
    /// Empty scopes renew in-place through the arena without vector allocation.
    /// A cleanup may dispose this computation; false means abandon execution.
    pub(super) fn prepare_execution(&self, id: SlotId) -> Result<bool, RuntimeError> {
        // Even the callback-free renewal retains the checked depth failure.
        // It need not suspend/restore the reader or copy a ContextGuard because
        // an empty scope cannot run cleanup, release payloads or invoke user code.
        let disposal_depth = self.next_disposal_depth()?;
        let old = {
            let mut storage = self.inner.storage.borrow_mut();
            let Some(node) = storage.nodes.get(id) else {
                return Ok(false);
            };
            let Some(old) = node.execution_owner else {
                drop(storage);
                return self.prepare_new_execution(id, disposal_depth, None);
            };
            let Some(scope) = storage.owners.get(old) else {
                drop(storage);
                return self.prepare_new_execution(id, disposal_depth, None);
            };
            if scope.nodes.is_empty() && scope.children.is_empty() && scope.cleanups.is_empty() {
                let parent = scope.parent.expect("execution parent");
                let index = scope.parent_index;
                let new = if let Some(new) = storage.owners.renew(old) {
                    new
                } else {
                    if !storage.owners.can_insert() {
                        return Err(RuntimeError::CounterExhausted);
                    }
                    // Exhausted generation retires exactly as before.
                    let scope = storage.owners.remove(old).expect("empty scope");
                    storage.owners.insert(scope)
                };
                storage.owners.get_mut(parent).expect("life owner").children[index] = new;
                storage
                    .nodes
                    .get_mut(id)
                    .expect("computation")
                    .execution_owner = Some(new);
                return Ok(true);
            }
            Some(old)
        };
        self.prepare_new_execution(id, disposal_depth, old)
    }

    fn prepare_new_execution(
        &self,
        id: SlotId,
        disposal_depth: usize,
        old: Option<SlotId>,
    ) -> Result<bool, RuntimeError> {
        let _context = self.inner.context.enter(|snapshot| {
            snapshot.disposal_depth = disposal_depth;
            snapshot.reader = None;
        });
        let mut reusable = None;
        if let Some(old) = old {
            let mut teardown = self.detach_subtree(&mut self.inner.storage.borrow_mut(), old);
            let (error, panic) = self.drain_teardown(&mut teardown);
            if let Some(panic) = panic {
                std::panic::resume_unwind(panic);
            }
            if let Some(error) = error {
                return Err(error);
            }
            reusable = teardown.owners.pop().map(|entry| entry.owner);
        }
        // Revalidate after arbitrary cleanup reentry; no scope or closure may
        // be installed in a reused node slot or disposed life owner.
        let mut storage = self.inner.storage.borrow_mut();
        let Some(node) = storage.nodes.get(id) else {
            return Ok(false);
        };
        if self.status() != RuntimeStatus::Healthy {
            return Err(self
                .first_failure()
                .as_deref()
                .cloned()
                .unwrap_or(RuntimeError::RuntimeClosed));
        }
        let parent = node.owner;
        let owner = storage
            .owners
            .get(parent)
            .ok_or(RuntimeError::OwnerDisposed)?;
        let depth = owner.depth.checked_add(1).ok_or(RuntimeError::DepthLimit)?;
        let mut scope = reusable.unwrap_or(Owner {
            parent: None,
            parent_index: 0,
            depth: 0,
            children: Vec::new(),
            nodes: Vec::new(),
            cleanups: Vec::new(),
        });
        scope.parent = Some(parent);
        scope.parent_index = owner.children.len();
        scope.depth = depth;
        if !storage.owners.can_insert() {
            return Err(RuntimeError::CounterExhausted);
        }
        let new = storage.owners.insert(scope);
        storage
            .owners
            .get_mut(parent)
            .expect("parent")
            .children
            .push(new);
        storage
            .nodes
            .get_mut(id)
            .expect("computation")
            .execution_owner = Some(new);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EvaluationPhase;
    use crate::{DisposeOutcome, EffectHandle, FlushOutcome, OwnerHandle, Signal, State};
    use std::cell::{Cell, RefCell};

    #[test]
    fn memo_start_renews_scope_and_restores_tracking_context() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let memo = rt
            .with_owner(root, |rt| rt.memo(move |rt| input.try_get_copy(rt)))
            .unwrap();
        memo.try_get_copy(&rt).unwrap();
        input.try_set(&rt, 1).unwrap();
        let old = rt
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .execution_owner
            .unwrap();
        let before = rt.inner.context.snapshot();
        {
            let mut guard = rt
                .enter_evaluation(memo.id, EvaluationPhase::Checking)
                .unwrap();
            guard.set_phase(EvaluationPhase::Cleaning).unwrap();
            assert!(rt.prepare_execution(memo.id.slot).unwrap());
            guard.set_phase(EvaluationPhase::Executing).unwrap();
            assert!(guard.tracking);
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(memo.id.slot).unwrap();
            let new = node.execution_owner.unwrap();
            assert_eq!(new.index, old.index);
            assert_ne!(new.generation, old.generation);
            assert!(storage.owners.get(old).is_none());
            assert_eq!(node.metadata.phase, EvaluationPhase::Executing);
            assert_eq!(storage.tracking_stack.last(), Some(&memo.id.slot));
            assert_eq!(rt.inner.context.snapshot().owner, Some(new));
            assert_eq!(rt.inner.context.snapshot().reader, Some(memo.id.slot));
        }
        assert_eq!(rt.inner.context.snapshot(), before);
        assert!(rt.inner.storage.borrow().tracking_stack.is_empty());
        assert_eq!(memo.try_get_copy(&rt), Ok(1));
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn memo_start_retires_resourceful_scopes_through_protected_path() {
        for kind in ["node", "child", "cleanup"] {
            let rt = Runtime::new();
            let root = rt.create_root().unwrap();
            let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
            let memo = rt
                .with_owner(root, |rt| rt.memo(move |rt| input.try_get_copy(rt)))
                .unwrap();
            memo.try_get_copy(&rt).unwrap();
            let old = rt
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .execution_owner
                .unwrap();
            let scope = crate::OwnerHandle::new(rt.handle_id(old));
            rt.with_owner(scope, |rt| {
                match kind {
                    "node" => {
                        rt.signal(7_u32)?;
                    }
                    "child" => {
                        rt.create_owner(scope)?;
                    }
                    _ => rt.on_cleanup(|_| Ok(()))?,
                }
                Ok(())
            })
            .unwrap();
            input.try_set(&rt, 1).unwrap();
            let epoch = rt.inner.storage.borrow().next_execution_epoch;
            {
                let _guard = rt
                    .enter_evaluation(memo.id, EvaluationPhase::Checking)
                    .unwrap();
                let before = rt.inner.context.snapshot();
                assert_eq!(rt.inner.context.snapshot(), before);
                let storage = rt.inner.storage.borrow();
                assert_eq!(storage.next_execution_epoch, epoch);
                assert!(storage.owners.get(old).is_some());
                let node = storage.nodes.get(memo.id.slot).unwrap();
                assert_eq!(node.execution_owner, Some(old));
                assert_eq!(node.metadata.phase, EvaluationPhase::Checking);
            }
            assert_eq!(memo.try_get_copy(&rt), Ok(1));
            assert!(scope.validate(&rt).is_err());
            rt.verify_invariants().unwrap();
        }
    }

    #[test]
    fn memo_start_preserves_disposal_depth_and_tracking_epoch_overflow_order() {
        for disposal in [true, false] {
            let rt = Runtime::new();
            let root = rt.create_root().unwrap();
            let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
            let memo = rt
                .with_owner(root, |rt| rt.memo(move |rt| input.try_get_copy(rt)))
                .unwrap();
            memo.try_get_copy(&rt).unwrap();
            input.try_set(&rt, 1).unwrap();
            let old = rt
                .inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .execution_owner
                .unwrap();
            let before = rt.inner.context.snapshot();
            {
                let _depth = rt.inner.context.enter(|s| {
                    if disposal {
                        s.disposal_depth = usize::MAX;
                    }
                });
                if !disposal {
                    rt.inner.storage.borrow_mut().next_execution_epoch = u64::MAX;
                }
                assert_eq!(memo.try_get_copy(&rt), Err(RuntimeError::CounterExhausted));
            }
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(memo.id.slot).unwrap();
            assert_eq!(node.execution_owner == Some(old), disposal);
            assert_eq!(node.metadata.phase, EvaluationPhase::Idle);
            assert!(storage.tracking_stack.is_empty());
            #[cfg(feature = "diagnostics")]
            assert!(storage.diagnostics.active.is_empty());
            drop(storage);
            assert_eq!(rt.inner.context.snapshot(), before);
            assert_eq!(rt.status(), RuntimeStatus::Poisoned);
            rt.verify_invariants().unwrap();
        }
    }

    #[test]
    fn empty_renewal_keeps_context_and_checks_depth_before_mutating_generations() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let memo = rt.with_owner(root, |rt| rt.memo(|_| Ok(1_u64))).unwrap();
        memo.try_get_copy(&rt).unwrap();
        let old = rt
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .execution_owner
            .unwrap();
        let context = rt.inner.context.snapshot();
        assert!(rt.prepare_execution(memo.id.slot).unwrap());
        assert_eq!(rt.inner.context.snapshot(), context);
        let new = rt
            .inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .execution_owner
            .unwrap();
        assert_eq!(new.index, old.index);
        assert_ne!(new.generation, old.generation);
        let _context = rt.inner.context.enter(|s| s.disposal_depth = usize::MAX);
        assert_eq!(
            rt.prepare_execution(memo.id.slot),
            Err(RuntimeError::CounterExhausted)
        );
        assert_eq!(
            rt.inner
                .storage
                .borrow()
                .nodes
                .get(memo.id.slot)
                .unwrap()
                .execution_owner,
            Some(new)
        );
        drop(_context);
        rt.verify_invariants().unwrap();
    }

    fn setup() -> (Runtime, OwnerHandle) {
        let runtime = Runtime::new();
        let owner = runtime.create_root().unwrap();
        (runtime, owner)
    }
    fn signal(runtime: &Runtime, owner: OwnerHandle, initial: i32) -> Signal<i32> {
        runtime
            .with_owner(owner, |runtime| runtime.signal(initial))
            .unwrap()
    }
    fn flush(runtime: &Runtime) -> crate::FlushReport {
        runtime.flush(Default::default()).unwrap()
    }

    #[test]
    fn shutdown_preserves_root_reverse_bfs_cleanup_and_payload_order() {
        struct Resource(&'static str, Rc<RefCell<Vec<(&'static str, u8)>>>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.1.borrow_mut().push((self.0, 0));
            }
        }
        let rt = Runtime::new();
        let a = rt.create_root().unwrap();
        let removed = rt.create_root().unwrap();
        let c = rt.create_root().unwrap();
        let d = rt.create_root().unwrap();
        rt.dispose(removed).unwrap(); // swap_remove gives roots [a, d, c]
        let a1 = rt.create_owner(a).unwrap();
        let a2 = rt.create_owner(a).unwrap();
        let a11 = rt.create_owner(a1).unwrap();
        let a21 = rt.create_owner(a2).unwrap();
        let log = Rc::new(RefCell::new(Vec::new()));
        for (owner, label) in [
            (a, "a"),
            (a1, "a1"),
            (a2, "a2"),
            (a11, "a11"),
            (a21, "a21"),
            (c, "c"),
            (d, "d"),
        ] {
            rt.with_owner(owner, |rt| {
                rt.signal_with(Resource(label, log.clone()), |_, _| false)?;
                for sequence in [1, 2] {
                    let log = log.clone();
                    rt.on_cleanup(move |rt| {
                        let storage = rt.inner.storage.borrow();
                        assert!(storage.roots.is_empty());
                        assert_eq!(storage.nodes.len(), 0);
                        assert_eq!(storage.owners.len(), 0);
                        drop(storage);
                        assert_eq!(rt.dispose(a)?, DisposeOutcome::AlreadyDisposed);
                        assert_eq!(rt.create_root(), Err(RuntimeError::RuntimeClosed));
                        rt.shutdown()?; // reentrant, closed and already empty
                        log.borrow_mut().push((label, sequence));
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .unwrap();
        }
        rt.shutdown().unwrap();
        let expected: Vec<_> = ["c", "d", "a21", "a11", "a2", "a1", "a"]
            .into_iter()
            .flat_map(|label| [(label, 2), (label, 1), (label, 0)])
            .collect();
        assert_eq!(*log.borrow(), expected);
        rt.shutdown().unwrap();
        assert_eq!(*log.borrow(), expected);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn shutdown_cross_root_edges_never_mark_readers_that_will_die() {
        let rt = Runtime::new();
        let readers = rt.create_root().unwrap();
        let sources = rt.create_root().unwrap();
        let input = signal(&rt, sources, 1);
        let memo = rt
            .with_owner(readers, |rt| rt.memo(move |rt| input.try_get_copy(rt)))
            .unwrap();
        rt.with_owner(readers, |rt| {
            rt.effect(move |rt| {
                memo.try_get_copy(rt)?;
                Ok(())
            })
        })
        .unwrap();
        flush(&rt);
        // Old shutdown marks the memo before removing the readers' root,
        // exhausting freshness and recording poison despite no surviving reader.
        rt.inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(memo.id.slot)
            .unwrap()
            .metadata
            .freshness_revision = u64::MAX;
        #[cfg(feature = "diagnostics")]
        rt.reset_diagnostic_counters().unwrap();
        rt.shutdown().unwrap();
        assert!(rt.first_failure().is_none());
        assert!(!rt.has_pending_effects());
        #[cfg(feature = "diagnostics")]
        {
            let d = rt.diagnostics().unwrap();
            assert_eq!(d.counters.mark_visits, 0);
            assert_eq!(d.counters.tickets_enqueued, 0);
            assert_eq!(d.counters.edges_removed, 2);
        }
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn shutdown_cross_root_failures_drain_in_order_and_preserve_first_failure() {
        let rt = Runtime::new();
        let a = rt.create_root().unwrap();
        let b = rt.create_root().unwrap();
        let log = Rc::new(RefCell::new(Vec::new()));
        for (owner, label) in [(a, 1), (b, 2)] {
            let log = log.clone();
            rt.with_owner(owner, |rt| {
                rt.on_cleanup(move |_| {
                    log.borrow_mut().push(label);
                    Err(if label == 2 {
                        RuntimeError::CounterExhausted
                    } else {
                        RuntimeError::InvalidHandle
                    })
                })
            })
            .unwrap();
        }
        assert_eq!(rt.shutdown(), Err(RuntimeError::CounterExhausted));
        assert_eq!(*log.borrow(), [2, 1]);
        assert_eq!(
            rt.first_failure().as_deref(),
            Some(&RuntimeError::CounterExhausted)
        );
        assert_eq!(
            rt.secondary_failures()
                .iter()
                .map(|error| error.as_ref().clone())
                .collect::<Vec<_>>(),
            [RuntimeError::InvalidHandle]
        );
        rt.shutdown().unwrap();
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn shutdown_inside_effect_retires_tracking_and_never_restores_callback() {
        let rt = Runtime::new();
        let readers = rt.create_root().unwrap();
        let sources = rt.create_root().unwrap();
        let input = signal(&rt, sources, 1);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let effect = rt
            .with_owner(readers, |rt| {
                rt.effect(move |rt| {
                    input.try_get_copy(rt)?;
                    captured.set(captured.get() + 1);
                    rt.shutdown()?;
                    assert!(rt.inner.storage.borrow().nodes.get(input.id.slot).is_none());
                    Ok(())
                })
            })
            .unwrap();
        rt.flush(Default::default()).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(rt.dispose(effect).unwrap(), DisposeOutcome::AlreadyDisposed);
        let storage = rt.inner.storage.borrow();
        assert!(storage.tracking_stack.is_empty());
        assert!(storage.retired_tracking.is_empty());
        assert_eq!(storage.nodes.len(), 0);
        assert_eq!(storage.owners.len(), 0);
        drop(storage);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn shutdown_panic_drains_other_roots_and_keeps_original_payload() {
        let rt = Runtime::new();
        let a = rt.create_root().unwrap();
        let b = rt.create_root().unwrap();
        let attempts = Rc::new(Cell::new(0));
        let captured = attempts.clone();
        rt.with_owner(a, |rt| {
            rt.on_cleanup(move |_| {
                captured.set(captured.get() + 1);
                Err(RuntimeError::InvalidHandle)
            })
        })
        .unwrap();
        rt.with_owner(b, |rt| {
            rt.on_cleanup(|_| std::panic::panic_any("shutdown root b"))
        })
        .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.shutdown()));
        assert_eq!(
            result.unwrap_err().downcast_ref::<&str>(),
            Some(&"shutdown root b")
        );
        assert_eq!(attempts.get(), 1);
        assert_eq!(rt.first_failure().as_deref(), Some(&RuntimeError::Panic));
        assert_eq!(
            rt.secondary_failures()[0].as_ref(),
            &RuntimeError::InvalidHandle
        );
        rt.verify_invariants().unwrap();
        rt.shutdown().unwrap();
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn descendants_before_parent_and_lifo_cleanups_with_entire_subtree_dead() {
        let (runtime, root) = setup();
        let child = runtime.create_owner(root).unwrap();
        let grandchild = runtime.create_owner(child).unwrap();
        let input = signal(&runtime, grandchild, 1);
        let log = Rc::new(RefCell::new(Vec::new()));
        for (owner, label) in [(root, 1), (root, 2), (child, 3), (grandchild, 4)] {
            let log = log.clone();
            runtime
                .with_owner(owner, |runtime| {
                    runtime.on_cleanup(move |runtime| {
                        assert!(runtime.inner.storage.try_borrow_mut().is_ok());
                        assert_eq!(
                            input.try_peek_copy(runtime),
                            Err(RuntimeError::InvalidHandle)
                        );
                        assert_eq!(runtime.dispose(root)?, DisposeOutcome::AlreadyDisposed);
                        assert_eq!(runtime.dispose(child)?, DisposeOutcome::AlreadyDisposed);
                        log.borrow_mut().push(label);
                        Ok(())
                    })
                })
                .unwrap();
        }
        assert!(log.borrow().is_empty());
        runtime.dispose(root).unwrap();
        assert_eq!(*log.borrow(), [4, 3, 2, 1]);
        runtime.dispose(root).unwrap();
        assert_eq!(log.borrow().len(), 4);
        assert_eq!(runtime.owner_count(), 0);
    }

    #[test]
    fn reexecution_disposes_previous_content_but_preserves_computation() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let scopes = Rc::new(RefCell::new(Vec::new()));
        let children = Rc::new(RefCell::new(Vec::new()));
        let values = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::new(RefCell::new(Vec::new()));
        let (captured_scopes, captured_children, captured_values, captured_log) = (
            scopes.clone(),
            children.clone(),
            values.clone(),
            log.clone(),
        );
        let effect = runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    let value = trigger.try_get_copy(runtime)?;
                    captured_scopes
                        .borrow_mut()
                        .push(runtime.current_owner()?.unwrap());
                    captured_values.borrow_mut().push(runtime.signal(value)?);
                    let child = runtime.create_owner(runtime.current_owner()?.unwrap())?;
                    captured_children.borrow_mut().push(child);
                    let log = captured_log.clone();
                    runtime.on_cleanup_infallible(move |_| log.borrow_mut().push(value))?;
                    Ok(())
                })
            })
            .unwrap();
        flush(&runtime);
        let old_scope = scopes.borrow()[0];
        let old_signal = values.borrow()[0];
        let old_child = children.borrow()[0];
        trigger.try_set(&runtime, 1).unwrap();
        assert_eq!(flush(&runtime).callbacks, 1);
        assert_eq!(*log.borrow(), [0]);
        assert!(effect.validate(&runtime).is_ok());
        assert_eq!(
            old_scope.validate(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(
            old_child.validate(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_eq!(
            old_signal.try_peek_copy(&runtime),
            Err(RuntimeError::InvalidHandle)
        );
        assert_ne!(old_scope, scopes.borrow()[1]);
        assert_eq!(
            runtime.dispose(old_scope),
            Ok(DisposeOutcome::AlreadyDisposed)
        );
        runtime.dispose(effect).unwrap();
        assert_eq!(*log.borrow(), [0, 1]);
        assert_eq!(runtime.owner_count(), 1);
        assert_eq!(runtime.node_count(), 1);
        runtime.verify_graph();
    }

    #[test]
    fn empty_execution_scope_renews_generation_without_growing_arena() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let scope = Rc::new(Cell::new(None));
        let captured = scope.clone();
        runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    trigger.try_get_copy(runtime)?;
                    captured.set(runtime.current_owner()?);
                    Ok(())
                })
            })
            .unwrap();
        flush(&runtime);
        let first = scope.get().unwrap();
        for value in 1..1000 {
            trigger.try_set(&runtime, value).unwrap();
            flush(&runtime);
            let current = scope.get().unwrap();
            assert_eq!(current.id.slot.index, first.id.slot.index);
            assert_eq!(runtime.owner_count(), 2);
            assert_eq!(runtime.dispose(first), Ok(DisposeOutcome::AlreadyDisposed));
        }
        assert_ne!(scope.get().unwrap(), first);
        runtime.verify_graph();
    }

    #[test]
    fn execution_scope_reuses_vectors_after_content_teardown() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let scopes = Rc::new(RefCell::new(Vec::new()));
        let captured = scopes.clone();
        runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    trigger.try_get_copy(runtime)?;
                    let scope = runtime.current_owner()?.unwrap();
                    for value in 0..20 {
                        runtime.signal(value)?;
                        runtime.create_owner(scope)?;
                        runtime.on_cleanup(|_| Ok(()))?;
                    }
                    let storage = runtime.inner.storage.borrow();
                    let scope = storage.owners.get(scope.id.slot).unwrap();
                    captured.borrow_mut().push((
                        scope.children.as_ptr(),
                        scope.nodes.as_ptr(),
                        scope.cleanups.as_ptr(),
                    ));
                    Ok(())
                })
            })
            .unwrap();
        flush(&runtime);
        trigger.try_set(&runtime, 1).unwrap();
        flush(&runtime);
        assert_eq!(scopes.borrow()[0], scopes.borrow()[1]);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn old_child_current_ticket_is_cancelled_before_parent_reexecution() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let log = Rc::new(RefCell::new(Vec::new()));
        let captured = log.clone();
        runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    let version = trigger.try_get_copy(runtime)?;
                    let log = captured.clone();
                    runtime.effect(move |runtime| {
                        trigger.try_get_copy(runtime)?;
                        log.borrow_mut().push(version);
                        Ok(())
                    })?;
                    Ok(())
                })
            })
            .unwrap();
        flush(&runtime);
        trigger.try_set(&runtime, 1).unwrap();
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (2, 2));
        assert_eq!(*log.borrow(), [0, 1]);
        assert!(!runtime.has_pending_effects());
        runtime.verify_graph();
    }

    #[test]
    fn cleanup_writes_are_untracked_and_consumed_before_the_next_callback() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let input = signal(&runtime, root, 0);
        let unrelated = signal(&runtime, root, 0);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let captured = seen.clone();
        let effect = runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    trigger.try_get_copy(runtime)?;
                    captured.borrow_mut().push(input.try_get_copy(runtime)?);
                    runtime.on_cleanup(move |runtime| {
                        unrelated.try_get_copy(runtime)?;
                        input.try_set(runtime, 5)?;
                        assert_eq!(
                            runtime.on_cleanup(|_| Ok(())),
                            Err(RuntimeError::Reentrancy)
                        );
                        assert_eq!(runtime.signal(1), Err(RuntimeError::Reentrancy));
                        assert_eq!(
                            runtime.flush(Default::default()).unwrap_err(),
                            RuntimeError::Reentrancy
                        );
                        Ok(())
                    })?;
                    Ok(())
                })
            })
            .unwrap();
        flush(&runtime);
        trigger.try_set(&runtime, 1).unwrap();
        assert_eq!(flush(&runtime).rounds, 1);
        assert_eq!(*seen.borrow(), [0, 5]);
        unrelated.try_set(&runtime, 1).unwrap();
        assert!(!runtime.has_pending_effects());
        runtime.dispose(effect).unwrap();
        runtime.verify_graph();
    }

    #[test]
    fn cleanup_can_dispose_other_targets_and_cross_runtime_reads_are_not_tracked() {
        let (runtime, root) = setup();
        let doomed = runtime.create_root().unwrap();
        let (other, other_root) = setup();
        let external = signal(&other, other_root, 1);
        let weak = other.downgrade();
        runtime
            .with_owner(root, |runtime| {
                runtime.on_cleanup(move |runtime| {
                    let other = weak.upgrade().unwrap();
                    assert_eq!(external.try_get_copy(&other)?, 1);
                    external.try_set(&other, 2)?;
                    runtime.dispose(doomed)?;
                    Ok(())
                })
            })
            .unwrap();
        runtime.dispose(root).unwrap();
        assert_eq!(external.try_peek_copy(&other).unwrap(), 2);
        assert_eq!(runtime.owner_count(), 0);
    }

    #[test]
    fn cleanup_self_disposal_during_cleaning_skips_the_new_callback() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let holder = Rc::new(Cell::new(None::<EffectHandle>));
        let calls = Rc::new(Cell::new(0));
        let (captured, count) = (holder.clone(), calls.clone());
        let effect = runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    trigger.try_get_copy(runtime)?;
                    count.set(count.get() + 1);
                    let holder = captured.clone();
                    runtime.on_cleanup(move |runtime| {
                        runtime.dispose(holder.get().unwrap())?;
                        Ok(())
                    })?;
                    Ok(())
                })
            })
            .unwrap();
        holder.set(Some(effect));
        flush(&runtime);
        trigger.try_set(&runtime, 1).unwrap();
        let report = flush(&runtime);
        assert_eq!(
            (report.outcome, report.callbacks),
            (FlushOutcome::Stabilized, 0)
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(effect.validate(&runtime), Err(RuntimeError::InvalidHandle));
        assert!(!runtime.has_pending_effects());
        runtime.verify_graph();
    }

    #[test]
    fn cleanup_owner_disposal_during_cleaning_prevents_scope_resurrection() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    trigger.try_get_copy(runtime)?;
                    runtime.on_cleanup(move |runtime| {
                        runtime.dispose(root)?;
                        Ok(())
                    })?;
                    Ok(())
                })
            })
            .unwrap();
        flush(&runtime);
        trigger.try_set(&runtime, 1).unwrap();
        assert_eq!(flush(&runtime).callbacks, 0);
        assert_eq!((runtime.owner_count(), runtime.node_count()), (0, 0));
    }

    #[test]
    fn active_tracking_buffers_survive_self_disposal_until_callback_returns() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        let holder = Rc::new(Cell::new(None::<EffectHandle>));
        let captured = holder.clone();
        let handle = runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    input.try_get_copy(runtime)?;
                    let old_scope = runtime.current_owner()?.unwrap();
                    runtime.dispose(captured.get().unwrap())?;
                    assert_eq!(runtime.inner.storage.borrow().retired_tracking.len(), 1);
                    assert_eq!(runtime.signal(1), Err(RuntimeError::OwnerDisposed));
                    assert_eq!(
                        runtime.on_cleanup(|_| Ok(())),
                        Err(RuntimeError::OwnerDisposed)
                    );
                    assert_eq!(
                        runtime.dispose(old_scope),
                        Ok(DisposeOutcome::AlreadyDisposed)
                    );
                    // Explicitly enter another live owner, causing slot reuse. The
                    // suspended old tracking context must not be installed in that node.
                    runtime.with_owner(root, |runtime| runtime.effect(|_| Ok(())))?;
                    Ok(())
                })
            })
            .unwrap();
        holder.set(Some(handle));
        assert_eq!(flush(&runtime).callbacks, 2);
        assert!(runtime.inner.storage.borrow().retired_tracking.is_empty());
        runtime.verify_graph();
    }

    #[test]
    fn failed_cleanup_runs_remaining_once_and_records_secondary_failures() {
        let (runtime, root) = setup();
        let survivor = runtime.create_root().unwrap();
        let input = signal(&runtime, survivor, 0);
        let calls = Rc::new(RefCell::new(Vec::new()));
        for label in 0..3 {
            let calls = calls.clone();
            runtime
                .with_owner(root, |runtime| {
                    runtime.on_cleanup(move |runtime| {
                        calls.borrow_mut().push(label);
                        if label < 2 {
                            assert_eq!(input.try_set(runtime, 1), Err(RuntimeError::Poisoned));
                        }
                        Err(RuntimeError::CallbackFailed(match label {
                            2 => "first",
                            1 => "second",
                            _ => "third",
                        }))
                    })
                })
                .unwrap();
        }
        assert_eq!(
            runtime.dispose(root),
            Err(RuntimeError::CallbackFailed("first"))
        );
        assert_eq!(*calls.borrow(), [2, 1, 0]);
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::CallbackFailed("first"))
        );
        assert_eq!(
            runtime
                .secondary_failures()
                .iter()
                .map(|error| error.as_ref())
                .collect::<Vec<_>>(),
            [
                &RuntimeError::CallbackFailed("second"),
                &RuntimeError::CallbackFailed("third")
            ]
        );
        assert_eq!(runtime.dispose(root), Ok(DisposeOutcome::AlreadyDisposed));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn cleanup_panic_and_capture_drop_panic_drain_all_and_preserve_original_panic() {
        struct PanicDrop;
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                std::panic::panic_any("capture drop");
            }
        }
        let (runtime, root) = setup();
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        runtime
            .with_owner(root, |runtime| {
                runtime.on_cleanup_infallible(move |_| count.set(count.get() + 1))
            })
            .unwrap();
        let capture = PanicDrop;
        runtime
            .with_owner(root, |runtime| {
                runtime.on_cleanup(move |_| {
                    let _capture = &capture;
                    std::panic::panic_any("cleanup");
                })
            })
            .unwrap();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.dispose(root)));
        assert_eq!(result.unwrap_err().downcast_ref::<&str>(), Some(&"cleanup"));
        assert_eq!(calls.get(), 1);
        assert_eq!(runtime.owner_count(), 0);
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::Panic)
        );
        assert_eq!(runtime.secondary_failures().len(), 1);
        assert_eq!(runtime.dispose(root), Ok(DisposeOutcome::AlreadyDisposed));
        assert_eq!(super::super::context::check(Operation::Create), Ok(()));
    }

    #[test]
    fn cleanup_failure_poison_stops_reexecution_but_drains_old_content() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let handle = runtime
            .with_owner(root, |runtime| {
                runtime.effect(move |runtime| {
                    trigger.try_get_copy(runtime)?;
                    captured.set(captured.get() + 1);
                    runtime.signal(5)?;
                    runtime.on_cleanup(|_| Err(RuntimeError::CallbackFailed("cleanup")))?;
                    Ok(())
                })
            })
            .unwrap();
        flush(&runtime);
        trigger.try_set(&runtime, 1).unwrap();
        let report = flush(&runtime);
        assert_eq!(
            (report.outcome, report.callbacks),
            (FlushOutcome::Failed, 0)
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(runtime.node_count(), 2);
        let storage = runtime.inner.storage.borrow();
        assert_eq!(
            storage.nodes.get(handle.id.slot).unwrap().metadata.state,
            State::Dirty
        );
        drop(storage);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn memo_and_its_internal_cleanups_cannot_expand_pure_capabilities() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        let scope = Rc::new(Cell::new(None));
        let captured = scope.clone();
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| {
                    captured.set(runtime.current_owner()?);
                    assert_eq!(
                        runtime.on_cleanup(|_| Ok(())),
                        Err(RuntimeError::MutationForbidden)
                    );
                    input.try_get_copy(runtime)
                })
            })
            .unwrap();
        memo.try_get_copy(&runtime).unwrap();
        runtime
            .with_owner(scope.get().unwrap(), |runtime| {
                runtime.on_cleanup(move |runtime| {
                    assert_eq!(input.try_set(runtime, 5), Err(RuntimeError::WriteForbidden));
                    assert_eq!(runtime.dispose(root), Err(RuntimeError::MutationForbidden));
                    Ok(())
                })
            })
            .unwrap();
        input.try_set(&runtime, 1).unwrap();
        assert_eq!(memo.try_get_copy(&runtime).unwrap(), 1);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn final_facade_drop_drops_cleanup_captures_but_never_invokes_cleanup() {
        struct DropCount(Rc<Cell<usize>>);
        impl Drop for DropCount {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let (runtime, root) = setup();
        let drops = Rc::new(Cell::new(0));
        let capture = DropCount(drops.clone());
        runtime
            .with_owner(root, |runtime| {
                runtime.on_cleanup(move |_| {
                    let _capture = &capture;
                    panic!("implicit cleanup is forbidden");
                })
            })
            .unwrap();
        drop(runtime);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn shutdown_attempts_cleanups_even_after_poison_without_writes_or_effects() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        runtime
            .with_owner(root, |runtime| {
                runtime.on_cleanup(move |runtime| {
                    captured.set(captured.get() + 1);
                    assert_eq!(input.try_get_copy(runtime), Err(RuntimeError::Poisoned));
                    assert_eq!(runtime.effect(|_| Ok(())), Err(RuntimeError::Poisoned));
                    Ok(())
                })
            })
            .unwrap();
        runtime
            .with_owner(root, |runtime| {
                runtime.effect(|_| Err(RuntimeError::CallbackFailed("effect")))
            })
            .unwrap();
        assert_eq!(flush(&runtime).outcome, FlushOutcome::Failed);
        runtime.shutdown().unwrap();
        runtime.shutdown().unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!((runtime.owner_count(), runtime.node_count()), (0, 0));
        assert!(!runtime.has_pending_effects());
    }

    #[test]
    fn cleanup_registration_errors_do_not_poison_or_invoke_callbacks() {
        let runtime = Runtime::new();
        assert_eq!(
            runtime.on_cleanup(|_| panic!("no owner")),
            Err(RuntimeError::MissingOwner)
        );
        let root = runtime.create_root().unwrap();
        runtime
            .with_owner(root, |runtime| {
                runtime.dispose(root)?;
                assert_eq!(
                    runtime.on_cleanup(|_| panic!("dead owner")),
                    Err(RuntimeError::OwnerDisposed)
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn effect_failure_discards_provisional_reads_and_preserves_previous_live_edges() {
        for panics in [false, true] {
            let (runtime, root) = setup();
            let branch = signal(&runtime, root, 0);
            let a = signal(&runtime, root, 1);
            let b = signal(&runtime, root, 2);
            let effect = runtime
                .with_owner(root, |runtime| {
                    runtime.effect(move |runtime| {
                        if branch.try_get_copy(runtime)? == 0 {
                            a.try_get_copy(runtime)?;
                            return Ok(());
                        }
                        b.try_get_copy(runtime)?;
                        if panics {
                            panic!("effect")
                        }
                        Err(RuntimeError::CallbackFailed("effect"))
                    })
                })
                .unwrap();
            flush(&runtime);
            branch.try_set(&runtime, 1).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| flush(&runtime)));
            if panics {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().outcome, FlushOutcome::Failed);
            }
            let storage = runtime.inner.storage.borrow();
            let node = storage.nodes.get(effect.id.slot).unwrap();
            assert!(node.reads.is_empty());
            assert!(node.tracking.seen_is_empty());
            assert_eq!(
                node.sources
                    .iter()
                    .map(|edge| edge.source)
                    .collect::<Vec<_>>(),
                [branch.id.slot, a.id.slot]
            );
            assert_eq!(node.metadata.state, State::Dirty);
            drop(storage);
            runtime.verify_graph();
            runtime.shutdown().unwrap();
        }
    }
}
