#!/usr/bin/env python3
"""Combine chain timing, exclusive perf samples and separately instrumented work."""
import hashlib
import json
import os
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"


def load(label):
    value = json.loads((LOG / "reports" / f"{label}.json").read_text())
    assert value["status_code"] == 0, label
    return value


def estimate(value):
    return value.get("slope") or value["mean"]


def main():
    timing_labels = ["chain-investigation-scaling", "chain-investigation-confirm"]
    timing = [load(label) for label in timing_labels]
    assert all(not value["instrumented"] and not value["profiling"] and not value["features"] for value in timing)
    sizes = [1, 8, 32, 64, 128, 256, 384]
    scaling = {}
    for size in sizes:
        engines = {name: estimate(timing[-1]["estimates_ns"][f"chain_scaling/{name}/{size}"]) for name in ["reactive", "sycamore", "leptos"]}
        scaling[size] = {"estimates_ns": engines,
                         "reactive_over_sycamore": engines["reactive"]["point_estimate"] / engines["sycamore"]["point_estimate"],
                         "reactive_over_leptos": engines["reactive"]["point_estimate"] / engines["leptos"]["point_estimate"]}
    phases = {size: {mode: estimate(timing[-1]["estimates_ns"][f"chain_phases_{mode}_{size}"]) for mode in ["native_total", "mark_only", "update_only", "ordered_refresh", "equality_cut", "lazy_read"]} for size in sizes}
    profiles = {}
    for engine, label in [("reactive", "chain-investigation-local-perf"), ("sycamore", "chain-investigation-sycamore-perf"), ("leptos", "chain-investigation-leptos-perf")]:
        report = load(label)
        assert report["profiling"] and not report["instrumented"] and not report["features"]
        symbols = []
        for line in report["perf_report"].splitlines():
            match = re.match(r"\s*(\d+\.\d+)%.*?\[\.\]\s+(.*)", line)
            if match:
                symbols.append({"exclusive_sample_percent": float(match[1]), "symbol": match[2]})
        profiles[engine] = {"input": label, "symbols": symbols}
    for value in timing[1:] + [load(profile["input"]) for profile in profiles.values()]:
        for path, digest in timing[0]["source_sha256"].items():
            if path.startswith("src/") or path in ["Cargo.toml", "Cargo.lock"]:
                assert value["source_sha256"][path] == digest, path
    memory_label = "chain-investigation-memory-final"
    memory = [row for row in load(memory_label)["metrics"]["rows"] if row["scenario"] in ["chain_work", "reference_chain_work"]]
    for row in memory:
        if row["scenario"] == "chain_work":
            size, iterations, cut = row["size"], row["iterations"], row["equality_cut"]
            work = row["work"]["work"]
            assert work["memo_executions"] == iterations * (1 if cut else size)
            assert work["effect_executions"] == iterations * (0 if cut else 1)
            assert work["edges_added"] == work["edges_removed"] == 0
            assert work["source_checks"] == iterations * size
            assert row["after_runtime_drop"]["net_requested_bytes"] == 0
    registry = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / "registry/src"
    reference_sources = {}
    for package, files in {"sycamore-reactive-0.9.4": ["root.rs", "node.rs", "memos.rs"], "leptos_reactive-0.6.15": ["runtime.rs", "node.rs", "memo.rs"]}.items():
        for name in files:
            found = list(registry.glob(f"*/{package}/src/{name}"))
            assert len(found) == 1, (package, name)
            reference_sources[f"{package}/src/{name}"] = hashlib.sha256(found[0].read_bytes()).hexdigest()
    result = {"timing_inputs": timing_labels, "scaling": scaling, "phases": phases,
              "memory_input": memory_label, "memory": memory, "profiles": profiles,
              "pinned_reference_source_sha256": reference_sources,
              "limitations": ["One machine, exploratory repetitions not ABBA", "Exclusive symbols contain inline work; percentages are not isolated function times or potential speedups", "Phase timers include clock overhead and are not exactly additive", "Ordered refresh adds public reads and changes scheduling: diagnostic, not production optimization", "Allocator executable has diagnostics, unlike timing/perf", "Cache/layout benefit not established by cycle samples", "No runtime algorithm changed"]}
    # Supplemental disassembly/source attribution is optional if raw perf data
    # was removed. Keep already captured evidence in the versionable analysis.
    path = LOG / "chain-analysis.json"
    old = json.loads(path.read_text()) if path.exists() else {}
    for name in ["stack-prologues", "perf-lines"]:
        source = LOG / "runs/chain-investigation-local-perf" / f"{name}.txt"
        result[name] = source.read_text() if source.exists() else old.get(name)
    path.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
