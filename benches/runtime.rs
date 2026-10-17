#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/memo_owners.rs"]
mod memo_owners;
mod support;
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use fixture::{Fixture, Mode};
use reactive::{FlushOutcome, Runtime};
use std::{
    cell::Cell,
    hint::black_box,
    rc::Rc,
    time::{Duration, Instant},
};

fn runtime(c: &mut Criterion) {
    value_publication::<u64>(c, "no_drop");
    value_publication::<DropNumber>(c, "drop");
    explicit_value_reads(c);
    handle_access(c);
    isolated_writes::<u64>(c, "no_drop");
    isolated_writes::<DropNumber>(c, "drop");
    memo_owner_execution(c);
    let mut group = c.benchmark_group("fan_in_lazy_read");
    for mode in [Mode::Stable, Mode::Reorder, Mode::Duplicate, Mode::Nested] {
        for width in [0, 1, 2, 4, 8, 16, 32, 64, 128, 256] {
            let graph = Fixture::new(width, mode, false);
            graph.step(1);
            let mut value = 1;
            group.bench_function(BenchmarkId::new(mode.name(), width), |b| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        value += 1;
                        graph.driver.try_set(&graph.rt, value).unwrap(); // marking excluded
                        let start = Instant::now();
                        black_box(graph.memo.try_get_copy(&graph.rt).unwrap());
                        elapsed += start.elapsed();
                    }
                    elapsed
                })
            });
        }
    }
    group.finish();
    phases(c);
    for width in [8, 64, 256, 4096] {
        c.bench_function(&format!("effect_complete_contraction/{width}"), |b| {
            b.iter_batched_ref(
                || fixture::EffectContraction::new(width),
                |graph| {
                    let report = graph.rt.flush(Default::default()).unwrap();
                    assert_eq!(report.callbacks, 1);
                    assert_eq!(graph.output.get(), 0);
                },
                BatchSize::PerIteration,
            )
        });
    }
    for width in [64, 256, 1024, 4096] {
        for shape in ["fan_in", "fan_out", "independent"] {
            c.bench_function(&format!("shutdown_roots/{shape}/{width}"), |b| {
                b.iter_batched_ref(
                    || fixture::RootShutdown::new(width, shape),
                    |graph| {
                        graph.rt.shutdown().unwrap();
                    },
                    BatchSize::PerIteration,
                )
            });
        }
    }
    for width in [64, 256, 1024, 4096] {
        for (name, bulk) in [("individual", false), ("bulk", true)] {
            for reconcile in [false, true] {
                let preflight = fixture::SourceDisposal::new(width);
                preflight.dispose_all(bulk);
                preflight.reconcile();
                drop(preflight);
                let suffix = if reconcile { "and_reconcile" } else { "only" };
                c.bench_function(&format!("dispose_sources/{name}_{suffix}/{width}"), |b| {
                    b.iter_batched_ref(
                        || fixture::SourceDisposal::new(width),
                        |graph| {
                            graph.dispose_all(bulk);
                            if reconcile {
                                graph.reconcile();
                            }
                        },
                        BatchSize::PerIteration,
                    )
                });
            }
        }
    }
    for width in [8, 64, 256, 4096] {
        let graph = fixture::EqualityCut::new(width);
        let mut value = 0_u64;
        c.bench_function(&format!("equality_cut_flush/{width}"), |b| {
            b.iter_custom(|n| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..n {
                    value += 2;
                    graph.driver.try_set(&graph.rt, value).unwrap();
                    let start = Instant::now();
                    let report = graph.rt.flush(Default::default()).unwrap();
                    elapsed += start.elapsed();
                    assert_eq!(report.outcome, FlushOutcome::Stabilized);
                    assert_eq!(report.callbacks, 0);
                }
                elapsed
            })
        });
        graph.assert_cut();
    }
    for width in [8, 64, 256, 4096] {
        let (rt, _, driver, output) = fixture::direct_effect(width);
        let mut value = 0_u64;
        c.bench_function(&format!("effect_complete_fan_in/{width}"), |b| {
            b.iter_custom(|n| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..n {
                    value += 1;
                    driver.try_set(&rt, value).unwrap();
                    let start = Instant::now();
                    black_box(rt.flush(Default::default()).unwrap());
                    elapsed += start.elapsed();
                }
                elapsed
            })
        });
        assert_eq!(output.get(), value + (width * (width - 1) / 2) as u64);
        rt.shutdown().unwrap();
    }
    values(c);
    layout(c);
}

