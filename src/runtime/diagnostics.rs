//! Optional metadata only. No value formatting, user callbacks, or clocks.
use super::{Runtime, Storage};
use crate::{EvaluationPhase, QueueMembership, RuntimeError, State, arena::SlotId, node::NodeKind};

/// Scoped to the runtime identity in RuntimeDiagnostics/GraphSnapshot.
/// Not a handle and cannot be used to access reactive state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiagnosticId {
    pub index: usize,
    pub generation: u64,
}
impl From<SlotId> for DiagnosticId {
    fn from(id: SlotId) -> Self {
        Self {
            index: id.index as usize,
            generation: u64::from(id.generation.get()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn counts_work_and_distinguishes_equality_cut_from_effect_execution() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let signal = rt.with_owner(root, |rt| rt.signal(0)).unwrap();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| Ok(signal.try_get_copy(rt)? / 2))
            })
            .unwrap();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                memo.try_get_copy(rt)?;
                Ok(())
            })
        })
        .unwrap();
        rt.flush(Default::default()).unwrap();
        let initial = rt.diagnostics().unwrap();
        assert_eq!((initial.nodes, initial.edges), (3, 2));
        assert_eq!(
            (
                initial.counters.memo_executions,
                initial.counters.effect_executions,
                initial.counters.rounds
            ),
            (1, 1, 1)
        );
        assert_eq!(rt.graph_snapshot().unwrap().nodes.len(), 3);
        rt.reset_diagnostic_counters().unwrap();
        signal.try_set(&rt, 1).unwrap();
        assert_eq!(rt.diagnostics().unwrap().pending_tickets, 1);
        rt.flush(Default::default()).unwrap();
        let cut = rt.diagnostics().unwrap();
        assert_eq!(
            (
                cut.counters.memo_executions,
                cut.counters.effect_executions,
                cut.counters.tickets_processed
            ),
            (1, 0, 1)
        );
        assert_eq!(cut.counters.source_checks, 1);
        assert_eq!(cut.counters.reconciliations, 1);
        rt.reset_diagnostic_counters().unwrap();
        signal.try_set(&rt, 1).unwrap();
        assert_eq!(
            rt.diagnostics().unwrap().counters,
            DiagnosticCounters::default()
        );
        rt.shutdown().unwrap();
        let closed = rt.diagnostics().unwrap();
        assert_eq!((closed.nodes, closed.owners, closed.edges), (0, 0, 0));
        assert!(rt.graph_snapshot().unwrap().nodes.is_empty());
    }

    #[test]
    fn cycle_path_includes_checking_and_executing_and_survives_poison_teardown() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let slot = Rc::new(Cell::new(None::<crate::Memo<u32>>));
        let captured = slot.clone();
        let first = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| captured.get().unwrap().try_get_copy(rt))
            })
            .unwrap();
        let second = rt
            .with_owner(root, |rt| rt.memo(move |rt| first.try_get_copy(rt)))
            .unwrap();
        slot.set(Some(second));
        assert_eq!(first.try_get_copy(&rt), Err(RuntimeError::Cycle));
        let path = rt.diagnostics().unwrap().cycle;
        assert_eq!(path.len(), 3);
        assert_eq!(path[0].node, first.id.slot.into());
        assert_eq!(path[1].node, second.id.slot.into());
        assert_eq!(path[2].node, first.id.slot.into());
        assert!(
            path.iter()
                .all(|frame| frame.phase == EvaluationPhase::Executing)
        );
        assert!(rt.inner.storage.borrow().diagnostics.active.is_empty());
        rt.shutdown().unwrap();
        assert_eq!(rt.diagnostics().unwrap().cycle, path);
    }

    #[test]
    fn observation_refuses_borrow_and_active_callback_and_never_evaluates_memo() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(|rt| {
                    assert!(matches!(rt.diagnostics(), Err(RuntimeError::Reentrancy)));
                    assert!(matches!(rt.graph_snapshot(), Err(RuntimeError::Reentrancy)));
                    assert_eq!(
                        rt.reset_diagnostic_counters(),
                        Err(RuntimeError::Reentrancy)
                    );
                    Ok(1)
                })
            })
            .unwrap();
        assert_eq!(rt.diagnostics().unwrap().counters.memo_executions, 0);
        assert!(!rt.graph_snapshot().unwrap().nodes[0].initialized);
        let borrow = rt.inner.storage.borrow_mut();
        assert!(matches!(rt.diagnostics(), Err(RuntimeError::Reentrancy)));
        drop(borrow);
        memo.try_get_copy(&rt).unwrap();
    }

    #[test]
    fn counters_saturate_and_active_path_restores_after_panic() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        rt.inner
            .storage
            .borrow_mut()
            .diagnostics
            .counters
            .memo_executions = u64::MAX;
        let memo = rt
            .with_owner(root, |rt| rt.memo::<u32>(|_| panic!("compute")))
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| memo.try_get_copy(&rt)))
                .is_err()
        );
        assert_eq!(rt.diagnostics().unwrap().counters.memo_executions, u64::MAX);
        assert!(rt.inner.storage.borrow().diagnostics.active.is_empty());
        rt.shutdown().unwrap();
    }

    #[test]
    fn checking_cycle_records_active_phases_and_guards_restore_stack() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let a = rt.with_owner(root, |rt| rt.memo(|_| Ok(1))).unwrap();
        let b = rt.with_owner(root, |rt| rt.memo(|_| Ok(2))).unwrap();
        let first = rt
            .enter_evaluation(a.id, EvaluationPhase::Checking)
            .unwrap();
        let second = rt
            .enter_evaluation(b.id, EvaluationPhase::Checking)
            .unwrap();
        assert!(matches!(
            rt.enter_evaluation(a.id, EvaluationPhase::Checking),
            Err(RuntimeError::Cycle)
        ));
        drop(second);
        drop(first);
        let path = rt.diagnostics().unwrap().cycle;
        assert_eq!(path.len(), 3);
        assert!(
            path.iter()
                .all(|frame| frame.phase == EvaluationPhase::Checking)
        );
        assert!(rt.inner.storage.borrow().diagnostics.active.is_empty());
    }

    #[test]
    fn isolated_work_does_not_visit_unrelated_computations() {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0)).unwrap();
        let unrelated = rt.with_owner(root, |rt| rt.signal(0)).unwrap();
        for _ in 0..100 {
            let memo = rt
                .with_owner(root, |rt| rt.memo(move |rt| unrelated.try_get_copy(rt)))
                .unwrap();
            memo.try_get_copy(&rt).unwrap();
        }
        rt.reset_diagnostic_counters().unwrap();
        input.try_set(&rt, 1).unwrap();
        assert_eq!(
            rt.diagnostics().unwrap().counters,
            DiagnosticCounters::default()
        );
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    Signal,
    Memo,
    Effect,
}
impl From<NodeKind> for DiagnosticKind {
    fn from(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Signal => Self::Signal,
            NodeKind::Memo => Self::Memo,
            NodeKind::Effect => Self::Effect,
        }
    }
}
/// Cumulative attempted work (including failed executions); saturates at u64::MAX.
/// These counters never affect correctness or poison the runtime on overflow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticCounters {
    pub mark_visits: u64,
    pub state_changes: u64,
    pub source_checks: u64,
    pub tracked_reads: u64,
    pub reconciliations: u64,
    pub edges_added: u64,
    pub edges_removed: u64,
    pub memo_executions: u64,
    pub effect_executions: u64,
    pub rounds: u64,
    pub tickets_enqueued: u64,
    pub tickets_processed: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleFrame {
    pub node: DiagnosticId,
    pub phase: EvaluationPhase,
}
#[derive(Default)]
pub(super) struct DiagnosticStorage {
    pub counters: DiagnosticCounters,
    pub active: Vec<(SlotId, EvaluationPhase)>,
    pub cycle: Vec<CycleFrame>,
}
pub(super) fn capture_cycle(storage: &mut Storage, target: SlotId) {
    if !storage.diagnostics.cycle.is_empty() {
        return;
    }
    let start = storage
        .diagnostics
        .active
        .iter()
        .position(|frame| frame.0 == target)
        .unwrap_or(0);
    storage.diagnostics.cycle = storage.diagnostics.active[start..]
        .iter()
        .map(|&(node, phase)| CycleFrame {
            node: node.into(),
            phase,
        })
        .collect();
    let phase = storage
        .nodes
        .get(target)
        .map_or(EvaluationPhase::Idle, |node| node.metadata.phase);
    storage.diagnostics.cycle.push(CycleFrame {
        node: target.into(),
        phase,
    });
}
#[derive(Clone, Debug)]
pub struct RuntimeDiagnostics {
    pub runtime_identity: u64,
    pub counters: DiagnosticCounters,
    pub owners: usize,
    pub nodes: usize,
    pub edges: usize,
    pub pending_tickets: usize,
    pub current_tickets: usize,
    pub pending_buffer_len: usize,
    pub current_buffer_len: usize,
    pub cycle: Vec<CycleFrame>,
    pub storage: StorageMetrics,
}

