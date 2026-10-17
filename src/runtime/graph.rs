use super::{Runtime, Storage};
use crate::{
    EvaluationPhase, RuntimeError, State,
    arena::SlotId,
    node::{NodeKind, ObserverEdge, ReadRecord, SourceEdge},
};

// Calibrated with stable/reordered/duplicate/nested fan-in sweeps at 16/32/64/128.
// Small sets stay linear; larger sets keep bounded hash lookup costs. See
// benches/.log/RESULTS.md (not a universal hardware-independent crossover).
pub(crate) const HASH_THRESHOLD: usize = 128;

impl Runtime {
    pub(super) fn start_tracking(
        storage: &mut Storage,
        reader: SlotId,
    ) -> Result<(), RuntimeError> {
        let epoch = storage
            .next_execution_epoch
            .checked_add(1)
            .ok_or(RuntimeError::CounterExhausted)?;
        let node = storage
            .nodes
            .get_mut(reader)
            .ok_or(RuntimeError::InvalidHandle)?;
        storage.next_execution_epoch = epoch;
        node.reads.clear();
        node.tracking.execution_epoch = epoch;
        node.tracking.stable_prefix_cursor = 0;
        node.tracking.diverged = false;
        node.tracking.seen_clear();
        node.tracking.hashing = node.sources.len() > HASH_THRESHOLD;
        storage.tracking_stack.push(reader);
        Ok(())
    }

    pub(super) fn record_read(storage: &mut Storage, reader: SlotId, read: ReadRecord) {
        #[cfg(feature = "diagnostics")]
        {
            storage.diagnostics.counters.tracked_reads =
                storage.diagnostics.counters.tracked_reads.saturating_add(1);
        }
        let Some(node) = storage.nodes.get_mut(reader) else {
            return;
        };
        // Committed live sources are unique and ordered (teardown leaves
        // tombstones at fixed positions). Until a NEW source diverges, the
        // cursor advances once per unique read. Matching requires a live edge;
        // reaching a tombstone takes the fallback: no scan/hash on matches.
        // Duplicate reads take the fallback but do not end the stable prefix.
        if !node.tracking.diverged {
            let cursor = node.tracking.stable_prefix_cursor;
            if node
                .sources
                .get(cursor)
                .is_some_and(|edge| edge.live && edge.source == read.source)
            {
                node.tracking.stable_prefix_cursor += 1;
                node.reads.push(read);
                return;
            }
        }
        if node.tracking.hashing {
            // Fast-prefix reads deliberately skip hashing. The set covers the
            // first len() unique records: catch up only the unseeded suffix on
            // fallback, including after duplicates followed by more fast reads.
            let seeded = node.tracking.seen_len();
            node.tracking
                .seen_extend(node.reads[seeded..].iter().map(|read| read.source));
            if !node.tracking.seen_insert(read.source) {
                return;
            }
        } else {
            if node.reads.iter().any(|old| old.source == read.source) {
                return;
            }
            if node.reads.len() >= HASH_THRESHOLD {
                node.tracking
                    .seen_extend(node.reads.iter().map(|read| read.source));
                node.tracking.seen_insert(read.source);
                node.tracking.hashing = true;
            }
        }
        node.tracking.diverged = true;
        node.reads.push(read);
    }

    pub(super) fn add_source(storage: &mut Storage, reader: SlotId, source: SlotId, revision: u64) {
        #[cfg(feature = "diagnostics")]
        {
            storage.diagnostics.counters.edges_added =
                storage.diagnostics.counters.edges_added.saturating_add(1);
        }
        let source_index = storage
            .nodes
            .get(reader)
            .expect("live reader")
            .sources
            .len();
        let source_node = storage.nodes.get_mut(source).expect("live source");
        let observer_index = source_node.observers.len();
        source_node.observers.push(ObserverEdge {
            reader,
            source_index,
        });
        storage
            .nodes
            .get_mut(reader)
            .expect("live reader")
            .sources
            .push(SourceEdge {
                source,
                observer_index,
                value_revision: revision,
                live: true,
            });
    }

