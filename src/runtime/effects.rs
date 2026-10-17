use std::{rc::Rc, time::Instant};

use super::{Runtime, RuntimeStatus, Storage};
use crate::{
    EffectHandle, EvaluationPhase, QueueMembership, RuntimeError, State,
    arena::SlotId,
    context::{ContextGuard, Operation},
    node::{NodeKind, Payload},
};

/// Budgets are cooperative: an opened round always finishes. Work counts live
/// tickets processed, including equality cuts, not only callbacks. No clock is
/// consulted by reactive reads or graph traversal.
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

#[derive(Clone, Debug)]
pub struct FlushReport {
    pub outcome: FlushOutcome,
    pub rounds: usize,
    pub callbacks: usize,
    pub processed_effects: usize,
    pub has_pending_effects: bool,
    pub failure: Option<Rc<RuntimeError>>,
}

#[derive(Default)]
pub(super) struct EffectQueue {
    pending: Vec<SlotId>,
    current: Vec<SlotId>,
    pending_live: usize,
    current_live: usize,
    round: u64,
    #[cfg(any(test, feature = "verification"))]
    executed: std::collections::HashSet<SlotId>,
}

#[cfg(feature = "diagnostics")]
impl EffectQueue {
    pub(super) fn capacity_bytes(&self) -> usize {
        (self.pending.capacity() + self.current.capacity()) * std::mem::size_of::<SlotId>()
    }
    pub(super) fn diagnostic_sizes(&self) -> (usize, usize, usize, usize) {
        (
            self.pending_live,
            self.current_live,
            self.pending.len(),
            self.current.len(),
        )
    }
}

// Measured with queue churn and large-empty-queue cases; retain the conservative
// 64-dead/50% compaction and 4096-empty-slot limits. See benches/.log/RESULTS.md.
const COMPACT_MIN: usize = 64;
const RETAIN_EMPTY_CAPACITY: usize = 4096;

#[cfg(any(test, feature = "verification"))]
impl EffectQueue {
    pub(super) fn verify(&self, storage: &Storage) -> Result<(), RuntimeError> {
        let mut pending = std::collections::HashSet::new();
        let mut current = std::collections::HashSet::new();
        for &id in &self.pending {
            if storage
                .nodes
                .get(id)
                .is_some_and(|node| node.metadata.queue == QueueMembership::Pending)
                && !pending.insert(id)
            {
                return Err(RuntimeError::InvariantViolation("duplicate pending ticket"));
            }
        }
        for &id in &self.current {
            if storage
                .nodes
                .get(id)
                .is_some_and(|node| node.metadata.queue == QueueMembership::Current(self.round))
                && !current.insert(id)
            {
                return Err(RuntimeError::InvariantViolation("duplicate current ticket"));
            }
        }
        if pending.len() != self.pending_live || current.len() != self.current_live {
            return Err(RuntimeError::InvariantViolation("live ticket counts"));
        }
        for (id, node) in storage.nodes.iter() {
            if node
                .metadata
                .last_processed_round
                .is_some_and(|round| round > self.round)
                || node
                    .metadata
                    .last_executed_round
                    .is_some_and(|round| round > self.round)
                || (node.metadata.last_executed_round == Some(self.round)
                    && !self.executed.contains(&id))
            {
                return Err(RuntimeError::InvariantViolation("effect round history"));
            }
            let present = match node.metadata.queue {
                QueueMembership::None => true,
                QueueMembership::Pending => pending.contains(&id),
                QueueMembership::Current(round) => round == self.round && current.contains(&id),
            };
            if !present {
                return Err(RuntimeError::InvariantViolation(
                    "logical ticket missing from buffer",
                ));
            }
        }
        Ok(())
    }
}

struct FlushGuard<'a> {
    runtime: &'a Runtime,
    _context: ContextGuard<'a>,
}

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        let mut storage = self.runtime.inner.storage.borrow_mut();
        // On error/panic, unprocessed Current tickets become Pending. No arena
        // scan or callback; normal completion has already consumed all tickets.
        let count = storage.effects.current.len();
        for index in 0..count {
            let id = storage.effects.current[index];
            if storage.nodes.get(id).is_some_and(|node| {
                node.metadata.queue == QueueMembership::Current(storage.effects.round)
            }) {
                Runtime::cancel_effect_ticket(&mut storage, id);
                if let Err(error) = Runtime::enqueue_effect(&mut storage, id) {
                    self.runtime.record_failure(error);
                }
            }
        }
        storage.effects.current.clear();
        Runtime::maintain_pending(&mut storage);
        release_empty_buffer(&mut storage.effects.current);
    }
}

fn release_empty_buffer(buffer: &mut Vec<SlotId>) {
    if buffer.is_empty() && buffer.capacity() > RETAIN_EMPTY_CAPACITY {
        *buffer = Vec::new();
    }
}

impl Runtime {
    /// Creates an effect in the explicit current owner and schedules its first
    /// callback. Construction and writes never execute effects synchronously.
    pub fn effect(
        &self,
        callback: impl FnMut(&Runtime) -> Result<(), RuntimeError> + 'static,
    ) -> Result<EffectHandle, RuntimeError> {
        self.resource_boundary(
            Some(callback),
            |callback| {
                self.check(Operation::Create)?;
                let owner = self
                    .inner
                    .context
                    .snapshot()
                    .owner
                    .ok_or(RuntimeError::MissingOwner)?;
                if self.inner.storage.borrow().owners.get(owner).is_none() {
                    return Err(RuntimeError::OwnerDisposed);
                }
                let id = self.insert_node(Payload::effect(Box::new(
                    callback.take().expect("callback"),
                )))?;
                let result = Self::enqueue_effect(&mut self.inner.storage.borrow_mut(), id.slot);
                if let Err(error) = &result {
                    self.record_failure(error.clone());
                }
                result.map(|()| EffectHandle::new(id))
            },
            |callback| self.release_one(callback),
        )
    }

