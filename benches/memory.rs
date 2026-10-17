#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/memo_owners.rs"]
mod memo_owners;
use fixture::{Fixture, Mode};
use reactive::{Runtime, RuntimeDiagnostics};
use serde_json::{Value, json};
use stats_alloc::{INSTRUMENTED_SYSTEM, Stats, StatsAlloc};
use std::{alloc::System, cell::Cell, hint::black_box, rc::Rc};

// Allocator instrumentation exists only in this separate benchmark executable;
// no project-owned unsafe allocator or wrapper in timing binaries.
#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn stats(s: Stats) -> Value {
    json!({"allocations": s.allocations, "deallocations": s.deallocations, "reallocations": s.reallocations,
        "bytes_allocated": s.bytes_allocated, "bytes_deallocated": s.bytes_deallocated,
        "net_requested_bytes": s.bytes_allocated as i64 - s.bytes_deallocated as i64,
        "bytes_reallocated": s.bytes_reallocated})
}

fn memo_owner_work(width: usize, resources: memo_owners::Resources) -> Value {
    use memo_owners::{MemoOwners, Resources};
    MemoOwners::preflight(width, resources);
    let origin = GLOBAL.stats();
    let graph = MemoOwners::new(width);
    for value in 1..=8 {
        graph.attach(resources);
        assert_eq!(graph.step(value), value + width as u64);
    }
    graph.rt.reset_diagnostic_counters().unwrap();
    let mut steady = Stats::default();
    for value in 9..=136 {
        graph.attach(resources); // Host resource creation excluded from the window.
        let before = GLOBAL.stats();
        black_box(graph.step(value));
        let delta = GLOBAL.stats() - before;
        steady.allocations += delta.allocations;
        steady.reallocations += delta.reallocations;
        steady.deallocations += delta.deallocations;
        steady.bytes_allocated += delta.bytes_allocated;
        steady.bytes_deallocated += delta.bytes_deallocated;
        steady.bytes_reallocated += delta.bytes_reallocated;
    }
    let work = graph.rt.diagnostics().unwrap();
    assert_eq!(work.counters.memo_executions, width as u64 * 128);
    let releases = graph.released.get();
    assert_eq!(
        releases,
        if matches!(resources, Resources::Empty) {
            0
        } else {
            136 * width as u64
        }
    );
    if matches!(resources, Resources::Empty) {
        assert_eq!((steady.allocations, steady.reallocations), (0, 0));
    }
    drop(graph);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "memo_owner_execution", "width": width, "resources": resources.name(),
        "iterations": 128, "steady_state": stats(steady), "work": diagnostics(&work),
        "resources_released_including_warmup": releases, "after_runtime_drop": stats(released)})
}

#[derive(PartialEq)]
struct WriteDropNumber(u64);
impl From<u64> for WriteDropNumber {
    fn from(value: u64) -> Self {
        Self(value)
    }
}
impl Drop for WriteDropNumber {
    fn drop(&mut self) {}
}

