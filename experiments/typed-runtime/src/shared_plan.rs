//! Executable lazy node programs shared by all effects in a round. A program
//! consists of committed-source validation instructions followed by Execute.
//! Its cursor stops at the FIRST changed source; no old suffix is preexecuted.
//! Programs contain scalar metadata only, and are rebuilt after callbacks.
use super::*;

#[derive(Default)]
struct Program {
    generation: u32,
    epoch: Option<u64>,
    sources: Vec<Source>,
    ready_session: u64,
}
#[derive(Default)]
pub(super) struct Storage {
    pub(super) global: executable_plan::Plan,
    programs: Vec<Program>,
    roots: Vec<Key>,
    order: Vec<Key>,
    active: usize,
    session: u64,
    #[cfg(feature = "diagnostics")]
    stats: SharedPlanCounters,
}
#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedPlanCounters {
    pub sessions: u64,
    pub rounds: u64,
    pub programs_built: u64,
    pub program_hits: u64,
    pub cursor_repairs: u64,
    pub shared_clean_reuses: u64,
    pub stable_programs: u64,
    pub source_instructions_written: u64,
}
#[cfg(feature = "diagnostics")]
#[derive(Clone, Debug)]
pub struct SharedPlanSnapshot {
    pub counters: SharedPlanCounters,
    pub roots: Vec<DiagnosticId>,
    /// Last session's demanded ready prefix, not an eager downstream order.
    pub ready_order: Vec<DiagnosticId>,
    pub callback_epoch: Option<u64>,
}
pub(super) fn enabled(rt: &Runtime) -> bool {
    matches!(
        rt.0.strategy,
        EvaluationStrategy::SharedPlan
            | EvaluationStrategy::SharedPlanDirty
            | EvaluationStrategy::SharedPlanOptimized
            | EvaluationStrategy::SharedPlanDirtyOptimized
            | EvaluationStrategy::GlobalPlan
            | EvaluationStrategy::GlobalPlanDirty
            | EvaluationStrategy::LocalPlan
            | EvaluationStrategy::LocalPlanDirty
    )
}

/// Stage 7 experiment: only an actually different published memo value makes
/// its IMMEDIATE consumers Dirty. Existing transitive invalidation is retained
/// as the lazy-read and effect-wakeup fallback; it is never replaced here.
pub(super) fn published(g: &mut Graph, key: Key) {
    if !header(g, key).is_ok_and(|h| h.kind == Kind::Memo) {
        return;
    }
    let count = g.cold[key.index as usize].as_ref().unwrap().observers.len();
    for i in 0..count {
        let target = g.cold[key.index as usize].as_ref().unwrap().observers[i].key;
        let Ok(h) = header_mut(g, target) else {
            continue;
        };
        // Transitive invalidation already advanced freshness. Publication's
        // value revision is the new evidence here; do not increment an extra
        // counter or invent an additional exhaustion/failure boundary.
        if h.state != State::Dirty {
            h.state = State::Dirty;
            #[cfg(feature = "diagnostics")]
            {
                g.work.state_changes = g.work.state_changes.saturating_add(1);
            }
            scheduling::enqueue(g, target);
        }
    }
}

/// No graph/reference borrowing: invalidates certificates before and after
/// arbitrary user code, including unwind. None permanently disables caching
/// on epoch exhaustion; it NEVER wraps into an old certificate.
pub(super) struct Callback<'a>(Option<&'a Runtime>);
fn invalidate(rt: &Runtime) {
    rt.0.plan_epoch
        .set(rt.0.plan_epoch.get().and_then(|n| n.checked_add(1)));
}
impl<'a> Callback<'a> {
    pub(super) fn enter(rt: &'a Runtime) -> Self {
        if enabled(rt) || rt.0.strategy == EvaluationStrategy::OrderedDemand {
            invalidate(rt);
            Self(Some(rt))
        } else {
            Self(None)
        }
    }
}
impl Drop for Callback<'_> {
    fn drop(&mut self) {
        if let Some(rt) = self.0 {
            invalidate(rt);
        }
    }
}

