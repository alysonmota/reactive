//! Postorder bytecode with demand gates. Compilation never runs user code.
//! Execution follows gate jumps and threaded return addresses in the program,
//! not any of the frame evaluators. A compiled suffix is NOT an authorization.
use super::*;

const NONE: usize = usize::MAX;
#[derive(Clone, Copy)]
enum Instruction {
    Gate { source: usize, target: usize },
    Execute,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Temporary,
    Permanent,
}
struct Node {
    key: Key,
    start: usize,
    end: usize,
    color: Color,
    active: Option<Active>,
    invalid: bool,
}
struct Walk {
    node: usize,
    source: usize,
}
struct Waiting {
    key: Key,
    revision: u64,
}
struct Active {
    parent: Option<usize>,
    pc: usize,
    epoch: Option<u64>,
    waiting: Option<Waiting>,
    depth: usize,
    caps: u8,
    pure: bool,
}
#[derive(Default)]
pub(super) struct Storage {
    map: Vec<usize>,
    nodes: Vec<Node>,
    code: Vec<Instruction>,
    walk: Vec<Walk>,
    scopes: usize,
    live_code: usize,
    #[cfg(feature = "diagnostics")]
    counters: OrderedCounters,
    #[cfg(feature = "diagnostics")]
    executed: Vec<Key>,
}
#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug, Default)]
pub struct OrderedCounters {
    pub builds: u64,
    pub metadata_visits: u64,
    pub activations: u64,
    pub gate_checks: u64,
    pub executions: u64,
    pub prefix_repairs: u64,
    pub extensions: u64,
}
#[cfg(feature = "diagnostics")]
#[derive(Clone, Debug)]
pub struct OrderedSnapshot {
    pub counters: OrderedCounters,
    pub instruction_count: usize,
    pub executed: Vec<DiagnosticId>,
}
impl Storage {
    #[cfg(test)]
    pub(super) fn arena(&self) -> (usize, *const ()) {
        (self.nodes.capacity(), self.nodes.as_ptr().cast())
    }
    fn reset(&mut self, g: &Graph) {
        debug_assert!(self.nodes.iter().all(|n| n.active.is_none()));
        self.nodes.clear();
        self.code.clear();
        self.walk.clear();
        self.live_code = 0;
        self.map.resize(g.hot.len(), NONE);
        self.map.fill(NONE);
        #[cfg(feature = "diagnostics")]
        {
            self.counters.builds = self.counters.builds.saturating_add(1);
            self.executed.clear();
        }
    }
    fn prepare(&mut self, g: &Graph) {
        // Compact only retired generations/spans, never active return links.
        // Stable rounds execute the SAME compiled program without another DFS.
        if self.nodes.is_empty()
            || self.nodes.len() > g.hot.len().saturating_mul(2).saturating_add(64)
            || self.code.len() > self.live_code.saturating_mul(2).saturating_add(64)
        {
            self.reset(g);
        }
        #[cfg(feature = "diagnostics")]
        self.executed.clear();
    }
    fn find(&self, key: Key) -> Option<usize> {
        let id = *self.map.get(key.index as usize)?;
        self.nodes.get(id).filter(|n| n.key == key).map(|_| id)
    }
    fn enter(&mut self, g: &Graph, key: Key) -> Option<usize> {
        if header(g, key).is_err() {
            return None;
        }
        if let Some(id) = self.find(key) {
            let n = &mut self.nodes[id];
            if n.start != NONE
                && n.end - n.start != g.cold[key.index as usize].as_ref().unwrap().sources.len()
            {
                n.invalid = true;
            }
            if n.invalid && n.color == Color::Permanent && n.active.is_none() {
                n.color = Color::Temporary;
                self.walk.push(Walk {
                    node: id,
                    source: 0,
                });
            }
            return Some(id);
        }
        self.map.resize(g.hot.len(), NONE);
        let id = self.nodes.len();
        self.map[key.index as usize] = id;
        self.nodes.push(Node {
            key,
            start: NONE,
            end: NONE,
            color: Color::Temporary,
            active: None,
            invalid: true,
        });
        self.walk.push(Walk {
            node: id,
            source: 0,
        });
        Some(id)
    }
    fn compile(&mut self, g: &Graph, key: Key) -> Option<usize> {
        let root = self.enter(g, key)?;
        // Temporary source cycles are metadata only. Actual demand uses phase
        // guards, so an abandoned old dependency cycle never executes/fails.
        while let Some(w) = self.walk.last_mut() {
            #[cfg(feature = "diagnostics")]
            {
                self.counters.metadata_visits = self.counters.metadata_visits.saturating_add(1);
            }
            let id = w.node;
            let key = self.nodes[id].key;
            let sources = &g.cold[key.index as usize].as_ref().unwrap().sources;
            if let Some(source) = sources.get(w.source).copied() {
                w.source += 1;
                if source.live {
                    self.enter(g, source.key);
                }
            } else {
                self.walk.pop();
                let start = self.code.len();
                for (index, source) in sources.iter().enumerate() {
                    let target = if source.live {
                        self.find(source.key).unwrap_or(NONE)
                    } else {
                        NONE
                    };
                    self.code.push(Instruction::Gate {
                        source: index,
                        target,
                    });
                }
                let end = self.code.len();
                self.code.push(Instruction::Execute);
                let n = &mut self.nodes[id];
                if n.start != NONE {
                    self.live_code -= n.end - n.start + 1;
                }
                self.live_code += end - start + 1;
                n.start = start;
                n.end = end;
                n.color = Color::Permanent;
                n.invalid = false;
            }
        }
        Some(root)
    }
}
pub(super) struct Scope<'a>(&'a Runtime);
pub(super) fn structure_changed(rt: &Runtime, key: Key, source_count: usize) {
    if rt.0.strategy != EvaluationStrategy::OrderedDemand {
        return;
    }
    let mut p = rt.0.ordered.borrow_mut();
    if let Some(id) = p.find(key) {
        let n = &mut p.nodes[id];
        // Same-length rewiring needs no code rewrite: every consumed gate
        // resolves its current Source and revalidates the target generation.
        n.invalid = n.end - n.start != source_count;
    }
}
impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.0.graph(|_| self.0.0.ordered.borrow_mut().scopes -= 1);
    }
}
pub(super) fn begin_round(rt: &Runtime) -> Option<Scope<'_>> {
    if rt.0.strategy != EvaluationStrategy::OrderedDemand {
        return None;
    }
    rt.graph(|g| {
        let mut p = rt.0.ordered.borrow_mut();
        debug_assert_eq!(p.scopes, 0);
        p.prepare(g);
        for &key in &g.current {
            if header(g, key).is_ok_and(|h| h.queue == 2) {
                p.compile(g, key);
            }
        }
        p.scopes = 1;
    });
    Some(Scope(rt))
}
#[derive(Clone, Copy)]
struct Context {
    reader: Option<Key>,
    owner: Option<Key>,
    pending: Option<ownership::PendingOwner>,
    depth: usize,
    caps: u8,
    pure: bool,
    reader_runtime: Option<u64>,
}
impl Context {
    fn capture(rt: &Runtime) -> Self {
        Self {
            reader: rt.0.reader.get(),
            owner: rt.0.owner.get(),
            pending: rt.0.pending_owner.get(),
            depth: rt.0.depth.get(),
            caps: CAPS.get(),
            pure: PURE.get(),
            reader_runtime: READER_RUNTIME.get(),
        }
    }
    fn restore(self, rt: &Runtime) {
        rt.0.reader.set(self.reader);
        rt.0.owner.set(self.owner);
        rt.0.pending_owner.set(self.pending);
        rt.0.depth.set(self.depth);
        CAPS.set(self.caps);
        PURE.set(self.pure);
        READER_RUNTIME.set(self.reader_runtime);
    }
    fn validation(self, rt: &Runtime) {
        rt.0.reader.set(None);
        rt.0.owner.set(self.owner);
        rt.0.pending_owner.set(self.pending);
        CAPS.set(self.caps & READ);
        PURE.set(true);
        READER_RUNTIME.set(Some(rt.0.id));
    }
}
struct Execution<'a> {
    rt: &'a Runtime,
    context: Context,
    head: Option<usize>,
    cycle: Option<Key>,
}
enum Step {
    Execute(Key, Kind, u8, bool),
    Done,
}
impl Execution<'_> {
    fn activate(
        &mut self,
        g: &mut Graph,
        p: &mut Storage,
        id: usize,
        parent: Option<usize>,
    ) -> Result<()> {
        let key = p.nodes[id].key;
        let h = header(g, key)?;
        if h.phase != EvaluationPhase::Idle {
            self.cycle = Some(key);
            return Err(Error::Cycle);
        }
        let (depth, caps, pure) = if let Some(parent) = parent {
            let a = p.nodes[parent].active.as_ref().unwrap();
            (a.depth.checked_add(1).ok_or(Error::Depth)?, a.caps, a.pure)
        } else {
            (self.context.depth, self.context.caps, self.context.pure)
        };
        if depth >= 512 {
            return Err(Error::Depth);
        }
        let kind = h.kind;
        if p.nodes[id].end - p.nodes[id].start
            != g.cold[key.index as usize].as_ref().unwrap().sources.len()
        {
            p.nodes[id].invalid = true;
            p.compile(g, key);
        }
        p.nodes[id].active = Some(Active {
            parent,
            pc: p.nodes[id].start,
            epoch: self.rt.0.plan_epoch.get(),
            waiting: None,
            depth,
            caps: caps
                & if kind == Kind::Effect {
                    ALL & !FLUSH
                } else {
                    READ
                },
            pure: pure || kind != Kind::Effect,
        });
        self.head = Some(id);
        header_mut(g, key)?.phase = EvaluationPhase::Checking;
        self.rt.0.depth.set(depth + 1);
        #[cfg(feature = "diagnostics")]
        {
            p.counters.activations = p.counters.activations.saturating_add(1);
            self.rt.0.diagnostic.borrow_mut().active.push(CycleFrame {
                node: key.into(),
                phase: EvaluationPhase::Checking,
            });
        }
        Ok(())
    }
    fn finish(&mut self, g: &mut Graph, p: &mut Storage, id: usize) {
        let key = p.nodes[id].key;
        let a = p.nodes[id].active.take().unwrap();
        if let Ok(h) = header_mut(g, key) {
            h.phase = EvaluationPhase::Idle;
            if self.rt.is_poisoned() {
                h.state = State::Dirty;
                let c = g.cold[key.index as usize].as_mut().unwrap();
                c.failure = self.rt.first_failure();
                c.reads.clear();
                c.tracking_prefix = true;
            }
        }
        self.head = a.parent;
        self.rt
            .0
            .depth
            .set(a.parent.map_or(self.context.depth, |parent| {
                p.nodes[parent].active.as_ref().unwrap().depth + 1
            }));
        #[cfg(feature = "diagnostics")]
        {
            self.rt.0.diagnostic.borrow_mut().active.pop();
        }
    }
    fn tick(&mut self, completed: bool) -> Result<Step> {
        let rt = self.rt;
        rt.graph(|g| {
            let mut p = rt.0.ordered.borrow_mut();
            if completed {
                self.context.validation(rt);
                self.finish(g, &mut p, self.head.unwrap());
            }
            while let Some(id) = self.head {
                rt.check(READ)?;
                let key = p.nodes[id].key;
                if header(g, key).is_err() {
                    self.finish(g, &mut p, id);
                    continue;
                }
                let mut checked_first = false;
                if let Some(waiting) = p.nodes[id].active.as_mut().unwrap().waiting.take() {
                    let h = header(g, waiting.key)?;
                    if h.revision != waiting.revision {
                        let h = header_mut(g, key)?;
                        h.freshness = h.freshness.checked_add(1).ok_or(Error::Exhausted)?;
                        h.state = State::Dirty;
                    } else {
                        let a = p.nodes[id].active.as_ref().unwrap();
                        checked_first = a.pc == p.nodes[id].start
                            && h.phase == EvaluationPhase::Idle
                            && h.state == State::Clean
                            && h.initialized
                            && g.cold[key.index as usize]
                                .as_ref()
                                .unwrap()
                                .sources
                                .first()
                                .is_some_and(|s| {
                                    s.live && s.key == waiting.key && s.revision == waiting.revision
                                });
                        p.nodes[id].active.as_mut().unwrap().pc += 1;
                    }
                }
                let epoch = rt.0.plan_epoch.get();
                let a = p.nodes[id].active.as_ref().unwrap();
                if header(g, key)?.state != State::Dirty && (epoch.is_none() || a.epoch != epoch) {
                    let start = p.nodes[id].start;
                    let a = p.nodes[id].active.as_mut().unwrap();
                    a.pc = start + usize::from(checked_first);
                    a.epoch = epoch;
                    #[cfg(feature = "diagnostics")]
                    {
                        p.counters.prefix_repairs = p.counters.prefix_repairs.saturating_add(1);
                    }
                }
                let h = header(g, key)?;
                if h.state == State::Dirty || !h.initialized {
                    let end = p.nodes[id].end;
                    p.nodes[id].active.as_mut().unwrap().pc = end;
                }
                let pc = p.nodes[id].active.as_ref().unwrap().pc;
                match p.code[pc] {
                    Instruction::Gate {
                        source: index,
                        target,
                    } => {
                        // Local revalidation, not a whole-plan epoch repair.
                        let source = g.cold[key.index as usize]
                            .as_ref()
                            .unwrap()
                            .sources
                            .get(index)
                            .copied();
                        let Some(source) = source else {
                            let end = p.nodes[id].end;
                            p.nodes[id].active.as_mut().unwrap().pc = end;
                            continue;
                        };
                        #[cfg(feature = "diagnostics")]
                        {
                            g.work.source_checks = g.work.source_checks.saturating_add(1);
                            p.counters.gate_checks = p.counters.gate_checks.saturating_add(1);
                        }
                        if !source.live {
                            header_mut(g, key)?.state = State::Dirty;
                            continue;
                        }
                        let h = header(g, source.key)?;
                        if h.kind == Kind::Effect {
                            return Err(Error::TypeMismatch);
                        }
                        if h.phase != EvaluationPhase::Idle
                            || h.state != State::Clean
                            || !h.initialized
                        {
                            let target = if p
                                .nodes
                                .get(target)
                                .is_some_and(|n| n.key == source.key && !n.invalid)
                            {
                                target
                            } else {
                                #[cfg(feature = "diagnostics")]
                                {
                                    p.counters.extensions = p.counters.extensions.saturating_add(1);
                                }
                                p.compile(g, source.key).ok_or(Error::InvalidHandle)?
                            };
                            p.nodes[id].active.as_mut().unwrap().waiting = Some(Waiting {
                                key: source.key,
                                revision: source.revision,
                            });
                            self.activate(g, &mut p, target, Some(id))?;
                            continue;
                        }
                        if h.revision != source.revision {
                            let h = header_mut(g, key)?;
                            h.freshness = h.freshness.checked_add(1).ok_or(Error::Exhausted)?;
                            h.state = State::Dirty;
                        } else {
                            p.nodes[id].active.as_mut().unwrap().pc += 1;
                        }
                    }
                    Instruction::Execute => {
                        let h = header(g, key)?;
                        if h.state != State::Dirty && h.initialized {
                            header_mut(g, key)?.state = State::Clean;
                            self.finish(g, &mut p, id);
                            continue;
                        }
                        let kind = h.kind;
                        let c = g.cold[key.index as usize].as_mut().unwrap();
                        c.scope_epoch = c.scope_epoch.checked_add(1).ok_or(Error::Exhausted)?;
                        c.reads.clear();
                        c.tracking_prefix = true;
                        header_mut(g, key)?.phase = EvaluationPhase::Cleaning;
                        #[cfg(feature = "diagnostics")]
                        {
                            if let Some(f) = rt.0.diagnostic.borrow_mut().active.last_mut() {
                                f.phase = EvaluationPhase::Cleaning;
                            }
                            p.counters.executions = p.counters.executions.saturating_add(1);
                            p.executed.push(key);
                        }
                        let a = p.nodes[id].active.as_ref().unwrap();
                        return Ok(Step::Execute(key, kind, a.caps, a.pure));
                    }
                }
            }
            Ok(Step::Done)
        })
    }
}
impl Drop for Execution<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.rt.record_failure(Error::Panic);
        }
        self.rt.graph(|g| {
            let mut p = self.rt.0.ordered.borrow_mut();
            while let Some(id) = self.head {
                self.finish(g, &mut p, id);
            }
        });
        self.context.restore(self.rt);
    }
}
pub(super) fn update(rt: &Runtime, key: Key) -> Result<()> {
    rt.check(READ)?;
    let prepared = rt.graph(|g| {
        let h = header(g, key)?;
        if h.phase != EvaluationPhase::Idle {
            return Err(Error::Cycle);
        }
        if h.state == State::Clean && h.initialized {
            return Ok(None);
        }
        let mut p = rt.0.ordered.borrow_mut();
        if p.scopes == 0 {
            p.prepare(g);
        }
        let id = p.compile(g, key).ok_or(Error::InvalidHandle)?;
        p.scopes += 1;
        Ok(Some(id))
    });
    let id = match prepared {
        Ok(None) => return Ok(()),
        Ok(Some(id)) => id,
        Err(e) => {
            #[cfg(feature = "diagnostics")]
            if e == Error::Cycle {
                rt.capture_cycle(key);
            }
            rt.record_failure(e);
            return Err(e);
        }
    };
    let _scope = Scope(rt);
    let mut e = Execution {
        rt,
        context: Context::capture(rt),
        head: None,
        cycle: None,
    };
    let result = (|| {
        rt.graph(|g| e.activate(g, &mut rt.0.ordered.borrow_mut(), id, None))?;
        e.context.validation(rt);
        let mut completed = false;
        loop {
            match e.tick(completed)? {
                Step::Done => return Ok(()),
                Step::Execute(key, kind, caps, pure) => {
                    CAPS.set(caps);
                    PURE.set(pure);
                    // Reuse only the protected callback/cleanup/publication boundary,
                    // NEVER an old evaluator or its frame stack.
                    planning::execute(rt, key, kind)?;
                    completed = true;
                }
            }
        }
    })();
    if let Err(error) = result {
        #[cfg(feature = "diagnostics")]
        if let Some(key) = e.cycle {
            rt.capture_cycle(key);
        }
        rt.record_failure(error);
    }
    result
}
#[cfg(feature = "diagnostics")]
impl Runtime {
    pub fn ordered_snapshot(&self) -> Result<OrderedSnapshot> {
        self.check_observation()?;
        self.graph(|_| {
            let p = self.0.ordered.borrow();
            Ok(OrderedSnapshot {
                counters: p.counters,
                instruction_count: p.code.len(),
                executed: p.executed.iter().copied().map(Into::into).collect(),
            })
        })
    }
}
#[cfg(test)]
mod tests;
