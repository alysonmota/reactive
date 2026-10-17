//! Demand-driven postorder evaluation. Only committed-source traversal is
//! flattened: dynamic reads inside user callbacks may start a nested session.
//! Frames contain metadata, never graph/value references or callback owners.
use super::*;

pub(super) struct Frame {
    key: Key,
    kind: Kind,
    cursor: usize,
    waiting: Option<Source>,
    reader: Option<Key>,
    owner: Option<Key>,
    pending_owner: Option<ownership::PendingOwner>,
    depth: usize,
    caps: u8,
    pure: bool,
    reader_runtime: Option<u64>,
}
struct Session<'a> {
    rt: &'a Runtime,
    base: usize,
}
enum Action {
    Descend(Key),
    Execute(Key, Kind),
    Done,
}

pub(super) fn update(rt: &Runtime, key: Key) -> Result<()> {
    rt.check(READ)?;
    let base = rt.graph(|_| rt.0.frames.borrow().len());
    let mut session = Session { rt, base };
    let result = session.run(key);
    if let Err(error) = result {
        rt.record_failure(error);
    }
    result
}
impl Session<'_> {
    fn enter(&mut self, key: Key) -> Result<bool> {
        let rt = self.rt;
        let result = rt.graph(|g| {
            let h = header(g, key)?;
            if h.phase != EvaluationPhase::Idle {
                return Err(Error::Cycle);
            }
            if h.state == State::Clean && h.initialized {
                return Ok(false);
            }
            let depth = rt.0.depth.get();
            if depth >= 512 {
                return Err(Error::Depth);
            }
            let kind = h.kind;
            rt.0.frames.borrow_mut().push(Frame {
                key,
                kind,
                cursor: 0,
                waiting: None,
                reader: rt.0.reader.get(),
                owner: rt.0.owner.get(),
                pending_owner: rt.0.pending_owner.get(),
                depth,
                caps: CAPS.get(),
                pure: PURE.get(),
                reader_runtime: READER_RUNTIME.get(),
            });
            // Frame allocation is inside the guarded kernel: a custom allocator
            // cannot reborrow Graph during Vec growth. No user payload is owned.
            header_mut(g, key)?.phase = EvaluationPhase::Checking;
            rt.0.reader.set(None);
            rt.0.depth.set(depth + 1);
            CAPS.set(
                CAPS.get()
                    & if kind == Kind::Effect {
                        ALL & !FLUSH
                    } else {
                        READ
                    },
            );
            if kind != Kind::Effect {
                PURE.set(true);
            }
            READER_RUNTIME.set(Some(rt.0.id));
            #[cfg(feature = "diagnostics")]
            rt.0.diagnostic.borrow_mut().active.push(CycleFrame {
                node: key.into(),
                phase: EvaluationPhase::Checking,
            });
            Ok(true)
        });
        #[cfg(feature = "diagnostics")]
        if result == Err(Error::Cycle) {
            rt.capture_cycle(key);
        }
        result
    }
    fn next(&mut self) -> Result<Action> {
        let rt = self.rt;
        rt.check(READ)?;
        rt.graph(|g| {
            let mut frames = rt.0.frames.borrow_mut();
            let frame = frames.last_mut().unwrap();
            let key = frame.key;
            if header(g, key).is_err() {
                return Ok(Action::Done);
            }
            if let Some(source) = frame.waiting.take() {
                if observe(g, key, source)? {
                    frame.cursor = usize::MAX;
                } else {
                    frame.cursor += 1;
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
                    .get(frame.cursor)
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
                    frame.cursor += 1;
                } else {
                    frame.waiting = Some(source);
                    return Ok(Action::Descend(source.key));
                }
            }
            let h = header_mut(g, key)?;
            if h.state != State::Dirty && h.initialized {
                h.state = State::Clean;
                return Ok(Action::Done);
            }
            let c = g.cold[key.index as usize].as_mut().unwrap();
            c.scope_epoch = c.scope_epoch.checked_add(1).ok_or(Error::Exhausted)?;
            c.reads.clear();
            c.tracking_prefix = true;
            header_mut(g, key)?.phase = EvaluationPhase::Cleaning;
            #[cfg(feature = "diagnostics")]
            if let Some(f) = rt.0.diagnostic.borrow_mut().active.last_mut() {
                f.phase = EvaluationPhase::Cleaning;
            }
            Ok(Action::Execute(key, frame.kind))
        })
    }
    fn run(&mut self, key: Key) -> Result<()> {
        if !self.enter(key)? {
            return Ok(());
        }
        while self.rt.0.frames.borrow().len() > self.base {
            match self.next()? {
                Action::Descend(key) => {
                    self.enter(key)?;
                }
                Action::Done => self.pop(),
                Action::Execute(key, kind) => {
                    if self.rt.prepare_execution(key)? {
                        // Cleaning may dispose/reuse the slot. Resolve only now;
                        // no pointer, descriptor or reference crossed cleanup.
                        let executable = self.rt.graph(|g| {
                            if header(g, key).is_err() {
                                return None;
                            }
                            header_mut(g, key).unwrap().phase = EvaluationPhase::Executing;
                            #[cfg(feature = "diagnostics")]
                            {
                                if let Some(f) = self.rt.0.diagnostic.borrow_mut().active.last_mut()
                                {
                                    f.phase = EvaluationPhase::Executing;
                                }
                                match kind {
                                    Kind::Effect => {
                                        g.work.effect_executions =
                                            g.work.effect_executions.saturating_add(1)
                                    }
                                    _ => {
                                        g.work.memo_executions =
                                            g.work.memo_executions.saturating_add(1)
                                    }
                                }
                            }
                            let c = g.cold[key.index as usize].as_ref().unwrap();
                            Some((c.value, c.descriptor.execute))
                        });
                        if let Some((value, execute)) = executable {
                            self.rt.0.reader.set(Some(key));
                            if kind == Kind::Effect {
                                self.rt.0.executions.set(
                                    self.rt
                                        .0
                                        .executions
                                        .get()
                                        .checked_add(1)
                                        .ok_or(Error::Exhausted)?,
                                );
                            }
                            // SAFETY: descriptor/type/allocation pairing resolved
                            // after cleanup. No internal reference crosses compute.
                            unsafe {
                                execute(value, self.rt, key)?;
                            }
                            if self.rt.graph(|g| header(g, key).is_ok()) {
                                self.rt.reconcile(key)?;
                            }
                        }
                    }
                    self.pop();
                }
            }
        }
        Ok(())
    }
    fn pop(&mut self) {
        let rt = self.rt;
        let failure = if rt.is_poisoned() {
            rt.first_failure()
        } else {
            None
        };
        rt.graph(|g| {
            let f = rt.0.frames.borrow_mut().pop().unwrap();
            CAPS.set(f.caps);
            PURE.set(f.pure);
            READER_RUNTIME.set(f.reader_runtime);
            rt.0.reader.set(f.reader);
            rt.0.owner.set(f.owner);
            rt.0.pending_owner.set(f.pending_owner);
            rt.0.depth.set(f.depth);
            if let Ok(h) = header_mut(g, f.key) {
                h.phase = EvaluationPhase::Idle;
                if failure.is_some() {
                    h.state = State::Dirty;
                    let c = g.cold[f.key.index as usize].as_mut().unwrap();
                    c.failure = failure;
                    c.reads.clear();
                    c.tracking_prefix = true;
                }
            }
        });
        #[cfg(feature = "diagnostics")]
        {
            rt.0.diagnostic.borrow_mut().active.pop();
        }
    }
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.rt.record_failure(Error::Panic);
        }
        while self.rt.0.frames.borrow().len() > self.base {
            self.pop();
        }
        // Nested sessions share the one scalar-only arena in strict LIFO order.
        // Capacity remains available to both outer and nested future updates.
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
