//! Optional metadata snapshots. Never formats values or invokes user code.
use super::*;
pub type DiagnosticCounters = Work;
pub type DiagnosticKind = Kind;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiagnosticId {
    pub index: usize,
    pub generation: u64,
}
impl From<Key> for DiagnosticId {
    fn from(k: Key) -> Self {
        Self {
            index: k.index as usize,
            generation: u64::from(k.generation),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleFrame {
    pub node: DiagnosticId,
    pub phase: EvaluationPhase,
}
#[derive(Default)]
pub(super) struct DiagnosticStorage {
    pub active: Vec<CycleFrame>,
    pub cycle: Vec<CycleFrame>,
}
#[derive(Clone, Debug, Default)]
pub struct StorageMetrics {
    pub node_slots: usize,
    pub owner_slots: usize,
    pub arena_capacity_bytes: usize,
    pub queue_capacity_bytes: usize,
    pub edge_capacity_bytes: usize,
    pub source_slots: usize,
    pub source_tombstones: usize,
    pub read_capacity_bytes: usize,
    pub scratch_capacity_bytes: usize,
    pub node_size: usize,
    pub metadata_size: usize,
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
    pub status: RuntimeStatus,
    pub first_failure: Option<Rc<Error>>,
    pub secondary_failures: Vec<Rc<Error>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueMembership {
    None,
    Pending,
    Current(u64),
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
    pub execution_owner: Option<DiagnosticId>,
    pub kind: DiagnosticKind,
    pub state: State,
    pub phase: EvaluationPhase,
    pub initialized: bool,
    pub value_revision: u64,
    pub freshness_revision: u64,
    pub queue: QueueMembership,
    pub sources: Vec<SourceSnapshot>,
    pub observers: usize,
    pub failure: Option<Rc<Error>>,
}
#[derive(Clone, Debug)]
pub struct GraphSnapshot {
    pub runtime_identity: u64,
    pub nodes: Vec<NodeSnapshot>,
}
impl Runtime {
    pub(super) fn check_observation(&self) -> Result<()> {
        if self.0.depth.get() != 0 || self.0.disposal.get() != 0 {
            return Err(Error::Reentrancy);
        }
        Ok(())
    }
    pub fn reset_diagnostic_counters(&self) -> Result<()> {
        self.check_observation()?;
        self.reset_work();
        Ok(())
    }
    pub fn diagnostics(&self) -> Result<RuntimeDiagnostics> {
        self.check_observation()?;
        self.graph(|g| {
            let mut metrics = StorageMetrics {
                node_slots: g.hot.len(),
                owner_slots: g.owners.len(),
                arena_capacity_bytes: g.hot.capacity() * std::mem::size_of::<Hot>()
                    + g.cold.capacity() * std::mem::size_of::<Option<Cold>>()
                    + ownership::capacity_bytes(g),
                queue_capacity_bytes: (g.pending.capacity() + g.current.capacity())
                    * std::mem::size_of::<Key>(),
                scratch_capacity_bytes: g.marks.capacity() * std::mem::size_of::<(Key, State)>()
                    + self.0.frames.borrow().capacity() * std::mem::size_of::<evaluation::Frame>()
                    + self.0.fused_frames.borrow().capacity() * std::mem::size_of::<fused::Frame>(),
                node_size: std::mem::size_of::<Hot>() + std::mem::size_of::<Cold>(),
                metadata_size: std::mem::size_of::<Hot>(),
                ..Default::default()
            };
            #[cfg(feature = "topology-diagnostics")]
            {
                metrics.scratch_capacity_bytes += self.0.topology.borrow().capacity_bytes();
            }
            let mut nodes = 0;
            let mut edges = 0;
            for c in g.cold.iter().flatten() {
                nodes += 1;
                edges += c.sources.iter().filter(|s| s.live).count();
                metrics.source_slots += c.sources.len();
                metrics.source_tombstones += c.sources.iter().filter(|s| !s.live).count();
                metrics.edge_capacity_bytes += c.sources.capacity() * std::mem::size_of::<Source>()
                    + c.observers.capacity() * std::mem::size_of::<Observer>();
                metrics.read_capacity_bytes += c.reads.capacity() * std::mem::size_of::<Source>();
            }
            Ok(RuntimeDiagnostics {
                runtime_identity: self.0.id,
                counters: g.work,
                owners: ownership::owner_count(g),
                nodes,
                edges,
                pending_tickets: g.pending_live,
                current_tickets: g.current_live,
                pending_buffer_len: g.pending.len(),
                current_buffer_len: g.current.len(),
                cycle: self.0.diagnostic.borrow().cycle.clone(),
                storage: metrics,
                status: self.status(),
                first_failure: self.first_failure(),
                secondary_failures: self.secondary_failures(),
            })
        })
    }
    pub fn graph_snapshot(&self) -> Result<GraphSnapshot> {
        self.check_observation()?;
        self.graph(|g| {
            // Requesting owner identities is an observation of those scopes.
            // Materialize without evaluating callbacks or inspecting values.
            for i in 0..g.cold.len() {
                if let Some(c) = g.cold[i].as_ref()
                    && c.scope_deferred
                {
                    let pending = ownership::PendingOwner {
                        node: Key {
                            index: i as u32,
                            generation: g.hot[i].generation,
                        },
                        epoch: c.scope_epoch,
                    };
                    ownership::materialize(g, pending)?;
                }
            }
            Ok(GraphSnapshot {
                runtime_identity: self.0.id,
                nodes: g
                    .hot
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.alive)
                    .map(|(index, h)| {
                        let c = g.cold[index].as_ref().unwrap();
                        NodeSnapshot {
                            id: Key {
                                index: index as u32,
                                generation: h.generation,
                            }
                            .into(),
                            owner: c.owner.into(),
                            execution_owner: c.execution_owner.map(Into::into),
                            kind: h.kind,
                            state: h.state,
                            phase: h.phase,
                            initialized: h.initialized,
                            value_revision: h.revision,
                            freshness_revision: h.freshness,
                            queue: match h.queue {
                                1 => QueueMembership::Pending,
                                2 => QueueMembership::Current(g.round),
                                _ => QueueMembership::None,
                            },
                            sources: c
                                .sources
                                .iter()
                                .filter(|s| s.live)
                                .map(|s| SourceSnapshot {
                                    source: s.key.into(),
                                    value_revision: s.revision,
                                    observer_index: s.observer_index,
                                })
                                .collect(),
                            observers: c.observers.len(),
                            failure: c.failure.clone(),
                        }
                    })
                    .collect(),
            })
        })
    }
    pub(super) fn capture_cycle(&self, key: Key) {
        let mut d = self.0.diagnostic.borrow_mut();
        if !d.cycle.is_empty() {
            return;
        }
        let id = DiagnosticId::from(key);
        let start = d.active.iter().position(|f| f.node == id).unwrap_or(0);
        d.cycle = d.active[start..].to_vec();
        let phase = d
            .cycle
            .first()
            .map_or(EvaluationPhase::Checking, |f| f.phase);
        d.cycle.push(CycleFrame { node: id, phase });
    }
}
