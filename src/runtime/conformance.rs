//! Stage 8 regressions and a deterministic, independent observable-state model.
//! Test-only: no production fields, dependencies, or instrumentation.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use super::*;
use crate::{EffectHandle, EvaluationPhase, FlushOptions, FlushOutcome, Memo, Signal, State};

fn flush(rt: &Runtime) -> crate::FlushReport {
    let report = rt.flush(Default::default()).unwrap();
    assert_eq!(report.outcome, FlushOutcome::Stabilized);
    rt.verify_invariants().unwrap();
    report
}

#[test]
fn individual_source_disposals_keep_indices_release_values_and_reconcile_once() {
    struct Resource(u64, Rc<Cell<usize>>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.1.set(self.1.get() + 1);
        }
    }
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let drops = Rc::new(Cell::new(0));
    let removed = Rc::new(Cell::new(0));
    let mut owners = Vec::new();
    let mut inputs = Vec::new();
    for _ in 0..256 {
        let owner = rt.create_owner(root).unwrap();
        inputs.push(
            rt.with_owner(owner, |rt| {
                rt.signal_with(Resource(1, drops.clone()), |a, b| a.0 == b.0)
            })
            .unwrap(),
        );
        owners.push(owner);
    }
    let (captured, count) = (inputs.clone(), removed.clone());
    let memo = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                let mut sum = 0;
                for input in captured.iter().skip(count.get()) {
                    sum += input.try_with(rt, |value| value.0)?;
                }
                Ok(sum)
            })
        })
        .unwrap();
    assert_eq!(memo.try_get_copy(&rt).unwrap(), 256);
    for (i, &owner) in owners.iter().enumerate() {
        removed.set(i + 1);
        rt.dispose(owner).unwrap();
        assert_eq!(drops.get(), i + 1); // tombstones never retain payloads
        assert_eq!(
            inputs[i].try_with(&rt, |_| ()).err(),
            Some(RuntimeError::InvalidHandle)
        );
        // Force slot reuse; the tombstone must still denote the OLD generation.
        let replacement = rt.with_owner(root, |rt| rt.signal(42_u64)).unwrap();
        assert_eq!(replacement.id.slot.index, inputs[i].id.slot.index);
        assert_ne!(replacement.id.slot.generation, inputs[i].id.slot.generation);
        {
            let storage = rt.inner.storage.borrow();
            let node = storage.nodes.get(memo.id.slot).unwrap();
            assert_eq!(node.sources.len(), 256); // no per-disposal retain/scan
            assert!(node.source_holes);
            assert!(!node.sources[i].live);
            if i + 1 < inputs.len() {
                let source = storage.nodes.get(inputs[i + 1].id.slot).unwrap();
                assert_eq!(source.observers[0].source_index, i + 1);
            }
        }
        rt.verify_invariants().unwrap();
    }
    #[cfg(feature = "diagnostics")]
    {
        let d = rt.diagnostics().unwrap();
        assert_eq!(d.edges, 0);
        assert_eq!(d.storage.source_slots, 256);
        assert_eq!(d.storage.source_tombstones, 256);
        assert!(
            rt.graph_snapshot()
                .unwrap()
                .nodes
                .iter()
                .all(|node| node.sources.is_empty())
        );
    }
    assert_eq!(memo.try_get_copy(&rt).unwrap(), 0);
    let storage = rt.inner.storage.borrow();
    let node = storage.nodes.get(memo.id.slot).unwrap();
    assert!(node.sources.is_empty());
    assert!(!node.source_holes);
    drop(storage);
    rt.verify_invariants().unwrap();
}

#[test]
fn equality_cut_reconciles_disposal_tombstones_without_extra_effect_callback() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let child = rt.create_owner(root).unwrap();
    let input = rt.with_owner(child, |rt| rt.signal(0_u64)).unwrap();
    let enabled = Rc::new(Cell::new(true));
    let captured = enabled.clone();
    let memo = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                if captured.get() {
                    input.try_get_copy(rt)?;
                }
                Ok(0_u64)
            })
        })
        .unwrap();
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            memo.try_get_copy(rt)?;
            captured.set(captured.get() + 1);
            Ok(())
        })
    })
    .unwrap();
    flush(&rt);
    enabled.set(false);
    rt.dispose(child).unwrap();
    rt.verify_invariants().unwrap();
    let report = flush(&rt);
    assert_eq!(report.callbacks, 0);
    assert_eq!(calls.get(), 1);
    let storage = rt.inner.storage.borrow();
    let node = storage.nodes.get(memo.id.slot).unwrap();
    assert!(node.sources.is_empty());
    assert!(!node.source_holes);
}