pub(super) struct Scope<'a>(&'a Runtime);
impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.0.graph(|_| {
            let mut p = self.0.0.shared_plan.borrow_mut();
            p.active -= 1;
        });
    }
}
fn begin(rt: &Runtime, key: Option<Key>) -> Scope<'_> {
    rt.graph(|g| begin_in_graph(rt, g, key));
    Scope(rt)
}
fn begin_in_graph(rt: &Runtime, g: &Graph, key: Option<Key>) {
    let mut p = rt.0.shared_plan.borrow_mut();
    if p.active == 0 {
        if let Some(n) = p.session.checked_add(1) {
            p.session = n;
        } else {
            // Session marks are only a dedup optimization, never a life or
            // value certificate. Clearing them makes ID reuse safe.
            for node in &mut p.programs {
                node.ready_session = 0;
            }
            p.session = 1;
        }
        p.roots.clear();
        p.order.clear();
        if let Some(key) = key {
            p.roots.push(key);
        } else {
            p.roots.extend(
                g.current
                    .iter()
                    .copied()
                    .filter(|&k| header(g, k).is_ok_and(|h| h.queue == 2)),
            );
        }
        #[cfg(feature = "diagnostics")]
        {
            p.stats.sessions = p.stats.sessions.saturating_add(1);
            if key.is_none() {
                p.stats.rounds = p.stats.rounds.saturating_add(1);
            }
        }
        if matches!(
            rt.0.strategy,
            EvaluationStrategy::GlobalPlan
                | EvaluationStrategy::GlobalPlanDirty
                | EvaluationStrategy::LocalPlan
                | EvaluationStrategy::LocalPlanDirty
        ) {
            let Storage { global, roots, .. } = &mut *p;
            if matches!(
                rt.0.strategy,
                EvaluationStrategy::LocalPlan | EvaluationStrategy::LocalPlanDirty
            ) {
                global.begin_local(roots);
            } else {
                global.begin(roots);
            }
        }
    }
    if matches!(
        rt.0.strategy,
        EvaluationStrategy::GlobalPlan | EvaluationStrategy::GlobalPlanDirty
    ) && let Some(key) = key
    {
        p.global.demand(key);
    }
    p.active += 1;
}
pub(super) fn begin_round(rt: &Runtime) -> Option<Scope<'_>> {
    if enabled(rt) {
        Some(begin(rt, None))
    } else {
        None
    }
}

pub(super) struct Frame {
    key: Key,
    kind: Kind,
    cursor: usize,
    waiting: Option<Waiting>,
    caps: u8,
    pure: bool,
    epoch: Option<u64>,
    repair: bool,
}
struct Waiting {
    key: Key,
    revision: u64,
}
#[derive(Clone, Copy)]
struct Context {
    reader: Option<Key>,
    owner: Option<Key>,
    pending_owner: Option<ownership::PendingOwner>,
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
            pending_owner: rt.0.pending_owner.get(),
            depth: rt.0.depth.get(),
            caps: CAPS.get(),
            pure: PURE.get(),
            reader_runtime: READER_RUNTIME.get(),
        }
    }
    fn restore(self, rt: &Runtime) {
        rt.0.reader.set(self.reader);
        rt.0.owner.set(self.owner);
        rt.0.pending_owner.set(self.pending_owner);
        rt.0.depth.set(self.depth);
        CAPS.set(self.caps);
        PURE.set(self.pure);
        READER_RUNTIME.set(self.reader_runtime);
    }
}
struct Restore<'a>(&'a Runtime, Context);
impl Drop for Restore<'_> {
    fn drop(&mut self) {
        self.1.restore(self.0);
    }
}
struct Evaluation<'a> {
    rt: &'a Runtime,
    base: usize,
    context: Context,
    cycle: Option<Key>,
}
enum Step {
    Execute(Key, Kind, u8, bool),
    Finished,
}

