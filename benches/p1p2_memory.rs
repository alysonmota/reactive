#[allow(dead_code)]
#[path = "comparison.rs"]
mod comparison;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::alloc::System;
#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;
fn main() {
    let metrics = comparison::p1p2_memory(GLOBAL);
    if let Some(path) = std::env::var_os("REACTIVE_METRICS_LOG") {
        std::fs::write(path, serde_json::to_string_pretty(&metrics).unwrap()).unwrap();
    }
    println!("{}", serde_json::to_string_pretty(&metrics).unwrap());
}