#[test]
fn tombstones_survive_poison_and_are_released_by_shutdown() {
    struct Panics;
    impl Drop for Panics {
        fn drop(&mut self) {
            panic!("disposed value");
        }
    }
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let child = rt.create_owner(root).unwrap();
    let input = rt
        .with_owner(child, |rt| rt.signal_with(Panics, |_, _| false))
        .unwrap();
    let memo = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                input.try_with(rt, |_| ())?;
                Ok(0_u64)
            })
        })
        .unwrap();
    memo.try_get_copy(&rt).unwrap();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.dispose(child)));
    assert!(failure.is_err());
    assert_eq!(rt.status(), RuntimeStatus::Poisoned);
    assert!(
        rt.inner
            .storage
            .borrow()
            .nodes
            .get(memo.id.slot)
            .unwrap()
            .source_holes
    );
    rt.verify_invariants().unwrap();
    rt.shutdown().unwrap();
    rt.verify_invariants().unwrap();
    assert_eq!(rt.inner.storage.borrow().nodes.len(), 0);
}

#[test]
fn prefix_fast_path_handles_source_tombstones_during_effect_execution() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let child = rt.create_owner(root).unwrap();
    let a = rt.with_owner(root, |rt| rt.signal(1_u64)).unwrap();
    let b = rt.with_owner(child, |rt| rt.signal(2_u64)).unwrap();
    let c = rt.with_owner(root, |rt| rt.signal(3_u64)).unwrap();
    let stage = Rc::new(Cell::new(0));
    let seen = Rc::new(Cell::new(0));
    let (captured_stage, captured_seen) = (stage.clone(), seen.clone());
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            let mut sum = a.try_get_copy(rt)?;
            if captured_stage.get() == 0 {
                sum += b.try_get_copy(rt)?;
            } else {
                if captured_stage.get() == 1 {
                    rt.dispose(child)?; // removes the next committed source
                    captured_stage.set(2);
                }
                let reader = rt.inner.context.snapshot().reader.unwrap();
                let first = rt.inner.storage.borrow().nodes.get(reader).unwrap().reads[0];
                a.try_get_copy(rt)?; // duplicate still preserves its first revision
                assert_eq!(
                    rt.inner.storage.borrow().nodes.get(reader).unwrap().reads[0],
                    first
                );
            }
            sum += c.try_get_copy(rt)?;
            captured_seen.set(sum);
            Ok(())
        })
    })
    .unwrap();
    flush(&rt);
    assert_eq!(seen.get(), 6);
    stage.set(1);
    a.try_set(&rt, 10).unwrap();
    flush(&rt);
    assert_eq!(stage.get(), 2);
    assert_eq!(seen.get(), 13);
    rt.dispose(root).unwrap();
    rt.verify_invariants().unwrap();
}

#[test]
fn read_buffer_inline_boundary_spill_and_branch_shrink_preserve_graph() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let width = rt.with_owner(root, |rt| rt.signal(0_usize)).unwrap();
    let inputs: Vec<_> = (0..8)
        .map(|i| rt.with_owner(root, |rt| rt.signal(i as u64 + 1)).unwrap())
        .collect();
    let captured = inputs.clone();
    let memo = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                let n = width.try_get_copy(rt)?;
                let mut sum = 0;
                for input in captured.iter().take(n) {
                    sum += input.try_get_copy(rt)?;
                    input.try_get_copy(rt)?; // duplicate retains first-read order/revision
                }
                Ok(sum)
            })
        })
        .unwrap();
    for n in [0, 1, 2, 8, 1, 0, 8, 2] {
        width.try_set(&rt, n).unwrap();
        assert_eq!(memo.try_get_copy(&rt).unwrap(), (n * (n + 1) / 2) as u64);
        rt.verify_invariants().unwrap();
        let storage = rt.inner.storage.borrow();
        let node = storage.nodes.get(memo.id.slot).unwrap();
        assert_eq!(node.sources.len(), n + 1);
        assert_eq!(node.reads.len(), n + 1);
        assert_eq!(node.sources[0].source, width.id.slot);
        assert!(
            node.sources[1..]
                .iter()
                .zip(&inputs)
                .all(|(edge, input)| edge.source == input.id.slot)
        );
        #[cfg(feature = "experimental-inline-reads")]
        if n >= 2 {
            assert!(node.reads.spilled());
        }
    }
    rt.dispose(root).unwrap();
    rt.verify_invariants().unwrap();
    assert_eq!(memo.try_get_copy(&rt), Err(RuntimeError::InvalidHandle));
}