fn isolated_writes<T: PartialEq + From<u64> + 'static>(kind: &str, mode: &str) -> Value {
    let origin = GLOBAL.stats();
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    let input = rt
        .with_owner(root, |rt| {
            if mode == "unconditional" {
                rt.signal_with(T::from(7), |_, _| false)
            } else {
                rt.signal(T::from(7))
            }
        })
        .unwrap();
    let mut value = 7;
    for _ in 0..8 {
        if mode != "equal" {
            value += 1;
        }
        input.try_set(&rt, T::from(value)).unwrap();
    }
    rt.reset_diagnostic_counters().unwrap();
    let before = GLOBAL.stats();
    for _ in 0..128 {
        if mode != "equal" {
            value += 1;
        }
        input.try_set(&rt, black_box(T::from(value))).unwrap();
    }
    let steady = GLOBAL.stats() - before;
    assert_eq!(
        (
            steady.allocations,
            steady.reallocations,
            steady.deallocations
        ),
        (0, 0, 0)
    );
    assert!(input.try_with(&rt, |v| *v == T::from(value)).unwrap());
    let work = rt.diagnostics().unwrap();
    assert_eq!(work.counters.mark_visits, 0);
    assert_eq!(work.counters.memo_executions, 0);
    rt.shutdown().unwrap();
    drop(rt);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "isolated_writes", "value_kind": kind, "mode": mode,
        "iterations": 128, "steady_state": stats(steady), "work": diagnostics(&work),
        "after_runtime_drop": stats(released)})
}
fn diagnostics(d: &RuntimeDiagnostics) -> Value {
    let c = &d.counters;
    let m = &d.storage;
    json!({"nodes": d.nodes, "owners": d.owners, "edges": d.edges,
        "pending_tickets": d.pending_tickets, "pending_buffer_len": d.pending_buffer_len,
        "node_slots": m.node_slots, "owner_slots": m.owner_slots,
        "arena_capacity_bytes": m.arena_capacity_bytes, "queue_capacity_bytes": m.queue_capacity_bytes,
        "edge_capacity_bytes": m.edge_capacity_bytes, "read_capacity_bytes": m.read_capacity_bytes,
        "source_slots": m.source_slots, "source_tombstones": m.source_tombstones,
        "read_heap_capacity_bytes": m.read_heap_capacity_bytes, "read_inline_capacity_bytes": m.read_inline_capacity_bytes,
        "dedup_capacity_entries": m.dedup_capacity_entries, "scratch_capacity_bytes": m.scratch_capacity_bytes,
        "node_size": m.node_size, "metadata_size": m.metadata_size,
        "work": {"mark_visits": c.mark_visits, "state_changes": c.state_changes, "source_checks": c.source_checks,
            "tracked_reads": c.tracked_reads, "reconciliations": c.reconciliations,
            "edges_added": c.edges_added, "edges_removed": c.edges_removed,
            "memo_executions": c.memo_executions, "effect_executions": c.effect_executions,
            "rounds": c.rounds, "tickets_enqueued": c.tickets_enqueued, "tickets_processed": c.tickets_processed}})
}

fn graph(width: usize, mode: Mode) -> Value {
    let origin = GLOBAL.stats();
    let graph = Fixture::new(width, mode, true);
    let built = GLOBAL.stats() - origin;
    for value in 1..=8 {
        graph.step(value);
    }
    let warm = GLOBAL.stats() - origin;
    graph.rt.reset_diagnostic_counters().unwrap();
    let before = GLOBAL.stats();
    for value in 9..=136 {
        graph.step(value);
    }
    let steady = GLOBAL.stats() - before;
    let work = graph.rt.diagnostics().unwrap();
    assert_eq!(work.counters.effect_executions, 128);
    assert_eq!(work.counters.rounds, 128);
    graph.rt.dispose(graph.root).unwrap();
    let disposed = GLOBAL.stats() - origin;
    let retained = graph.rt.diagnostics().unwrap();
    assert_eq!((retained.nodes, retained.owners), (0, 0));
    drop(graph);
    let released = GLOBAL.stats() - origin;
    assert_eq!(
        released.bytes_allocated, released.bytes_deallocated,
        "fixture must release all requested heap bytes"
    );
    json!({"scenario": mode.name(), "width": width, "iterations": 128,
        "after_build": stats(built), "after_warmup": stats(warm), "steady_state": stats(steady),
        "after_dispose": stats(disposed), "after_runtime_drop": stats(released),
        "steady_work": diagnostics(&work), "retained_structure": diagnostics(&retained)})
}

fn churn(iterations: usize) -> Value {
    let origin = GLOBAL.stats();
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    rt.with_owner(root, |rt| rt.effect(|_| Ok(()))).unwrap();
    let mut max_len = 0;
    for _ in 0..iterations {
        let effect = rt.with_owner(root, |rt| rt.effect(|_| Ok(()))).unwrap();
        rt.dispose(effect).unwrap();
        max_len = max_len.max(rt.diagnostics().unwrap().pending_buffer_len);
    }
    let live = rt.diagnostics().unwrap();
    let steady = GLOBAL.stats() - origin;
    rt.dispose(root).unwrap();
    let disposed = GLOBAL.stats() - origin;
    let retained = rt.diagnostics().unwrap();
    drop(rt);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "queue_churn", "iterations": iterations, "max_pending_buffer_len": max_len,
        "live": diagnostics(&live), "after_churn": stats(steady), "after_dispose": stats(disposed),
        "retained_structure": diagnostics(&retained), "after_runtime_drop": stats(released)})
}