    pub fn effect_infallible(
        &self,
        mut callback: impl FnMut(&Runtime) + 'static,
    ) -> Result<EffectHandle, RuntimeError> {
        self.effect(move |runtime| {
            callback(runtime);
            Ok(())
        })
    }

    /// Constant-time count of logical live tickets, including the current round.
    /// Diagnostics remain available after poison or shutdown.
    pub fn has_pending_effects(&self) -> bool {
        let storage = self.inner.storage.borrow();
        storage.effects.pending_live != 0 || storage.effects.current_live != 0
    }

    pub(super) fn enqueue_effect(storage: &mut Storage, id: SlotId) -> Result<(), RuntimeError> {
        let Some(node) = storage.nodes.get_mut(id) else {
            return Ok(());
        };
        if node.payload.kind() != NodeKind::Effect || node.metadata.queue != QueueMembership::None {
            return Ok(());
        }
        let count = storage
            .effects
            .pending_live
            .checked_add(1)
            .ok_or(RuntimeError::CounterExhausted)?;
        storage.effects.pending.push(id);
        storage.effects.pending_live = count;
        node.metadata.queue = QueueMembership::Pending;
        #[cfg(feature = "diagnostics")]
        {
            storage.diagnostics.counters.tickets_enqueued = storage
                .diagnostics
                .counters
                .tickets_enqueued
                .saturating_add(1);
        }
        Ok(())
    }

    pub(super) fn cancel_effect_ticket(storage: &mut Storage, id: SlotId) {
        let Some(node) = storage.nodes.get_mut_teardown(id) else {
            return;
        };
        match node.metadata.queue {
            QueueMembership::None => (),
            QueueMembership::Pending => {
                storage.effects.pending_live = storage
                    .effects
                    .pending_live
                    .checked_sub(1)
                    .expect("pending ticket count")
            }
            QueueMembership::Current(_) => {
                storage.effects.current_live = storage
                    .effects
                    .current_live
                    .checked_sub(1)
                    .expect("current ticket count")
            }
        }
        node.metadata.queue = QueueMembership::None;
    }

    pub(super) fn maintain_pending(storage: &mut Storage) {
        let queue = &mut storage.effects;
        if queue.pending_live == 0 {
            queue.pending.clear();
            release_empty_buffer(&mut queue.pending);
        } else if queue.pending.len().saturating_sub(queue.pending_live) >= COMPACT_MIN
            && queue.pending.len() / 2 >= queue.pending_live
        {
            queue.pending.retain(|id| {
                storage
                    .nodes
                    .get(*id)
                    .is_some_and(|node| node.metadata.queue == QueueMembership::Pending)
            });
        }
    }

    fn open_round(storage: &mut Storage) -> Result<u64, RuntimeError> {
        let round = storage
            .effects
            .round
            .checked_add(1)
            .ok_or(RuntimeError::CounterExhausted)?;
        let Storage {
            nodes,
            owners,
            effects: queue,
            ..
        } = storage;
        queue.pending.retain(|id| {
            nodes
                .get(*id)
                .is_some_and(|node| node.metadata.queue == QueueMembership::Pending)
        });
        queue.current.clear();
        std::mem::swap(&mut queue.current, &mut queue.pending);
        queue.current_live = queue.pending_live;
        queue.pending_live = 0;
        queue.round = round;
        #[cfg(any(test, feature = "verification"))]
        queue.executed.clear();
        for &id in &queue.current {
            nodes
                .get_mut(id)
                .expect("live pending ticket")
                .metadata
                .queue = QueueMembership::Current(round);
        }
        queue.current.sort_unstable_by_key(|id| {
            let node = nodes.get(*id).expect("live current ticket");
            (
                owners.get(node.owner).expect("live node owner").depth,
                node.metadata.creation_sequence,
            )
        });
        #[cfg(feature = "diagnostics")]
        {
            storage.diagnostics.counters.rounds =
                storage.diagnostics.counters.rounds.saturating_add(1);
        }
        Ok(round)
    }

    /// Runs whole snapshots until stabilization or a cooperative limit. Invalid
    /// entry/reentry returns Err; evaluation failures produce a Failed report
    /// and poison. User panics propagate after restoring guards and queues.
    pub fn flush(&self, options: FlushOptions) -> Result<FlushReport, RuntimeError> {
        self.resource_boundary((), |_| self.flush_inner(options), |_| None)
    }