#[test]
fn newly_read_memo_recomputed_before_effect_completion_uses_first_revision() {
    for (write, expected_calls) in [(2, 2), (3, 1)] {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(1)).unwrap();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| Ok(input.try_get_copy(rt)? % 2))
            })
            .unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let captured = seen.clone();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                let before = memo.try_get_copy(rt)?;
                input.try_set(rt, write)?;
                let after = memo.try_get_copy(rt)?;
                captured.borrow_mut().push((before, after));
                Ok(())
            })
        })
        .unwrap();
        let report = flush(&rt);
        // First-read freshness was invalidated in both cases: a second round
        // consumes its Check ticket, but equality cuts the extra callback.
        assert_eq!((report.callbacks, report.rounds), (expected_calls, 2));
        assert_eq!(report.processed_effects, 2);
        assert_eq!(seen.borrow()[0], (1, write % 2));
        if write == 2 {
            assert_eq!(seen.borrow()[1], (0, 0));
        }
        assert!(!rt.has_pending_effects());
    }
}

#[test]
fn rereading_signal_after_write_preserves_first_consumed_revision() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let signal = rt.with_owner(root, |rt| rt.signal(0)).unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let captured = seen.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            let reader = rt.inner.context.snapshot().reader.unwrap();
            let before = signal.try_get_copy(rt)?;
            let revision =
                rt.inner.storage.borrow().nodes.get(reader).unwrap().reads[0].value_revision;
            signal.try_set(rt, 1)?;
            let after = signal.try_get_copy(rt)?;
            let storage = rt.inner.storage.borrow();
            let reads = &storage.nodes.get(reader).unwrap().reads;
            assert_eq!(reads.len(), 1);
            assert_eq!(reads[0].value_revision, revision);
            captured.borrow_mut().push((before, after));
            Ok(())
        })
    })
    .unwrap();
    let report = flush(&rt);
    assert_eq!((report.rounds, report.callbacks), (2, 2));
    assert_eq!(*seen.borrow(), [(0, 1), (1, 1)]);
}

#[test]
fn equality_cuts_at_multiple_depths_preserve_snapshots_and_skip_downstream() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let input = rt.with_owner(root, |rt| rt.signal(1)).unwrap();
    let counts = Rc::new([const { Cell::new(0) }; 4]);
    let captured = counts.clone();
    let first = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                captured[0].set(captured[0].get() + 1);
                Ok(input.try_get_copy(rt)? % 4)
            })
        })
        .unwrap();
    let captured = counts.clone();
    let second = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                captured[1].set(captured[1].get() + 1);
                Ok(first.try_get_copy(rt)? % 2)
            })
        })
        .unwrap();
    let captured = counts.clone();
    let last = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                captured[2].set(captured[2].get() + 1);
                Ok(second.try_get_copy(rt)? + 1)
            })
        })
        .unwrap();
    let captured = counts.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            last.try_get_copy(rt)?;
            captured[3].set(captured[3].get() + 1);
            Ok(())
        })
    })
    .unwrap();
    flush(&rt);
    let first_snapshot = first.try_peek_copy(&rt).unwrap();
    let second_snapshot = second.try_peek_copy(&rt).unwrap();
    for (value, expected) in [
        (5, [2, 1, 1, 1]),
        (5, [2, 1, 1, 1]),
        (3, [3, 2, 1, 1]),
        (2, [4, 3, 2, 2]),
    ] {
        input.try_set(&rt, value).unwrap();
        flush(&rt);
        assert_eq!(counts.each_ref().map(Cell::get), expected);
        if value == 5 {
            assert_eq!(first_snapshot, first.try_peek_copy(&rt).unwrap());
        }
        if value == 3 {
            assert_eq!(second_snapshot, second.try_peek_copy(&rt).unwrap());
        }
    }
    assert_eq!(last.try_get_copy(&rt).unwrap(), 1);
}

