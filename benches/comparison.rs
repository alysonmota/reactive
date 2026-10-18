use std::{cell::Cell, hint::black_box, rc::Rc};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use leptos_reactive::{SignalGet, SignalSet};
#[path = "support/chain.rs"]
mod chain;
#[allow(dead_code)]
#[path = "support/contracts.rs"]
mod contracts;
mod support;

/// Adapters preserve each library's native scheduling. Only our runtime needs
/// an explicit flush; Leptos render effects and Sycamore effects are synchronous.
trait CallbackView {
    type Signal: Copy + 'static;
    type Memo: Copy + 'static;
    fn get(&self, signal: Self::Signal) -> u64;
    fn read(&self, memo: Self::Memo) -> u64;
}
struct RuntimeView<'a, R>(&'a R);
impl CallbackView for RuntimeView<'_, reactive::Runtime> {
    type Signal = reactive::Signal<u64>;
    type Memo = reactive::Memo<u64>;
    fn get(&self, signal: Self::Signal) -> u64 {
        signal.try_get_copy(self.0).unwrap()
    }
    fn read(&self, memo: Self::Memo) -> u64 {
        memo.try_get_copy(self.0).unwrap()
    }
}
impl CallbackView for RuntimeView<'_, typed_runtime_prototype::Runtime> {
    type Signal = typed_runtime_prototype::Handle<u64>;
    type Memo = typed_runtime_prototype::Handle<u64>;
    fn get(&self, signal: Self::Signal) -> u64 {
        self.0.get(signal).unwrap()
    }
    fn read(&self, memo: Self::Memo) -> u64 {
        self.0.get(memo).unwrap()
    }
}
struct SycamoreView;
impl CallbackView for SycamoreView {
    type Signal = sycamore_reactive::Signal<u64>;
    type Memo = sycamore_reactive::ReadSignal<u64>;
    fn get(&self, signal: Self::Signal) -> u64 {
        signal.get()
    }
    fn read(&self, memo: Self::Memo) -> u64 {
        memo.get()
    }
}
struct LeptosView;
impl CallbackView for LeptosView {
    type Signal = leptos_reactive::RwSignal<u64>;
    type Memo = leptos_reactive::Memo<u64>;
    fn get(&self, signal: Self::Signal) -> u64 {
        signal.get()
    }
    fn read(&self, memo: Self::Memo) -> u64 {
        memo.get()
    }
}
trait Engine: Sized {
    const NAME: &'static str;
    type Signal: Copy + 'static;
    type Memo: Copy + 'static;
    type View<'a>: CallbackView<Signal = Self::Signal, Memo = Self::Memo>;
    fn new() -> Self;
    fn signal(&self, value: u64) -> Self::Signal;
    fn get(&self, signal: Self::Signal) -> u64;
    fn set(&self, signal: Self::Signal, value: u64);
    fn memo(&self, f: impl for<'a> Fn(&Self::View<'a>) -> u64 + 'static) -> Self::Memo;
    fn effect(&self, f: impl for<'a> Fn(&Self::View<'a>) + 'static);
    fn settle(&self);
    fn reset_work(&self) {}
    fn work(&self) -> Option<serde_json::Value> {
        None
    }
}

struct Local {
    runtime: reactive::Runtime,
    owner: Option<reactive::OwnerHandle>,
}

impl Engine for Local {
    const NAME: &'static str = "reactive";
    type Signal = reactive::Signal<u64>;
    type Memo = reactive::Memo<u64>;
    type View<'a> = RuntimeView<'a, reactive::Runtime>;
    fn new() -> Self {
        let runtime = reactive::Runtime::new();
        let owner = Some(runtime.create_root().unwrap());
        Self { runtime, owner }
    }
    fn signal(&self, value: u64) -> Self::Signal {
        self.runtime
            .with_owner(self.owner.unwrap(), |rt| rt.signal(value))
            .unwrap()
    }
    fn get(&self, signal: Self::Signal) -> u64 {
        signal.try_get_copy(&self.runtime).unwrap()
    }
    fn set(&self, signal: Self::Signal, value: u64) {
        signal.try_set(&self.runtime, value).unwrap();
    }
    fn memo(&self, f: impl for<'a> Fn(&Self::View<'a>) -> u64 + 'static) -> Self::Memo {
        self.runtime
            .with_owner(self.owner.unwrap(), |rt| {
                rt.memo_infallible(move |rt| f(&RuntimeView(rt)))
            })
            .unwrap()
    }
    fn effect(&self, f: impl for<'a> Fn(&Self::View<'a>) + 'static) {
        self.runtime
            .with_owner(self.owner.unwrap(), |rt| {
                rt.effect_infallible(move |rt| f(&RuntimeView(rt)))
            })
            .unwrap();
    }
    fn settle(&self) {
        let report = self.runtime.flush(Default::default()).unwrap();
        assert_eq!(report.outcome, reactive::FlushOutcome::Stabilized);
    }
    fn reset_work(&self) {
        #[cfg(feature = "diagnostics")]
        self.runtime.reset_diagnostic_counters().unwrap();
    }
    fn work(&self) -> Option<serde_json::Value> {
        #[cfg(feature = "diagnostics")]
        {
            let c = self.runtime.diagnostics().unwrap().counters;
            Some(
                serde_json::json!({"memo_executions": c.memo_executions, "effect_executions": c.effect_executions,
                "source_checks": c.source_checks, "mark_visits": c.mark_visits}),
            )
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            None
        }
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        if self.owner.is_some() {
            self.runtime.shutdown().unwrap();
        }
    }
}