fn large_queue(size: usize) -> Value {
    let origin = GLOBAL.stats();
    let rt = Runtime::new();
    let root = rt.create_root().unwrap();
    rt.with_owner(root, |rt| {
        for _ in 0..size {
            rt.effect(|_| Ok(()))?;
        }
        Ok(())
    })
    .unwrap();
    let full = rt.diagnostics().unwrap();
    rt.dispose(root).unwrap();
    let disposed = GLOBAL.stats() - origin;
    let empty = rt.diagnostics().unwrap();
    assert_eq!(empty.pending_buffer_len, 0);
    drop(rt);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "large_queue_dispose", "size": size, "full": diagnostics(&full), "empty": diagnostics(&empty),
        "after_dispose": stats(disposed), "after_runtime_drop": stats(released)})
}

fn value_costs() -> Value {
    let value = Rc::new(vec![0_u64; 1024]);
    let origin = GLOBAL.stats();
    for _ in 0..128 {
        black_box(value.clone());
    }
    let rc_clone = GLOBAL.stats() - origin;
    let origin = GLOBAL.stats();
    for _ in 0..128 {
        black_box(value.as_ref().clone());
    }
    let vec_clone = GLOBAL.stats() - origin;
    let origin = GLOBAL.stats();
    for i in 0..128 {
        black_box(Rc::new(black_box(i as u64)));
    }
    let candidate = GLOBAL.stats() - origin;
    assert_eq!(rc_clone.allocations, 0);
    assert_eq!(vec_clone.allocations, 128);
    assert_eq!(candidate.allocations, 128);
    json!({"scenario": "value_costs", "iterations": 128, "rc_clone": stats(rc_clone), "vec_clone_1024_u64": stats(vec_clone), "rc_candidate_u64": stats(candidate)})
}

fn native_graph(width: usize, sycamore: bool) -> Value {
    use leptos_reactive::{SignalGet, SignalSet};
    let origin = GLOBAL.stats();
    let output = Rc::new(Cell::new(0));
    let calls = Rc::new(Cell::new(0));
    let (built, warm, steady, disposed);
    if sycamore {
        let mut driver = None;
        let root = sycamore_reactive::create_root(|| {
            let input = sycamore_reactive::create_signal(0_u64);
            driver = Some(input);
            let inputs: Vec<_> = (0..width)
                .map(|i| sycamore_reactive::create_signal(i as u64))
                .collect();
            let memo = sycamore_reactive::create_selector(move || {
                input.get() + inputs.iter().map(|input| input.get()).sum::<u64>()
            });
            let (output, calls) = (output.clone(), calls.clone());
            sycamore_reactive::create_effect(move || {
                output.set(memo.get());
                calls.set(calls.get() + 1);
            });
        });
        built = GLOBAL.stats() - origin;
        let driver = driver.unwrap();
        for i in 1..=8 {
            driver.set(i);
        }
        warm = GLOBAL.stats() - origin;
        let before = GLOBAL.stats();
        for i in 9..=136 {
            driver.set(i);
        }
        steady = GLOBAL.stats() - before;
        assert_eq!(
            output.get(),
            136 + (width * width.saturating_sub(1) / 2) as u64
        );
        root.dispose();
        disposed = GLOBAL.stats() - origin;
    } else {
        let runtime = leptos_reactive::create_runtime();
        let driver = leptos_reactive::create_rw_signal(0_u64);
        let inputs: Vec<_> = (0..width)
            .map(|i| leptos_reactive::create_rw_signal(i as u64))
            .collect();
        let memo = leptos_reactive::create_memo(move |_| {
            driver.get() + inputs.iter().map(SignalGet::get).sum::<u64>()
        });
        let (captured, count) = (output.clone(), calls.clone());
        leptos_reactive::create_render_effect(move |_| {
            captured.set(memo.get());
            count.set(count.get() + 1);
        });
        built = GLOBAL.stats() - origin;
        for i in 1..=8 {
            driver.set(i);
        }
        warm = GLOBAL.stats() - origin;
        let before = GLOBAL.stats();
        for i in 9..=136 {
            driver.set(i);
        }
        steady = GLOBAL.stats() - before;
        assert_eq!(
            output.get(),
            136 + (width * width.saturating_sub(1) / 2) as u64
        );
        runtime.dispose();
        disposed = GLOBAL.stats() - origin;
    }
    assert_eq!(calls.get(), 137);
    drop(calls);
    drop(output);
    let released = GLOBAL.stats() - origin;
    json!({"scenario": "native_stable", "library": if sycamore { "sycamore" } else { "leptos" }, "width": width,
        "iterations": 128, "callbacks": 128, "after_build": stats(built), "after_warmup": stats(warm),
        "steady_state": stats(steady), "after_dispose": stats(disposed), "after_facade_drop": stats(released),
        "note": "native root/TLS retention differs; Sycamore roots are explicitly leaked by its API"})
}