#[derive(Clone, PartialEq)]
struct DropNumber(u64);
impl From<u64> for DropNumber {
    fn from(value: u64) -> Self {
        Self(value)
    }
}
impl Drop for DropNumber {
    fn drop(&mut self) {}
}

// Equal-sized scalar values; DropNumber has an empty destructor so the
// difference probes runtime resource guards, not allocation or destructor work.
fn isolated_writes<T: PartialEq + From<u64> + 'static>(c: &mut Criterion, name: &str) {
    for mode in ["unconditional", "equal", "changed"] {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt
            .with_owner(root, |rt| {
                if mode == "unconditional" {
                    // The public API still calls a protected comparator; this is
                    // unconditional publication, not an unchecked setter.
                    rt.signal_with(T::from(7), |_, _| false)
                } else {
                    rt.signal(T::from(7))
                }
            })
            .unwrap();
        let mut value = 7_u64;
        input.try_set(&rt, T::from(7)).unwrap();
        assert!(input.try_with(&rt, |v| *v == T::from(7)).unwrap());
        c.bench_function(&format!("isolated_writes/{name}/{mode}"), |b| {
            b.iter(|| {
                if mode != "equal" {
                    value = value.wrapping_add(1);
                }
                black_box(input)
                    .try_set(black_box(&rt), black_box(T::from(value)))
                    .unwrap();
            })
        });
        assert!(input.try_with(&rt, |v| *v == T::from(value)).unwrap());
        rt.shutdown().unwrap();
    }
}

fn memo_owner_execution(c: &mut Criterion) {
    use memo_owners::{MemoOwners, Resources};
    for width in [1, 32] {
        for resources in [
            Resources::Empty,
            Resources::Cleanup,
            Resources::Node,
            Resources::Child,
        ] {
            MemoOwners::preflight(width, resources);
            let graph = MemoOwners::new(width);
            let mut value = 0_u64;
            c.bench_function(
                &format!("memo_owner_execution/{}/{width}", resources.name()),
                |b| {
                    b.iter_custom(|iterations| {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            graph.attach(resources);
                            value += 1;
                            let start = Instant::now();
                            black_box(graph.step(value));
                            elapsed += start.elapsed();
                        }
                        elapsed
                    })
                },
            );
            assert_eq!(
                graph.tail.try_get_copy(&graph.rt).unwrap(),
                value + width as u64
            );
            assert_eq!(
                graph.released.get(),
                if matches!(resources, Resources::Empty) {
                    0
                } else {
                    value * width as u64
                }
            );
        }
    }
}