struct Typed {
    runtime: typed_runtime_prototype::Runtime,
    owner: Option<typed_runtime_prototype::OwnerHandle>,
}
impl Engine for Typed {
    const NAME: &'static str = "typed_research";
    type Signal = typed_runtime_prototype::Handle<u64>;
    type Memo = typed_runtime_prototype::Handle<u64>;
    type View<'a> = RuntimeView<'a, typed_runtime_prototype::Runtime>;
    fn new() -> Self {
        let runtime = contracts::evaluation_strategy::runtime();
        let owner = Some(runtime.create_root().unwrap());
        Self { runtime, owner }
    }
    fn signal(&self, value: u64) -> Self::Signal {
        self.runtime
            .with_owner(self.owner.unwrap(), |rt| rt.signal(value))
            .unwrap()
    }
    fn get(&self, signal: Self::Signal) -> u64 {
        self.runtime.get(signal).unwrap()
    }
    fn set(&self, signal: Self::Signal, value: u64) {
        self.runtime.set(signal, value).unwrap();
    }
    fn memo(&self, f: impl for<'a> Fn(&Self::View<'a>) -> u64 + 'static) -> Self::Memo {
        self.runtime
            .with_owner(self.owner.unwrap(), |rt| {
                rt.memo(move |rt| Ok(f(&RuntimeView(rt))))
            })
            .unwrap()
    }
    fn effect(&self, f: impl for<'a> Fn(&Self::View<'a>) + 'static) {
        self.runtime
            .with_owner(self.owner.unwrap(), |rt| {
                rt.effect(move |rt| {
                    f(&RuntimeView(rt));
                    Ok(())
                })
            })
            .unwrap();
    }
    fn settle(&self) {
        self.runtime.flush().unwrap();
    }
    fn reset_work(&self) {
        self.runtime.reset_work();
    }
    fn work(&self) -> Option<serde_json::Value> {
        self.runtime.work().map(|c|serde_json::json!({"memo_executions": c.memo_executions, "effect_executions": c.effect_executions,
            "source_checks": c.source_checks, "mark_visits": c.mark_visits}))
    }
}
impl Drop for Typed {
    fn drop(&mut self) {
        if self.owner.is_some() {
            self.runtime.shutdown().unwrap();
        }
    }
}

struct Sycamore(Option<sycamore_reactive::RootHandle>);
impl Engine for Sycamore {
    const NAME: &'static str = "sycamore";
    type Signal = sycamore_reactive::Signal<u64>;
    type Memo = sycamore_reactive::ReadSignal<u64>;
    type View<'a> = SycamoreView;
    fn new() -> Self {
        Self(Some(sycamore_reactive::create_root(|| {})))
    }
    fn signal(&self, value: u64) -> Self::Signal {
        self.0
            .as_ref()
            .unwrap()
            .run_in(|| sycamore_reactive::create_signal(value))
    }
    fn get(&self, signal: Self::Signal) -> u64 {
        signal.get()
    }
    fn set(&self, signal: Self::Signal, value: u64) {
        signal.set(value);
    }
    fn memo(&self, f: impl for<'a> Fn(&Self::View<'a>) -> u64 + 'static) -> Self::Memo {
        // selector, not create_memo: use PartialEq like the other engines.
        self.0
            .as_ref()
            .unwrap()
            .run_in(|| sycamore_reactive::create_selector(move || f(&SycamoreView)))
    }
    fn effect(&self, f: impl for<'a> Fn(&Self::View<'a>) + 'static) {
        self.0
            .as_ref()
            .unwrap()
            .run_in(|| sycamore_reactive::create_effect(move || f(&SycamoreView)));
    }
    fn settle(&self) {}
}
impl Drop for Sycamore {
    fn drop(&mut self) {
        if let Some(root) = &self.0 {
            root.dispose();
        }
    }
}