    fn flush_inner(&self, options: FlushOptions) -> Result<FlushReport, RuntimeError> {
        self.check(Operation::Flush)?;
        let context = self.inner.context.snapshot();
        if context.flushing
            || context.evaluation_depth != 0
            || context.batch_depth != 0
            || context.disposal_depth != 0
        {
            return Err(RuntimeError::Reentrancy);
        }
        self.inner.notification_outstanding.set(false);
        let guard = FlushGuard {
            runtime: self,
            _context: self
                .inner
                .context
                .enter(|snapshot| snapshot.flushing = true),
        };
        let mut report = FlushReport {
            outcome: FlushOutcome::Stabilized,
            rounds: 0,
            callbacks: 0,
            processed_effects: 0,
            has_pending_effects: false,
            failure: None,
        };
        loop {
            if !self.has_pending_effects() {
                break;
            }
            if report.rounds >= options.max_rounds {
                report.outcome = FlushOutcome::RoundLimit;
                break;
            }
            if options
                .max_work
                .is_some_and(|limit| report.processed_effects >= limit)
                || options
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                report.outcome = FlushOutcome::Yielded;
                break;
            }
            let round = Self::open_round(&mut self.inner.storage.borrow_mut());
            let round = match round {
                Ok(round) => round,
                Err(error) => {
                    self.record_failure(error);
                    report.outcome = FlushOutcome::Failed;
                    break;
                }
            };
            report.rounds += 1;
            let count = self.inner.storage.borrow().effects.current.len();
            let mut failure = None;
            for index in 0..count {
                let id = self.inner.storage.borrow().effects.current[index];
                if !self
                    .inner
                    .storage
                    .borrow()
                    .nodes
                    .get(id)
                    .is_some_and(|node| node.metadata.queue == QueueMembership::Current(round))
                {
                    continue;
                }
                let work = report
                    .processed_effects
                    .checked_add(1)
                    .ok_or(RuntimeError::CounterExhausted);
                let result = work.and_then(|work| {
                    report.processed_effects = work;
                    #[cfg(feature = "diagnostics")]
                    {
                        let mut storage = self.inner.storage.borrow_mut();
                        storage.diagnostics.counters.tickets_processed = storage
                            .diagnostics
                            .counters
                            .tickets_processed
                            .saturating_add(1);
                    }
                    self.process_effect(id, round, &mut report.callbacks)
                });
                if let Err(error) = result {
                    failure = Some(error);
                    break;
                }
            }
            if let Some(error) = failure {
                self.record_failure(error);
                report.outcome = FlushOutcome::Failed;
                break;
            }
            let mut storage = self.inner.storage.borrow_mut();
            storage.effects.current.clear();
            debug_assert_eq!(storage.effects.current_live, 0);
            Self::maintain_pending(&mut storage);
        }
        drop(guard);
        report.has_pending_effects = self.has_pending_effects();
        if report.outcome == FlushOutcome::Failed {
            report.failure = self.first_failure();
        }
        Ok(report)
    }

    fn process_effect(
        &self,
        id: SlotId,
        round: u64,
        callbacks: &mut usize,
    ) -> Result<(), RuntimeError> {
        let mut guard = self.enter_evaluation(self.handle_id(id), EvaluationPhase::Checking)?;
        self.inner
            .storage
            .borrow_mut()
            .nodes
            .get_mut(id)
            .expect("live ticket")
            .metadata
            .last_processed_round = Some(round);
        if self.check_computation_sources(id)? {
            guard.set_phase(EvaluationPhase::Cleaning)?;
            if !self.prepare_execution(id)? {
                return Ok(());
            }
            guard.set_phase(EvaluationPhase::Executing)?;
            let next_callbacks = callbacks
                .checked_add(1)
                .ok_or(RuntimeError::CounterExhausted)?;
            let callback = {
                let mut storage = self.inner.storage.borrow_mut();
                #[cfg(any(test, feature = "verification"))]
                if !storage.effects.executed.insert(id) {
                    return Err(RuntimeError::InvariantViolation(
                        "effect executed twice in one round",
                    ));
                }
                #[cfg(feature = "diagnostics")]
                {
                    storage.diagnostics.counters.effect_executions = storage
                        .diagnostics
                        .counters
                        .effect_executions
                        .saturating_add(1);
                }
                let node = storage
                    .nodes
                    .get_mut(id)
                    .ok_or(RuntimeError::InvalidHandle)?;
                node.metadata.last_executed_round = Some(round);
                let Payload::Effect(callback) = &mut node.payload else {
                    return Err(RuntimeError::TypeMismatch);
                };
                callback
                    .take()
                    .ok_or(RuntimeError::InvariantViolation("missing effect callback"))?
            };
            *callbacks = next_callbacks;
            self.resource_boundary(
                Some(callback),
                |callback| {
                    let result = callback.as_mut().expect("effect callback")(self);
                    if let Err(error) = &result {
                        self.record_failure(error.clone());
                    }
                    result?;
                    // A disposed reader abandons commit but does not interrupt its callback.
                    if self.inner.storage.borrow().nodes.get(id).is_none() {
                        return Ok(());
                    }
                    self.check(Operation::Read)?;
                    Self::complete_effect(&mut self.inner.storage.borrow_mut(), id)
                },
                |mut callback| {
                    let mut storage = self.inner.storage.borrow_mut();
                    if let Some(node) = storage.nodes.get_mut(id) {
                        if let Payload::Effect(stored) = &mut node.payload {
                            *stored = callback.take();
                        }
                        if self.status() == RuntimeStatus::Poisoned {
                            node.metadata.state = State::Dirty;
                            node.metadata.set_failure(self.first_failure());
                        }
                    }
                    drop(storage);
                    self.release_one(callback)
                },
            )?;
        }
        drop(guard);
        if self.status() == RuntimeStatus::Poisoned {
            return Err(RuntimeError::Poisoned);
        }
        let mut storage = self.inner.storage.borrow_mut();
        if let Some(node) = storage.nodes.get_mut(id) {
            node.metadata.last_processed_round = Some(round);
            let pending = node.metadata.state != State::Clean;
            Self::cancel_effect_ticket(&mut storage, id);
            if pending {
                Self::enqueue_effect(&mut storage, id)?;
            }
        }
        Ok(())
    }

    // O(k) revision validation/prefix recovery plus O(m - p + k) reconciliation.
    // Removing the old suffix must unlink each live source's observer entry.
    fn complete_effect(storage: &mut Storage, id: SlotId) -> Result<(), RuntimeError> {
        let count = storage
            .nodes
            .get(id)
            .ok_or(RuntimeError::InvalidHandle)?
            .reads
            .len();
        let mut state = State::Clean;
        let mut kept = 0;
        let mut prefix = 0;
        for index in 0..count {
            let reader = storage.nodes.get(id).expect("reader");
            let read = reader.reads[index];
            let matches_prefix = prefix == kept
                && reader
                    .sources
                    .get(kept)
                    .is_some_and(|edge| edge.live && edge.source == read.source);
            let Some(source) = storage.nodes.get(read.source) else {
                state = State::Dirty;
                continue;
            };
            if source.metadata.value_revision != read.value_revision {
                state = State::Dirty;
            } else if source.metadata.freshness_revision != read.freshness_revision
                || source.metadata.state != State::Clean
            {
                state = state.max(State::Check);
            }
            // Validate the usable prefix against compacted live reads in this
            // same pass. Missing sources do not consume a position; tombstones
            // stop reuse. A mismatch keeps prefix < kept on subsequent reads.
            if matches_prefix {
                prefix += 1;
            }
            if kept != index {
                storage.nodes.get_mut(id).expect("reader").reads[kept] = read;
            }
            kept += 1;
        }
        let node = storage.nodes.get_mut(id).expect("reader");
        node.reads.truncate(kept);
        node.tracking.stable_prefix_cursor = prefix;
        Self::commit_sources(storage, id);
        let node = storage.nodes.get_mut(id).expect("reader");
        node.metadata.initialized = true;
        node.metadata.state = state;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OwnerHandle, Signal};
    use std::{
        cell::{Cell, RefCell},
        time::Duration,
    };

    fn setup() -> (Runtime, OwnerHandle) {
        let runtime = Runtime::new();
        let root = runtime.create_root().unwrap();
        (runtime, root)
    }

    #[test]
    fn verifier_rejects_duplicate_missing_and_miscounted_tickets() {
        let (runtime, owner) = setup();
        let effect = runtime
            .with_owner(owner, |rt| rt.effect(|_| Ok(())))
            .unwrap();
        runtime.verify_invariants().unwrap();
        runtime
            .inner
            .storage
            .borrow_mut()
            .effects
            .pending
            .push(effect.id.slot);
        assert!(runtime.verify_invariants().is_err());
        runtime.inner.storage.borrow_mut().effects.pending.pop();
        runtime.inner.storage.borrow_mut().effects.pending_live += 1;
        assert!(runtime.verify_invariants().is_err());
        runtime.inner.storage.borrow_mut().effects.pending_live -= 1;
        runtime.inner.storage.borrow_mut().effects.pending.clear();
        assert!(runtime.verify_invariants().is_err());
        runtime
            .inner
            .storage
            .borrow_mut()
            .effects
            .pending
            .push(effect.id.slot);
        runtime.flush(Default::default()).unwrap();
        runtime.verify_invariants().unwrap();
    }

    #[test]
    fn completion_contracts_wide_sources_and_abandoned_writes_do_not_schedule() {
        let (rt, root) = setup();
        let driver = rt.with_owner(root, |rt| rt.signal(true)).unwrap();
        let inputs: Vec<_> = (0..256)
            .map(|_| rt.with_owner(root, |rt| rt.signal(1_u64)).unwrap())
            .collect();
        let captured = inputs.clone();
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        let effect = rt
            .with_owner(root, |rt| {
                rt.effect(move |rt| {
                    count.set(count.get() + 1);
                    if driver.try_get_copy(rt)? {
                        for input in &captured {
                            input.try_get_copy(rt)?;
                        }
                    }
                    Ok(())
                })
            })
            .unwrap();
        rt.flush(Default::default()).unwrap();
        assert_eq!(
            rt.inner
                .storage
                .borrow()
                .nodes
                .get(effect.id.slot)
                .unwrap()
                .sources
                .len(),
            257
        );
        driver.try_set(&rt, false).unwrap();
        assert_eq!(rt.flush(Default::default()).unwrap().callbacks, 1);
        {
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(effect.id.slot).unwrap();
            assert_eq!(node.sources.len(), 1);
            assert_eq!(node.sources[0].source, driver.id.slot);
            assert_eq!(node.tracking.stable_prefix_cursor, 1);
            for input in &inputs {
                assert!(
                    storage
                        .nodes
                        .get(input.id.slot)
                        .unwrap()
                        .observers
                        .is_empty()
                );
            }
        }
        for input in &inputs {
            input.try_set(&rt, 2).unwrap();
        }
        assert!(!rt.has_pending_effects());
        assert_eq!(calls.get(), 2);
        rt.verify_invariants().unwrap();
    }

    #[test]
    fn fused_completion_compacts_missing_reads_at_start_middle_and_end() {
        for removed in 0..3 {
            let (rt, root) = setup();
            let driver = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
            let owners: Vec<_> = (0..3).map(|_| rt.create_owner(root).unwrap()).collect();
            let inputs: Vec<_> = owners
                .iter()
                .map(|&owner| rt.with_owner(owner, |rt| rt.signal(1_u64)).unwrap())
                .collect();
            let disposed_owner = owners[removed];
            let stage = Rc::new(Cell::new(0));
            let captured = stage.clone();
            let effect = rt
                .with_owner(root, |rt| {
                    rt.effect(move |rt| {
                        driver.try_get_copy(rt)?;
                        for (index, input) in inputs.iter().enumerate() {
                            if captured.get() != 2 || index != removed {
                                input.try_get_copy(rt)?;
                            }
                        }
                        if captured.get() == 1 {
                            rt.dispose(disposed_owner)?;
                            captured.set(2);
                        }
                        Ok(())
                    })
                })
                .unwrap();
            rt.flush(Default::default()).unwrap();
            stage.set(1);
            driver.try_set(&rt, 1).unwrap();
            assert_eq!(rt.flush(Default::default()).unwrap().callbacks, 2);
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(effect.id.slot).unwrap();
            assert_eq!(node.sources.len(), 3);
            assert!(!node.source_holes);
            assert!(node.sources.iter().all(|edge| edge.live));
            assert_eq!(node.metadata.state, State::Clean);
            drop(storage);
            rt.verify_invariants().unwrap();
        }
    }

    fn effect(
        runtime: &Runtime,
        owner: OwnerHandle,
        callback: impl FnMut(&Runtime) -> Result<(), RuntimeError> + 'static,
    ) -> EffectHandle {
        runtime
            .with_owner(owner, |runtime| runtime.effect(callback))
            .unwrap()
    }

    fn signal(runtime: &Runtime, owner: OwnerHandle, value: i32) -> Signal<i32> {
        runtime
            .with_owner(owner, |runtime| runtime.signal(value))
            .unwrap()
    }

    fn flush(runtime: &Runtime) -> FlushReport {
        runtime.flush(FlushOptions::default()).unwrap()
    }

    fn check_queue(runtime: &Runtime) {
        let storage = runtime.inner.storage.borrow();
        let queue = &storage.effects;
        let mut seen = std::collections::HashSet::new();
        let pending = queue
            .pending
            .iter()
            .filter(|&&id| {
                storage
                    .nodes
                    .get(id)
                    .is_some_and(|node| node.metadata.queue == QueueMembership::Pending)
            })
            .count();
        let current = queue
            .current
            .iter()
            .filter(|&&id| {
                storage.nodes.get(id).is_some_and(|node| {
                    node.metadata.queue == QueueMembership::Current(queue.round)
                })
            })
            .count();
        assert_eq!(pending, queue.pending_live);
        assert_eq!(current, queue.current_live);
        for &id in queue.pending.iter().chain(&queue.current) {
            if let Some(node) = storage.nodes.get(id)
                && node.metadata.queue != QueueMembership::None
            {
                assert!(seen.insert(id));
            }
        }
        drop(storage);
        runtime.verify_graph();
    }

    #[test]
    fn deferred_initial_run_dedup_writes_and_empty_flush() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        let values = Rc::new(RefCell::new(Vec::new()));
        let captured = values.clone();
        let handle = effect(&runtime, root, move |runtime| {
            assert!(runtime.inner.storage.try_borrow_mut().is_ok());
            let scope = runtime.current_owner()?.unwrap();
            assert_ne!(scope, root);
            assert_eq!(runtime.owner_depth(scope)?, 1);
            captured.borrow_mut().push(input.try_get_copy(runtime)?);
            Ok(())
        });
        assert!(runtime.has_pending_effects());
        assert!(values.borrow().is_empty());
        for value in 1..10 {
            input.try_set(&runtime, value).unwrap();
        }
        let report = flush(&runtime);
        assert_eq!(
            (report.outcome, report.rounds, report.callbacks),
            (FlushOutcome::Stabilized, 1, 1)
        );
        assert_eq!(*values.borrow(), [9]);
        for value in 10..20 {
            input.try_set(&runtime, value).unwrap();
        }
        assert_eq!(runtime.inner.storage.borrow().effects.pending_live, 1);
        assert_eq!(flush(&runtime).callbacks, 1);
        assert_eq!(*values.borrow(), [9, 19]);
        assert_eq!(flush(&runtime).rounds, 0);
        runtime.dispose(handle).unwrap();
        check_queue(&runtime);
    }

    #[test]
    fn owner_depth_then_creation_sequence_order_not_enqueue_order() {
        let (runtime, root) = setup();
        let child = runtime.create_owner(root).unwrap();
        let log = Rc::new(RefCell::new(Vec::new()));
        let a = signal(&runtime, root, 0);
        let b = signal(&runtime, root, 0);
        for (owner, label, input) in [(child, 1, a), (root, 2, a), (root, 3, b)] {
            let log = log.clone();
            effect(&runtime, owner, move |runtime| {
                input.try_get_copy(runtime)?;
                log.borrow_mut().push(label);
                Ok(())
            });
        }
        flush(&runtime);
        assert_eq!(*log.borrow(), [2, 3, 1]);
        log.borrow_mut().clear();
        b.try_set(&runtime, 1).unwrap();
        a.try_set(&runtime, 1).unwrap();
        flush(&runtime);
        assert_eq!(*log.borrow(), [2, 3, 1]);
        check_queue(&runtime);
    }

    #[test]
    fn new_source_read_then_write_requeues_once_and_preserves_first_read() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let handle = effect(&runtime, root, move |runtime| {
            captured.set(captured.get() + 1);
            let value = input.try_get_copy(runtime)?;
            if value < 3 {
                input.try_set(runtime, value + 1)?;
                input.try_get_copy(runtime)?;
            }
            Ok(())
        });
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (4, 4));
        assert_eq!(calls.get(), 4);
        assert!(!runtime.has_pending_effects());
        let storage = runtime.inner.storage.borrow();
        let metadata = &storage.nodes.get(handle.id.slot).unwrap().metadata;
        assert_eq!(metadata.last_processed_round, Some(4));
        assert_eq!(metadata.last_executed_round, Some(4));
        drop(storage);
        check_queue(&runtime);
    }

    #[test]
    fn waiting_current_absorbs_writes_and_processed_effect_enters_next_round() {
        let (runtime, root) = setup();
        let trigger = signal(&runtime, root, 0);
        let input = signal(&runtime, root, 0);
        let log = Rc::new(RefCell::new(Vec::new()));
        let captured = log.clone();
        effect(&runtime, root, move |runtime| {
            captured.borrow_mut().push(input.try_get_copy(runtime)?);
            Ok(())
        });
        effect(&runtime, root, move |runtime| {
            if trigger.try_get_copy(runtime)? != 0 {
                input.try_set(runtime, 5)?;
            }
            Ok(())
        });
        let captured = log.clone();
        effect(&runtime, root, move |runtime| {
            captured
                .borrow_mut()
                .push(input.try_get_copy(runtime)? + 100);
            Ok(())
        });
        flush(&runtime);
        log.borrow_mut().clear();
        input.try_set(&runtime, 1).unwrap();
        trigger.try_set(&runtime, 1).unwrap();
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (2, 4));
        assert_eq!(*log.borrow(), [1, 105, 5]);
        check_queue(&runtime);
    }

    #[test]
    fn equality_cut_consumes_ticket_without_callback_or_extra_round() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| Ok(input.try_get_copy(runtime)? % 2))
            })
            .unwrap();
        let handle = effect(&runtime, root, move |runtime| {
            memo.try_get_copy(runtime)?;
            Ok(())
        });
        flush(&runtime);
        input.try_set(&runtime, 3).unwrap();
        let report = flush(&runtime);
        assert_eq!(
            (report.rounds, report.callbacks, report.processed_effects),
            (1, 0, 1)
        );
        let storage = runtime.inner.storage.borrow();
        let metadata = &storage.nodes.get(handle.id.slot).unwrap().metadata;
        assert_eq!(metadata.last_processed_round, Some(2));
        assert_eq!(metadata.last_executed_round, Some(1));
        drop(storage);
        check_queue(&runtime);
    }

    #[test]
    fn memo_read_before_write_requeues_check_and_does_not_compute_at_completion() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let computes = Rc::new(Cell::new(0));
        let captured = computes.clone();
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| {
                    captured.set(captured.get() + 1);
                    Ok(input.try_get_copy(runtime)? % 2)
                })
            })
            .unwrap();
        let handle = effect(&runtime, root, move |runtime| {
            memo.try_get_copy(runtime)?;
            input.try_set(runtime, 3)?;
            Ok(())
        });
        let report = runtime
            .flush(FlushOptions {
                max_rounds: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(report.outcome, FlushOutcome::RoundLimit);
        assert_eq!(computes.get(), 1);
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(handle.id.slot)
                .unwrap()
                .metadata
                .state,
            State::Check
        );
        let report = flush(&runtime);
        assert_eq!(report.callbacks, 0);
        assert_eq!(computes.get(), 2);
        check_queue(&runtime);
    }

    #[test]
    fn repeated_lazy_memo_evaluations_in_one_round_are_fresh() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| input.try_get_copy(runtime))
            })
            .unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let captured = seen.clone();
        effect(&runtime, root, move |runtime| {
            captured.borrow_mut().push(memo.try_get_copy(runtime)?);
            Ok(())
        });
        effect(&runtime, root, move |runtime| {
            input.try_set(runtime, 2)?;
            Ok(())
        });
        let captured = seen.clone();
        effect(&runtime, root, move |runtime| {
            captured.borrow_mut().push(memo.try_get_copy(runtime)?);
            Ok(())
        });
        let report = flush(&runtime);
        assert_eq!(*seen.borrow(), [1, 2, 2]);
        assert_eq!(report.callbacks, 4);
        check_queue(&runtime);
    }

    #[test]
    fn abandoned_source_and_no_source_writes_do_not_requeue() {
        let (runtime, root) = setup();
        let branch = signal(&runtime, root, 1);
        let old = signal(&runtime, root, 0);
        let handle = effect(&runtime, root, move |runtime| {
            if branch.try_get_copy(runtime)? == 1 {
                old.try_get_copy(runtime)?;
            } else {
                old.try_set(runtime, 5)?;
            }
            Ok(())
        });
        flush(&runtime);
        branch.try_set(&runtime, 0).unwrap();
        assert_eq!(flush(&runtime).rounds, 1);
        effect(&runtime, root, move |runtime| {
            old.try_set(runtime, 6)?;
            Ok(())
        });
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (1, 1));
        assert_eq!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(handle.id.slot)
                .unwrap()
                .sources
                .len(),
            1
        );
        check_queue(&runtime);
    }

    #[test]
    fn creation_inside_round_is_deferred_and_not_run_synchronously() {
        let (runtime, root) = setup();
        let log = Rc::new(RefCell::new(Vec::new()));
        let captured = log.clone();
        effect(&runtime, root, move |runtime| {
            captured.borrow_mut().push(1);
            let child_log = captured.clone();
            runtime.effect_infallible(move |_| child_log.borrow_mut().push(2))?;
            assert_eq!(*captured.borrow(), [1]);
            Ok(())
        });
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (2, 2));
        assert_eq!(*log.borrow(), [1, 2]);
        check_queue(&runtime);
    }

    #[test]
    fn round_limit_and_budgets_preserve_pending_without_poison() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        effect(&runtime, root, move |runtime| {
            let value = input.try_get_copy(runtime)?;
            input.try_set(runtime, value + 1)?;
            Ok(())
        });
        let report = runtime
            .flush(FlushOptions {
                max_rounds: 3,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            (report.outcome, report.rounds, report.callbacks),
            (FlushOutcome::RoundLimit, 3, 3)
        );
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        let report = runtime
            .flush(FlushOptions {
                max_work: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!((report.outcome, report.rounds), (FlushOutcome::Yielded, 0));
        let report = runtime
            .flush(FlushOptions {
                deadline: Some(Instant::now() - Duration::from_secs(1)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(report.outcome, FlushOutcome::Yielded);
        assert!(report.has_pending_effects);
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        check_queue(&runtime);
        runtime.shutdown().unwrap();
        assert!(!runtime.has_pending_effects());
    }

    #[test]
    fn work_budget_finishes_started_round_even_if_over_budget() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        effect(&runtime, root, move |runtime| {
            let value = input.try_get_copy(runtime)?;
            input.try_set(runtime, value + 1)?;
            Ok(())
        });
        for _ in 0..10 {
            effect(&runtime, root, |_| Ok(()));
        }
        let report = runtime
            .flush(FlushOptions {
                max_work: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            (report.outcome, report.rounds, report.processed_effects),
            (FlushOutcome::Yielded, 1, 11)
        );
        check_queue(&runtime);
    }

    #[test]
    fn zero_round_limit_and_empty_zero_budget_are_not_failures() {
        let (runtime, root) = setup();
        assert_eq!(
            runtime
                .flush(FlushOptions {
                    max_rounds: 0,
                    ..Default::default()
                })
                .unwrap()
                .outcome,
            FlushOutcome::Stabilized
        );
        effect(&runtime, root, |_| Ok(()));
        assert_eq!(
            runtime
                .flush(FlushOptions {
                    max_rounds: 0,
                    ..Default::default()
                })
                .unwrap()
                .outcome,
            FlushOutcome::RoundLimit
        );
        assert_eq!(flush(&runtime).callbacks, 1);
        check_queue(&runtime);
    }

    #[test]
    fn reentrant_flush_is_rejected_including_other_runtime_and_update() {
        let (runtime, root) = setup();
        let (other, _) = setup();
        let weak = other.downgrade();
        effect(&runtime, root, move |runtime| {
            assert_eq!(
                runtime.flush(Default::default()).unwrap_err(),
                RuntimeError::Reentrancy
            );
            assert_eq!(
                weak.upgrade()
                    .unwrap()
                    .flush(Default::default())
                    .unwrap_err(),
                RuntimeError::Reentrancy
            );
            Ok(())
        });
        assert_eq!(flush(&runtime).callbacks, 1);
        let input = signal(&runtime, root, 0);
        input
            .try_update_cloned(&runtime, |_| {
                assert_eq!(
                    runtime.flush(Default::default()).unwrap_err(),
                    RuntimeError::Reentrancy
                )
            })
            .unwrap();
        assert_eq!(runtime.status(), RuntimeStatus::Healthy);
        assert_eq!(other.status(), RuntimeStatus::Healthy);
    }

    #[test]
    fn pending_and_current_disposal_skip_dead_tickets_and_reused_slots() {
        let (runtime, root) = setup();
        let doomed = Rc::new(Cell::new(None::<EffectHandle>));
        let captured = doomed.clone();
        effect(&runtime, root, move |runtime| {
            runtime.dispose(captured.get().unwrap())?;
            runtime.effect(|_| Ok(()))?;
            Ok(())
        });
        let old = effect(&runtime, root, |_| panic!("obsolete Current must not run"));
        doomed.set(Some(old));
        let pending = effect(&runtime, root, |_| panic!("cancelled pending must not run"));
        runtime.dispose(pending).unwrap();
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (2, 2));
        check_queue(&runtime);
    }

    #[test]
    fn self_disposal_releases_callback_after_it_finishes_and_does_not_commit() {
        let (runtime, root) = setup();
        let holder = Rc::new(Cell::new(None::<EffectHandle>));
        let captured = holder.clone();
        let done = Rc::new(Cell::new(false));
        let captured_done = done.clone();
        let handle = effect(&runtime, root, move |runtime| {
            runtime.dispose(captured.get().unwrap())?;
            assert_eq!(runtime.node_count(), 0);
            captured_done.set(true);
            Ok(())
        });
        holder.set(Some(handle));
        assert_eq!(flush(&runtime).callbacks, 1);
        assert!(done.get());
        assert!(!runtime.has_pending_effects());
        check_queue(&runtime);
    }

    #[test]
    fn owner_disposal_rejects_new_inscriptions_and_cancels_current_children() {
        let (runtime, root) = setup();
        let child = runtime.create_owner(root).unwrap();
        effect(&runtime, root, move |runtime| {
            runtime.dispose(root)?;
            assert_eq!(runtime.effect(|_| Ok(())), Err(RuntimeError::OwnerDisposed));
            Ok(())
        });
        effect(&runtime, child, |_| panic!("disposed child"));
        assert_eq!(flush(&runtime).callbacks, 1);
        assert_eq!(runtime.owner_count(), 0);
        check_queue(&runtime);
    }

    #[test]
    fn new_source_read_then_disposed_is_not_committed_and_can_be_abandoned() {
        let (runtime, root) = setup();
        let source_owner = runtime.create_root().unwrap();
        let input = signal(&runtime, source_owner, 1);
        let mut first = true;
        let handle = effect(&runtime, root, move |runtime| {
            if first {
                input.try_get_copy(runtime)?;
                runtime.dispose(source_owner)?;
                first = false;
            }
            Ok(())
        });
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (2, 2));
        assert!(
            runtime
                .inner
                .storage
                .borrow()
                .nodes
                .get(handle.id.slot)
                .unwrap()
                .sources
                .is_empty()
        );
        check_queue(&runtime);
    }

    #[test]
    fn error_report_counts_failed_callback_and_preserves_unprocessed_tickets() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 0);
        let failed = effect(&runtime, root, move |runtime| {
            input.try_set(runtime, 10)?;
            Err(RuntimeError::CallbackFailed("effect"))
        });
        effect(&runtime, root, |_| panic!("must stop on first failure"));
        let report = flush(&runtime);
        assert_eq!(
            (report.outcome, report.callbacks),
            (FlushOutcome::Failed, 1)
        );
        assert_eq!(
            report.failure.as_deref(),
            Some(&RuntimeError::CallbackFailed("effect"))
        );
        assert!(report.has_pending_effects);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        let storage = runtime.inner.storage.borrow();
        assert_eq!(
            *storage
                .nodes
                .get(input.id.slot)
                .unwrap()
                .snapshot::<i32>(NodeKind::Signal)
                .unwrap(),
            10
        );
        assert_eq!(
            storage.nodes.get(failed.id.slot).unwrap().metadata.state,
            State::Dirty
        );
        drop(storage);
        check_queue(&runtime);
        assert_eq!(
            runtime.flush(Default::default()).unwrap_err(),
            RuntimeError::Poisoned
        );
        runtime.shutdown().unwrap();
        check_queue(&runtime);
    }

    #[test]
    fn panic_restores_flush_context_and_queue_without_running_remaining_effects() {
        let (runtime, root) = setup();
        effect(&runtime, root, |_| panic!("effect panic"));
        effect(&runtime, root, |_| panic!("must not run"));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| flush(&runtime))).is_err()
        );
        assert!(!runtime.inner.context.snapshot().flushing);
        assert!(runtime.inner.storage.borrow().tracking_stack.is_empty());
        assert_eq!(
            runtime.first_failure().as_deref(),
            Some(&RuntimeError::Panic)
        );
        check_queue(&runtime);
        runtime.shutdown().unwrap();
        check_queue(&runtime);
    }

    #[test]
    fn dead_ticket_retention_is_bounded_without_flush_and_empty_large_buffers_release() {
        let (runtime, root) = setup();
        let survivor = effect(&runtime, root, |_| Ok(()));
        for _ in 0..1000 {
            let handle = effect(&runtime, root, |_| panic!("cancelled"));
            runtime.dispose(handle).unwrap();
        }
        assert!(runtime.inner.storage.borrow().effects.pending.len() <= COMPACT_MIN + 1);
        runtime.dispose(survivor).unwrap();
        assert!(!runtime.has_pending_effects());
        assert!(runtime.inner.storage.borrow().effects.pending.is_empty());
        let handles: Vec<_> = (0..5000)
            .map(|_| effect(&runtime, root, |_| Ok(())))
            .collect();
        for handle in handles {
            runtime.dispose(handle).unwrap();
        }
        assert_eq!(runtime.inner.storage.borrow().effects.pending.capacity(), 0);
        check_queue(&runtime);
    }

    #[test]
    fn round_counter_overflow_reports_failure_without_consuming_pending() {
        let (runtime, root) = setup();
        effect(&runtime, root, |_| panic!("must not execute"));
        runtime.inner.storage.borrow_mut().effects.round = u64::MAX;
        let report = flush(&runtime);
        assert_eq!(report.outcome, FlushOutcome::Failed);
        assert_eq!(
            report.failure.as_deref(),
            Some(&RuntimeError::CounterExhausted)
        );
        assert!(runtime.has_pending_effects());
        check_queue(&runtime);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_during_effect_cancels_all_tickets_without_reentry() {
        let (runtime, root) = setup();
        effect(&runtime, root, |runtime| runtime.shutdown());
        effect(&runtime, root, |_| panic!("closed"));
        assert_eq!(flush(&runtime).callbacks, 1);
        assert_eq!(runtime.status(), RuntimeStatus::Closed);
        assert!(!runtime.has_pending_effects());
        check_queue(&runtime);
    }

    #[test]
    fn actual_memo_cycle_reports_failure_not_a_round_limit() {
        let (runtime, root) = setup();
        let holder = Rc::new(Cell::new(None::<crate::Memo<i32>>));
        let captured = holder.clone();
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| captured.get().unwrap().try_get_copy(runtime))
            })
            .unwrap();
        holder.set(Some(memo));
        effect(&runtime, root, move |runtime| {
            memo.try_get_copy(runtime)?;
            Ok(())
        });
        let report = flush(&runtime);
        assert_eq!(report.outcome, FlushOutcome::Failed);
        assert_eq!(report.failure.as_deref(), Some(&RuntimeError::Cycle));
        assert_eq!(report.callbacks, 1);
        assert_eq!(runtime.status(), RuntimeStatus::Poisoned);
        check_queue(&runtime);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn pending_compaction_inside_callback_never_reorders_current_cursor() {
        let (runtime, root) = setup();
        let log = Rc::new(RefCell::new(Vec::new()));
        let captured = log.clone();
        effect(&runtime, root, move |runtime| {
            captured.borrow_mut().push(0);
            runtime.effect(|_| Ok(()))?;
            for _ in 0..200 {
                let temporary = runtime.effect(|_| panic!("cancelled pending"))?;
                runtime.dispose(temporary)?;
            }
            assert_eq!(runtime.inner.storage.borrow().effects.current.len(), 3);
            Ok(())
        });
        for label in [1, 2] {
            let captured = log.clone();
            effect(&runtime, root, move |_| {
                captured.borrow_mut().push(label);
                Ok(())
            });
        }
        let report = flush(&runtime);
        assert_eq!(*log.borrow(), [0, 1, 2]);
        assert_eq!((report.rounds, report.callbacks), (2, 4));
        check_queue(&runtime);
    }

    #[test]
    fn check_updates_memo_and_consumes_marking_before_callback_without_extra_ticket() {
        let (runtime, root) = setup();
        let input = signal(&runtime, root, 1);
        let memo = runtime
            .with_owner(root, |runtime| {
                runtime.memo(move |runtime| input.try_get_copy(runtime))
            })
            .unwrap();
        effect(&runtime, root, move |runtime| {
            memo.try_get_copy(runtime)?;
            Ok(())
        });
        flush(&runtime);
        input.try_set(&runtime, 2).unwrap();
        let report = flush(&runtime);
        assert_eq!((report.rounds, report.callbacks), (1, 1));
        assert!(!report.has_pending_effects);
        check_queue(&runtime);
    }

    #[test]
    fn creating_without_owner_and_closed_flush_are_explicit_entry_errors() {
        let runtime = Runtime::new();
        assert_eq!(
            runtime.effect(|_| panic!("no owner")),
            Err(RuntimeError::MissingOwner)
        );
        assert!(!runtime.has_pending_effects());
        runtime.shutdown().unwrap();
        assert_eq!(
            runtime.flush(Default::default()).unwrap_err(),
            RuntimeError::RuntimeClosed
        );
    }
}