    fn remove_last_source(storage: &mut Storage, reader: SlotId) {
        let node = storage
            .nodes
            .get_mut_teardown(reader)
            .expect("reader during edge removal");
        let edge = node.sources.pop().expect("source edge");
        if !edge.live {
            return;
        }
        #[cfg(feature = "diagnostics")]
        {
            storage.diagnostics.counters.edges_removed =
                storage.diagnostics.counters.edges_removed.saturating_add(1);
        }
        let source = storage
            .nodes
            .get_mut_teardown(edge.source)
            .expect("source during edge removal");
        source.observers.swap_remove(edge.observer_index);
        let moved = source.observers.get(edge.observer_index).copied();
        if let Some(moved) = moved {
            storage
                .nodes
                .get_mut_teardown(moved.reader)
                .expect("moved observer reader")
                .sources[moved.source_index]
                .observer_index = edge.observer_index;
        }
    }

    pub(super) fn reconcile_sources(
        storage: &mut Storage,
        reader: SlotId,
    ) -> Result<(), RuntimeError> {
        let node = storage
            .nodes
            .get(reader)
            .ok_or(RuntimeError::InvalidHandle)?;
        // This preflight reads only IDs/metadata; no edge is changed until every
        // first-read revision is confirmed. Commit then reuses the stable prefix.
        for read in &node.reads {
            let source = storage
                .nodes
                .get(read.source)
                .ok_or(RuntimeError::InvalidHandle)?;
            if source.metadata.state != State::Clean
                || source.metadata.freshness_revision != read.freshness_revision
                || source.metadata.value_revision != read.value_revision
            {
                return Err(RuntimeError::InvariantViolation(
                    "memo source changed after first read",
                ));
            }
        }
        Self::commit_sources(storage, reader);
        Ok(())
    }

    pub(super) fn commit_sources(storage: &mut Storage, reader: SlotId) {
        #[cfg(feature = "diagnostics")]
        {
            storage.diagnostics.counters.reconciliations = storage
                .diagnostics
                .counters
                .reconciliations
                .saturating_add(1);
        }
        let node = storage.nodes.get(reader).expect("validated reader");
        let prefix = node.tracking.stable_prefix_cursor;
        let count = node.reads.len();
        let had_holes = node.source_holes;
        while storage.nodes.get(reader).expect("reader").sources.len() > prefix {
            Self::remove_last_source(storage, reader);
        }
        for index in 0..count {
            let read = storage.nodes.get(reader).expect("reader").reads[index];
            if index < prefix {
                storage.nodes.get_mut(reader).expect("reader").sources[index].value_revision =
                    read.value_revision;
            } else {
                Self::add_source(storage, reader, read.source, read.value_revision);
            }
        }
        // The preserved prefix contains only live edges; every old suffix
        // (including tombstones) was removed and new edges are live.
        if had_holes {
            storage.nodes.get_mut(reader).expect("reader").source_holes = false;
        }
    }

    pub(super) fn mark_observers(
        storage: &mut Storage,
        source: SlotId,
    ) -> Result<(), RuntimeError> {
        // First-level inline fast path: the overwhelmingly common topology is
        // a handful of observers reached directly from the written signal.
        // Mark them without pushing/popping the shared mark stack; only when a
        // first-level node transitions from Clean do we enqueue its Check
        // successors for the transitive drain loop. Semantics are identical to
        // the original push-all-then-drain: `changed` for requested=Dirty is
        // exactly `state != Dirty`, and the stack was empty on entry.
        let Some(observers) = storage.nodes.get(source).map(|node| &node.observers) else {
            return Ok(());
        };
        let mut propagated = false;
        for index in 0..observers.len() {
            let reader = observers[index].reader;
            #[cfg(feature = "diagnostics")]
            {
                storage.diagnostics.counters.mark_visits =
                    storage.diagnostics.counters.mark_visits.saturating_add(1);
            }
            let Some(node) = storage.nodes.get_mut(reader) else {
                continue;
            };
            if node.payload.kind() == NodeKind::Signal {
                return Err(RuntimeError::InvariantViolation(
                    "signal cannot have sources",
                ));
            }
            let executing = node.metadata.phase == EvaluationPhase::Executing;
            let changed = node.metadata.state != State::Dirty;
            if executing {
                node.metadata.invalidation_revision = node
                    .metadata
                    .invalidation_revision
                    .checked_add(1)
                    .ok_or(RuntimeError::CounterExhausted)?;
            }
            if node.payload.kind() == NodeKind::Memo && (changed || executing) {
                node.metadata.freshness_revision = node
                    .metadata
                    .freshness_revision
                    .checked_add(1)
                    .ok_or(RuntimeError::CounterExhausted)?;
            }
            if node.payload.kind() == NodeKind::Effect {
                Self::enqueue_effect(storage, reader)?;
            }
            let node = storage.nodes.get_mut(reader).expect("live marked node");
            if !changed {
                continue;
            }
            let was_clean = node.metadata.state == State::Clean;
            node.metadata.state = State::Dirty;
            #[cfg(feature = "diagnostics")]
            {
                storage.diagnostics.counters.state_changes =
                    storage.diagnostics.counters.state_changes.saturating_add(1);
            }
            if was_clean {
                storage.mark_stack.extend(
                    node.observers
                        .iter()
                        .map(|edge| (edge.reader, State::Check)),
                );
                propagated = true;
            }
        }
        if propagated {
            let result = Self::drain_marks(storage);
            storage.mark_stack.clear();
            return result;
        }
        Ok(())
    }