fn direct_effect(width: usize) -> Value {
    let origin = GLOBAL.stats();
    let (rt, root, driver, output) = fixture::direct_effect(width);
    for value in 1..=8 {
        driver.try_set(&rt, value).unwrap();
        rt.flush(Default::default()).unwrap();
    }
    let warm = GLOBAL.stats() - origin;
    rt.reset_diagnostic_counters().unwrap();
    let before = GLOBAL.stats();
    for value in 9..=136 {
        driver.try_set(&rt, value).unwrap();
        rt.flush(Default::default()).unwrap();
    }
    let steady = GLOBAL.stats() - before;
    let work = rt.diagnostics().unwrap();
    assert_eq!(work.counters.tracked_reads, 128 * (width as u64 + 1));
    assert_eq!(work.counters.reconciliations, 128);
    assert_eq!(steady.allocations, 0); // typed scalar storage is reused
    assert_eq!(steady.reallocations, 0);
    rt.dispose(root).unwrap();
    let disposed = GLOBAL.stats() - origin;
    drop(rt);
    drop(output);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "direct_effect_fan_in", "width": width, "iterations": 128,
        "after_warmup": stats(warm), "steady_state": stats(steady), "work": diagnostics(&work),
        "after_dispose": stats(disposed), "after_runtime_drop": stats(released)})
}

fn equality_cut(width: usize) -> Value {
    let origin = GLOBAL.stats();
    let graph = fixture::EqualityCut::new(width);
    for value in 1..=8 {
        graph.driver.try_set(&graph.rt, value * 2).unwrap();
        graph.rt.flush(Default::default()).unwrap();
    }
    let warm = GLOBAL.stats() - origin;
    graph.rt.reset_diagnostic_counters().unwrap();
    let before = GLOBAL.stats();
    for value in 9..=136 {
        graph.driver.try_set(&graph.rt, value * 2).unwrap();
        let report = graph.rt.flush(Default::default()).unwrap();
        assert_eq!(report.outcome, reactive::FlushOutcome::Stabilized);
        assert_eq!(report.callbacks, 0);
    }
    let steady = GLOBAL.stats() - before;
    graph.assert_cut();
    let work = graph.rt.diagnostics().unwrap();
    assert_eq!(work.counters.effect_executions, 0);
    assert_eq!(work.counters.memo_executions, 128 * width as u64);
    drop(graph);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "observed_equality_cut", "width": width, "iterations": 128,
        "after_warmup": stats(warm), "steady_state": stats(steady), "work": diagnostics(&work),
        "after_runtime_drop": stats(released)})
}

fn source_disposal(width: usize, bulk: bool) -> Value {
    let origin = GLOBAL.stats();
    let graph = fixture::SourceDisposal::new(width);
    let built = GLOBAL.stats() - origin;
    graph.rt.reset_diagnostic_counters().unwrap();
    let before = GLOBAL.stats();
    graph.dispose_all(bulk);
    let disposal = GLOBAL.stats() - before;
    let after_dispose = GLOBAL.stats() - origin;
    let disposed = graph.rt.diagnostics().unwrap();
    assert_eq!(disposed.edges, 0);
    let before = GLOBAL.stats();
    graph.reconcile();
    let reconciliation = GLOBAL.stats() - before;
    let reconciled = graph.rt.diagnostics().unwrap();
    drop(graph);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": if bulk { "source_disposal_bulk" } else { "source_disposal_individual" },
        "width": width, "after_build": stats(built), "disposal": stats(disposal),
        "after_dispose": stats(after_dispose), "disposed_work": diagnostics(&disposed),
        "reconciliation": stats(reconciliation), "reconciled_work": diagnostics(&reconciled),
        "after_runtime_drop": stats(released)})
}