fn value_publication<T: Clone + PartialEq + From<u64> + 'static>(c: &mut Criterion, name: &str) {
    for held in [false, true] {
        let suffix = if held { "held" } else { "unheld" };
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(T::from(0))).unwrap();
        let mut value = 0;
        c.bench_function(
            &format!("value_publication_v2/signal_{name}_{suffix}"),
            |b| {
                b.iter_custom(|n| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..n {
                        value += 1;
                        let previous = held.then(|| input.try_get_cloned(&rt).unwrap());
                        let candidate = T::from(value);
                        let start = Instant::now();
                        input.try_set(&rt, candidate).unwrap();
                        elapsed += start.elapsed();
                        if let Some(previous) = &previous {
                            assert!(*previous == T::from(value - 1));
                        }
                        drop(previous); // retained snapshot/destructor outside timer
                    }
                    elapsed
                })
            },
        );
        assert!(
            input
                .try_with(&rt, |value_ref| *value_ref == T::from(value))
                .unwrap()
        );
        rt.shutdown().unwrap();

        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let driver = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let memo = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| Ok(T::from(driver.try_get_copy(rt)?)))
            })
            .unwrap();
        memo.try_with(&rt, |_| ()).unwrap();
        let mut value = 0;
        c.bench_function(&format!("value_publication_v2/memo_{name}_{suffix}"), |b| {
            b.iter_custom(|n| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..n {
                    value += 1;
                    let previous = held.then(|| memo.try_get_cloned(&rt).unwrap());
                    driver.try_set(&rt, value).unwrap();
                    let start = Instant::now();
                    memo.try_with(&rt, |value| {
                        black_box(value);
                    })
                    .unwrap();
                    elapsed += start.elapsed();
                    if let Some(previous) = &previous {
                        assert!(*previous == T::from(value - 1));
                    }
                    drop(previous);
                }
                elapsed
            })
        });
        assert!(
            memo.try_with(&rt, |value_ref| *value_ref == T::from(value))
                .unwrap()
        );
        rt.shutdown().unwrap();
    }
}

fn explicit_value_reads(c: &mut Criterion) {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let scalar = rt.with_owner(root, |rt| rt.signal(7_u64)).unwrap();
    c.bench_function("value_reads/copy_u64", |b| {
        b.iter(|| black_box(scalar.try_get_copy(&rt).unwrap()))
    });
    for size in [16, 4096] {
        let value = rt
            .with_owner(root, |rt| rt.signal(vec![1_u64; size]))
            .unwrap();
        c.bench_function(&format!("value_reads/borrow_vec/{size}"), |b| {
            b.iter(|| value.try_with(&rt, |value| black_box(value.len())).unwrap())
        });
        c.bench_function(&format!("value_reads/clone_vec/{size}"), |b| {
            b.iter(|| black_box(value.try_get_cloned(&rt).unwrap()))
        });
        let shared = rt
            .with_owner(root, |rt| rt.signal(std::rc::Rc::new(vec![1_u64; size])))
            .unwrap();
        c.bench_function(&format!("value_reads/clone_rc_vec/{size}"), |b| {
            b.iter(|| black_box(shared.try_get_cloned(&rt).unwrap()))
        });
    }
    rt.shutdown().unwrap();
}

fn handle_access(c: &mut Criterion) {
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let signal = rt.with_owner(root, |rt| rt.signal(7_u64)).unwrap();
    let memo = rt
        .with_owner(root, |rt| rt.memo(move |rt| signal.try_get_copy(rt)))
        .unwrap();
    assert_eq!(memo.try_get_copy(&rt).unwrap(), 7);
    c.bench_function("handle_access/signal_validate", |b| {
        b.iter(|| black_box(signal).validate(black_box(&rt)).unwrap())
    });
    c.bench_function("handle_access/memo_validate", |b| {
        b.iter(|| black_box(memo).validate(black_box(&rt)).unwrap())
    });
    c.bench_function("handle_access/signal_copy", |b| {
        b.iter(|| black_box(black_box(signal).try_get_copy(black_box(&rt)).unwrap()))
    });
    c.bench_function("handle_access/memo_copy_clean", |b| {
        b.iter(|| black_box(black_box(memo).try_get_copy(black_box(&rt)).unwrap()))
    });
    for size in [64, 4096] {
        let values: Vec<_> = (0..size)
            .map(|value| rt.with_owner(root, |rt| rt.signal(value as u64)).unwrap())
            .collect();
        c.bench_function(&format!("handle_access/signal_sweep/{size}"), |b| {
            b.iter(|| {
                let mut sum = 0;
                for value in black_box(&values) {
                    sum += value.try_get_copy(black_box(&rt)).unwrap();
                }
                black_box(sum)
            })
        });
    }
    rt.shutdown().unwrap();
}