pub(super) fn update<const OPT: bool, const GLOBAL: bool, const LOCAL: bool>(
    rt: &Runtime,
    key: Key,
) -> Result<()> {
    rt.check(READ)?;
    let base = rt.graph(|g| {
        if LOCAL {
            let h = header(g, key)?;
            // Keep phase before Clean: an initialized node may still be active.
            if h.phase != EvaluationPhase::Idle {
                return Err(Error::Cycle);
            }
            if h.state == State::Clean && h.initialized {
                return Ok(None);
            }
        }
        begin_in_graph(rt, g, Some(key));
        Ok(Some(rt.0.shared_frames.borrow().len()))
    });
    let base = match base {
        Ok(None) => return Ok(()),
        Ok(Some(base)) => base,
        Err(error) => {
            #[cfg(feature = "diagnostics")]
            if error == Error::Cycle {
                rt.capture_cycle(key);
            }
            rt.record_failure(error);
            return Err(error);
        }
    };
    let _scope = Scope(rt);
    let mut eval = Evaluation {
        rt,
        base,
        context: Context::capture(rt),
        cycle: None,
    };
    rt.0.reader.set(None);
    CAPS.set(CAPS.get() & READ);
    PURE.set(true);
    READER_RUNTIME.set(Some(rt.0.id));
    let result = eval.run::<OPT, GLOBAL, LOCAL>(key);
    if let Err(error) = result {
        #[cfg(feature = "diagnostics")]
        if let Some(key) = eval.cycle {
            rt.capture_cycle(key);
        }
        rt.record_failure(error);
    }
    result
}
impl Evaluation<'_> {
    fn push(&mut self, g: &mut Graph, f: &mut Vec<Frame>, key: Key) -> Result<bool> {
        let h = header(g, key)?;
        if h.phase != EvaluationPhase::Idle {
            self.cycle = Some(key);
            return Err(Error::Cycle);
        }
        if h.state == State::Clean && h.initialized {
            return Ok(false);
        }
        let depth = self
            .context
            .depth
            .checked_add(f.len() - self.base)
            .ok_or(Error::Depth)?;
        if depth >= 512 {
            return Err(Error::Depth);
        }
        let kind = h.kind;
        let (caps, pure) = if f.len() > self.base {
            let p = f.last().unwrap();
            (p.caps, p.pure)
        } else {
            (self.context.caps, self.context.pure)
        };
        f.push(Frame {
            key,
            kind,
            cursor: 0,
            waiting: None,
            caps: caps
                & if kind == Kind::Effect {
                    ALL & !FLUSH
                } else {
                    READ
                },
            pure: pure || kind != Kind::Effect,
            epoch: self.rt.0.plan_epoch.get(),
            repair: false,
        });
        header_mut(g, key)?.phase = EvaluationPhase::Checking;
        self.rt.0.depth.set(depth + 1);
        #[cfg(feature = "diagnostics")]
        self.rt.0.diagnostic.borrow_mut().active.push(CycleFrame {
            node: key.into(),
            phase: EvaluationPhase::Checking,
        });
        Ok(true)
    }
    fn pop(&self, g: &mut Graph, f: &mut Vec<Frame>) {
        let node = f.pop().unwrap();
        if let Ok(h) = header_mut(g, node.key) {
            h.phase = EvaluationPhase::Idle;
            if self.rt.is_poisoned() {
                h.state = State::Dirty;
                let c = g.cold[node.key.index as usize].as_mut().unwrap();
                c.failure = self.rt.first_failure();
                c.reads.clear();
                c.tracking_prefix = true;
            }
        }
        self.rt
            .0
            .depth
            .set(self.context.depth + f.len() - self.base);
        #[cfg(feature = "diagnostics")]
        {
            self.rt.0.diagnostic.borrow_mut().active.pop();
        }
    }
    fn ready(&self, p: &mut Storage, key: Key) {
        if p.programs.len() <= key.index as usize {
            p.programs
                .resize_with(key.index as usize + 1, Program::default);
        }
        let node = &mut p.programs[key.index as usize];
        if node.generation != key.generation {
            node.sources.clear();
            node.epoch = None;
            node.generation = key.generation;
            node.ready_session = 0;
        }
        if node.ready_session != p.session {
            node.ready_session = p.session;
            p.order.push(key);
        }
    }
    fn tick<const OPT: bool, const GLOBAL: bool, const LOCAL: bool>(
        &mut self,
        completed: bool,
    ) -> Result<Step> {
        let rt = self.rt;
        rt.graph(|g| {
            let mut frames = rt.0.shared_frames.borrow_mut();
            let mut plan = rt.0.shared_plan.borrow_mut();
            if completed {
                if LOCAL {
                    // Successful callback return and frame pop form ONE
                    // callback-free boundary. Restore validation context here,
                    // not by capturing a second full context per callback.
                    rt.0.reader.set(None);
                    rt.0.owner.set(self.context.owner);
                    rt.0.pending_owner.set(self.context.pending_owner);
                    CAPS.set(self.context.caps & READ);
                    PURE.set(true);
                    READER_RUNTIME.set(Some(rt.0.id));
                }
                self.pop(g, &mut frames);
            }
            if GLOBAL && !LOCAL {
                for f in frames.iter() {
                    plan.global.demand(f.key);
                }
                plan.global.ensure(g, rt.0.plan_epoch.get());
            }
            if rt.0.plan_epoch.get().is_none() {
                for f in &mut frames[self.base..] {
                    f.repair = true;
                }
            }
            loop {
                if frames.len() == self.base {
                    return Ok(Step::Finished);
                }
                rt.check(READ)?;
                let f = frames.last_mut().unwrap();
                let key = f.key;
                if header(g, key).is_err() {
                    self.pop(g, &mut frames);
                    continue;
                }
                let local_slot = if LOCAL {
                    Some(plan.global.select_local(g, key))
                } else {
                    None
                };
                if GLOBAL && !LOCAL {
                    plan.global.select(key);
                }
                let mut checked_first = false;
                if let Some(source) = f.waiting.take() {
                    let revision = header(g, source.key)?.revision;
                    if revision != source.revision {
                        let h = header_mut(g, key)?;
                        h.freshness = h.freshness.checked_add(1).ok_or(Error::Exhausted)?;
                        h.state = State::Dirty;
                    } else {
                        let h = header(g, source.key)?;
                        checked_first = f.cursor == 0
                            && h.phase == EvaluationPhase::Idle
                            && h.state == State::Clean
                            && h.initialized
                            && g.cold[key.index as usize]
                                .as_ref()
                                .unwrap()
                                .sources
                                .first()
                                .is_some_and(|s| {
                                    s.live && s.key == source.key && s.revision == source.revision
                                });
                        f.cursor += 1;
                    }
                }
                let epoch = rt.0.plan_epoch.get();
                if (!OPT || header(g, key)?.state != State::Dirty) && (f.repair || f.epoch != epoch)
                {
                    // Any callback can invalidate the already-checked prefix.
                    // Restart conservatively; never trust the old instruction
                    // cursor across cleanup/compute/equality/Drop/host callbacks.
                    // The first source was just revalidated above, with no
                    // callback between that check and this repair. There is
                    // no earlier prefix to rebuild in that special case.
                    f.cursor = usize::from(checked_first);
                    f.epoch = epoch;
                    f.repair = false;
                    #[cfg(feature = "diagnostics")]
                    {
                        plan.stats.cursor_repairs = plan.stats.cursor_repairs.saturating_add(1);
                    }
                }
                let h = header(g, key)?;
                if h.state != State::Dirty && h.initialized {
                    let source = if LOCAL {
                        plan.global
                            .source_local_at(g, key, local_slot.unwrap(), f.cursor)
                    } else if GLOBAL {
                        plan.global.source(key, f.cursor)
                    } else {
                        if plan.programs.len() <= key.index as usize {
                            plan.programs
                                .resize_with(key.index as usize + 1, Program::default);
                        }
                        let program = &mut plan.programs[key.index as usize];
                        if epoch.is_none()
                            || program.epoch != epoch
                            || program.generation != key.generation
                        {
                            if program.generation != key.generation {
                                program.ready_session = 0;
                            }
                            let same_generation = program.generation == key.generation;
                            program.generation = key.generation;
                            program.epoch = epoch;
                            let sources = &g.cold[key.index as usize].as_ref().unwrap().sources;
                            // A matching prefix is established from CURRENT metadata,
                            // never from an epoch certificate crossing a callback.
                            let prefix = if OPT && same_generation {
                                program
                                    .sources
                                    .iter()
                                    .zip(sources)
                                    .take_while(|(a, b)| a == b)
                                    .count()
                            } else {
                                0
                            };
                            #[cfg(feature = "diagnostics")]
                            let stable = OPT
                                && same_generation
                                && prefix == sources.len()
                                && prefix == program.sources.len();
                            program.sources.truncate(prefix);
                            program.sources.extend_from_slice(&sources[prefix..]);
                            #[cfg(feature = "diagnostics")]
                            {
                                plan.stats.programs_built =
                                    plan.stats.programs_built.saturating_add(1);
                                plan.stats.source_instructions_written = plan
                                    .stats
                                    .source_instructions_written
                                    .saturating_add((sources.len() - prefix) as u64);
                                if stable {
                                    plan.stats.stable_programs =
                                        plan.stats.stable_programs.saturating_add(1);
                                }
                            }
                        } else {
                            #[cfg(feature = "diagnostics")]
                            {
                                plan.stats.program_hits = plan.stats.program_hits.saturating_add(1);
                            }
                        }
                        plan.programs[key.index as usize]
                            .sources
                            .get(f.cursor)
                            .copied()
                    };
                    if let Some(source) = source {
                        #[cfg(feature = "diagnostics")]
                        {
                            g.work.source_checks = g.work.source_checks.saturating_add(1);
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
                            f.waiting = Some(Waiting {
                                key: source.key,
                                revision: source.revision,
                            });
                            self.push(g, &mut frames, source.key)?;
                            continue;
                        }
                        #[cfg(feature = "diagnostics")]
                        if plan
                            .programs
                            .get(source.key.index as usize)
                            .is_some_and(|p| {
                                p.generation == source.key.generation
                                    && p.ready_session == plan.session
                            })
                        {
                            plan.stats.shared_clean_reuses =
                                plan.stats.shared_clean_reuses.saturating_add(1);
                        }
                        if h.revision != source.revision {
                            let h = header_mut(g, key)?;
                            h.freshness = h.freshness.checked_add(1).ok_or(Error::Exhausted)?;
                            h.state = State::Dirty;
                        } else {
                            f.cursor += 1;
                        }
                        continue;
                    }
                    header_mut(g, key)?.state = State::Clean;
                    if !LOCAL || cfg!(feature = "diagnostics") {
                        self.ready(&mut plan, key);
                    }
                    self.pop(g, &mut frames);
                    continue;
                }
                // The next demanded program has reached its Execute instruction.
                // Sources after the first change have NOT been traversed.
                if !LOCAL || cfg!(feature = "diagnostics") {
                    self.ready(&mut plan, key);
                }
                let c = g.cold[key.index as usize].as_mut().unwrap();
                c.scope_epoch = c.scope_epoch.checked_add(1).ok_or(Error::Exhausted)?;
                c.reads.clear();
                c.tracking_prefix = true;
                header_mut(g, key)?.phase = EvaluationPhase::Cleaning;
                #[cfg(feature = "diagnostics")]
                if let Some(node) = rt.0.diagnostic.borrow_mut().active.last_mut() {
                    node.phase = EvaluationPhase::Cleaning;
                }
                return Ok(Step::Execute(key, f.kind, f.caps, f.pure));
            }
        })
    }
    fn run<const OPT: bool, const GLOBAL: bool, const LOCAL: bool>(
        &mut self,
        key: Key,
    ) -> Result<()> {
        let rt = self.rt;
        if !rt.graph(|g| self.push(g, &mut rt.0.shared_frames.borrow_mut(), key))? {
            return Ok(());
        }
        let mut completed = false;
        loop {
            match self.tick::<OPT, GLOBAL, LOCAL>(completed)? {
                Step::Finished => return Ok(()),
                Step::Execute(key, kind, caps, pure) => {
                    if LOCAL {
                        // Err/unwind is restored by Evaluation::drop. On Ok,
                        // tick restores context together with the pending pop.
                        CAPS.set(caps);
                        PURE.set(pure);
                        planning::execute(rt, key, kind)?;
                    } else {
                        let _restore = Restore(rt, Context::capture(rt));
                        CAPS.set(caps);
                        PURE.set(pure);
                        planning::execute(rt, key, kind)?;
                    }
                    if OPT {
                        completed = true;
                    } else {
                        rt.graph(|g| self.pop(g, &mut rt.0.shared_frames.borrow_mut()));
                    }
                }
            }
        }
    }
}
impl Drop for Evaluation<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.rt.record_failure(Error::Panic);
        }
        self.rt.graph(|g| {
            let mut f = self.rt.0.shared_frames.borrow_mut();
            while f.len() > self.base {
                self.pop(g, &mut f);
            }
        });
        self.context.restore(self.rt);
    }
}

