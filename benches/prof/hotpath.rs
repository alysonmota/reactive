//! CPU flamegraph harness for the hottest reactive paths.
//! Build:
//!   cargo build --profile prof --bin hotpath
//! Run (perf):
//!   perf record -g --call-graph dwarf,16384 -F 3997 -o hot.data ./target/prof/hotpath write_chain 10
//!   perf script | inferno-flamegraph > hot.write_chain.svg
use std::{cell::Cell, rc::Rc};

use reactive::Runtime;

fn read_signal(iters: u64) {
    let runtime = Runtime::new();
    let root = runtime.create_root().unwrap();
    let signal = runtime.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    let mut acc = 0_u64;
    for _ in 0..iters {
        acc = acc.wrapping_add(signal.try_peek_copy(&runtime).unwrap());
    }
    assert_eq!(acc, 0);
    runtime.shutdown().unwrap();
}

fn write_signal_unobserved(iters: u64) {
    let runtime = Runtime::new();
    let root = runtime.create_root().unwrap();
    let signal = runtime.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    let mut value = 0_u64;
    for _ in 0..iters {
        value = value.wrapping_add(1);
        signal.try_set(&runtime, value).unwrap();
    }
    runtime.shutdown().unwrap();
}

fn write_chain(iters: u64, depth: usize) {
    let runtime = Runtime::new();
    let root = runtime.create_root().unwrap();
    let input = runtime.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    // Simple chain: memo_i = memo_{i-1} + 1
    let mut tail = runtime
        .with_owner(root, |rt| rt.memo_infallible(move |rt| input.try_get_copy(rt).unwrap() + 1))
        .unwrap();
    for _ in 1..depth {
        let previous = tail;
        tail = runtime
            .with_owner(root, |rt| {
                rt.memo_infallible(move |rt| previous.try_get_copy(rt).unwrap() + 1)
            })
            .unwrap();
    }
    let output = Rc::new(Cell::new(0_u64));
    let captured = output.clone();
    runtime
        .with_owner(root, |rt| {
            rt.effect_infallible(move |rt| captured.set(tail.try_get_copy(rt).unwrap()))
        })
        .unwrap();
    runtime.flush(Default::default()).unwrap();
    let mut value = 0_u64;
    for _ in 0..iters {
        value = value.wrapping_add(1);
        input.try_set(&runtime, value).unwrap();
        runtime.flush(Default::default()).unwrap();
        let _ = output.get();
    }
    runtime.shutdown().unwrap();
}

fn fan_out(iters: u64, width: usize) {
    let runtime = Runtime::new();
    let root = runtime.create_root().unwrap();
    let input = runtime.with_owner(root, |rt| rt.signal(0_u64)).unwrap();
    let mut leaves = Vec::new();
    for i in 0..width {
        let leaf = runtime
            .with_owner(root, |rt| {
                rt.memo_infallible(move |rt| input.try_get_copy(rt).unwrap() + i as u64)
            })
            .unwrap();
        leaves.push(leaf);
    }
    let outputs: Vec<_> = (0..width).map(|_| Rc::new(Cell::new(0_u64))).collect();
    for (leaf, output) in leaves.iter().zip(&outputs) {
        let leaf = *leaf;
        let output = output.clone();
        runtime
            .with_owner(root, |rt| {
                rt.effect_infallible(move |rt| output.set(leaf.try_get_copy(rt).unwrap()))
            })
            .unwrap();
    }
    runtime.flush(Default::default()).unwrap();
    let mut value = 0_u64;
    for _ in 0..iters {
        value = value.wrapping_add(1);
        input.try_set(&runtime, value).unwrap();
        runtime.flush(Default::default()).unwrap();
    }
    runtime.shutdown().unwrap();
}

fn tracked_read(iters: u64) {
    let runtime = Runtime::new();
    let root = runtime.create_root().unwrap();
    let a = runtime.with_owner(root, |rt| rt.signal(1_u64)).unwrap();
    let b = runtime.with_owner(root, |rt| rt.signal(2_u64)).unwrap();
    let sum = runtime
        .with_owner(root, |rt| {
            rt.memo_infallible(move |rt| a.try_get_copy(rt).unwrap() + b.try_get_copy(rt).unwrap())
        })
        .unwrap();
    let mut acc = 0_u64;
    for _ in 0..iters {
        acc = acc.wrapping_add(sum.try_peek_copy(&runtime).unwrap());
    }
    assert_eq!(acc, 3 * iters);
    runtime.shutdown().unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("write_chain");
    let iters: u64 = args
        .get(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000_000);
    match scenario {
        "read_signal" => read_signal(iters),
        "write_unobserved" => write_signal_unobserved(iters),
        "write_chain" => write_chain(iters, 8),
        "write_chain_deep" => write_chain(iters, 64),
        "fan_out" => fan_out(iters, 256),
        "tracked_read" => tracked_read(iters),
        other => panic!("unknown scenario {other}"),
    }
}