fn phases(c: &mut Criterion) {
    c.bench_function("clock_pair_control", |b| {
        b.iter_custom(|n| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..n {
                let start = Instant::now();
                elapsed += start.elapsed();
            }
            elapsed
        })
    });
    for width in [8, 64, 256, 4096] {
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let output = Rc::new(Cell::new(0));
        for _ in 0..width {
            let output = output.clone();
            rt.with_owner(root, |rt| {
                rt.effect(move |rt| {
                    output.set(input.try_get_copy(rt)?);
                    Ok(())
                })
            })
            .unwrap();
        }
        rt.flush(Default::default()).unwrap();
        let mut value = 0;
        c.bench_function(&format!("mark_only/{width}"), |b| {
            b.iter_custom(|n| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..n {
                    value += 1;
                    let start = Instant::now();
                    input.try_set(&rt, value).unwrap();
                    elapsed += start.elapsed();
                    assert_eq!(
                        rt.flush(Default::default()).unwrap().outcome,
                        FlushOutcome::Stabilized
                    );
                }
                elapsed
            })
        });
        c.bench_function(&format!("effect_flush/{width}"), |b| {
            b.iter_custom(|n| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..n {
                    value += 1;
                    input.try_set(&rt, value).unwrap();
                    let start = Instant::now();
                    black_box(rt.flush(Default::default()).unwrap());
                    elapsed += start.elapsed();
                }
                elapsed
            })
        });
        c.bench_function(&format!("has_pending/{width}"), |b| {
            b.iter(|| black_box(black_box(&rt).has_pending_effects()))
        });
        rt.shutdown().unwrap();

        // All upstream parity memos are already Clean/equal before timing:
        // the terminal memo performs only Check validation, no callback.
        let rt = Runtime::new();
        let root = rt.create_root().unwrap();
        let input = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
        let leaves: Vec<_> = (0..width)
            .map(|_| {
                rt.with_owner(root, |rt| {
                    rt.memo(move |rt| Ok(input.try_get_copy(rt)? % 2))
                })
                .unwrap()
            })
            .collect();
        let captured = leaves.clone();
        let terminal = rt
            .with_owner(root, |rt| {
                rt.memo(move |rt| {
                    let mut sum = 0;
                    for leaf in &captured {
                        sum += leaf.try_get_copy(rt)?;
                    }
                    Ok(sum)
                })
            })
            .unwrap();
        terminal.try_get_copy(&rt).unwrap();
        let mut value = 0;
        c.bench_function(&format!("check_clean_sources/{width}"), |b| {
            b.iter_custom(|n| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..n {
                    value += 2;
                    input.try_set(&rt, value).unwrap();
                    for leaf in &leaves {
                        leaf.try_peek_copy(&rt).unwrap();
                    }
                    let start = Instant::now();
                    black_box(terminal.try_get_copy(&rt).unwrap());
                    elapsed += start.elapsed();
                }
                elapsed
            })
        });
        rt.shutdown().unwrap();
    }
    for size in [64, 256, 4096] {
        c.bench_function(&format!("sort_round_keys/{size}"), |b| {
            b.iter_batched(
                || (0..size).rev().map(|i| (i % 4, i, i)).collect::<Vec<_>>(),
                |mut keys| {
                    keys.sort_unstable_by_key(|&(depth, sequence, _)| (depth, sequence));
                    black_box(keys);
                },
                BatchSize::PerIteration,
            )
        });
        // Queue cancellation/compaction plus teardown (public disposal contract).
        c.bench_function(&format!("dispose_queued/{size}"), |b| {
            b.iter_batched_ref(
                || {
                    let rt = Runtime::new();
                    let root = rt.create_root().unwrap();
                    rt.with_owner(root, |rt| {
                        for _ in 0..size {
                            rt.effect(|_| Ok(()))?;
                        }
                        Ok(())
                    })
                    .unwrap();
                    (rt, root)
                },
                |(rt, root)| black_box(rt.dispose(*root).unwrap()),
                BatchSize::PerIteration,
            )
        });
        c.bench_function(&format!("queue_churn/{size}"), |b| {
            b.iter_batched_ref(
                || {
                    let rt = Runtime::new();
                    let root = rt.create_root().unwrap();
                    rt.with_owner(root, |rt| rt.effect(|_| Ok(()))).unwrap();
                    (rt, root)
                },
                |(rt, root)| {
                    for _ in 0..size {
                        let effect = rt.with_owner(*root, |rt| rt.effect(|_| Ok(()))).unwrap();
                        rt.dispose(effect).unwrap();
                    }
                    black_box(rt.has_pending_effects());
                },
                BatchSize::PerIteration,
            )
        });
    }
}