#[cfg(feature = "diagnostics")]
impl Storage {
    pub(super) fn capacity_bytes(&self) -> usize {
        self.programs.capacity() * std::mem::size_of::<Program>()
            + self.global.capacity_bytes()
            + (self.roots.capacity() + self.order.capacity()) * std::mem::size_of::<Key>()
            + self
                .programs
                .iter()
                .map(|p| p.sources.capacity() * std::mem::size_of::<Source>())
                .sum::<usize>()
    }
}
#[cfg(feature = "diagnostics")]
impl Runtime {
    pub fn executable_plan_snapshot(&self) -> Result<executable_plan::ExecutablePlanSnapshot> {
        self.check_observation()?;
        self.graph(|_| {
            let p = self.0.shared_plan.borrow();
            Ok(p.global.snapshot(p.session))
        })
    }
    pub fn shared_plan_snapshot(&self) -> Result<SharedPlanSnapshot> {
        self.check_observation()?;
        self.graph(|_| {
            let p = self.0.shared_plan.borrow();
            Ok(SharedPlanSnapshot {
                counters: p.stats,
                roots: p.roots.iter().copied().map(Into::into).collect(),
                ready_order: p.order.iter().copied().map(Into::into).collect(),
                callback_epoch: self.0.plan_epoch.get(),
            })
        })
    }
}