struct Leptos(Option<leptos_reactive::RuntimeId>);
impl Engine for Leptos {
    const NAME: &'static str = "leptos";
    type Signal = leptos_reactive::RwSignal<u64>;
    type Memo = leptos_reactive::Memo<u64>;
    type View<'a> = LeptosView;
    fn new() -> Self {
        Self(Some(leptos_reactive::create_runtime()))
    }
    fn signal(&self, value: u64) -> Self::Signal {
        leptos_reactive::create_rw_signal(value)
    }
    fn get(&self, signal: Self::Signal) -> u64 {
        signal.get()
    }
    fn set(&self, signal: Self::Signal, value: u64) {
        signal.set(value);
    }
    fn memo(&self, f: impl for<'a> Fn(&Self::View<'a>) -> u64 + 'static) -> Self::Memo {
        leptos_reactive::create_memo(move |_| f(&LeptosView))
    }
    fn effect(&self, f: impl for<'a> Fn(&Self::View<'a>) + 'static) {
        leptos_reactive::create_render_effect(move |_| f(&LeptosView));
    }
    fn settle(&self) {}
}
impl Drop for Leptos {
    fn drop(&mut self) {
        if let Some(runtime) = self.0 {
            runtime.dispose();
        }
    }
}

#[derive(Clone, Copy)]
enum Shape {
    Chain,
    Diamond,
    FanOut,
    Conditional,
}
impl Shape {
    fn name(self) -> &'static str {
        match self {
            Self::Chain => "chain",
            Self::Diamond => "diamond",
            Self::FanOut => "fan_out",
            Self::Conditional => "conditional",
        }
    }
}

struct Graph<E: Engine> {
    engine: E,
    input: E::Signal,
    output: Rc<Cell<u64>>,
    callbacks: Rc<Cell<u64>>,
    shape: Shape,
    size: usize,
}
impl<E: Engine + 'static> Graph<E> {
    fn new<const COUNT: bool>(shape: Shape, size: usize) -> Self {
        let engine = E::new();
        let input = engine.signal(0);
        let output = Rc::new(Cell::new(0));
        let callbacks = Rc::new(Cell::new(0));
        let leaves = match shape {
            Shape::Chain => {
                let mut tail = engine.memo(move |e| e.get(input) + 1);
                for _ in 1..size {
                    let previous = tail;
                    tail = engine.memo(move |e| e.read(previous) + 1);
                }
                vec![tail]
            }
            Shape::Diamond => {
                let branches: Vec<_> = (0..size)
                    .map(|i| engine.memo(move |e| e.get(input) + i as u64))
                    .collect();
                vec![engine.memo(move |e| branches.iter().map(|&memo| e.read(memo)).sum())]
            }
            Shape::FanOut => (0..size)
                .map(|_| engine.memo(move |e| e.get(input)))
                .collect(),
            Shape::Conditional => {
                let a = engine.signal(1);
                let b = engine.signal(2);
                let branches: Vec<_> = (0..size)
                    .map(|i| {
                        engine.memo(move |e| {
                            let selected = if e.get(input).is_multiple_of(2) { a } else { b };
                            e.get(selected) + i as u64
                        })
                    })
                    .collect();
                vec![engine.memo(move |e| branches.iter().map(|&memo| e.read(memo)).sum())]
            }
        };
        for leaf in leaves {
            let output = output.clone();
            let callbacks = callbacks.clone();
            engine.effect(move |e| {
                output.set(e.read(leaf));
                if COUNT {
                    callbacks.set(callbacks.get() + 1);
                }
            });
        }
        engine.settle();
        Self {
            engine,
            input,
            output,
            callbacks,
            shape,
            size,
        }
    }
    fn step(&self, value: u64) -> u64 {
        self.engine.set(self.input, value);
        self.engine.settle();
        self.output.get()
    }
    fn expected(&self, value: u64) -> u64 {
        match self.shape {
            Shape::Chain => value + self.size as u64,
            Shape::Diamond => self.size as u64 * value + (self.size * (self.size - 1) / 2) as u64,
            Shape::FanOut => value,
            Shape::Conditional => {
                self.size as u64 * (1 + value % 2) + (self.size * (self.size - 1) / 2) as u64
            }
        }
    }
    fn validate(&self) {
        for value in 1..=4 {
            let before = self.callbacks.get();
            assert_eq!(self.step(value), self.expected(value));
            let expected_calls = match self.shape {
                Shape::FanOut => self.size as u64,
                _ => 1,
            };
            assert_eq!(self.callbacks.get() - before, expected_calls);
        }
    }
}