/// Structural capacities only, not allocator RSS or value/closure sizes. The
/// dedup table capacity is in entries (its private hash layout is not assumed).
#[derive(Clone, Debug, Default)]
pub struct StorageMetrics {
    pub node_slots: usize,
    pub owner_slots: usize,
    pub arena_capacity_bytes: usize,
    pub queue_capacity_bytes: usize,
    pub edge_capacity_bytes: usize,
    /// Physical source slots, including ID-only tombstones awaiting reconciliation.
    pub source_slots: usize,
    pub source_tombstones: usize,
    pub read_capacity_bytes: usize,
    /// Heap bytes only; inline storage is already included in arena/Node size.
    pub read_heap_capacity_bytes: usize,
    pub read_inline_capacity_bytes: usize,
    pub dedup_capacity_entries: usize,
    pub scratch_capacity_bytes: usize,
    pub node_size: usize,
    pub metadata_size: usize,
}
#[derive(Clone, Debug)]
pub struct SourceSnapshot {
    pub source: DiagnosticId,
    pub value_revision: u64,
    pub observer_index: usize,
}
#[derive(Clone, Debug)]
pub struct NodeSnapshot {
    pub id: DiagnosticId,
    pub owner: DiagnosticId,
    pub kind: DiagnosticKind,
    pub state: State,
    pub phase: EvaluationPhase,
    pub initialized: bool,
    pub value_revision: u64,
    pub freshness_revision: u64,
    pub queue: QueueMembership,
    pub sources: Vec<SourceSnapshot>,
    pub observers: usize,
}
#[derive(Clone, Debug)]
pub struct GraphSnapshot {
    pub runtime_identity: u64,
    pub nodes: Vec<NodeSnapshot>,
}

