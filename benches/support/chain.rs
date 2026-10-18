use super::{CallbackView, Engine, Graph, Leptos, Local, Shape, Sycamore};
#[path = "fixture.rs"]
mod fixture;
use criterion::{BenchmarkId, Criterion};
use std::{
    cell::Cell,
    hint::black_box,
    rc::Rc,
    time::{Duration, Instant},
};

const SIZES: [usize; 7] = [1, 8, 32, 64, 128, 256, 384];

fn count_preflight<E: Engine + 'static>(size: usize, cut: bool) {
    let engine = E::new();
    let input = engine.signal(0);
    let memos = Rc::new(Cell::new(0));
    let effects = Rc::new(Cell::new(0));
    let captured = memos.clone();
    let mut tail = engine.memo(move |e| {
        captured.set(captured.get() + 1);
        let value = e.get(input);
        if cut { 1 } else { value + 1 }
    });
    for _ in 1..size {
        let previous = tail;
        let captured = memos.clone();
        tail = engine.memo(move |e| {
            captured.set(captured.get() + 1);
            e.read(previous) + 1
        });
    }
    let output = Rc::new(Cell::new(0));
    let captured = output.clone();
    let calls = effects.clone();
    engine.effect(move |e| {
        calls.set(calls.get() + 1);
        captured.set(e.read(tail));
    });
    engine.settle();
    for value in 1..=4 {
        memos.set(0);
        effects.set(0);
        engine.set(input, value);
        engine.settle();
        assert_eq!(memos.get(), if cut { 1 } else { size });
        assert_eq!(effects.get(), usize::from(!cut));
        assert_eq!(output.get(), size as u64 + if cut { 0 } else { value });
    }
}

fn scaling<E: Engine + 'static>(c: &mut Criterion) {
    let mut group = c.benchmark_group("chain_scaling");
    for size in SIZES {
        count_preflight::<E>(size, false);
        count_preflight::<E>(size, true);
        let preflight = Graph::<E>::new::<true>(Shape::Chain, size);
        preflight.validate();
        drop(preflight);
        let graph = Graph::<E>::new::<false>(Shape::Chain, size);
        let mut value = 0_u64;
        group.bench_function(BenchmarkId::new(E::NAME, size), |b| {
            b.iter(|| {
                value += 1;
                black_box(graph.step(value));
            })
        });
    }
    group.finish();
}

pub(super) fn benchmarks(c: &mut Criterion) {
    scaling::<Local>(c);
    scaling::<Sycamore>(c);
    scaling::<Leptos>(c);
    for size in SIZES {
        for mode in [
            "native_total",
            "mark_only",
            "update_only",
            "ordered_refresh",
            "equality_cut",
            "lazy_read",
        ] {
            let cut = mode == "equality_cut";
            let lazy = mode == "lazy_read";
            let graph = fixture::NativeChain::new(size, !lazy, cut);
            graph.input.try_set(&graph.rt, 1).unwrap();
            if lazy {
                assert_eq!(graph.read_tail(), size as u64 + 1);
            } else {
                let preflight = graph.rt.flush(Default::default()).unwrap();
                assert_eq!(preflight.callbacks, usize::from(!cut));
                assert_eq!(graph.output.get(), size as u64 + u64::from(!cut));
            }
            let mut value = 1_u64;
            c.bench_function(&format!("chain_phases/{mode}/{size}"), |b| {
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        value += 1;
                        if mode == "update_only" {
                            graph.input.try_set(&graph.rt, value).unwrap();
                        }
                        let start = Instant::now();
                        if mode != "update_only" {
                            graph.input.try_set(&graph.rt, value).unwrap();
                        }
                        if mode != "mark_only" {
                            if mode == "ordered_refresh" {
                                for memo in &graph.memos {
                                    black_box(memo.try_get_copy(&graph.rt).unwrap());
                                }
                            }
                            if lazy {
                                black_box(graph.read_tail());
                            } else {
                                black_box(graph.rt.flush(Default::default()).unwrap());
                            }
                        }
                        elapsed += start.elapsed();
                        if mode == "mark_only" {
                            graph.rt.flush(Default::default()).unwrap();
                        }
                    }
                    elapsed
                })
            });
            assert_eq!(
                if lazy {
                    graph.read_tail()
                } else {
                    graph.output.get()
                },
                size as u64 + if cut { 0 } else { value }
            );
        }
    }
}