fn root_shutdown(width: usize, shape: &str) -> Value {
    let origin = GLOBAL.stats();
    let graph = fixture::RootShutdown::new(width, shape);
    let built = GLOBAL.stats() - origin;
    graph.rt.reset_diagnostic_counters().unwrap();
    let before = GLOBAL.stats();
    graph.rt.shutdown().unwrap();
    let shutdown = GLOBAL.stats() - before;
    let work = graph.rt.diagnostics().unwrap();
    assert_eq!(work.nodes, 0);
    assert_eq!(work.edges, 0);
    drop(graph);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "root_shutdown", "shape": shape, "width": width,
        "after_build": stats(built), "shutdown": stats(shutdown), "work": diagnostics(&work),
        "after_runtime_drop": stats(released)})
}

fn chain_work(size: usize, cut: bool) -> Value {
    let origin = GLOBAL.stats();
    let graph = fixture::NativeChain::new(size, true, cut);
    for value in 1..=8 {
        graph.input.try_set(&graph.rt, value).unwrap();
        graph.rt.flush(Default::default()).unwrap();
    }
    let warm = GLOBAL.stats() - origin;
    graph.rt.reset_diagnostic_counters().unwrap();
    let before = GLOBAL.stats();
    for value in 9..=136 {
        graph.input.try_set(&graph.rt, value).unwrap();
        let report = graph.rt.flush(Default::default()).unwrap();
        assert_eq!(report.callbacks, usize::from(!cut));
    }
    let steady = GLOBAL.stats() - before;
    assert_eq!(graph.output.get(), size as u64 + if cut { 0 } else { 136 });
    let work = graph.rt.diagnostics().unwrap();
    assert_eq!(
        work.counters.memo_executions,
        128 * if cut { 1 } else { size as u64 }
    );
    assert_eq!(work.counters.edges_added, 0);
    assert_eq!(work.counters.edges_removed, 0);
    assert_eq!(steady.allocations, 0);
    assert_eq!(steady.reallocations, 0);
    drop(graph);
    let released = GLOBAL.stats() - origin;
    assert_eq!(released.bytes_allocated, released.bytes_deallocated);
    json!({"scenario": "chain_work", "size": size, "equality_cut": cut, "iterations": 128,
        "after_warmup": stats(warm), "steady_state": stats(steady), "work": diagnostics(&work),
        "after_runtime_drop": stats(released)})
}

fn reference_chain_work(size: usize, sycamore: bool) -> Value {
    use leptos_reactive::{SignalGet, SignalSet};
    type ReferenceActions = (Box<dyn Fn(u64)>, Box<dyn FnOnce()>);
    let origin = GLOBAL.stats();
    let output = Rc::new(Cell::new(0));
    let (step, dispose): ReferenceActions = if sycamore {
        let root = sycamore_reactive::create_root(|| {});
        let captured = output.clone();
        let input = root.run_in(|| {
            let input = sycamore_reactive::create_signal(0_u64);
            let mut tail = sycamore_reactive::create_selector(move || input.get() + 1);
            for _ in 1..size {
                let previous = tail;
                tail = sycamore_reactive::create_selector(move || previous.get() + 1);
            }
            sycamore_reactive::create_effect(move || captured.set(tail.get()));
            input
        });
        (
            Box::new(move |value| input.set(value)),
            Box::new(move || root.dispose()),
        )
    } else {
        let root = leptos_reactive::create_runtime();
        let input = leptos_reactive::create_rw_signal(0_u64);
        let mut tail = leptos_reactive::create_memo(move |_| input.get() + 1);
        for _ in 1..size {
            let previous = tail;
            tail = leptos_reactive::create_memo(move |_| previous.get() + 1);
        }
        let captured = output.clone();
        leptos_reactive::create_render_effect(move |_| captured.set(tail.get()));
        (
            Box::new(move |value| input.set(value)),
            Box::new(move || root.dispose()),
        )
    };
    for value in 1..=8 {
        step(value);
    }
    let warm = GLOBAL.stats() - origin;
    let before = GLOBAL.stats();
    for value in 9..=136 {
        step(value);
    }
    let steady = GLOBAL.stats() - before;
    assert_eq!(output.get(), size as u64 + 136);
    drop(step);
    dispose();
    drop(output);
    let released = GLOBAL.stats() - origin;
    json!({"scenario": "reference_chain_work", "engine": if sycamore { "sycamore" } else { "leptos" },
        "size": size, "iterations": 128, "after_warmup": stats(warm), "steady_state": stats(steady),
        "after_runtime_dispose": stats(released),
        "note": "Native global/TLS/root storage may persist after disposal; not a leak or RSS comparison"})
}