    fn mark_node(storage: &mut Storage, id: SlotId, state: State) -> Result<(), RuntimeError> {
        storage.mark_stack.clear();
        storage.mark_stack.push((id, state));
        let result = Self::drain_marks(storage);
        storage.mark_stack.clear();
        result
    }

    fn drain_marks(storage: &mut Storage) -> Result<(), RuntimeError> {
        while let Some((id, requested)) = storage.mark_stack.pop() {
            #[cfg(feature = "diagnostics")]
            {
                storage.diagnostics.counters.mark_visits =
                    storage.diagnostics.counters.mark_visits.saturating_add(1);
            }
            let Some(node) = storage.nodes.get_mut(id) else {
                continue;
            };
            let kind = node.payload.kind();
            if kind == NodeKind::Signal {
                return Err(RuntimeError::InvariantViolation(
                    "signal cannot have sources",
                ));
            }
            let executing = node.metadata.phase == EvaluationPhase::Executing;
            let changed = node.metadata.state < requested;
            if executing {
                node.metadata.invalidation_revision = node
                    .metadata
                    .invalidation_revision
                    .checked_add(1)
                    .ok_or(RuntimeError::CounterExhausted)?;
            }
            if kind == NodeKind::Memo && (changed || executing) {
                node.metadata.freshness_revision = node
                    .metadata
                    .freshness_revision
                    .checked_add(1)
                    .ok_or(RuntimeError::CounterExhausted)?;
            }
            if kind == NodeKind::Effect {
                Self::enqueue_effect(storage, id)?;
            }
            let node = storage.nodes.get_mut(id).expect("live marked node");
            if !changed {
                continue;
            }
            let was_clean = node.metadata.state == State::Clean;
            node.metadata.state = requested;
            #[cfg(feature = "diagnostics")]
            {
                storage.diagnostics.counters.state_changes =
                    storage.diagnostics.counters.state_changes.saturating_add(1);
            }
            if was_clean {
                storage.mark_stack.extend(
                    node.observers
                        .iter()
                        .map(|edge| (edge.reader, State::Check)),
                );
            }
        }
        Ok(())
    }

    pub(super) fn detach_node_edges(&self, storage: &mut Storage, id: SlotId) {
        self.detach_edges(storage, id);
    }

    pub(super) fn detach_edges(&self, storage: &mut Storage, id: SlotId) {
        Self::cancel_effect_ticket(storage, id);
        while !storage
            .nodes
            .get_teardown(id)
            .expect("removing node")
            .sources
            .is_empty()
        {
            Self::remove_last_source(storage, id);
        }
        while let Some(edge) = storage
            .nodes
            .get_mut_teardown(id)
            .expect("removing source")
            .observers
            .pop()
        {
            let surviving = storage.nodes.get(edge.reader).is_some();
            #[cfg(feature = "diagnostics")]
            {
                storage.diagnostics.counters.edges_removed =
                    storage.diagnostics.counters.edges_removed.saturating_add(1);
            }
            let reader = storage
                .nodes
                .get_mut_teardown(edge.reader)
                .expect("observer reader");
            reader.sources[edge.source_index].live = false;
            let first_hole = !reader.source_holes;
            if surviving {
                reader.source_holes = true;
                // Keep physical positions/back-indices stable until this reader
                // reconciles or is disposed. Idle readers already invalidated
                // by a previous hole need no repeated propagation. Executing
                // readers still record subsequent invalidation events.
                if (first_hole || reader.metadata.phase == EvaluationPhase::Executing)
                    && let Err(error) = Self::mark_node(storage, edge.reader, State::Dirty)
                {
                    self.record_failure(error);
                }
            }
        }
        Self::maintain_pending(storage);
    }

