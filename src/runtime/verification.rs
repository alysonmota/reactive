//! Explicit structural audits. No auto scan on reads/writes or callback execution.
use super::{Runtime, RuntimeStatus};
use crate::{
    EvaluationPhase, QueueMembership, RuntimeError, State,
    node::{NodeKind, Payload},
};
use std::collections::HashSet;

impl Runtime {
    /// O(S+A+T+Q) expected time (S = allocated slots, T = source tombstones)
    /// and O(V+A+Q) temporary
    /// memory. Checks structure,
    /// ownership and scheduling, not derived values. Never evaluates callbacks.
    /// Available after poison/shutdown; returns errors without changing poison.
    /// Call only outside evaluation, disposal and flush (transitional states).
    #[cfg_attr(not(feature = "verification"), allow(dead_code))]
    pub fn verify_invariants(&self) -> Result<(), RuntimeError> {
        let context = self.inner.context.snapshot();
        if context.evaluation_depth != 0 || context.disposal_depth != 0 || context.flushing {
            return Err(RuntimeError::Reentrancy);
        }
        let storage = self
            .inner
            .storage
            .try_borrow()
            .map_err(|_| RuntimeError::Reentrancy)?;
        macro_rules! require {
            ($condition:expr, $message:literal) => {
                if !$condition {
                    return Err(RuntimeError::InvariantViolation($message));
                }
            };
        }
        require!(
            (self.status() == RuntimeStatus::Poisoned) == self.first_failure().is_some(),
            "runtime status/error disagreement"
        );
        require!(
            storage.tracking_stack.is_empty() && storage.retired_tracking.is_empty(),
            "tracking outside evaluation"
        );
        let mut reachable = HashSet::new();
        let mut stack = Vec::new();
        for (index, &id) in storage.roots.iter().enumerate() {
            let owner = storage
                .owners
                .get(id)
                .ok_or(RuntimeError::InvariantViolation("dead root"))?;
            require!(
                owner.parent.is_none() && owner.parent_index == index && owner.depth == 0,
                "root ownership"
            );
            stack.push(id);
        }
        let mut owned_nodes = HashSet::new();
        while let Some(id) = stack.pop() {
            require!(reachable.insert(id), "duplicate or cyclic owner");
            let owner = storage
                .owners
                .get(id)
                .ok_or(RuntimeError::InvariantViolation("dead owner"))?;
            for (index, &child) in owner.children.iter().enumerate() {
                let value = storage
                    .owners
                    .get(child)
                    .ok_or(RuntimeError::InvariantViolation("dead child owner"))?;
                require!(
                    value.parent == Some(id)
                        && value.parent_index == index
                        && owner.depth.checked_add(1) == Some(value.depth),
                    "owner back-index or depth"
                );
                stack.push(child);
            }
            for (index, &node) in owner.nodes.iter().enumerate() {
                let value = storage
                    .nodes
                    .get(node)
                    .ok_or(RuntimeError::InvariantViolation("dead owned node"))?;
                require!(
                    owned_nodes.insert(node) && value.owner == id && value.owner_index == index,
                    "node ownership back-index"
                );
            }
        }
        require!(
            reachable.len() == storage.owners.len() && owned_nodes.len() == storage.nodes.len(),
            "unreachable arena entry"
        );
        let healthy = self.status() == RuntimeStatus::Healthy;
        for (id, node) in storage.nodes.iter() {
            require!(
                node.metadata.phase == EvaluationPhase::Idle,
                "transitional node outside evaluation"
            );
            require!(
                node.source_holes == node.sources.iter().any(|edge| !edge.live),
                "source tombstone flag"
            );
            require!(
                !node.source_holes || !healthy || node.metadata.state == State::Dirty,
                "healthy reader with source tombstones must be dirty"
            );
            if let Some(scope) = node.execution_owner {
                let owner = storage
                    .owners
                    .get(scope)
                    .ok_or(RuntimeError::InvariantViolation("dead execution scope"))?;
                require!(owner.parent == Some(node.owner), "execution scope parent");
            }
            if node.payload.kind() == NodeKind::Signal {
                require!(
                    node.sources.is_empty()
                        && node.metadata.state == State::Clean
                        && node.metadata.initialized,
                    "signal validity"
                );
            }
            if node.payload.kind() != NodeKind::Effect {
                require!(
                    node.metadata.queue == QueueMembership::None,
                    "non-effect ticket"
                );
            }
            if let Payload::Signal(data) = &node.payload {
                require!(
                    (data.value_type.initialized)(&data.value)?,
                    "empty signal cell"
                );
            }
            if let Payload::Memo {
                value_type, value, ..
            } = &node.payload
            {
                require!(
                    node.metadata.initialized == value.is_some(),
                    "memo initialization"
                );
                if let Some(value) = value {
                    require!(
                        value.as_ref().type_id() == value_type.storage_id,
                        "memo cache type"
                    );
                    require!((value_type.initialized)(value)?, "empty memo cache cell");
                }
            }
            let mut seen = HashSet::new();
            for (index, edge) in node.sources.iter().enumerate() {
                if !edge.live {
                    // A tombstone retains only IDs/revisions, not resources or
                    // a bidirectional edge. Its old generation must be dead.
                    require!(
                        storage.nodes.get(edge.source).is_none(),
                        "live source tombstone"
                    );
                    continue;
                }
                require!(seen.insert(edge.source), "dead or duplicate source");
                let source = storage
                    .nodes
                    .get(edge.source)
                    .ok_or(RuntimeError::InvariantViolation("dead source"))?;
                let observer = source
                    .observers
                    .get(edge.observer_index)
                    .ok_or(RuntimeError::InvariantViolation("observer index"))?;
                require!(
                    observer.reader == id && observer.source_index == index,
                    "source back-index"
                );
                // Poison preserves structural evidence, not freshness guarantees.
                if healthy && node.metadata.state == State::Clean && node.metadata.initialized {
                    require!(
                        source.metadata.state == State::Clean
                            && source.metadata.value_revision == edge.value_revision,
                        "clean reader has stale source"
                    );
                }
            }
            for (index, edge) in node.observers.iter().enumerate() {
                let reader = storage
                    .nodes
                    .get(edge.reader)
                    .ok_or(RuntimeError::InvariantViolation("dead reader"))?;
                let source = reader
                    .sources
                    .get(edge.source_index)
                    .ok_or(RuntimeError::InvariantViolation("source index"))?;
                require!(
                    source.source == id && source.observer_index == index && source.live,
                    "observer back-index"
                );
            }
            if healthy
                && node.metadata.initialized
                && node.metadata.state == State::Clean
                && node.payload.kind() != NodeKind::Signal
            {
                require!(
                    node.sources.len() == node.reads.len()
                        && node
                            .sources
                            .iter()
                            .zip(&node.reads)
                            .all(|(source, read)| source.source == read.source),
                    "sources not in first-read order"
                );
            }
        }
        storage.effects.verify(&storage)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_tombstone_flags_clean_readers_and_live_source_ids() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let child = rt.create_owner(root).unwrap();
        let input = rt.with_owner(child, |rt| rt.signal(1_u64)).unwrap();
        let memo = rt
            .with_owner(root, |rt| rt.memo(move |rt| input.try_get_copy(rt)))
            .unwrap();
        memo.try_get_copy(&rt).unwrap();
        rt.dispose(child).unwrap();
        rt.verify_invariants().unwrap();
        rt.inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(memo.id.slot)
            .unwrap()
            .source_holes = false;
        assert_eq!(
            rt.verify_invariants(),
            Err(RuntimeError::InvariantViolation("source tombstone flag"))
        );
        {
            let mut storage = rt.inner.storage.borrow_mut();
            let node = storage.nodes.get_mut(memo.id.slot).unwrap();
            node.source_holes = true;
            node.metadata.state = State::Clean;
        }
        assert_eq!(
            rt.verify_invariants(),
            Err(RuntimeError::InvariantViolation(
                "healthy reader with source tombstones must be dirty"
            ))
        );
        rt.inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(memo.id.slot)
            .unwrap()
            .metadata
            .state = State::Dirty;
        let replacement = rt.with_owner(root, |rt| rt.signal(2_u64)).unwrap();
        rt.inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(memo.id.slot)
            .unwrap()
            .sources[0]
            .source = replacement.id.slot;
        assert_eq!(
            rt.verify_invariants(),
            Err(RuntimeError::InvariantViolation("live source tombstone"))
        );
        rt.inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(memo.id.slot)
            .unwrap()
            .sources[0]
            .source = input.id.slot;
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn verifies_dynamic_edges_queue_disposal_and_poison_teardown() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let (branch, a, b) = rt
            .with_owner(root, |rt| {
                Ok((rt.signal(true)?, rt.signal(1)?, rt.signal(2)?))
            })
            .unwrap();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    if branch.try_get_copy(rt)? {
                        Ok(a.try_get_copy(rt)?)
                    } else {
                        Ok(b.try_get_copy(rt)?)
                    }
                })
            })
            .unwrap();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                memo.try_get_copy(rt)?;
                Ok(())
            })
        })
        .unwrap();
        for i in 0..100 {
            rt.verify_invariants().unwrap();
            branch.try_set(&rt, i % 2 == 0).unwrap();
            a.try_set(&rt, i).unwrap();
            b.try_set(&rt, i + 1).unwrap();
            rt.verify_invariants().unwrap();
            rt.flush(Default::default()).unwrap();
            rt.verify_invariants().unwrap();
        }
        let effect = rt
            .with_owner(root, |rt| {
                rt.effect(|_| Err(RuntimeError::CallbackFailed("test")))
            })
            .unwrap();
        rt.flush(Default::default()).unwrap();
        rt.verify_invariants().unwrap();
        rt.dispose(effect).unwrap();
        rt.verify_invariants().unwrap();
        rt.shutdown().unwrap();
        rt.verify_invariants().unwrap();
    }
    #[test]
    fn catches_bad_indices_duplicates_stale_generations_and_owner_orphans() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let signal = rt.with_owner(root, |rt| rt.signal(1)).unwrap();
        let memo = rt
            .with_owner(root, |rt| rt.memo(move |rt| signal.try_get_copy(rt)))
            .unwrap();
        memo.try_get_copy(&rt).unwrap();
        rt.verify_invariants().unwrap();
        {
            let mut storage = rt.inner.storage.borrow_mut();
            storage.nodes.get_mut(memo.id.slot).unwrap().sources[0].observer_index = 999;
        }
        assert!(rt.verify_invariants().is_err());
        {
            let mut storage = rt.inner.storage.borrow_mut();
            let node = storage.nodes.get_mut(memo.id.slot).unwrap();
            node.sources[0].observer_index = 0;
            node.sources.push(node.sources[0]);
        }
        assert!(rt.verify_invariants().is_err());
        {
            let mut storage = rt.inner.storage.borrow_mut();
            let node = storage.nodes.get_mut(memo.id.slot).unwrap();
            node.sources.pop();
            node.sources[0].source.generation =
                node.sources[0].source.generation.checked_add(1).unwrap();
        }
        assert!(rt.verify_invariants().is_err());
        {
            let mut storage = rt.inner.storage.borrow_mut();
            storage.nodes.get_mut(memo.id.slot).unwrap().sources[0].source = signal.id.slot;
            storage.nodes.get_mut(signal.id.slot).unwrap().owner_index = 999;
        }
        assert!(rt.verify_invariants().is_err());
        rt.inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(signal.id.slot)
            .unwrap()
            .owner_index = 0;
        rt.verify_invariants().unwrap();
        assert_eq!(rt.status(), RuntimeStatus::Healthy); // audits never poison
    }
    #[test]
    fn refuses_transitional_states_without_panicking_or_evaluating_callbacks() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(|rt| {
                    assert_eq!(rt.verify_invariants(), Err(RuntimeError::Reentrancy));
                    Ok(1)
                })
            })
            .unwrap();
        memo.try_get_copy(&rt).unwrap();
        let borrow = rt.inner.storage.borrow_mut();
        assert_eq!(rt.verify_invariants(), Err(RuntimeError::Reentrancy));
        drop(borrow);
        rt.verify_invariants().unwrap();
    }
}
