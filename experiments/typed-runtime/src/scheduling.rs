use super::*;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct FlushOptions {
    pub max_rounds: usize,
    pub max_work: Option<usize>,
    pub deadline: Option<Instant>,
}
impl Default for FlushOptions {
    fn default() -> Self {
        Self {
            max_rounds: 100,
            max_work: None,
            deadline: None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    Stabilized,
    Yielded,
    RoundLimit,
    Failed,
}
#[derive(Debug)]
pub struct FlushReport {
    pub outcome: FlushOutcome,
    pub rounds: usize,
    pub callbacks: usize,
    pub processed_effects: usize,
    pub has_pending_effects: bool,
    pub failure: Option<Error>,
}
pub(super) fn enqueue(g: &mut Graph, key: Key) {
    let Ok(h) = header_mut(g, key) else {
        return;
    };
    if h.kind == Kind::Effect && h.queue == 0 {
        h.queue = 1;
        g.pending.push(key);
        g.pending_live += 1;
        #[cfg(feature = "diagnostics")]
        {
            g.work.tickets_enqueued = g.work.tickets_enqueued.saturating_add(1);
        }
    }
}
pub(super) fn cancel(g: &mut Graph, key: Key) {
    // Teardown may already have marked life false, but no generation has been
    // changed yet. Queue ownership is still valid for this original slot.
    let h = &mut g.hot[key.index as usize];
    let was_pending = h.queue == 1;
    match h.queue {
        1 => g.pending_live -= 1,
        2 => g.current_live -= 1,
        _ => (),
    }
    h.queue = 0;
    if was_pending {
        maintain_pending(g);
    }
}
fn maintain_pending(g: &mut Graph) {
    if g.pending_live == 0 {
        g.pending.clear();
        if g.pending.capacity() > 64 {
            g.pending.shrink_to(64);
        }
    } else if g.pending.len().saturating_sub(g.pending_live) >= 64
        && g.pending.len() / 2 >= g.pending_live
    {
        let mut pending = std::mem::take(&mut g.pending);
        pending.retain(|&k| header(g, k).is_ok_and(|h| h.queue == 1));
        g.pending = pending;
    }
}
struct Batch<'a>(&'a Cell<usize>, usize);
impl Drop for Batch<'_> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}
struct Flush<'a>(&'a Runtime);
impl Drop for Flush<'_> {
    fn drop(&mut self) {
        self.0.0.flushing.set(false);
        // Failure/panic: clear current tickets without dropping user resources.
        // Dirty nodes are not made Clean, and poison prevents another flush.
        self.0.graph(|g| {
            let current = std::mem::take(&mut g.current);
            for &key in &current {
                if header(g, key).is_ok_and(|h| h.queue == 2) {
                    cancel(g, key);
                    enqueue(g, key);
                }
            }
            g.current = current;
            g.current.clear();
        });
    }
}
impl Runtime {
    pub fn untrack<R>(&self, f: impl FnOnce(&Runtime) -> Result<R>) -> Result<R> {
        self.boundary(|| self.untrack_inner(f))
    }
    fn untrack_inner<R>(&self, f: impl FnOnce(&Runtime) -> Result<R>) -> Result<R> {
        let mut f = Protected(self, Some(f));
        self.check(READ)?;
        struct Restore<'a>(&'a Cell<Option<Key>>, Option<Key>);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }
        let _restore = Restore(&self.0.reader, self.0.reader.replace(None));
        let _reader = ReaderRuntime::enter(None);
        self.user(|| f.1.take().unwrap()(self))
    }
    pub fn batch<R>(&self, f: impl FnOnce(&Runtime) -> Result<R>) -> Result<R> {
        self.boundary(|| self.batch_inner(f))
    }
    fn batch_inner<R>(&self, f: impl FnOnce(&Runtime) -> Result<R>) -> Result<R> {
        let mut f = Protected(self, Some(f));
        self.check(READ)?;
        let old = self.0.batch.get();
        let next = old.checked_add(1).ok_or_else(|| self.exhausted())?;
        self.0.batch.set(next);
        let _batch = Batch(&self.0.batch, old);
        self.user(|| f.1.take().unwrap()(self))
    }
    pub fn has_pending_effects(&self) -> bool {
        self.graph(|g| g.pending_live != 0 || g.current_live != 0)
    }
    pub fn flush_with(&self, options: FlushOptions) -> Result<FlushReport> {
        self.boundary(|| self.flush_inner(options))
    }
    fn flush_inner(&self, options: FlushOptions) -> Result<FlushReport> {
        self.check(FLUSH)?;
        if self.0.depth.get() != 0
            || self.0.batch.get() != 0
            || self.0.disposal.get() != 0
            || self.0.flushing.get()
        {
            return Err(capability_error(FLUSH));
        }
        self.0.notification_outstanding.set(false);
        self.0.flushing.set(true);
        let _flush = Flush(self);
        let mut report = FlushReport {
            outcome: FlushOutcome::Stabilized,
            rounds: 0,
            callbacks: 0,
            processed_effects: 0,
            has_pending_effects: false,
            failure: None,
        };
        while self.has_pending_effects() {
            if report.rounds >= options.max_rounds {
                report.outcome = FlushOutcome::RoundLimit;
                break;
            }
            if options
                .max_work
                .is_some_and(|n| report.processed_effects >= n)
                || options.deadline.is_some_and(|d| Instant::now() >= d)
            {
                report.outcome = FlushOutcome::Yielded;
                break;
            }
            let opened = self.graph(|g| {
                g.round = g.round.checked_add(1).ok_or(Error::Exhausted)?;
                #[cfg(feature = "diagnostics")]
                {
                    g.work.rounds = g.work.rounds.saturating_add(1);
                }
                let mut pending = std::mem::take(&mut g.pending);
                pending.retain(|&key| header(g, key).is_ok_and(|h| h.queue == 1));
                std::mem::swap(&mut pending, &mut g.current);
                g.pending = pending;
                g.current_live = g.pending_live;
                g.pending_live = 0;
                for &key in &g.current {
                    g.hot[key.index as usize].queue = 2;
                }
                let mut current = std::mem::take(&mut g.current);
                current.sort_unstable_by_key(|key| {
                    let h = &g.hot[key.index as usize];
                    let c = g.cold[key.index as usize].as_ref().unwrap();
                    (ownership::owner(g, c.owner).unwrap().depth, h.sequence)
                });
                g.current = current;
                Ok(g.current.len())
            });
            let count = match opened {
                Ok(n) => n,
                Err(e) => {
                    report.failure = Some(e);
                    break;
                }
            };
            report.rounds += 1;
            for i in 0..count {
                let key = self.graph(|g| g.current[i]);
                if !self.graph(|g| header(g, key).is_ok_and(|h| h.queue == 2)) {
                    continue;
                }
                report.processed_effects += 1;
                #[cfg(feature = "diagnostics")]
                self.count(|w| w.tickets_processed = w.tickets_processed.saturating_add(1));
                // Callback may dispose itself; count executions independently
                // of payload life by the return value of the execution counter.
                let before = self.0.executions.get();
                let result = self.update(key);
                report.callbacks += self.0.executions.get() - before;
                if let Err(e) = result {
                    report.failure = Some(e);
                    break;
                }
                self.graph(|g| {
                    if header(g, key).is_ok() {
                        cancel(g, key);
                        if header(g, key).unwrap().state != State::Clean {
                            enqueue(g, key);
                        }
                    }
                });
            }
            if report.failure.is_some() {
                break;
            }
            self.graph(|g| {
                g.current.clear();
                assert_eq!(g.current_live, 0);
            });
        }
        if report.failure.is_some() {
            self.record_failure(report.failure.unwrap());
            report.failure = self.first_failure().as_deref().copied();
            report.outcome = FlushOutcome::Failed;
        }
        drop(_flush);
        report.has_pending_effects = self.has_pending_effects();
        Ok(report)
    }
}