fn signals<E: Engine>(c: &mut Criterion) {
    let engine = E::new();
    let signal = engine.signal(0);
    let mut group = c.benchmark_group("signal_unobserved");
    group.bench_function(BenchmarkId::new("read", E::NAME), |b| {
        b.iter(|| black_box(engine.get(black_box(signal))))
    });
    let mut value = 0_u64;
    group.bench_function(BenchmarkId::new("write_changed", E::NAME), |b| {
        b.iter(|| {
            value = value.wrapping_add(1);
            engine.set(signal, black_box(value));
        })
    });
    engine.set(signal, 42);
    group.bench_function(BenchmarkId::new("write_equal_native", E::NAME), |b| {
        b.iter(|| engine.set(signal, black_box(42)))
    });
    group.finish();
}

fn graphs<E: Engine + 'static>(c: &mut Criterion) {
    for shape in [
        Shape::Chain,
        Shape::Diamond,
        Shape::FanOut,
        Shape::Conditional,
    ] {
        let mut group = c.benchmark_group(format!("{}_write_to_observed", shape.name()));
        for size in [8, 64, 256] {
            // Independent correctness preflight; counters stay out of timed closures.
            let graph = Graph::<E>::new::<true>(shape, size);
            graph.validate();
            drop(graph);
            let graph = Graph::<E>::new::<false>(shape, size);
            let mut value = 0_u64;
            group.bench_with_input(BenchmarkId::new(E::NAME, size), &size, |b, _| {
                b.iter(|| {
                    value = value.wrapping_add(1);
                    black_box(graph.step(black_box(value)))
                })
            });
        }
        group.finish();
    }
}

fn comparison(c: &mut Criterion) {
    #[cfg(not(panic = "abort"))]
    if std::env::var_os("REACTIVE_REQUIRE_ABORT").is_some() {
        panic!("benchmark baseline requires effective panic=abort");
    }
    if std::env::var_os("REACTIVE_CHAIN_INVESTIGATION").is_some() {
        chain::benchmarks(c);
        return;
    }
    signals::<Local>(c);
    signals::<Sycamore>(c);
    signals::<Leptos>(c);
    graphs::<Local>(c);
    graphs::<Sycamore>(c);
    graphs::<Leptos>(c);
    if std::env::var_os("REACTIVE_TYPED_PROTOTYPE").is_some() {
        signals::<Typed>(c);
        graphs::<Typed>(c);
        cut::<Local>(c);
        cut::<Typed>(c);
        cut::<Sycamore>(c);
        cut::<Leptos>(c);
        contracts::benchmarks(c);
    }
}

fn cut<E: Engine + 'static>(c: &mut Criterion) {
    let engine = E::new();
    let signal = engine.signal(0);
    let mut tail = engine.memo(move |e| {
        e.get(signal);
        1
    });
    for _ in 1..256 {
        let previous = tail;
        tail = engine.memo(move |e| e.read(previous) + 1);
    }
    let output = Rc::new(Cell::new(0));
    let captured = output.clone();
    engine.effect(move |e| captured.set(e.read(tail)));
    engine.settle();
    let mut value = 0;
    c.bench_function(&format!("p1p2_cut/{}/256", E::NAME), |b| {
        b.iter(|| {
            value += 1;
            engine.set(signal, value);
            engine.settle();
            black_box(output.get());
        })
    });
    assert_eq!(output.get(), 256);
}

