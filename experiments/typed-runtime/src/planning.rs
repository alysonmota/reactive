//! Incremental, demand-driven topological frontier, not a speculative list of
//! all descendants. A frontier ends at the next callback. Resumption resolves
//! keys/sources again; no internal pointer/reference survives that boundary.
use super::*;

/// Selectable experiments. The established evaluator remains the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EvaluationStrategy {
    #[default]
    FullFrames,
    /// Lightweight frames; each enter/step/leave is a separate graph kernel.
    LightFrames,
    /// Advance the demand frontier in one callback-free kernel until ready.
    DemandDriven,
    /// Shared per-round lazy source programs, repaired after callbacks.
    SharedPlan,
    /// SharedPlan plus immediate Dirty publication; transitive Check remains.
    SharedPlanDirty,
    /// Stage 8: incremental programs and fused callback-return kernels.
    SharedPlanOptimized,
    /// Stage 8 specialization with immediate Dirty publication retained.
    SharedPlanDirtyOptimized,
    /// Shared postorder DAG with demand-gated instruction jumps.
    GlobalPlan,
    /// GlobalPlan plus topologically ordered immediate Dirty publication.
    GlobalPlanDirty,
    /// Round snapshot plus instruction-local revalidation; no callback DAG scan.
    LocalPlan,
    /// LocalPlan with immediate publication ordered by the schedule snapshot.
    LocalPlanDirty,
    /// Postorder gate bytecode with threaded demand returns; no frame evaluator.
    OrderedDemand,
}

pub(super) struct Frame {
    key: Key,
    kind: Kind,
    cursor: usize,
    waiting: Option<Source>,
    caps: u8,
    pure: bool,
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
struct Session<'a> {
    rt: &'a Runtime,
    base: usize,
    context: Context,
    cycle: Option<Key>,
}
enum Step {
    Descend(Key),
    Done,
    Execute(Key, Kind, u8, bool),
    Finished,
}