#[test]
fn untrack_allows_foreign_reads_without_creating_cross_runtime_edges() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let other = Runtime::new();
    let other_root = other.create_root().unwrap();
    let external = other.with_owner(other_root, |rt| rt.signal(7)).unwrap();
    let trigger = rt.with_owner(root, |rt| rt.signal(0)).unwrap();
    let weak = other.downgrade();
    let memo = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                trigger.try_get_copy(rt)?;
                let other = weak.upgrade().unwrap();
                assert_eq!(
                    external.try_get_copy(&other),
                    Err(RuntimeError::WrongRuntime)
                );
                rt.untrack(|_| external.try_get_copy(&other))
            })
        })
        .unwrap();
    assert_eq!(memo.try_get_copy(&rt).unwrap(), 7);
    external.try_set(&other, 9).unwrap();
    assert_eq!(memo.try_get_copy(&rt).unwrap(), 7); // intentionally untracked
    trigger.try_set(&rt, 1).unwrap();
    assert_eq!(memo.try_get_copy(&rt).unwrap(), 9);
    let storage = rt.inner.storage.borrow();
    let sources = &storage.nodes.get(memo.id.slot).unwrap().sources;
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].source, trigger.id.slot);
    drop(storage);
    assert!(
        other
            .inner
            .storage
            .borrow()
            .nodes
            .get(external.id.slot)
            .unwrap()
            .observers
            .is_empty()
    );
    rt.verify_invariants().unwrap();
    other.verify_invariants().unwrap();
}

#[test]
fn same_depth_effects_keep_creation_order_when_reenqueued_in_reverse() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let signals: Vec<_> = (0..3)
        .map(|_| rt.with_owner(root, |rt| rt.signal(0)).unwrap())
        .collect();
    let seen = Rc::new(RefCell::new(Vec::new()));
    for (i, &signal) in signals.iter().enumerate() {
        let seen = seen.clone();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                signal.try_get_copy(rt)?;
                seen.borrow_mut().push(i);
                Ok(())
            })
        })
        .unwrap();
    }
    flush(&rt);
    for value in 1..=3 {
        seen.borrow_mut().clear();
        for signal in signals.iter().rev() {
            signal.try_set(&rt, value).unwrap();
        }
        let report = flush(&rt);
        assert_eq!((report.rounds, report.callbacks), (1, 3));
        assert_eq!(*seen.borrow(), [0, 1, 2]);
    }
}

#[test]
fn cycle_in_committed_check_traversal_is_detected_before_compute() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let a = rt.with_owner(root, |rt| rt.memo(|_| Ok(1))).unwrap();
    let b = rt.with_owner(root, |rt| rt.memo(|_| Ok(2))).unwrap();
    a.try_get_copy(&rt).unwrap();
    b.try_get_copy(&rt).unwrap();
    // Inject a bidirectionally consistent committed cycle to exercise Checking,
    // not the separate cycle in executing closures. Public construction cannot
    // publish a cyclic dependency set after a successful evaluation.
    {
        let mut storage = rt.inner.storage.borrow_mut();
        let ar = storage
            .nodes
            .get(a.id.slot)
            .unwrap()
            .metadata
            .value_revision;
        let br = storage
            .nodes
            .get(b.id.slot)
            .unwrap()
            .metadata
            .value_revision;
        Runtime::add_source(&mut storage, a.id.slot, b.id.slot, br);
        Runtime::add_source(&mut storage, b.id.slot, a.id.slot, ar);
        storage.nodes.get_mut(a.id.slot).unwrap().metadata.state = State::Check;
        storage.nodes.get_mut(b.id.slot).unwrap().metadata.state = State::Check;
    }
    assert_eq!(a.try_get_copy(&rt), Err(RuntimeError::Cycle));
    assert_eq!(rt.first_failure().as_deref(), Some(&RuntimeError::Cycle));
    for memo in [a, b] {
        let storage = rt.inner.storage.borrow();
        let node = storage.nodes.get(memo.id.slot).unwrap();
        assert_eq!(node.metadata.phase, EvaluationPhase::Idle);
        assert_eq!(node.metadata.state, State::Dirty);
    }
    #[cfg(feature = "diagnostics")]
    {
        let path = rt.diagnostics().unwrap().cycle;
        assert_eq!(path.len(), 3);
        assert!(
            path.iter()
                .all(|frame| frame.phase == EvaluationPhase::Checking)
        );
        assert_eq!(rt.diagnostics().unwrap().counters.memo_executions, 2);
    }
    rt.verify_invariants().unwrap();
    rt.shutdown().unwrap();
    rt.verify_invariants().unwrap();
}

