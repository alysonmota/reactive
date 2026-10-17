//! Stages 2–3 ONLY: shared round sessions and a callback-free shadow DAG plan.
//! Neither this order nor its cursor is consulted by an evaluator/scheduler.
use super::*;
use std::collections::VecDeque;

const HISTORY_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologySessionKind {
    Round,
    Read,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceRelation {
    InOrder,
    BeforeCursor,
    Revisited,
    Unplanned,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologyTrace {
    pub node: DiagnosticId,
    pub planned_index: Option<usize>,
    pub relation: TraceRelation,
}
#[derive(Clone, Debug)]
pub struct TopologyPlan {
    pub id: u64,
    pub parent: Option<u64>,
    pub kind: TopologySessionKind,
    pub round: u64,
    pub changed_roots: Vec<DiagnosticId>,
    pub demand_roots: Vec<DiagnosticId>,
    /// Conservative committed-source order captured at session start. NOT a
    /// promise that these nodes will execute, or a plan valid after callbacks.
    pub order: Vec<DiagnosticId>,
    pub cycle: Vec<DiagnosticId>,
    pub stale_edges: usize,
    pub visited_nodes: usize,
    /// High-water mark of matched reconciliations, never an execution cursor.
    pub cursor: usize,
    pub trace: Vec<TopologyTrace>,
    pub status_at_close: RuntimeStatus,
}
#[derive(Clone, Debug)]
pub struct TopologySnapshot {
    pub enabled: bool,
    pub suppressed_sessions: u64,
    pub pending_changed_roots: Vec<DiagnosticId>,
    /// Close order (a nested read closes before its outer round).
    pub sessions: Vec<TopologyPlan>,
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Color {
    #[default]
    None,
    Temporary,
    Permanent,
}
#[derive(Clone, Copy, Default)]
struct Visit {
    epoch: u64,
    generation: u32,
    color: Color,
    position: usize,
    executed: bool,
}
struct Walk {
    key: Key,
    cursor: usize,
}
#[derive(Default)]
struct Buffer {
    id: u64,
    parent: Option<u64>,
    round: u64,
    kind: Option<TopologySessionKind>,
    roots: Vec<Key>,
    changed: Vec<Key>,
    marks: Vec<Visit>,
    dfs: Vec<Walk>,
    order: Vec<Key>,
    cycle: Vec<Key>,
    trace: Vec<TopologyTrace>,
    stale_edges: usize,
    visited: usize,
    cursor: usize,
}
#[derive(Default)]
pub(super) struct Storage {
    next_id: u64,
    pending: Vec<Key>,
    active: Vec<Buffer>,
    pool: Vec<Buffer>,
    history: VecDeque<TopologyPlan>,
    suppressed: u64,
}
impl Storage {
    pub(super) fn changed(&mut self, key: Key) {
        // At most one pending entry per slot; old generations cannot grow this
        // list indefinitely when a host writes/disposes without flushing.
        if let Some(old) = self.pending.iter_mut().find(|k| k.index == key.index) {
            *old = key;
        } else {
            self.pending.push(key);
        }
    }
    pub(super) fn capacity_bytes(&self) -> usize {
        let buffers = self
            .active
            .iter()
            .chain(&self.pool)
            .map(|b| {
                (b.roots.capacity()
                    + b.changed.capacity()
                    + b.order.capacity()
                    + b.cycle.capacity())
                    * std::mem::size_of::<Key>()
                    + b.marks.capacity() * std::mem::size_of::<Visit>()
                    + b.dfs.capacity() * std::mem::size_of::<Walk>()
                    + b.trace.capacity() * std::mem::size_of::<TopologyTrace>()
            })
            .sum::<usize>();
        buffers
            + (self.active.capacity() + self.pool.capacity()) * std::mem::size_of::<Buffer>()
            + self.pending.capacity() * std::mem::size_of::<Key>()
            + self.history.capacity() * std::mem::size_of::<TopologyPlan>()
            + self
                .history
                .iter()
                .map(|p| {
                    (p.changed_roots.capacity()
                        + p.demand_roots.capacity()
                        + p.order.capacity()
                        + p.cycle.capacity())
                        * std::mem::size_of::<DiagnosticId>()
                        + p.trace.capacity() * std::mem::size_of::<TopologyTrace>()
                })
                .sum::<usize>()
    }
}
impl Buffer {
    fn reset(&mut self, id: u64, parent: Option<u64>, round: u64, kind: TopologySessionKind) {
        self.id = id;
        self.parent = parent;
        self.round = round;
        self.kind = Some(kind);
        self.roots.clear();
        self.changed.clear();
        self.dfs.clear();
        self.order.clear();
        self.cycle.clear();
        self.trace.clear();
        self.stale_edges = 0;
        self.visited = 0;
        self.cursor = 0;
    }
    fn enter(&mut self, g: &Graph, key: Key) -> bool {
        if header(g, key).is_err() {
            self.stale_edges += 1;
            return false;
        }
        let mark = &mut self.marks[key.index as usize];
        if mark.epoch == self.id && mark.generation == key.generation {
            match mark.color {
                Color::Permanent => return false,
                Color::Temporary => {
                    let start = self.dfs.iter().position(|f| f.key == key).unwrap();
                    self.cycle.extend(self.dfs[start..].iter().map(|f| f.key));
                    self.cycle.push(key);
                    return false;
                }
                Color::None => (),
            }
        }
        *mark = Visit {
            epoch: self.id,
            generation: key.generation,
            color: Color::Temporary,
            position: usize::MAX,
            executed: false,
        };
        self.visited += 1;
        self.dfs.push(Walk { key, cursor: 0 });
        true
    }
    fn build(&mut self, g: &Graph) {
        self.marks.resize(g.hot.len(), Visit::default());
        for i in 0..self.roots.len() {
            if !self.enter(g, self.roots[i]) {
                continue;
            }
            while let Some(f) = self.dfs.last_mut() {
                let key = f.key;
                let next = g.cold[key.index as usize]
                    .as_ref()
                    .unwrap()
                    .sources
                    .get(f.cursor)
                    .copied();
                if let Some(source) = next {
                    f.cursor += 1;
                    if !source.live {
                        self.stale_edges += 1;
                        continue;
                    }
                    self.enter(g, source.key);
                    if !self.cycle.is_empty() {
                        self.dfs.clear();
                        self.order.clear();
                        return;
                    }
                } else {
                    self.dfs.pop();
                    let mark = &mut self.marks[key.index as usize];
                    mark.color = Color::Permanent;
                    mark.position = self.order.len();
                    self.order.push(key);
                }
            }
        }
    }
    fn record(&mut self, key: Key) {
        let mark = self.marks.get_mut(key.index as usize).filter(|m| {
            m.epoch == self.id
                && m.generation == key.generation
                && m.color == Color::Permanent
                && self.cycle.is_empty()
        });
        let (planned_index, relation) = if let Some(m) = mark {
            let relation = if m.executed {
                TraceRelation::Revisited
            } else if m.position < self.cursor {
                TraceRelation::BeforeCursor
            } else {
                TraceRelation::InOrder
            };
            m.executed = true;
            self.cursor = self.cursor.max(m.position + 1);
            (Some(m.position), relation)
        } else {
            (None, TraceRelation::Unplanned)
        };
        self.trace.push(TopologyTrace {
            node: key.into(),
            planned_index,
            relation,
        });
    }
    fn snapshot(&self, status: RuntimeStatus) -> TopologyPlan {
        TopologyPlan {
            id: self.id,
            parent: self.parent,
            kind: self.kind.unwrap(),
            round: self.round,
            changed_roots: self.changed.iter().copied().map(Into::into).collect(),
            demand_roots: self.roots.iter().copied().map(Into::into).collect(),
            order: self.order.iter().copied().map(Into::into).collect(),
            cycle: self.cycle.iter().copied().map(Into::into).collect(),
            stale_edges: self.stale_edges,
            visited_nodes: self.visited,
            cursor: self.cursor,
            trace: self.trace.clone(),
            status_at_close: status,
        }
    }
}
pub(super) struct Session<'a> {
    rt: &'a Runtime,
    id: u64,
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        // Only scalar metadata is cloned/dropped here; no callback or typed
        // payload is held, including on an evaluator's Err/unwind exit.
        self.rt.graph(|_| {
            let mut s = self.rt.0.topology.borrow_mut();
            let b = s.active.pop().expect("shadow session stack");
            assert_eq!(b.id, self.id);
            if s.history.len() == HISTORY_LIMIT {
                s.history.pop_front();
            }
            s.history.push_back(b.snapshot(self.rt.status()));
            s.pool.push(b);
        });
    }
}
fn begin(rt: &Runtime, kind: TopologySessionKind, key: Option<Key>) -> Option<Session<'_>> {
    if !rt.0.topology_enabled.get() || rt.check(READ).is_err() {
        return None;
    }
    let id = rt.graph(|g| {
        let mut s = rt.0.topology.borrow_mut();
        // Effect updates already belong to the round's shared plan. Dynamic
        // reads inside callbacks get an independent nested session/buffer.
        if key.is_some()
            && rt.0.depth.get() == 0
            && s.active
                .last()
                .is_some_and(|b| b.kind == Some(TopologySessionKind::Round))
        {
            return None;
        }
        let Some(id) = s.next_id.checked_add(1) else {
            s.suppressed = s.suppressed.saturating_add(1);
            return None;
        };
        s.next_id = id;
        let parent = s.active.last().map(|b| b.id);
        let mut b = s.pool.pop().unwrap_or_default();
        b.reset(id, parent, g.round, kind);
        b.changed
            .extend(s.pending.iter().copied().filter(|&k| header(g, k).is_ok()));
        if let Some(key) = key {
            b.roots.push(key);
        } else {
            b.roots.extend(
                g.current
                    .iter()
                    .copied()
                    .filter(|&k| header(g, k).is_ok_and(|h| h.queue == 2)),
            );
            s.pending.clear();
        }
        b.build(g);
        s.active.push(b);
        Some(id)
    })?;
    Some(Session { rt, id })
}
pub(super) fn begin_round(rt: &Runtime) -> Option<Session<'_>> {
    begin(rt, TopologySessionKind::Round, None)
}
pub(super) fn begin_read(rt: &Runtime, key: Key) -> Option<Session<'_>> {
    begin(rt, TopologySessionKind::Read, Some(key))
}
pub(super) fn completed(rt: &Runtime, key: Key) {
    rt.graph(|_| {
        for b in &mut rt.0.topology.borrow_mut().active {
            b.record(key);
        }
    });
}
impl Runtime {
    /// Optional shadow planning only. Does not select or alter an evaluator.
    pub fn set_topology_diagnostics(&self, enabled: bool) -> Result<()> {
        self.check(CREATE)?;
        self.check_observation()?;
        self.graph(|_| {
            assert!(self.0.topology.borrow().active.is_empty());
            self.0.topology_enabled.set(enabled);
        });
        Ok(())
    }
    pub fn topology_snapshot(&self) -> Result<TopologySnapshot> {
        self.check_observation()?;
        self.graph(|_| {
            let s = self.0.topology.borrow();
            Ok(TopologySnapshot {
                enabled: self.0.topology_enabled.get(),
                suppressed_sessions: s.suppressed,
                pending_changed_roots: s.pending.iter().copied().map(Into::into).collect(),
                sessions: s.history.iter().cloned().collect(),
            })
        })
    }
    pub fn clear_topology_diagnostics(&self) -> Result<()> {
        self.check(CREATE)?;
        self.check_observation()?;
        self.graph(|_| {
            let mut s = self.0.topology.borrow_mut();
            assert!(s.active.is_empty());
            s.history.clear();
            s.pending.clear();
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
