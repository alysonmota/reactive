//! Experimental compact frames and fused callback-free kernels. The default
//! FullFrames evaluator remains an independent control in evaluation.rs.
use super::*;

pub(super) struct Frame {
    key: Key,
    cursor: usize,
    verified: u64,
    kind: Kind,
    caps: u8,
    pure: bool,
    waiting: bool,
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
    fn validation(self, rt: &Runtime) {
        rt.0.reader.set(None);
        CAPS.set(self.caps & READ);
        PURE.set(true);
        READER_RUNTIME.set(Some(rt.0.id));
    }
}
struct Session<'a> {
    rt: &'a Runtime,
    base: usize,
    context: Context,
    cycle: Option<Key>,
    epoch: u64,
    finished: bool,
}
enum Action {
    Execute(Key, Kind, Option<Executable>),
    Finished,
}
struct Executable {
    value: NonNull<()>,
    execute: unsafe fn(NonNull<()>, &Runtime, Key) -> Result<bool>,
}
pub(super) fn update(rt: &Runtime, key: Key) -> Result<()> {
    rt.check(READ)?;
    let base = rt.graph(|_| rt.0.fused_frames.borrow().len());
    let mut session = Session {
        rt,
        base,
        context: Context::capture(rt),
        cycle: None,
        epoch: 0,
        finished: false,
    };
    session.context.validation(rt);
    let result = session.run(key);
    // Only successful run establishes that this session's arena suffix is
    // empty. Err/unwind must retain checked recovery, even after slot reuse.
    session.finished = result.is_ok();
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
    fn push(
        &mut self,
        g: &mut Graph,
        frames: &mut Vec<Frame>,
        key: Key,
        known: bool,
    ) -> Result<bool> {
        // known is supplied ONLY by a source gate in this same kernel, after
        // resolving the full generational Key. No user callback intervenes.
        let h = if known {
            &g.hot[key.index as usize]
        } else {
            header(g, key)?
        };
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
        // Sources cannot be effects, so descendants are pure READ frames.
        let (caps, pure) = (self.context.caps, self.context.pure);
        frames.push(Frame {
            key,
            cursor: 0,
            verified: self.epoch,
            kind,
            caps: caps
                & if kind == Kind::Effect {
                    ALL & !FLUSH
                } else {
                    READ
                },
            pure: pure || kind != Kind::Effect,
            waiting: false,
        });
        g.hot[key.index as usize].phase = EvaluationPhase::Checking;
        #[cfg(feature = "diagnostics")]
        self.rt.0.diagnostic.borrow_mut().active.push(CycleFrame {
            node: key.into(),
            phase: EvaluationPhase::Checking,
        });
        Ok(true)
    }
    fn next(&mut self, g: &mut Graph, frames: &mut Vec<Frame>) -> Result<Action> {
        'walk: loop {
            if frames.len() == self.base {
                return Ok(Action::Finished);
            }
            let frame_count = frames.len();
            let f = frames.last_mut().unwrap();
            let key = f.key;
            // A successful execute invalidates EVERY suspended certificate by one
            // scalar increment. Revalidate lazily on return, not by scanning frames.
            if f.verified != self.epoch {
                if header(g, key).is_err() {
                    self.pop(g, frames, true);
                    continue 'walk;
                }
                f.verified = self.epoch;
            }
            if std::mem::take(&mut f.waiting) {
                // A Checking reader cannot reconcile its committed source vector.
                // Disposal tombstones the same ordinal, preserving Key/revision;
                // backlink repair changes only observer_index. Re-read, never keep
                // Source or a slice/reference across callbacks. Resolve generation
                // even for a tombstone here, matching the checked control's error.
                let source = *g.cold[key.index as usize]
                    .as_ref()
                    .unwrap()
                    .sources
                    .get(f.cursor)
                    .ok_or(Error::Invariant)?;
                if !observe_verified(g, key, source)? {
                    f.cursor += 1;
                }
            }
            loop {
                let h = &g.hot[key.index as usize];
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
                    g.hot[key.index as usize].state = State::Dirty;
                    break;
                }
                let h = header(g, source.key)?;
                if h.kind == Kind::Effect {
                    return Err(Error::TypeMismatch);
                }
                if h.phase == EvaluationPhase::Idle && h.state == State::Clean && h.initialized {
                    let revision = h.revision;
                    if changed(g, key, revision, source.revision)? {
                        break;
                    }
                    f.cursor += 1;
                } else {
                    f.waiting = true;
                    self.push(g, frames, source.key, true)?;
                    continue 'walk;
                }
            }
            let h = &mut g.hot[key.index as usize];
            if h.state != State::Dirty && h.initialized {
                h.state = State::Clean;
                self.pop(g, frames, true);
                continue 'walk;
            }
            let c = g.cold[key.index as usize].as_mut().unwrap();
            c.scope_epoch = c.scope_epoch.checked_add(1).ok_or(Error::Exhausted)?;
            c.reads.clear();
            c.tracking_prefix = true;
            g.hot[key.index as usize].phase = EvaluationPhase::Cleaning;
            #[cfg(feature = "diagnostics")]
            if let Some(f) = self.rt.0.diagnostic.borrow_mut().active.last_mut() {
                f.phase = EvaluationPhase::Cleaning;
            }
            // Match the established execution context BEFORE preparing a scope,
            // including allocations on first logical registration. No callback or
            // unchecked Key crosses the transition out of this graph kernel.
            // Publish logical depth before prepare can reach physical cleanup.
            // Every push still checks the logical limit, including depth 512.
            self.rt
                .0
                .depth
                .set(self.context.depth + frame_count - self.base);
            CAPS.set(f.caps);
            PURE.set(f.pure);
            let prepared = self.rt.prepare_logical_execution(g, key)?;
            let executable = if prepared {
                self.resolve_executable(g, key, f.kind, true)
            } else {
                None
            };
            return Ok(Action::Execute(key, f.kind, executable));
        }
    }
    fn pop(&self, g: &mut Graph, frames: &mut Vec<Frame>, trust: bool) {
        let f = frames.pop().unwrap();
        // Err/unwind and immediate callback return ALWAYS take the checked
        // path. execute may have failed before incrementing the local epoch.
        if (trust && f.verified == self.epoch) || header(g, f.key).is_ok() {
            let h = &mut g.hot[f.key.index as usize];
            h.phase = EvaluationPhase::Idle;
            if self.rt.is_poisoned() {
                h.state = State::Dirty;
                let c = g.cold[f.key.index as usize].as_mut().unwrap();
                c.failure = self.rt.first_failure();
                c.reads.clear();
                c.tracking_prefix = true;
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            self.rt.0.diagnostic.borrow_mut().active.pop();
        }
    }
    fn tick(&mut self, completed: bool) -> Result<Action> {
        self.rt.graph(|g| {
            let mut frames = self.rt.0.fused_frames.borrow_mut();
            if completed {
                self.context.validation(self.rt);
                self.pop(g, &mut frames, false);
            }
            // No status/caps mutation within this callback-free walk. Empty
            // completion must succeed even after shutdown inside an effect.
            if frames.len() != self.base {
                self.rt.check(READ)?;
            }
            self.next(g, &mut frames)
        })
    }
    fn run(&mut self, key: Key) -> Result<()> {
        if !self
            .rt
            .graph(|g| self.push(g, &mut self.rt.0.fused_frames.borrow_mut(), key, false))?
        {
            return Ok(());
        }
        let mut completed = false;
        loop {
            match self.tick(completed)? {
                Action::Finished => return Ok(()),
                Action::Execute(key, kind, prepared) => {
                    self.execute(key, kind, prepared)?;
                    self.epoch = self.epoch.checked_add(1).ok_or(Error::Exhausted)?;
                    completed = true;
                }
            }
        }
    }
    // FullFrames' established execution protocol: no per-callback Context
    // capture/Restore and no additional interpreter beneath the fused tick.
    fn execute(&self, key: Key, kind: Kind, prepared: Option<Executable>) -> Result<()> {
        let rt = self.rt;
        let executable = match prepared {
            Some(executable) => Some(executable),
            None => {
                if !rt.prepare_execution(key)? {
                    return Ok(());
                }
                rt.graph(|g| self.resolve_executable(g, key, kind, false))
            }
        };
        if let Some(Executable { value, execute }) = executable {
            rt.0.reader.set(Some(key));
            if kind == Kind::Effect {
                rt.0.executions.set(
                    rt.0.executions
                        .get()
                        .checked_add(1)
                        .ok_or(Error::Exhausted)?,
                );
            }
            // SAFETY: descriptor/type/allocation resolved after cleanup. No
            // graph/frame/value reference is retained across user code.
            unsafe {
                execute(value, rt, key)?;
            }
            if rt.graph(|g| header(g, key).is_ok()) {
                rt.reconcile(key)?;
            }
        }
        Ok(())
    }
    // known only crosses callback-free logical preparation; physical cleanup
    // always takes full resolution after its callbacks have finished.
    fn resolve_executable(
        &self,
        g: &mut Graph,
        key: Key,
        kind: Kind,
        known: bool,
    ) -> Option<Executable> {
        if !known && header(g, key).is_err() {
            return None;
        }
        g.hot[key.index as usize].phase = EvaluationPhase::Executing;
        #[cfg(feature = "diagnostics")]
        {
            if let Some(f) = self.rt.0.diagnostic.borrow_mut().active.last_mut() {
                f.phase = EvaluationPhase::Executing;
            }
            match kind {
                Kind::Effect => {
                    g.work.effect_executions = g.work.effect_executions.saturating_add(1)
                }
                _ => g.work.memo_executions = g.work.memo_executions.saturating_add(1),
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = kind;
        let c = g.cold[key.index as usize].as_ref().unwrap();
        Some(Executable {
            value: c.value,
            execute: c.descriptor.execute,
        })
    }
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        #[cfg(panic = "unwind")]
        if std::thread::panicking() {
            self.rt.record_failure(Error::Panic);
        }
        if !self.finished {
            self.rt.graph(|g| {
                let mut frames = self.rt.0.fused_frames.borrow_mut();
                while frames.len() > self.base {
                    self.pop(g, &mut frames, false);
                }
            });
        }
        self.context.restore(self.rt);
    }
}
fn observe_verified(g: &mut Graph, key: Key, source: Source) -> Result<bool> {
    let revision = header(g, source.key)?.revision;
    changed(g, key, revision, source.revision)
}
// The reader's Key was validated in this kernel (or has a current session
// certificate). Source generations are NEVER elided by this helper.
fn changed(g: &mut Graph, key: Key, revision: u64, previous: u64) -> Result<bool> {
    let h = &mut g.hot[key.index as usize];
    if revision != previous {
        h.freshness = h.freshness.checked_add(1).ok_or(Error::Exhausted)?;
        h.state = State::Dirty;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