pub(super) fn update<const TOPOLOGICAL: bool>(rt: &Runtime, key: Key) -> Result<()> {
    rt.check(READ)?;
    let base = rt.graph(|_| rt.0.validation_frames.borrow().len());
    let mut session = Session {
        rt,
        base,
        context: Context::capture(rt),
        cycle: None,
    };
    // Checking cannot call user code. Restrict capabilities for the entire
    // planning phase, including allocator reentry; execution restores the
    // cumulative ancestor policy, NOT an unconditional permission grant.
    rt.0.reader.set(None);
    CAPS.set(CAPS.get() & READ);
    PURE.set(true);
    READER_RUNTIME.set(Some(rt.0.id));
    let result = session.run::<TOPOLOGICAL>(key);
    if let Err(error) = result {
        #[cfg(feature = "diagnostics")]
        if let Some(key) = session.cycle {
            rt.capture_cycle(key);
        }
        rt.record_failure(error);
    }
    result
}
impl Session<'_> {
    fn push(&mut self, g: &mut Graph, frames: &mut Vec<Frame>, key: Key) -> Result<bool> {
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
            .checked_add(frames.len() - self.base)
            .ok_or(Error::Depth)?;
        if depth >= 512 {
            return Err(Error::Depth);
        }
        let kind = h.kind;
        let (caps, pure) = if frames.len() > self.base {
            let p = frames.last().unwrap();
            (p.caps, p.pure)
        } else {
            (self.context.caps, self.context.pure)
        };
        frames.push(Frame {
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
    fn step(&self, g: &mut Graph, frames: &mut [Frame]) -> Result<Step> {
        self.rt.check(READ)?;
        let f = frames.last_mut().unwrap();
        let key = f.key;
        if header(g, key).is_err() {
            return Ok(Step::Done);
        }
        if let Some(source) = f.waiting.take() {
            // A callback may have disposed/reused nodes or invalidated sources.
            // Resolve both generations; never consume a cached clean decision.
            if !observe(g, key, source)? {
                f.cursor += 1;
            }
        }
        loop {
            let h = header(g, key)?;
            if h.state == State::Dirty || !h.initialized {
                break;
            }
            let source = g.cold[key.index as usize]
                .as_ref()
                .unwrap()
                .sources
                .get(f.cursor)
                .copied();
            let Some(source) = source else {
                break;
            };
            #[cfg(feature = "diagnostics")]
            {
                g.work.source_checks = g.work.source_checks.saturating_add(1);
            }
            if !source.live {
                header_mut(g, key)?.state = State::Dirty;
                break;
            }
            let h = header(g, source.key)?;
            if h.kind == Kind::Effect {
                return Err(Error::TypeMismatch);
            }
            if h.phase == EvaluationPhase::Idle && h.state == State::Clean && h.initialized {
                if observe(g, key, source)? {
                    break;
                }
                f.cursor += 1;
            } else {
                f.waiting = Some(source);
                return Ok(Step::Descend(source.key));
            }
        }
        let h = header_mut(g, key)?;
        if h.state != State::Dirty && h.initialized {
            h.state = State::Clean;
            return Ok(Step::Done);
        }
        let c = g.cold[key.index as usize].as_mut().unwrap();
        c.scope_epoch = c.scope_epoch.checked_add(1).ok_or(Error::Exhausted)?;
        c.reads.clear();
        c.tracking_prefix = true;
        header_mut(g, key)?.phase = EvaluationPhase::Cleaning;
        #[cfg(feature = "diagnostics")]
        if let Some(f) = self.rt.0.diagnostic.borrow_mut().active.last_mut() {
            f.phase = EvaluationPhase::Cleaning;
        }
        Ok(Step::Execute(key, f.kind, f.caps, f.pure))
    }
    fn pop(&self, g: &mut Graph, frames: &mut Vec<Frame>) {
        let f = frames.pop().unwrap();
        if let Ok(h) = header_mut(g, f.key) {
            h.phase = EvaluationPhase::Idle;
            if self.rt.is_poisoned() {
                h.state = State::Dirty;
                let c = g.cold[f.key.index as usize].as_mut().unwrap();
                c.failure = self.rt.first_failure();
                c.reads.clear();
                c.tracking_prefix = true;
            }
        }
        self.rt
            .0
            .depth
            .set(self.context.depth + frames.len() - self.base);
        #[cfg(feature = "diagnostics")]
        {
            self.rt.0.diagnostic.borrow_mut().active.pop();
        }
    }
    fn tick<const TOPOLOGICAL: bool>(&mut self) -> Result<Step> {
        let rt = self.rt;
        rt.graph(|g| {
            let mut frames = rt.0.validation_frames.borrow_mut();
            loop {
                if frames.len() == self.base {
                    return Ok(Step::Finished);
                }
                let step = self.step(g, &mut frames)?;
                if !TOPOLOGICAL {
                    return Ok(step);
                }
                match step {
                    Step::Descend(key) => {
                        self.push(g, &mut frames, key)?;
                    }
                    Step::Done => self.pop(g, &mut frames),
                    ready => return Ok(ready),
                }
            }
        })
    }
    fn run<const TOPOLOGICAL: bool>(&mut self, key: Key) -> Result<()> {
        let rt = self.rt;
        if !rt.graph(|g| self.push(g, &mut rt.0.validation_frames.borrow_mut(), key))? {
            return Ok(());
        }
        loop {
            match self.tick::<TOPOLOGICAL>()? {
                Step::Finished => return Ok(()),
                Step::Descend(key) => {
                    rt.graph(|g| self.push(g, &mut rt.0.validation_frames.borrow_mut(), key))?;
                }
                Step::Done => rt.graph(|g| self.pop(g, &mut rt.0.validation_frames.borrow_mut())),
                Step::Execute(key, kind, caps, pure) => {
                    {
                        let _restore = Restore(rt, Context::capture(rt));
                        CAPS.set(caps);
                        PURE.set(pure);
                        execute(rt, key, kind)?;
                    }
                    rt.graph(|g| self.pop(g, &mut rt.0.validation_frames.borrow_mut()));
                }
            }
        }
    }
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.rt.record_failure(Error::Panic);
        }
        self.rt.graph(|g| {
            let mut frames = self.rt.0.validation_frames.borrow_mut();
            while frames.len() > self.base {
                self.pop(g, &mut frames);
            }
        });
        self.context.restore(self.rt);
    }
}
fn observe(g: &mut Graph, key: Key, source: Source) -> Result<bool> {
    let revision = header(g, source.key)?.revision;
    let h = header_mut(g, key)?;
    if revision != source.revision {
        h.freshness = h.freshness.checked_add(1).ok_or(Error::Exhausted)?;
        h.state = State::Dirty;
        return Ok(true);
    }
    Ok(false)
}
pub(super) fn execute(rt: &Runtime, key: Key, kind: Kind) -> Result<()> {
    if !rt.prepare_execution(key)? {
        return Ok(());
    }
    let executable = rt.graph(|g| {
        if header(g, key).is_err() {
            return None;
        }
        header_mut(g, key).unwrap().phase = EvaluationPhase::Executing;
        #[cfg(feature = "diagnostics")]
        {
            if let Some(f) = rt.0.diagnostic.borrow_mut().active.last_mut() {
                f.phase = EvaluationPhase::Executing;
            }
            match kind {
                Kind::Effect => {
                    g.work.effect_executions = g.work.effect_executions.saturating_add(1)
                }
                _ => g.work.memo_executions = g.work.memo_executions.saturating_add(1),
            }
        }
        let c = g.cold[key.index as usize].as_ref().unwrap();
        Some((c.value, c.descriptor.execute))
    });
    if let Some((value, execute)) = executable {
        rt.0.reader.set(Some(key));
        if kind == Kind::Effect {
            rt.0.executions.set(
                rt.0.executions
                    .get()
                    .checked_add(1)
                    .ok_or(Error::Exhausted)?,
            );
        }
        // SAFETY: descriptor and allocation resolved together after cleanup,
        // with no Graph, frame or value reference held across compute.
        unsafe {
            execute(value, rt, key)?;
        }
        if rt.graph(|g| header(g, key).is_ok()) {
            rt.reconcile(key)?;
        }
    }
    Ok(())
}