fn values(c: &mut Criterion) {
    let snapshot = Rc::new(vec![0_u64; 1024]);
    c.bench_function("value_cost/rc_clone", |b| {
        b.iter(|| black_box(Rc::clone(black_box(&snapshot))))
    });
    c.bench_function("value_cost/vec_clone", |b| {
        b.iter(|| black_box(black_box(&snapshot).as_ref().clone()))
    });
    c.bench_function("value_cost/rc_candidate_u64", |b| {
        b.iter(|| black_box(Rc::new(black_box(1_u64))))
    });
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let value = rt
        .with_owner(root, |rt| rt.signal(vec![0_u64; 1024]))
        .unwrap();
    c.bench_function("value_cost/update_vec", |b| {
        b.iter(|| value.try_update_cloned(&rt, |value| value[0] += 1).unwrap())
    });
    let scalar = rt.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    let mut counter = 0;
    c.bench_function("value_cost/set_u64", |b| {
        b.iter(|| {
            counter += 1;
            scalar.try_set(&rt, counter).unwrap();
        })
    });
    rt.shutdown().unwrap();
}

// Experimental layout surrogate only. Same chain/star edges and hot operations;
// cold data models 256 untouched bytes/node. No core layout specialization.
#[derive(Clone)]
struct Hot {
    source: usize,
    revision: u64,
}
#[derive(Clone)]
struct Combined {
    hot: Hot,
    cold: [u64; 32],
}
fn layout(c: &mut Criterion) {
    for shape in ["chain", "fan_out"] {
        for size in [64_usize, 256, 4096] {
            let hot: Vec<_> = (0..size)
                .map(|i| Hot {
                    source: if shape == "chain" {
                        i.saturating_sub(1)
                    } else {
                        0
                    },
                    revision: 0,
                })
                .collect();
            let mut aos: Vec<_> = hot
                .iter()
                .cloned()
                .map(|hot| Combined { hot, cold: [0; 32] })
                .collect();
            let mut separated = hot;
            let cold = vec![[0_u64; 32]; size];
            c.bench_function(&format!("layout_surrogate/{shape}_aos/{size}"), |b| {
                b.iter(|| {
                    aos[0].hot.revision += 1;
                    for i in 1..size {
                        aos[i].hot.revision = aos[aos[i].hot.source].hot.revision;
                    }
                    black_box(aos.last().unwrap().hot.revision);
                    black_box(&aos[0].cold);
                })
            });
            c.bench_function(&format!("layout_surrogate/{shape}_hot_cold/{size}"), |b| {
                b.iter(|| {
                    separated[0].revision += 1;
                    for i in 1..size {
                        separated[i].revision = separated[separated[i].source].revision;
                    }
                    black_box(separated.last().unwrap().revision);
                    black_box(&cold);
                })
            });
            assert!(
                aos.iter()
                    .all(|node| node.hot.revision == aos[0].hot.revision)
            );
            assert!(
                separated
                    .iter()
                    .all(|node| node.revision == separated[0].revision)
            );
        }
    }
}
criterion_group! { name = benches; config = support::criterion(); targets = runtime }
criterion_main!(benches);