fn main() {
    let mut rows = Vec::new();
    for width in [1, 32] {
        for resources in [
            memo_owners::Resources::Empty,
            memo_owners::Resources::Cleanup,
            memo_owners::Resources::Node,
            memo_owners::Resources::Child,
        ] {
            rows.push(memo_owner_work(width, resources));
        }
    }
    for mode in ["unconditional", "equal", "changed"] {
        rows.push(isolated_writes::<u64>("no_drop", mode));
        rows.push(isolated_writes::<WriteDropNumber>("drop", mode));
    }
    for mode in [Mode::Stable, Mode::Reorder, Mode::Duplicate, Mode::Nested] {
        for width in [0, 1, 2, 4, 16, 64, 256, 4096] {
            rows.push(graph(width, mode));
        }
    }
    for size in [64, 256, 4096] {
        rows.push(churn(size));
    }
    for size in [64, 4096, 8192] {
        rows.push(large_queue(size));
    }
    rows.push(value_costs());
    rows.push(explicit_value_reads());
    for sycamore in [false, true] {
        for width in [4, 16, 64, 256] {
            rows.push(native_graph(width, sycamore));
        }
    }
    for width in [8, 64, 256, 4096] {
        rows.push(direct_effect(width));
        rows.push(equality_cut(width));
    }
    for width in [64, 256, 1024, 4096] {
        for bulk in [false, true] {
            rows.push(source_disposal(width, bulk));
        }
    }
    for width in [64, 256, 1024, 4096] {
        for shape in ["fan_in", "fan_out", "independent"] {
            rows.push(root_shutdown(width, shape));
        }
    }
    for size in [64, 256, 384] {
        for cut in [false, true] {
            rows.push(chain_work(size, cut));
        }
        for sycamore in [false, true] {
            rows.push(reference_chain_work(size, sycamore));
        }
    }
    let document = json!({"allocator": "stats_alloc/System", "instrumented": true,
        "panic_unwind": cfg!(panic = "unwind"), "experimental_inline_reads": cfg!(feature = "experimental-inline-reads"), "rows": rows,
        "note": "requested bytes, not RSS; diagnostic storage included; no timing collected"});
    let path = std::env::var_os("REACTIVE_METRICS_LOG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/.log/local/metrics.json")
        });
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    println!(
        "{} scenarios validated; metrics: {}",
        document["rows"].as_array().unwrap().len(),
        path.display()
    );
}

fn explicit_value_reads() -> Value {
    let rt = reactive::Runtime::new();
    let root = rt.create_root().unwrap();
    let value = rt
        .with_owner(root, |rt| rt.signal(vec![1_u64; 4096]))
        .unwrap();
    let shared = rt
        .with_owner(root, |rt| rt.signal(std::rc::Rc::new(vec![1_u64; 4096])))
        .unwrap();
    let before = GLOBAL.stats();
    for _ in 0..128 {
        assert_eq!(
            value
                .try_with(&rt, |value| std::hint::black_box(value.len()))
                .unwrap(),
            4096
        );
    }
    let borrowed = GLOBAL.stats() - before;
    let before = GLOBAL.stats();
    for _ in 0..128 {
        std::hint::black_box(value.try_get_cloned(&rt).unwrap());
    }
    let cloned = GLOBAL.stats() - before;
    let before = GLOBAL.stats();
    for _ in 0..128 {
        std::hint::black_box(shared.try_get_cloned(&rt).unwrap());
    }
    let shared_cloned = GLOBAL.stats() - before;
    assert_eq!(borrowed.allocations, 0);
    assert_eq!(cloned.allocations, 128);
    assert_eq!(
        cloned.bytes_allocated,
        128 * 4096 * std::mem::size_of::<u64>()
    );
    assert_eq!(cloned.bytes_allocated, cloned.bytes_deallocated);
    assert_eq!(shared_cloned.allocations, 0);
    rt.shutdown().unwrap();
    json!({"scenario": "explicit_value_reads", "iterations": 128, "elements": 4096,
        "borrowed": stats(borrowed), "cloned": stats(cloned), "explicit_rc_cloned": stats(shared_cloned)})
}