struct Capture(Rc<Cell<usize>>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn disposing_self_or_owner_and_reusing_node_as_signal_never_restores_old_closure() {
    for dispose_owner in [false, true] {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let survivor = rt.create_root().unwrap();
        let holder = Rc::new(Cell::new(None::<EffectHandle>));
        let captured = holder.clone();
        let replacement = Rc::new(Cell::new(None::<Signal<u64>>));
        let saved = replacement.clone();
        let drops = Rc::new(Cell::new(0));
        let capture = Capture(drops.clone());
        let calls = Rc::new(Cell::new(0));
        let new_calls = calls.clone();
        let effect = rt
            .with_owner(root, |rt| {
                rt.effect(move |rt| {
                    let _capture = &capture;
                    let old = captured.get().unwrap();
                    if dispose_owner {
                        rt.dispose(root)?;
                    } else {
                        rt.dispose(old)?;
                    }
                    let new = rt.with_owner(survivor, |rt| {
                        let new = rt.signal(42_u64)?;
                        let calls = new_calls.clone();
                        rt.effect(move |rt| {
                            assert_eq!(new.try_get_copy(rt)?, 42);
                            calls.set(calls.get() + 1);
                            Ok(())
                        })?;
                        Ok(new)
                    })?;
                    assert_eq!(new.id.slot.index, old.id.slot.index);
                    assert_ne!(new.id.slot.generation, old.id.slot.generation);
                    assert_eq!(rt.dispose(old), Ok(DisposeOutcome::AlreadyDisposed));
                    saved.set(Some(new));
                    Ok(())
                })
            })
            .unwrap();
        holder.set(Some(effect));
        let report = flush(&rt);
        assert_eq!((report.callbacks, report.rounds), (2, 2));
        assert_eq!(calls.get(), 1);
        assert_eq!(drops.get(), 1);
        assert_eq!(replacement.get().unwrap().try_get_copy(&rt).unwrap(), 42);
        assert!(rt.inner.storage.borrow().retired_tracking.is_empty());
        rt.shutdown().unwrap();
        assert_eq!(drops.get(), 1);
    }
}

/// No dependency graph or scheduler algorithm here: only published derived
/// value revisions and the last revision observed by a passive effect.
#[derive(Default)]
struct Model {
    branch: bool,
    a: u64,
    b: u64,
    cached: Option<u64>,
    revision: u64,
    consumed: Option<u64>,
    observed: Option<u64>,
    calls: usize,
}
impl Model {
    fn read(&mut self) -> u64 {
        let value = if self.branch { self.a } else { self.b } / 2;
        if self.cached != Some(value) {
            self.cached = Some(value);
            self.revision += 1;
        }
        value
    }
    fn flush(&mut self) -> usize {
        let value = self.read();
        if self.consumed == Some(self.revision) {
            return 0;
        }
        self.consumed = Some(self.revision);
        self.observed = Some(value);
        self.calls += 1;
        1
    }
}

struct Actual {
    root: OwnerHandle,
    branch: Signal<bool>,
    a: Signal<u64>,
    b: Signal<u64>,
    memo: Memo<u64>,
    effect: EffectHandle,
    observed: Rc<Cell<Option<u64>>>,
    calls: Rc<Cell<usize>>,
}
impl Actual {
    fn new(rt: &Runtime) -> Self {
        let root = rt.create_root().unwrap();
        rt.with_owner(root, |rt| {
            let branch = rt.signal(false)?;
            let a = rt.signal(0)?;
            let b = rt.signal(0)?;
            let memo = rt.memo(move |rt| {
                Ok(if branch.try_get_copy(rt)? {
                    a.try_get_copy(rt)?
                } else {
                    b.try_get_copy(rt)?
                } / 2)
            })?;
            let observed = Rc::new(Cell::new(None));
            let calls = Rc::new(Cell::new(0));
            let (output, count) = (observed.clone(), calls.clone());
            let effect = rt.effect(move |rt| {
                output.set(Some(memo.try_get_copy(rt)?));
                count.set(count.get() + 1);
                Ok(())
            })?;
            Ok(Self {
                root,
                branch,
                a,
                b,
                memo,
                effect,
                observed,
                calls,
            })
        })
        .unwrap()
    }
}

fn random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn randomized_reads_writes_branches_flush_and_disposal_match_reference_model() {
    for initial_seed in 1..=32 {
        let rt = Runtime::new();
        let mut seed = initial_seed;
        let mut graphs: Vec<Option<(Actual, Model)>> = (0..8)
            .map(|_| Some((Actual::new(&rt), Model::default())))
            .collect();
        let mut stale = Vec::new();
        for step in 0..1000 {
            let index = (random(&mut seed) % graphs.len() as u64) as usize;
            let operation = random(&mut seed) % 10;
            let value = random(&mut seed) % 16;
            if operation == 8 {
                if let Some((actual, _)) = graphs[index].take() {
                    let calls = actual.calls.get();
                    rt.dispose(actual.root).unwrap();
                    assert_eq!(rt.dispose(actual.root), Ok(DisposeOutcome::AlreadyDisposed));
                    stale.push((actual, calls));
                } else {
                    graphs[index] = Some((Actual::new(&rt), Model::default()));
                }
            } else if operation >= 6 && operation != 9 {
                let expected: usize = graphs
                    .iter_mut()
                    .flatten()
                    .map(|(_, model)| model.flush())
                    .sum();
                let report = flush(&rt);
                assert_eq!(
                    report.callbacks, expected,
                    "seed={initial_seed}, step={step}"
                );
                assert!(!report.has_pending_effects);
            } else if let Some((actual, model)) = &mut graphs[index] {
                match operation {
                    0 | 1 => {
                        actual.a.try_set(&rt, value).unwrap();
                        model.a = value;
                    }
                    2 => {
                        actual.b.try_set(&rt, value).unwrap();
                        model.b = value;
                    }
                    3 => {
                        model.branch = value.is_multiple_of(2);
                        actual.branch.try_set(&rt, model.branch).unwrap();
                    }
                    4 => assert_eq!(
                        actual.memo.try_get_copy(&rt).unwrap(),
                        model.read(),
                        "seed={initial_seed}, step={step}"
                    ),
                    5 => assert_eq!(
                        rt.untrack(|rt| actual.memo.try_peek_copy(rt)).unwrap(),
                        model.read(),
                        "seed={initial_seed}, step={step}"
                    ),
                    9 => {
                        rt.batch(|rt| {
                            actual.a.try_set(rt, value)?;
                            actual.b.try_set(rt, value + 1)?;
                            Ok(())
                        })
                        .unwrap();
                        model.a = value;
                        model.b = value + 1;
                    }
                    _ => unreachable!(),
                }
                assert_eq!(actual.a.try_peek_copy(&rt).unwrap(), model.a);
                assert_eq!(actual.b.try_peek_copy(&rt).unwrap(), model.b);
            }
            rt.verify_invariants()
                .unwrap_or_else(|error| panic!("seed={initial_seed}, step={step}: {error}"));
            for (actual, model) in graphs.iter().flatten() {
                assert_eq!(
                    actual.calls.get(),
                    model.calls,
                    "seed={initial_seed}, step={step}"
                );
                assert_eq!(
                    actual.observed.get(),
                    model.observed,
                    "seed={initial_seed}, step={step}"
                );
            }
            if let Some((dead, calls)) = stale.last() {
                assert_eq!(
                    dead.memo.try_get_copy(&rt),
                    Err(RuntimeError::InvalidHandle)
                );
                assert_eq!(dead.a.try_set(&rt, value), Err(RuntimeError::InvalidHandle));
                assert_eq!(rt.dispose(dead.effect), Ok(DisposeOutcome::AlreadyDisposed));
                assert_eq!(dead.calls.get(), *calls);
            }
        }
        let expected: usize = graphs
            .iter_mut()
            .flatten()
            .map(|(_, model)| model.flush())
            .sum();
        assert_eq!(flush(&rt).callbacks, expected);
        rt.shutdown().unwrap();
        rt.verify_invariants().unwrap();
        assert_eq!((rt.node_count(), rt.owner_count()), (0, 0));
        assert!(
            stale
                .iter()
                .all(|(actual, calls)| actual.calls.get() == *calls)
        );
    }
}

#[test]
fn repeated_create_dispose_without_flush_reuses_slots_and_drops_all_captures() {
    let rt = Runtime::new();
    let drops = Rc::new(Cell::new(0));
    let cleanups = Rc::new(Cell::new(0));
    let mut old = Vec::new();
    for i in 0..2000 {
        let root = rt.create_root().unwrap();
        let signal = rt
            .with_owner(root, |rt| {
                let signal = rt.signal_with(Capture(drops.clone()), |_, _| false)?;
                let capture = Capture(drops.clone());
                rt.memo::<u64>(move |_| {
                    let _capture = &capture;
                    panic!("unobserved memo")
                })?;
                let capture = Capture(drops.clone());
                rt.effect(move |_| {
                    let _capture = &capture;
                    panic!("disposed effect must never run")
                })?;
                let count = cleanups.clone();
                rt.on_cleanup(move |_| {
                    count.set(count.get() + 1);
                    Ok(())
                })?;
                Ok(signal)
            })
            .unwrap();
        assert!(signal.id.slot.index < 3);
        assert!(root.id.slot.index < 1);
        rt.dispose(root).unwrap();
        assert_eq!(rt.dispose(root), Ok(DisposeOutcome::AlreadyDisposed));
        old.push(signal);
        assert_eq!((drops.get(), cleanups.get()), ((i + 1) * 3, i + 1));
        assert_eq!((rt.node_count(), rt.owner_count()), (0, 0));
        assert!(!rt.has_pending_effects());
        rt.verify_invariants().unwrap();
    }
    for handle in old {
        assert!(matches!(
            handle.try_peek_with(&rt, |_| ()),
            Err(RuntimeError::InvalidHandle)
        ));
    }
    assert_eq!(flush(&rt).callbacks, 0);
    rt.shutdown().unwrap();
}

#[test]
fn local_errors_are_recoverable_but_caught_evaluation_errors_never_clear_poison() {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let signal = rt.with_owner(root, |rt| rt.signal(1)).unwrap();
    let other = Runtime::new();
    assert_eq!(signal.try_get_copy(&other), Err(RuntimeError::WrongRuntime));
    assert_eq!(
        rt.batch::<()>(|_| Err(RuntimeError::CallbackFailed("local"))),
        Err(RuntimeError::CallbackFailed("local"))
    );
    assert_eq!(rt.status(), RuntimeStatus::Healthy);
    let bad = rt
        .with_owner(root, |rt| {
            rt.memo::<i32>(|_| Err(RuntimeError::CallbackFailed("compute")))
        })
        .unwrap();
    assert_eq!(
        bad.try_get_copy(&rt),
        Err(RuntimeError::CallbackFailed("compute"))
    );
    assert_eq!(rt.status(), RuntimeStatus::Poisoned);
    assert_eq!(signal.try_get_copy(&rt), Err(RuntimeError::Poisoned));
    assert_eq!(signal.try_set(&rt, 2), Err(RuntimeError::Poisoned));
    assert_eq!(
        rt.flush(FlushOptions::default()).unwrap_err(),
        RuntimeError::Poisoned
    );
    rt.verify_invariants().unwrap();
    rt.shutdown().unwrap();
    assert_eq!(
        rt.first_failure().as_deref(),
        Some(&RuntimeError::CallbackFailed("compute"))
    );
}

#[test]
fn optimized_shared_release_guards_last_drop_even_with_weak_references() {
    struct Resource(WeakRuntime, Rc<Cell<usize>>);
    impl Drop for Resource {
        fn drop(&mut self) {
            let rt = self.0.upgrade().unwrap();
            assert_eq!(rt.create_root(), Err(RuntimeError::Reentrancy));
            self.1.set(self.1.get() + 1);
        }
    }
    let rt = Runtime::new();
    let drops = Rc::new(Cell::new(0));
    let value = Rc::new(Resource(rt.downgrade(), drops.clone()));
    let weak = Rc::downgrade(&value);
    assert!(rt.release_shared(Some(value.clone())).is_none());
    assert_eq!(drops.get(), 0);
    assert!(rt.release_shared(Some(value)).is_none());
    assert_eq!(drops.get(), 1);
    assert!(weak.upgrade().is_none());
    assert_eq!(rt.status(), RuntimeStatus::Healthy);
}