#[allow(dead_code)]
pub(crate) fn p1p2_memory(
    allocator: &stats_alloc::StatsAlloc<std::alloc::System>,
) -> serde_json::Value {
    #[cfg(not(panic = "abort"))]
    if std::env::var_os("REACTIVE_REQUIRE_ABORT").is_some() {
        panic!("benchmark baseline requires effective panic=abort");
    }
    fn measure<E: Engine + 'static>(
        allocator: &stats_alloc::StatsAlloc<std::alloc::System>,
        shape: Shape,
    ) -> serde_json::Value {
        let before = allocator.stats();
        let graph = Graph::<E>::new::<true>(shape, 256);
        graph.validate();
        for value in 5..=16 {
            assert_eq!(graph.step(value), graph.expected(value));
        }
        let live = allocator.stats() - before;
        graph.engine.reset_work();
        let start = allocator.stats();
        for value in 17..=144 {
            assert_eq!(graph.step(value), graph.expected(value));
        }
        let steady = allocator.stats() - start;
        let observer_start = allocator.stats();
        let work = graph.engine.work();
        let observer_cost = allocator.stats() - observer_start;
        if E::NAME == "typed_research" || E::NAME == "reactive" {
            assert_eq!((steady.allocations, steady.reallocations), (0, 0));
        }
        let callbacks = graph.callbacks.get();
        drop(graph);
        let total = allocator.stats() - before - observer_cost;
        serde_json::json!({"engine": E::NAME, "shape": shape.name(), "size": 256,
            "live_requested_bytes": live.bytes_allocated as i64 - live.bytes_deallocated as i64,
            "construction_and_warmup_allocations": live.allocations,
            "steady_allocations": steady.allocations, "steady_reallocations": steady.reallocations,
            "steady_allocated_bytes": steady.bytes_allocated, "effect_calls_including_warmup": callbacks,
            "after_drop_net_bytes": total.bytes_allocated as i64 - total.bytes_deallocated as i64, "work": work})
    }
    let mut rows = Vec::new();
    for shape in [
        Shape::Chain,
        Shape::Diamond,
        Shape::FanOut,
        Shape::Conditional,
    ] {
        rows.push(measure::<Local>(allocator, shape));
        rows.push(measure::<Typed>(allocator, shape));
        rows.push(measure::<Sycamore>(allocator, shape));
        rows.push(measure::<Leptos>(allocator, shape));
    }
    let resources = contracts::memory(allocator);
    serde_json::json!({"rows": rows, "prototype_layout": typed_runtime_prototype::Runtime::layout(),"owner_resources":resources,
        "scope": "Typed research core with explicit ownership, cleanups, writable effects, snapshot rounds, budgets, batching and hooks; not full API conformance"})
}

#[cfg(test)]
mod p1p2_tests {
    use super::*;
    #[test]
    fn shared_graphs_match_baseline_and_references() {
        for shape in [
            Shape::Chain,
            Shape::Diamond,
            Shape::FanOut,
            Shape::Conditional,
        ] {
            for size in [1, 8, 64, 256] {
                Graph::<Local>::new::<true>(shape, size).validate();
                Graph::<Typed>::new::<true>(shape, size).validate();
                Graph::<Sycamore>::new::<true>(shape, size).validate();
                Graph::<Leptos>::new::<true>(shape, size).validate();
            }
        }
    }

    #[allow(dead_code)] // Criterion's harness=false also compiles cfg(test).
    fn trace<E: Engine + 'static>(mut seed: u64) -> Vec<(u64, u64)> {
        let e = E::new();
        let control = e.signal(0);
        let a = e.signal(1);
        let b = e.signal(2);
        let branch = e.memo(move |e| {
            if e.get(control) % 2 == 0 {
                e.get(a) % 7
            } else {
                e.get(b) % 7
            }
        });
        let left = e.memo(move |e| e.read(branch) + 1);
        let right = e.memo(move |e| e.read(branch) * 2);
        let tail = e.memo(move |e| e.read(left) + e.read(right));
        let output = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let captured = output.clone();
        let count = calls.clone();
        e.effect(move |e| {
            captured.set(e.read(tail));
            count.set(count.get() + 1);
        });
        e.settle();
        let mut values = [0, 1, 2];
        let signals = [control, a, b];
        let mut log = Vec::new();
        for _ in 0..512 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = (seed >> 32) as usize % 3;
            let value = seed % 23;
            values[index] = value;
            e.set(signals[index], value);
            e.settle();
            let expected = values[if values[0] % 2 == 0 { 1 } else { 2 }] % 7 * 3 + 1;
            assert_eq!(output.get(), expected);
            log.push((output.get(), calls.get()));
        }
        log
    }
    #[test]
    fn seeded_dynamic_reconvergent_replay_matches_all_engines() {
        for seed in [1, 0xdeadbeef, 0x123456789abcdef0] {
            let baseline = trace::<Local>(seed);
            assert_eq!(trace::<Typed>(seed), baseline);
            assert_eq!(trace::<Sycamore>(seed), baseline);
            assert_eq!(trace::<Leptos>(seed), baseline);
        }
    }
}
criterion_group! { name = benches; config = support::criterion(); targets = comparison }
criterion_main!(benches);