pub(super) fn global_changed(rt: &Runtime, key: Key) {
    if matches!(
        rt.0.strategy,
        EvaluationStrategy::GlobalPlan
            | EvaluationStrategy::GlobalPlanDirty
            | EvaluationStrategy::LocalPlan
            | EvaluationStrategy::LocalPlanDirty
    ) {
        rt.0.shared_plan.borrow_mut().global.changed(key);
    }
}
pub(super) fn invalidate_global(rt: &Runtime) {
    invalidate(rt);
}
pub(super) fn global_published(rt: &Runtime, g: &mut Graph, key: Key) {
    rt.0.shared_plan
        .borrow_mut()
        .global
        .published(g, key, rt.0.plan_epoch.get());
}
pub(super) fn local_published(rt: &Runtime, g: &mut Graph, key: Key) {
    rt.0.shared_plan.borrow_mut().global.published_local(g, key);
}
pub(super) fn local_structure_changed(rt: &Runtime, g: &Graph, key: Key) {
    if matches!(
        rt.0.strategy,
        EvaluationStrategy::LocalPlan | EvaluationStrategy::LocalPlanDirty
    ) {
        rt.0.shared_plan
            .borrow_mut()
            .global
            .structure_changed(g, key);
    }
}

#[cfg(test)]
mod tests;