impl Runtime {
    /// Explicit O(S+A) observation (S = allocated arena slots); available after
    /// poison/shutdown. Never runs
    /// callbacks or evaluates cached memos. Refuses transitional execution scopes.
    pub fn diagnostics(&self) -> Result<RuntimeDiagnostics, RuntimeError> {
        let context = self.inner.context.snapshot();
        if context.evaluation_depth != 0 || context.disposal_depth != 0 {
            return Err(RuntimeError::Reentrancy);
        }
        let storage = self
            .inner
            .storage
            .try_borrow()
            .map_err(|_| RuntimeError::Reentrancy)?;
        let (pending_tickets, current_tickets, pending_buffer_len, current_buffer_len) =
            storage.effects.diagnostic_sizes();
        let (node_slots, node_bytes) = storage.nodes.allocation_info();
        let (owner_slots, owner_bytes) = storage.owners.allocation_info();
        let mut memory = StorageMetrics {
            node_slots,
            owner_slots,
            arena_capacity_bytes: node_bytes + owner_bytes,
            queue_capacity_bytes: storage.effects.capacity_bytes(),
            scratch_capacity_bytes: storage.mark_stack.capacity()
                * std::mem::size_of::<(SlotId, State)>()
                + storage.tracking_stack.capacity() * std::mem::size_of::<SlotId>(),
            node_size: std::mem::size_of::<crate::node::Node>(),
            metadata_size: std::mem::size_of::<crate::node::Metadata>(),
            ..Default::default()
        };
        for (_, node) in storage.nodes.iter() {
            memory.source_slots += node.sources.len();
            memory.source_tombstones += node.sources.iter().filter(|edge| !edge.live).count();
            memory.edge_capacity_bytes += node.sources.capacity()
                * std::mem::size_of::<crate::node::SourceEdge>()
                + node.observers.capacity() * std::mem::size_of::<crate::node::ObserverEdge>();
            let read_bytes = node.reads.capacity() * std::mem::size_of::<crate::node::ReadRecord>();
            memory.read_capacity_bytes += read_bytes;
            #[cfg(feature = "experimental-inline-reads")]
            {
                memory.read_inline_capacity_bytes +=
                    2 * std::mem::size_of::<crate::node::ReadRecord>();
                if node.reads.spilled() {
                    memory.read_heap_capacity_bytes += read_bytes;
                }
            }
            #[cfg(not(feature = "experimental-inline-reads"))]
            {
                memory.read_heap_capacity_bytes += read_bytes;
            }
            memory.dedup_capacity_entries += node.tracking.seen_capacity();
        }
        Ok(RuntimeDiagnostics {
            runtime_identity: self.inner.id.0,
            counters: storage.diagnostics.counters.clone(),
            owners: storage.owners.len(),
            nodes: storage.nodes.len(),
            edges: storage
                .nodes
                .iter()
                .map(|(_, node)| node.sources.iter().filter(|edge| edge.live).count())
                .sum(),
            pending_tickets,
            current_tickets,
            pending_buffer_len,
            current_buffer_len,
            cycle: storage.diagnostics.cycle.clone(),
            storage: memory,
        })
    }
    /// Reset counters only; preserves the first cycle and all runtime state.
    pub fn reset_diagnostic_counters(&self) -> Result<(), RuntimeError> {
        let context = self.inner.context.snapshot();
        if context.evaluation_depth != 0 || context.disposal_depth != 0 {
            return Err(RuntimeError::Reentrancy);
        }
        self.inner
            .storage
            .try_borrow_mut()
            .map_err(|_| RuntimeError::Reentrancy)?
            .diagnostics
            .counters = DiagnosticCounters::default();
        Ok(())
    }
    /// Independent from FlushReport. Allocation/graph traversal happen only here.
    pub fn graph_snapshot(&self) -> Result<GraphSnapshot, RuntimeError> {
        let context = self.inner.context.snapshot();
        if context.evaluation_depth != 0 || context.disposal_depth != 0 {
            return Err(RuntimeError::Reentrancy);
        }
        let storage = self
            .inner
            .storage
            .try_borrow()
            .map_err(|_| RuntimeError::Reentrancy)?;
        Ok(GraphSnapshot {
            runtime_identity: self.inner.id.0,
            nodes: storage
                .nodes
                .iter()
                .map(|(id, node)| NodeSnapshot {
                    id: id.into(),
                    owner: node.owner.into(),
                    kind: node.payload.kind().into(),
                    state: node.metadata.state,
                    phase: node.metadata.phase,
                    initialized: node.metadata.initialized,
                    value_revision: node.metadata.value_revision,
                    freshness_revision: node.metadata.freshness_revision,
                    queue: node.metadata.queue,
                    observers: node.observers.len(),
                    sources: node
                        .sources
                        .iter()
                        .filter(|edge| edge.live)
                        .map(|edge| SourceSnapshot {
                            source: edge.source.into(),
                            value_revision: edge.value_revision,
                            observer_index: edge.observer_index,
                        })
                        .collect(),
                })
                .collect(),
        })
    }
}
