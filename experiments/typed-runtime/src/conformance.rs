//! Ports of ALL 16 cases in src/runtime/conformance.rs. Private storage
//! assertions are expressed against this backend's representation, not omitted
//! because a source is dead or the runtime is poisoned. See the coverage table.
use super::*;
use std::cell::RefCell;
fn setup() -> (Runtime, OwnerHandle) {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    (rt, root)
}
fn signal<T: PartialEq + 'static>(rt: &Runtime, o: OwnerHandle, v: T) -> Handle<T> {
    rt.with_owner(o, |rt| rt.signal(v)).unwrap()
}
fn flush(rt: &Runtime) -> FlushReport {
    let r = rt.flush_with(Default::default()).unwrap();
    assert_eq!(r.outcome, FlushOutcome::Stabilized);
    rt.verify_invariants().unwrap();
    r
}

#[test]
fn individual_source_disposals_keep_indices_release_values_and_reconcile_once() {
    struct Resource(Rc<Cell<usize>>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let (rt, root) = setup();
    let drops = Rc::new(Cell::new(0));
    let removed = Rc::new(Cell::new(0));
    let mut owners = Vec::new();
    let mut inputs = Vec::new();
    for _ in 0..256 {
        let o = rt.create_owner(root).unwrap();
        inputs.push(
            rt.with_owner(o, |rt| {
                rt.signal_with(Resource(drops.clone()), |_, _| false)
            })
            .unwrap(),
        );
        owners.push(o);
    }
    let captured = inputs.clone();
    let count = removed.clone();
    let m = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                for s in captured.iter().skip(count.get()) {
                    rt.with(*s, |_| ())?;
                }
                Ok(256 - count.get())
            })
        })
        .unwrap();
    assert_eq!(rt.get(m), Ok(256));
    for (i, &o) in owners.iter().enumerate() {
        removed.set(i + 1);
        rt.dispose_owner(o).unwrap();
        assert_eq!(drops.get(), i + 1);
        assert_eq!(rt.with(inputs[i], |_| ()), Err(Error::InvalidHandle));
        let replacement = signal(&rt, root, 42_u64);
        assert_eq!(replacement.key.index, inputs[i].key.index);
        assert_ne!(replacement.key.generation, inputs[i].key.generation);
        rt.graph(|g| {
            let c = g.cold[m.key.index as usize].as_ref().unwrap();
            assert_eq!(c.sources.len(), 256);
            assert!(!c.sources[i].live);
            if i + 1 < inputs.len() {
                assert_eq!(
                    g.cold[inputs[i + 1].key.index as usize]
                        .as_ref()
                        .unwrap()
                        .observers[0]
                        .source_index,
                    i + 1
                );
            }
        });
        rt.verify_invariants().unwrap();
    }
    #[cfg(feature = "diagnostics")]
    {
        let d = rt.diagnostics().unwrap();
        assert_eq!(d.edges, 0);
        assert_eq!(d.storage.source_tombstones, 256);
    }
    assert_eq!(rt.get(m), Ok(0));
    rt.graph(|g| {
        assert!(
            g.cold[m.key.index as usize]
                .as_ref()
                .unwrap()
                .sources
                .is_empty()
        )
    });
}
#[test]
fn equality_cut_reconciles_disposal_tombstones_without_extra_effect_callback() {
    let (rt, root) = setup();
    let child = rt.create_owner(root).unwrap();
    let input = signal(&rt, child, 0);
    let enabled = Rc::new(Cell::new(true));
    let e = enabled.clone();
    let m = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                if e.get() {
                    rt.get(input)?;
                }
                Ok(0)
            })
        })
        .unwrap();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            rt.get(m)?;
            Ok(())
        })
    })
    .unwrap();
    assert_eq!(flush(&rt).callbacks, 1);
    enabled.set(false);
    rt.dispose_owner(child).unwrap();
    assert_eq!(flush(&rt).callbacks, 0);
    rt.graph(|g| {
        assert!(
            g.cold[m.key.index as usize]
                .as_ref()
                .unwrap()
                .sources
                .is_empty()
        )
    });
}
#[test]
fn tombstones_survive_poison_and_are_released_by_shutdown() {
    struct Panics;
    impl Drop for Panics {
        fn drop(&mut self) {
            panic!("disposed value");
        }
    }
    let (rt, root) = setup();
    let child = rt.create_owner(root).unwrap();
    let s = rt
        .with_owner(child, |rt| rt.signal_with(Panics, |_, _| false))
        .unwrap();
    let m = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                rt.with(s, |_| ())?;
                Ok(0)
            })
        })
        .unwrap();
    rt.get(m).unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| rt.dispose_owner(child))).is_err());
    assert_eq!(rt.status(), RuntimeStatus::Poisoned);
    rt.graph(|g| assert!(!g.cold[m.key.index as usize].as_ref().unwrap().sources[0].live));
    rt.verify_invariants().unwrap();
    rt.shutdown().unwrap();
    assert_eq!(rt.node_count(), 0);
    rt.verify_invariants().unwrap();
}
#[test]
fn prefix_fast_path_handles_source_tombstones_during_effect_execution() {
    let (rt, root) = setup();
    let child = rt.create_owner(root).unwrap();
    let a = signal(&rt, root, 1);
    let b = signal(&rt, child, 2);
    let c = signal(&rt, root, 3);
    let stage = Rc::new(Cell::new(0));
    let step = stage.clone();
    let seen = Rc::new(Cell::new(0));
    let out = seen.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            let mut sum = rt.get(a)?;
            if step.get() == 0 {
                sum += rt.get(b)?;
            } else {
                if step.get() == 1 {
                    rt.dispose_owner(child)?;
                    step.set(2);
                }
                let reader = rt.0.reader.get().unwrap();
                let first = rt.graph(|g| g.cold[reader.index as usize].as_ref().unwrap().reads[0]);
                rt.get(a)?;
                assert!(
                    rt.graph(|g| g.cold[reader.index as usize].as_ref().unwrap().reads[0] == first)
                );
            }
            sum += rt.get(c)?;
            out.set(sum);
            Ok(())
        })
    })
    .unwrap();
    flush(&rt);
    assert_eq!(seen.get(), 6);
    stage.set(1);
    rt.set(a, 10).unwrap();
    flush(&rt);
    assert_eq!(seen.get(), 13);
    rt.dispose_owner(root).unwrap();
    rt.verify_invariants().unwrap();
}
#[test]
fn read_buffer_inline_boundary_spill_and_branch_shrink_preserve_graph() {
    let (rt, root) = setup();
    let width = signal(&rt, root, 0_usize);
    let inputs: Vec<_> = (1..=8).map(|n| signal(&rt, root, n)).collect();
    let captured = inputs.clone();
    let m = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                let n = rt.get(width)?;
                let mut sum = 0;
                for s in captured.iter().take(n) {
                    sum += rt.get(*s)?;
                    rt.get(*s)?;
                }
                Ok(sum)
            })
        })
        .unwrap();
    for n in [0, 1, 2, 8, 1, 0, 8, 2] {
        rt.set(width, n).unwrap();
        assert_eq!(rt.get(m), Ok(n * (n + 1) / 2));
        rt.verify_invariants().unwrap();
        rt.graph(|g| {
            let c = g.cold[m.key.index as usize].as_ref().unwrap();
            assert_eq!(c.sources.len(), n + 1);
            assert_eq!(c.sources[0].key, width.key);
            assert!(
                c.sources[1..]
                    .iter()
                    .zip(&inputs)
                    .all(|(s, h)| s.key == h.key)
            );
        });
    }
    rt.dispose_owner(root).unwrap();
    assert_eq!(rt.get(m), Err(Error::InvalidHandle));
}
#[test]
fn newly_read_memo_recomputed_before_effect_completion_uses_first_revision() {
    for (write, calls) in [(2, 2), (3, 1)] {
        let (rt, root) = setup();
        let s = signal(&rt, root, 1);
        let m = rt
            .with_owner(root, |rt| rt.memo(move |rt| Ok(rt.get(s)? % 2)))
            .unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let out = seen.clone();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                let before = rt.get(m)?;
                rt.set(s, write)?;
                let after = rt.get(m)?;
                out.borrow_mut().push((before, after));
                Ok(())
            })
        })
        .unwrap();
        let r = flush(&rt);
        assert_eq!((r.callbacks, r.rounds, r.processed_effects), (calls, 2, 2));
        assert_eq!(seen.borrow()[0], (1, write % 2));
    }
}
#[test]
fn rereading_signal_after_write_preserves_first_consumed_revision() {
    let (rt, root) = setup();
    let s = signal(&rt, root, 0);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let out = seen.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            let before = rt.get(s)?;
            let reader = rt.0.reader.get().unwrap();
            let record = rt.graph(|g| g.cold[reader.index as usize].as_ref().unwrap().reads[0]);
            rt.set(s, 1)?;
            let after = rt.get(s)?;
            rt.graph(|g| {
                let reads = &g.cold[reader.index as usize].as_ref().unwrap().reads;
                assert_eq!(reads.len(), 1);
                assert!(reads[0] == record);
            });
            out.borrow_mut().push((before, after));
            Ok(())
        })
    })
    .unwrap();
    let r = flush(&rt);
    assert_eq!((r.rounds, r.callbacks), (2, 2));
    assert_eq!(*seen.borrow(), [(0, 1), (1, 1)]);
}
#[test]
fn equality_cuts_at_multiple_depths_preserve_snapshots_and_skip_downstream() {
    let (rt, root) = setup();
    let s = signal(&rt, root, 1);
    let counts = Rc::new([const { Cell::new(0) }; 4]);
    let count = counts.clone();
    let a = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                count[0].set(count[0].get() + 1);
                Ok(rt.get(s)? % 4)
            })
        })
        .unwrap();
    let count = counts.clone();
    let b = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                count[1].set(count[1].get() + 1);
                Ok(rt.get(a)? % 2)
            })
        })
        .unwrap();
    let count = counts.clone();
    let c = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                count[2].set(count[2].get() + 1);
                Ok(rt.get(b)? + 1)
            })
        })
        .unwrap();
    let count = counts.clone();
    rt.with_owner(root, |rt| {
        rt.effect(move |rt| {
            rt.get(c)?;
            count[3].set(count[3].get() + 1);
            Ok(())
        })
    })
    .unwrap();
    flush(&rt);
    for (n, expected) in [
        (5, [2, 1, 1, 1]),
        (5, [2, 1, 1, 1]),
        (3, [3, 2, 1, 1]),
        (2, [4, 3, 2, 2]),
    ] {
        rt.set(s, n).unwrap();
        flush(&rt);
        assert_eq!(counts.each_ref().map(Cell::get), expected);
    }
}
#[test]
fn untrack_allows_foreign_reads_without_creating_cross_runtime_edges() {
    let (rt, root) = setup();
    let (other, foreign) = setup();
    let external = signal(&other, foreign, 7);
    let trigger = signal(&rt, root, 0);
    let weak = other.downgrade();
    let m = rt
        .with_owner(root, |rt| {
            rt.memo(move |rt| {
                rt.get(trigger)?;
                let other = weak.upgrade().unwrap();
                assert_eq!(other.get(external), Err(Error::WrongRuntime));
                rt.untrack(|_| other.get(external))
            })
        })
        .unwrap();
    assert_eq!(rt.get(m), Ok(7));
    other.set(external, 9).unwrap();
    assert_eq!(rt.get(m), Ok(7));
    rt.set(trigger, 1).unwrap();
    assert_eq!(rt.get(m), Ok(9));
    rt.graph(|g| {
        assert_eq!(
            g.cold[m.key.index as usize].as_ref().unwrap().sources.len(),
            1
        )
    });
    other.graph(|g| {
        assert!(
            g.cold[external.key.index as usize]
                .as_ref()
                .unwrap()
                .observers
                .is_empty()
        )
    });
}
#[test]
fn same_depth_effects_keep_creation_order_when_reenqueued_in_reverse() {
    let (rt, root) = setup();
    let inputs: Vec<_> = (0..3).map(|_| signal(&rt, root, 0)).collect();
    let seen = Rc::new(RefCell::new(Vec::new()));
    for (i, &s) in inputs.iter().enumerate() {
        let seen = seen.clone();
        rt.with_owner(root, |rt| {
            rt.effect(move |rt| {
                rt.get(s)?;
                seen.borrow_mut().push(i);
                Ok(())
            })
        })
        .unwrap();
    }
    flush(&rt);
    for n in 1..=3 {
        seen.borrow_mut().clear();
        for s in inputs.iter().rev() {
            rt.set(*s, n).unwrap();
        }
        let r = flush(&rt);
        assert_eq!((r.rounds, r.callbacks), (1, 3));
        assert_eq!(*seen.borrow(), [0, 1, 2]);
    }
}
#[test]
fn cycle_in_committed_check_traversal_is_detected_before_compute() {
    let (rt, root) = setup();
    let a = rt.with_owner(root, |rt| rt.memo(|_| Ok(1))).unwrap();
    let b = rt.with_owner(root, |rt| rt.memo(|_| Ok(2))).unwrap();
    rt.get(a).unwrap();
    rt.get(b).unwrap();
    rt.graph(|g| {
        for (reader, source) in [(a.key, b.key), (b.key, a.key)] {
            let revision = header(g, source).unwrap().revision;
            g.cold[reader.index as usize]
                .as_mut()
                .unwrap()
                .sources
                .push(Source {
                    key: source,
                    revision,
                    freshness: 0,
                    observer_index: 0,
                    live: true,
                });
            g.cold[source.index as usize]
                .as_mut()
                .unwrap()
                .observers
                .push(Observer {
                    key: reader,
                    source_index: 0,
                });
            header_mut(g, reader).unwrap().state = State::Check;
        }
    });
    assert_eq!(rt.get(a), Err(Error::Cycle));
    assert_eq!(rt.first_failure().as_deref(), Some(&Error::Cycle));
    for h in [a, b] {
        rt.graph(|g| {
            let h = header(g, h.key).unwrap();
            assert_eq!(h.phase, EvaluationPhase::Idle);
            assert_eq!(h.state, State::Dirty);
        });
    }
    #[cfg(feature = "diagnostics")]
    {
        let d = rt.diagnostics().unwrap();
        assert_eq!(d.cycle.len(), 3);
        assert!(d.cycle.iter().all(|f| f.phase == EvaluationPhase::Checking));
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
        let (rt, root) = setup();
        let survivor = rt.create_root().unwrap();
        let holder = Rc::new(Cell::new(None::<Handle<()>>));
        let captured = holder.clone();
        let drops = Rc::new(Cell::new(0));
        let cap = Capture(drops.clone());
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        let e = rt
            .with_owner(root, |rt| {
                rt.effect(move |rt| {
                    let _ = &cap;
                    let old = captured.get().unwrap();
                    if dispose_owner {
                        rt.dispose_owner(root)?;
                    } else {
                        rt.dispose(old)?;
                    }
                    let new = rt.with_owner(survivor, |rt| {
                        let new = rt.signal(42_u64)?;
                        let count = count.clone();
                        rt.effect(move |rt| {
                            assert_eq!(rt.get(new)?, 42);
                            count.set(count.get() + 1);
                            Ok(())
                        })?;
                        Ok(new)
                    })?;
                    assert_eq!(new.key.index, old.key.index);
                    assert_ne!(new.key.generation, old.key.generation);
                    rt.dispose(old)?;
                    Ok(())
                })
            })
            .unwrap();
        holder.set(Some(e));
        let r = flush(&rt);
        assert_eq!((r.callbacks, r.rounds), (2, 2));
        assert_eq!(calls.get(), 1);
        assert_eq!(drops.get(), 1);
        rt.shutdown().unwrap();
        assert_eq!(drops.get(), 1);
    }
}

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
        let n = if self.branch { self.a } else { self.b } / 2;
        if self.cached != Some(n) {
            self.cached = Some(n);
            self.revision += 1;
        }
        n
    }
    fn flush(&mut self) -> usize {
        let n = self.read();
        if self.consumed == Some(self.revision) {
            return 0;
        }
        self.consumed = Some(self.revision);
        self.observed = Some(n);
        self.calls += 1;
        1
    }
}
struct Actual {
    root: OwnerHandle,
    branch: Handle<bool>,
    a: Handle<u64>,
    b: Handle<u64>,
    memo: Handle<u64>,
    effect: Handle<()>,
    observed: Rc<Cell<Option<u64>>>,
    calls: Rc<Cell<usize>>,
}
impl Actual {
    fn new(rt: &Runtime) -> Self {
        let root = rt.create_root().unwrap();
        rt.with_owner(root, |rt| {
            let branch = rt.signal(false)?;
            let a = rt.signal(0_u64)?;
            let b = rt.signal(0_u64)?;
            let memo = rt.memo(move |rt| {
                Ok(if rt.get(branch)? {
                    rt.get(a)?
                } else {
                    rt.get(b)?
                } / 2)
            })?;
            let observed = Rc::new(Cell::new(None));
            let calls = Rc::new(Cell::new(0));
            let out = observed.clone();
            let count = calls.clone();
            let effect = rt.effect(move |rt| {
                out.set(Some(rt.get(memo)?));
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
    // Optional explicit sharding for Miri. Default/native workload is unchanged;
    // the runner records the selected range and requires all 32 seeds for the
    // complete model claim. Each selected seed still executes all 1000 steps.
    let start = std::env::var("TYPED_MODEL_SEED_START")
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(1);
    let end = std::env::var("TYPED_MODEL_SEED_END")
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(32);
    assert!((1..=32).contains(&start) && start <= end && end <= 32);
    for initial in start..=end {
        eprintln!("model seed {initial}/32: 1000 operations");
        let rt = Runtime::new();
        let mut seed = initial;
        let mut graphs: Vec<_> = (0..8)
            .map(|_| Some((Actual::new(&rt), Model::default())))
            .collect();
        let mut stale = Vec::new();
        for step in 0..1000 {
            let index = random(&mut seed) as usize % 8;
            let op = random(&mut seed) % 10;
            let value = random(&mut seed) % 16;
            if op == 8 {
                if let Some((a, _)) = graphs[index].take() {
                    let count = a.calls.get();
                    rt.dispose_owner(a.root).unwrap();
                    rt.dispose_owner(a.root).unwrap();
                    stale.push((a, count));
                } else {
                    graphs[index] = Some((Actual::new(&rt), Model::default()));
                }
            } else if op >= 6 && op != 9 {
                let expected: usize = graphs.iter_mut().flatten().map(|(_, m)| m.flush()).sum();
                assert_eq!(flush(&rt).callbacks, expected, "seed={initial} step={step}");
            } else if let Some((a, m)) = &mut graphs[index] {
                match op {
                    0 | 1 => {
                        rt.set(a.a, value).unwrap();
                        m.a = value;
                    }
                    2 => {
                        rt.set(a.b, value).unwrap();
                        m.b = value;
                    }
                    3 => {
                        m.branch = value.is_multiple_of(2);
                        rt.set(a.branch, m.branch).unwrap();
                    }
                    4 => assert_eq!(rt.get(a.memo).unwrap(), m.read()),
                    5 => assert_eq!(rt.untrack(|rt| rt.peek(a.memo)).unwrap(), m.read()),
                    9 => {
                        rt.batch(|rt| {
                            rt.set(a.a, value)?;
                            rt.set(a.b, value + 1)
                        })
                        .unwrap();
                        m.a = value;
                        m.b = value + 1;
                    }
                    _ => unreachable!(),
                }
                assert_eq!(rt.peek(a.a).unwrap(), m.a);
                assert_eq!(rt.peek(a.b).unwrap(), m.b);
            }
            rt.verify_invariants()
                .unwrap_or_else(|e| panic!("seed={initial} step={step}: {e}"));
            for (a, m) in graphs.iter().flatten() {
                assert_eq!(a.calls.get(), m.calls);
                assert_eq!(a.observed.get(), m.observed);
            }
            if let Some((a, calls)) = stale.last() {
                assert_eq!(rt.get(a.memo), Err(Error::InvalidHandle));
                assert_eq!(rt.set(a.a, value), Err(Error::InvalidHandle));
                rt.dispose(a.effect).unwrap();
                assert_eq!(a.calls.get(), *calls);
            }
        }
        let expected: usize = graphs.iter_mut().flatten().map(|(_, m)| m.flush()).sum();
        assert_eq!(flush(&rt).callbacks, expected);
        rt.shutdown().unwrap();
        rt.verify_invariants().unwrap();
        assert_eq!((rt.node_count(), rt.owner_count()), (0, 0));
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
        let s = rt
            .with_owner(root, |rt| {
                let s = rt.signal_with(Capture(drops.clone()), |_, _| false)?;
                let cap = Capture(drops.clone());
                rt.memo::<u64>(move |_| {
                    let _ = &cap;
                    panic!("lazy");
                })?;
                let cap = Capture(drops.clone());
                rt.effect(move |_| {
                    let _ = &cap;
                    panic!("cancelled");
                })?;
                let count = cleanups.clone();
                rt.on_cleanup(move |_| {
                    count.set(count.get() + 1);
                    Ok(())
                })?;
                Ok(s)
            })
            .unwrap();
        assert!(s.key.index < 3);
        assert_eq!(root.key.index, 0);
        rt.dispose_owner(root).unwrap();
        rt.dispose_owner(root).unwrap();
        old.push(s);
        assert_eq!((drops.get(), cleanups.get()), ((i + 1) * 3, i + 1));
        assert_eq!((rt.node_count(), rt.owner_count()), (0, 0));
        assert!(!rt.has_pending_effects());
        rt.verify_invariants().unwrap();
    }
    for h in old {
        assert_eq!(rt.peek_with(h, |_| ()), Err(Error::InvalidHandle));
    }
    assert_eq!(flush(&rt).callbacks, 0);
}
#[test]
fn local_errors_are_recoverable_but_caught_evaluation_errors_never_clear_poison() {
    let (rt, root) = setup();
    let s = signal(&rt, root, 1);
    let other = Runtime::new();
    assert_eq!(other.get(s), Err(Error::WrongRuntime));
    assert_eq!(
        rt.batch::<()>(|_| Err(Error::CallbackFailed("local"))),
        Err(Error::CallbackFailed("local"))
    );
    assert_eq!(rt.status(), RuntimeStatus::Healthy);
    let m = rt
        .with_owner(root, |rt| {
            rt.memo::<u32>(|_| Err(Error::CallbackFailed("compute")))
        })
        .unwrap();
    assert_eq!(rt.get(m), Err(Error::CallbackFailed("compute")));
    assert_eq!(rt.get(s), Err(Error::Poisoned));
    rt.verify_invariants().unwrap();
    rt.shutdown().unwrap();
    assert_eq!(
        rt.first_failure().as_deref(),
        Some(&Error::CallbackFailed("compute"))
    );
}
#[test]
fn optimized_shared_release_guards_last_drop_even_with_weak_references() {
    struct Resource(WeakRuntime, Rc<Cell<usize>>);
    impl Drop for Resource {
        fn drop(&mut self) {
            let rt = self.0.upgrade().unwrap();
            assert_eq!(rt.create_root(), Err(Error::Reentrancy));
            self.1.set(self.1.get() + 1);
        }
    }
    let (rt, root) = setup();
    let drops = Rc::new(Cell::new(0));
    let value = Rc::new(Resource(rt.downgrade(), drops.clone()));
    let weak = Rc::downgrade(&value);
    let s = rt
        .with_owner(root, |rt| rt.signal_with(value.clone(), |_, _| false))
        .unwrap();
    rt.retire(value);
    assert_eq!(drops.get(), 0);
    rt.dispose(s).unwrap();
    assert_eq!(drops.get(), 1);
    assert!(weak.upgrade().is_none());
    assert_eq!(rt.status(), RuntimeStatus::Healthy);
}