    #[cfg(test)]
    pub(super) fn verify_graph(&self) {
        self.verify_invariants().expect("runtime invariants");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracking_fixture(width: usize) -> (Runtime, SlotId, Vec<SlotId>) {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let inputs: Vec<_> = (0..width)
            .map(|i| rt.with_owner(root, |rt| rt.signal(i as u64)).unwrap())
            .collect();
        let captured = inputs.clone();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    let mut sum = 0;
                    for input in &captured {
                        sum += input.try_get_copy(rt)?;
                    }
                    Ok(sum)
                })
            })
            .unwrap();
        memo.try_get_copy(&rt).unwrap();
        let id = memo.id.slot;
        Runtime::start_tracking(&mut rt.inner.storage.borrow_mut(), id).unwrap();
        (rt, id, inputs.iter().map(|input| input.id.slot).collect())
    }

    fn record(storage: &mut Storage, reader: SlotId, source: SlotId) -> ReadRecord {
        let node = storage.nodes.get(source).unwrap();
        let read = ReadRecord {
            source,
            freshness_revision: node.metadata.freshness_revision,
            value_revision: node.metadata.value_revision,
        };
        Runtime::record_read(storage, reader, read);
        read
    }

    fn finish_tracking(rt: &Runtime, reader: SlotId) {
        let mut storage = rt.inner.storage.borrow_mut();
        Runtime::reconcile_sources(&mut storage, reader).unwrap();
        assert_eq!(storage.tracking_stack.pop(), Some(reader));
        drop(storage);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn stable_unique_prefix_bypasses_linear_and_hash_dedup() {
        for width in [2, HASH_THRESHOLD, HASH_THRESHOLD + 1, 256] {
            let (rt, reader, sources) = tracking_fixture(width);
            let mut storage = rt.inner.storage.borrow_mut();
            for &source in &sources {
                record(&mut storage, reader, source);
            }
            let node = storage.nodes.get(reader).unwrap();
            assert_eq!(node.reads.len(), width);
            assert_eq!(node.tracking.stable_prefix_cursor, width);
            assert!(!node.tracking.diverged);
            assert!(node.tracking.seen_is_empty());
            drop(storage);
            finish_tracking(&rt, reader);
        }
    }

    #[test]
    fn hash_catches_up_after_duplicates_and_more_fast_prefix_reads() {
        let (rt, reader, sources) = tracking_fixture(256);
        let mut storage = rt.inner.storage.borrow_mut();
        for &source in &sources[..128] {
            record(&mut storage, reader, source);
        }
        let first = storage.nodes.get(reader).unwrap().reads[0];
        Runtime::record_read(
            &mut storage,
            reader,
            ReadRecord {
                value_revision: 99,
                freshness_revision: 100,
                ..first
            },
        );
        let node = storage.nodes.get(reader).unwrap();
        assert_eq!(node.reads[0], first);
        assert_eq!(node.tracking.seen_len(), 128);
        assert!(!node.tracking.diverged);
        for &source in &sources[128..] {
            record(&mut storage, reader, source);
        }
        let node = storage.nodes.get(reader).unwrap();
        assert_eq!(node.tracking.seen_len(), 128); // suffix still unhashed
        assert_eq!(node.reads.len(), 256);
        record(&mut storage, reader, sources[255]);
        let node = storage.nodes.get(reader).unwrap();
        assert_eq!(node.tracking.seen_len(), 256);
        assert_eq!(node.reads.len(), 256);
        assert_eq!(node.tracking.stable_prefix_cursor, 256);
        assert!(!node.tracking.diverged);
        drop(storage);
        finish_tracking(&rt, reader);
    }

    #[test]
    fn late_divergence_seeds_prefix_and_preserves_first_revisions() {
        for width in [4, HASH_THRESHOLD, 256] {
            let (rt, reader, sources) = tracking_fixture(width);
            // Reordering only the tail leaves a long prefix; duplicate reads of
            // that skipped prefix must not be registered again after divergence.
            let mut storage = rt.inner.storage.borrow_mut();
            for &source in &sources[..width - 2] {
                record(&mut storage, reader, source);
            }
            let first = storage.nodes.get(reader).unwrap().reads[0];
            record(&mut storage, reader, sources[width - 1]);
            Runtime::record_read(
                &mut storage,
                reader,
                ReadRecord {
                    value_revision: 99,
                    freshness_revision: 100,
                    ..first
                },
            );
            record(&mut storage, reader, sources[width - 2]);
            let node = storage.nodes.get(reader).unwrap();
            assert_eq!(node.reads[0], first);
            assert_eq!(node.reads.len(), width);
            assert_eq!(node.tracking.stable_prefix_cursor, width - 2);
            assert!(node.tracking.diverged);
            drop(storage);
            finish_tracking(&rt, reader);
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(reader).unwrap();
            assert_eq!(node.sources[width - 2].source, sources[width - 1]);
            assert_eq!(node.sources[width - 1].source, sources[width - 2]);
        }
    }

    #[test]
    fn promotion_after_unhashed_prefix_seeds_every_previous_read() {
        let (rt, reader, sources) = tracking_fixture(HASH_THRESHOLD);
        let root = rt.create_root().unwrap();
        let extra = rt.with_owner(root, |rt| rt.signal(1000_u64)).unwrap();
        let mut storage = rt.inner.storage.borrow_mut();
        for &source in &sources {
            record(&mut storage, reader, source);
        }
        assert!(!storage.nodes.get(reader).unwrap().tracking.hashing);
        record(&mut storage, reader, sources[0]); // duplicate does not promote
        assert!(!storage.nodes.get(reader).unwrap().tracking.hashing);
        record(&mut storage, reader, extra.id.slot);
        record(&mut storage, reader, sources[127]);
        let node = storage.nodes.get(reader).unwrap();
        assert!(node.tracking.hashing);
        assert!(node.tracking.diverged);
        assert_eq!(node.reads.len(), HASH_THRESHOLD + 1);
        assert_eq!(node.tracking.seen_len(), HASH_THRESHOLD + 1);
        drop(storage);
        finish_tracking(&rt, reader);
    }

    #[test]
    fn calibrated_dedup_boundary_preserves_first_read_order_and_stable_prefix() {
        for width in [HASH_THRESHOLD, HASH_THRESHOLD + 1] {
            let rt = Runtime::new();
            let root = rt.create_root().unwrap();
            let inputs: Vec<_> = (0..width)
                .map(|i| rt.with_owner(root, |rt| rt.signal(i as u64)).unwrap())
                .collect();
            let captured = inputs.clone();
            let memo = rt
                .with_owner(root, |rt| {
                    rt.memo(move |rt| {
                        let mut sum = 0;
                        for input in &captured {
                            sum += input.try_get_copy(rt)?;
                            input.try_get_copy(rt)?;
                        }
                        Ok(sum)
                    })
                })
                .unwrap();
            assert_eq!(
                memo.try_get_copy(&rt).unwrap(),
                (width * (width - 1) / 2) as u64
            );
            inputs[0].try_set(&rt, 10).unwrap();
            assert_eq!(
                memo.try_get_copy(&rt).unwrap(),
                (width * (width - 1) / 2) as u64 + 10
            );
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(memo.id.slot).unwrap();
            assert_eq!(node.sources.len(), width);
            assert_eq!(node.tracking.stable_prefix_cursor, width);
            assert_eq!(node.tracking.hashing, width > HASH_THRESHOLD);
            assert!(
                node.sources
                    .iter()
                    .zip(&inputs)
                    .all(|(edge, input)| edge.source == input.id.slot)
            );
            drop(storage);
            rt.verify_invariants().unwrap();
        }
    }
}
