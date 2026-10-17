//! Shared postorder DAG used as an executable, demand-gated address space.
//! Edges are jumps to dependency slots; Dirty jumps directly to its own slot.
//! A linear eager sweep would violate lazy branch abandonment and is forbidden.
use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Color {
    #[default]
    Unseen,
    Temporary,
    Permanent,
}
#[derive(Clone, Copy, Default)]
struct Mark {
    generation: u32,
    color: Color,
    position: usize,
}
struct Walk {
    key: Key,
    source: usize,
}
struct RankWalk {
    key: Key,
    source: usize,
    height: Option<usize>,
}
struct Instruction {
    key: Key,
    edges: Vec<Edge>,
}
#[derive(Clone, Copy)]
struct Edge {
    source: Source,
    target: Option<usize>,
}

#[derive(Default)]
pub(super) struct Plan {
    roots: Vec<Key>,
    changed: Vec<Key>,
    pending_changed: Vec<Key>,
    marks: Vec<Mark>,
    dfs: Vec<Walk>,
    order: Vec<Instruction>,
    spare: Vec<Instruction>,
    propagation: Vec<Key>,
    epoch: Option<u64>,
    invalid: bool,
    cursor: Option<usize>,
    revision: u64,
    // Local instruction addresses NEVER encode topological rank. Ranks are
    // computed only when multiple immediate promotions need ordering.
    ranks: Vec<Mark>,
    rank_dfs: Vec<RankWalk>,
    rank_invalidation: Vec<Key>,
    #[cfg(feature = "diagnostics")]
    builds: u64,
    #[cfg(feature = "diagnostics")]
    repairs: u64,
    #[cfg(feature = "diagnostics")]
    jumps: u64,
    #[cfg(feature = "diagnostics")]
    promotions: u64,
    #[cfg(feature = "diagnostics")]
    cycle_edges: usize,
    #[cfg(feature = "diagnostics")]
    local_extensions: u64,
    #[cfg(feature = "diagnostics")]
    local_source_reads: u64,
    #[cfg(feature = "diagnostics")]
    local_structure_changes: u64,
    #[cfg(feature = "diagnostics")]
    local_compile_visits: u64,
    #[cfg(feature = "diagnostics")]
    local_rank_visits: u64,
    #[cfg(feature = "diagnostics")]
    local_rank_invalidations: u64,
}
impl Plan {
    pub(super) fn changed(&mut self, key: Key) {
        if let Some(old) = self
            .pending_changed
            .iter_mut()
            .find(|k| k.index == key.index)
        {
            *old = key;
        } else {
            self.pending_changed.push(key);
        }
    }
    pub(super) fn begin(&mut self, roots: &[Key]) {
        self.roots.clear();
        self.roots.extend_from_slice(roots);
        self.changed.clear();
        self.changed.extend_from_slice(&self.pending_changed);
        self.pending_changed.clear();
        self.cursor = None;
        self.invalid = true;
    }
    pub(super) fn begin_local(&mut self, roots: &[Key]) {
        self.invalid = self.order.is_empty();
        self.roots.clear();
        self.roots.extend_from_slice(roots);
        self.changed.clear();
        self.changed.extend_from_slice(&self.pending_changed);
        self.pending_changed.clear();
        self.cursor = None;
    }
    pub(super) fn structure_changed(&mut self, g: &Graph, key: Key) {
        #[cfg(feature = "diagnostics")]
        {
            self.local_structure_changes = self.local_structure_changes.saturating_add(1);
        }
        // Source instructions are read LIVE: no instruction cache to repair.
        // Only cached ordering of this consumer and its cached descendants
        // becomes invalid. If a rank was never computed, no cached descendant
        // can depend on it (rank() computes the full ancestor segment).
        self.rank_invalidation.clear();
        self.invalidate_rank(key);
        while let Some(key) = self.rank_invalidation.pop() {
            if header(g, key).is_err() {
                continue;
            }
            for observer in &g.cold[key.index as usize].as_ref().unwrap().observers {
                self.invalidate_rank(observer.key);
            }
        }
    }
    fn invalidate_rank(&mut self, key: Key) {
        if let Some(mark) = self.ranks.get_mut(key.index as usize)
            && mark.generation == key.generation
            && mark.color != Color::Unseen
        {
            mark.color = Color::Unseen;
            self.rank_invalidation.push(key);
            #[cfg(feature = "diagnostics")]
            {
                self.local_rank_invalidations = self.local_rank_invalidations.saturating_add(1);
            }
        }
    }
    pub(super) fn demand(&mut self, key: Key) {
        if !self.roots.contains(&key) {
            if let Some(old) = self.roots.iter_mut().find(|k| k.index == key.index) {
                *old = key;
            } else {
                self.roots.push(key);
            }
            if self.position(key).is_none() {
                self.invalid = true;
            }
        }
    }
    fn position(&self, key: Key) -> Option<usize> {
        self.marks
            .get(key.index as usize)
            .filter(|m| m.generation == key.generation && m.color == Color::Permanent)
            .map(|m| m.position)
    }
    fn enter(&mut self, g: &Graph, key: Key) {
        if header(g, key).is_err() {
            return;
        }
        let mark = &mut self.marks[key.index as usize];
        if mark.generation == key.generation && mark.color != Color::Unseen {
            // Speculative cycles are diagnostic only: an abandoned old suffix
            // must not poison the runtime. Demanded cycles use phase guards.
            #[cfg(feature = "diagnostics")]
            if mark.color == Color::Temporary {
                self.cycle_edges += 1;
            }
            return;
        }
        let position = if mark.color == Color::Permanent
            && self
                .order
                .get(mark.position)
                .is_some_and(|i| i.key.index == key.index)
        {
            mark.position
        } else {
            usize::MAX
        };
        *mark = Mark {
            generation: key.generation,
            color: Color::Temporary,
            position,
        };
        self.dfs.push(Walk { key, source: 0 });
    }
    pub(super) fn ensure(&mut self, g: &Graph, epoch: Option<u64>) {
        if !self.invalid && epoch.is_some() && self.epoch == epoch {
            return;
        }
        // Stable topology is revalidated in full against CURRENT committed
        // edges, not assumed across callbacks. Refresh scalar certificates in
        // place; topology changes fall back to conservative iterative rebuild.
        let stable = !self.invalid
            && self.order.iter().all(|i| {
                header(g, i.key).is_ok() && {
                    let sources = &g.cold[i.key.index as usize].as_ref().unwrap().sources;
                    sources.len() == i.edges.len()
                        && sources
                            .iter()
                            .zip(&i.edges)
                            .all(|(a, b)| a.key == b.source.key && a.live == b.source.live)
                }
            });
        self.epoch = epoch;
        self.invalid = false;
        self.cursor = None;
        // Revision is diagnostic, not a validity certificate; no wrap reuse.
        self.revision = self.revision.saturating_add(1);
        if stable {
            for i in &mut self.order {
                for (edge, source) in i
                    .edges
                    .iter_mut()
                    .zip(&g.cold[i.key.index as usize].as_ref().unwrap().sources)
                {
                    edge.source = *source;
                }
            }
            #[cfg(feature = "diagnostics")]
            {
                self.repairs = self.repairs.saturating_add(1);
            }
            return;
        }
        #[cfg(feature = "diagnostics")]
        {
            self.builds = self.builds.saturating_add(1);
            self.cycle_edges = 0;
        }
        self.marks.clear();
        self.marks.resize(g.hot.len(), Mark::default());
        self.dfs.clear();
        self.spare.append(&mut self.order);
        for i in 0..self.roots.len() {
            self.enter(g, self.roots[i]);
            while let Some(walk) = self.dfs.last_mut() {
                let key = walk.key;
                let source = g.cold[key.index as usize]
                    .as_ref()
                    .unwrap()
                    .sources
                    .get(walk.source)
                    .copied();
                if let Some(source) = source {
                    walk.source += 1;
                    if source.live {
                        self.enter(g, source.key);
                    }
                } else {
                    self.dfs.pop();
                    let position = self.order.len();
                    let mut instruction = self.spare.pop().unwrap_or(Instruction {
                        key,
                        edges: Vec::new(),
                    });
                    instruction.key = key;
                    instruction.edges.clear();
                    instruction.edges.extend(
                        g.cold[key.index as usize]
                            .as_ref()
                            .unwrap()
                            .sources
                            .iter()
                            .copied()
                            .map(|source| Edge {
                                source,
                                target: None,
                            }),
                    );
                    self.order.push(instruction);
                    self.marks[key.index as usize] = Mark {
                        generation: key.generation,
                        color: Color::Permanent,
                        position,
                    };
                }
            }
        }
        // Resolve shared/back edges after all roots have been visited. Forward
        // edges can exist only in a speculative committed-source cycle.
        for i in 0..self.order.len() {
            for j in 0..self.order[i].edges.len() {
                let edge = self.order[i].edges[j];
                let target = if edge.source.live {
                    self.position(edge.source.key)
                } else {
                    None
                };
                self.order[i].edges[j].target = target;
            }
        }
    }
    pub(super) fn select(&mut self, key: Key) {
        self.cursor = self.position(key);
        assert!(
            self.cursor.is_some(),
            "demand must be compiled into executable DAG"
        );
        #[cfg(feature = "diagnostics")]
        {
            self.jumps = self.jumps.saturating_add(1);
        }
    }
    pub(super) fn source(&mut self, key: Key, source: usize) -> Option<Source> {
        self.select(key);
        let edge = *self.order[self.cursor.unwrap()].edges.get(source)?;
        let mut source = edge.source;
        if let Some(target) = edge.target {
            self.cursor = Some(target);
            source.key = self.order[target].key;
        }
        Some(source)
    }
    /// A round's schedule is a snapshot, not a certificate for future graph
    /// contents. Compile it ONCE, then extend only missing demanded subgraphs.
    /// Existing slots remain stable across callbacks and nested reads.
    pub(super) fn ensure_local(&mut self, g: &Graph, key: Key) -> usize {
        if self.invalid {
            self.invalid = false;
            self.revision = self.revision.saturating_add(1);
            self.marks.clear();
            self.marks.resize(g.hot.len(), Mark::default());
            self.dfs.clear();
            self.spare.append(&mut self.order);
            #[cfg(feature = "diagnostics")]
            {
                self.builds = self.builds.saturating_add(1);
                self.cycle_edges = 0;
            }
            for i in 0..self.roots.len() {
                self.compile_local(g, self.roots[i]);
            }
        }
        if let Some(position) = self.position(key) {
            return position;
        }
        self.marks.resize(g.hot.len(), Mark::default());
        self.roots.push(key);
        self.compile_local(g, key);
        // Dynamic segments may not form one global order after a callback.
        #[cfg(feature = "diagnostics")]
        {
            self.local_extensions = self.local_extensions.saturating_add(1);
        }
        self.position(key).expect("live demanded slot")
    }
    pub(super) fn select_local(&mut self, g: &Graph, key: Key) -> usize {
        let position = self.ensure_local(g, key);
        self.cursor = Some(position);
        #[cfg(feature = "diagnostics")]
        {
            self.jumps = self.jumps.saturating_add(1);
        }
        position
    }
    fn compile_local(&mut self, g: &Graph, key: Key) {
        self.enter(g, key);
        while let Some(walk) = self.dfs.last_mut() {
            #[cfg(feature = "diagnostics")]
            {
                self.local_compile_visits = self.local_compile_visits.saturating_add(1);
            }
            let key = walk.key;
            let source = g.cold[key.index as usize]
                .as_ref()
                .unwrap()
                .sources
                .get(walk.source)
                .copied();
            if let Some(source) = source {
                walk.source += 1;
                if source.live {
                    self.enter(g, source.key);
                }
            } else {
                self.dfs.pop();
                let reusable = self.marks[key.index as usize].position;
                let position = if reusable != usize::MAX {
                    // The old generation is dead. Reuse its instruction slot,
                    // not an ever-growing history of retired generations.
                    self.order[reusable].key = key;
                    self.order[reusable].edges.clear();
                    reusable
                } else {
                    let position = self.order.len();
                    let mut instruction = self.spare.pop().unwrap_or(Instruction {
                        key,
                        edges: Vec::new(),
                    });
                    instruction.key = key;
                    // Extension slots fetch CURRENT source instructions lazily;
                    // copying/repairing their entire source list is unnecessary.
                    instruction.edges.clear();
                    self.order.push(instruction);
                    position
                };
                self.marks[key.index as usize] = Mark {
                    generation: key.generation,
                    color: Color::Permanent,
                    position,
                };
            }
        }
    }
    /// Revalidate ONLY the instruction about to be consumed. No cached Source
    /// or edge address is trusted across a callback, even if topology changed.
    /// The remaining old suffix is not inspected or preexecuted.
    #[cfg(test)]
    pub(super) fn source_local(&mut self, g: &Graph, key: Key, index: usize) -> Option<Source> {
        self.select(key);
        self.source_local_at(g, key, self.cursor.unwrap(), index)
    }
    /// Slot resolved with the full generational Key in THIS callback-free
    /// kernel. Never retain this address across callbacks or session changes.
    pub(super) fn source_local_at(
        &mut self,
        g: &Graph,
        key: Key,
        position: usize,
        index: usize,
    ) -> Option<Source> {
        debug_assert_eq!(self.order[position].key, key);
        let sources = &g.cold[key.index as usize].as_ref().unwrap().sources;
        let source = *sources.get(index)?;
        let target = if source.live {
            self.position(source.key)
        } else {
            None
        };
        // Local execution never consumes cached Sources. Do not maintain a
        // second copy of metadata that must be fetched live on every read.
        self.cursor = target.or(Some(position));
        #[cfg(feature = "diagnostics")]
        {
            self.local_source_reads = self.local_source_reads.saturating_add(1);
        }
        Some(source)
    }
    /// The topological address space determines immediate publication order.
    /// Unknown/new consumers sort last, and are still promoted immediately.
    /// Nothing executes here, and transitive invalidation is NOT removed.
    pub(super) fn published(&mut self, g: &mut Graph, key: Key, epoch: Option<u64>) {
        self.publish::<false>(g, key, epoch);
    }
    pub(super) fn published_local(&mut self, g: &mut Graph, key: Key) {
        self.publish::<true>(g, key, None);
    }
    fn rank(&mut self, g: &Graph, key: Key) -> usize {
        if header(g, key).is_err() {
            return usize::MAX;
        }
        self.ranks.resize(g.hot.len(), Mark::default());
        let mark = self.ranks[key.index as usize];
        if mark.generation == key.generation && mark.color == Color::Permanent {
            return mark.position;
        }
        self.rank_dfs.clear();
        self.enter_rank(key);
        while let Some(walk) = self.rank_dfs.last_mut() {
            #[cfg(feature = "diagnostics")]
            {
                self.local_rank_visits = self.local_rank_visits.saturating_add(1);
            }
            let source = g.cold[walk.key.index as usize]
                .as_ref()
                .unwrap()
                .sources
                .get(walk.source)
                .copied();
            if let Some(source) = source {
                if !source.live || header(g, source.key).is_err() {
                    walk.source += 1;
                    continue;
                }
                let mark = self.ranks[source.key.index as usize];
                if mark.generation != source.key.generation || mark.color == Color::Unseen {
                    self.enter_rank(source.key);
                    continue;
                }
                walk.source += 1;
                // A speculative committed-source cycle has no topological
                // rank. Keep it metadata-only; demanded phase guards decide
                // errors. Propagate unknown to consumers of that cycle.
                walk.height = if mark.color == Color::Temporary || mark.position == usize::MAX {
                    None
                } else {
                    walk.height
                        .and_then(|h| mark.position.checked_add(1).map(|n| h.max(n)))
                };
            } else {
                let walk = self.rank_dfs.pop().unwrap();
                self.ranks[walk.key.index as usize] = Mark {
                    generation: walk.key.generation,
                    color: Color::Permanent,
                    position: walk.height.unwrap_or(usize::MAX),
                };
            }
        }
        self.ranks[key.index as usize].position
    }
    fn enter_rank(&mut self, key: Key) {
        self.ranks[key.index as usize] = Mark {
            generation: key.generation,
            color: Color::Temporary,
            position: 0,
        };
        self.rank_dfs.push(RankWalk {
            key,
            source: 0,
            height: Some(0),
        });
    }
    fn publish<const LOCAL: bool>(&mut self, g: &mut Graph, key: Key, epoch: Option<u64>) {
        if !header(g, key).is_ok_and(|h| h.kind == Kind::Memo) {
            return;
        }
        if !LOCAL {
            self.ensure(g, epoch);
        }
        self.propagation.clear();
        self.propagation.extend(
            g.cold[key.index as usize]
                .as_ref()
                .unwrap()
                .observers
                .iter()
                .map(|o| o.key),
        );
        if LOCAL && self.propagation.len() > 1 {
            for i in 0..self.propagation.len() {
                self.rank(g, self.propagation[i]);
            }
        }
        let marks = if LOCAL { &self.ranks } else { &self.marks };
        self.propagation.sort_unstable_by_key(|k| {
            let position = marks
                .get(k.index as usize)
                .filter(|m| m.generation == k.generation && m.color == Color::Permanent)
                .map_or(usize::MAX, |m| m.position);
            (position, k.index, k.generation)
        });
        for i in 0..self.propagation.len() {
            let target = self.propagation[i];
            let Ok(h) = header_mut(g, target) else {
                continue;
            };
            if h.state != State::Dirty {
                h.state = State::Dirty;
                #[cfg(feature = "diagnostics")]
                {
                    g.work.state_changes = g.work.state_changes.saturating_add(1);
                    self.promotions = self.promotions.saturating_add(1);
                }
                scheduling::enqueue(g, target);
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    pub(super) fn capacity_bytes(&self) -> usize {
        (self.roots.capacity()
            + self.changed.capacity()
            + self.pending_changed.capacity()
            + self.propagation.capacity())
            * std::mem::size_of::<Key>()
            + self.marks.capacity() * std::mem::size_of::<Mark>()
            + self.ranks.capacity() * std::mem::size_of::<Mark>()
            + self.rank_dfs.capacity() * std::mem::size_of::<RankWalk>()
            + self.rank_invalidation.capacity() * std::mem::size_of::<Key>()
            + self.dfs.capacity() * std::mem::size_of::<Walk>()
            + (self.order.capacity() + self.spare.capacity()) * std::mem::size_of::<Instruction>()
            + self
                .order
                .iter()
                .chain(&self.spare)
                .map(|i| i.edges.capacity() * std::mem::size_of::<Edge>())
                .sum::<usize>()
    }
}

#[cfg(test)]
mod local_tests;
#[cfg(test)]
mod tests;

#[cfg(feature = "diagnostics")]
#[derive(Clone, Debug)]
pub struct ExecutablePlanSnapshot {
    pub session: u64,
    pub revision: u64,
    pub roots: Vec<DiagnosticId>,
    pub changed_roots: Vec<DiagnosticId>,
    /// GlobalPlan: repaired compilation. LocalPlan: the round snapshot plus
    /// missing demand segments, NOT a freshly sorted graph after each callback.
    pub order: Vec<DiagnosticId>,
    /// Current gated instruction address, NOT a high-water mark or eager sweep.
    pub cursor: Option<usize>,
    pub builds: u64,
    pub repairs: u64,
    pub jumps: u64,
    pub immediate_promotions: u64,
    pub speculative_cycle_edges: usize,
    pub local_extensions: u64,
    pub local_source_reads: u64,
    pub local_structure_changes: u64,
    pub local_compile_visits: u64,
    pub local_rank_visits: u64,
    pub local_rank_invalidations: u64,
}
#[cfg(feature = "diagnostics")]
impl Plan {
    pub(super) fn snapshot(&self, session: u64) -> ExecutablePlanSnapshot {
        ExecutablePlanSnapshot {
            session,
            revision: self.revision,
            roots: self.roots.iter().copied().map(Into::into).collect(),
            changed_roots: self.changed.iter().copied().map(Into::into).collect(),
            order: self.order.iter().map(|i| i.key.into()).collect(),
            cursor: self.cursor,
            builds: self.builds,
            repairs: self.repairs,
            jumps: self.jumps,
            immediate_promotions: self.promotions,
            speculative_cycle_edges: self.cycle_edges,
            local_extensions: self.local_extensions,
            local_source_reads: self.local_source_reads,
            local_structure_changes: self.local_structure_changes,
            local_compile_visits: self.local_compile_visits,
            local_rank_visits: self.local_rank_visits,
            local_rank_invalidations: self.local_rank_invalidations,
        }
    }
}
