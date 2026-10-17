#!/usr/bin/env python3
"""Summarize paired ABBA blocks, exclusive profiles and allocator preflights."""
import json
import math
from pathlib import Path
import re
import statistics

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"


def load(label):
    result = json.loads((LOG / "reports" / f"{label}.json").read_text())
    assert result["status_code"] == 0, label
    return result


def point(run, key):
    estimates = run["estimates_ns"][key]
    return (estimates.get("slope") or estimates["mean"])["point_estimate"]


def abba(prefix, candidate):
    suites = {}
    for suite in ["chain", "runtime"]:
        cycles = [[load(f"{prefix}-c{cycle}-p{pos}-{variant}-{suite}")
                   for pos, variant in enumerate("abba", 1)] for cycle in [1, 2]]
        baseline = cycles[0][0]["source_sha256"]
        for cycle in cycles:
            for pos, run in enumerate(cycle):
                assert not run["profiling"] and not run["instrumented"]
                expected = ["experimental-inline-reads"] if candidate == "inline" and pos in [1, 2] else []
                assert run["features"] == expected
                differences = [name for name, digest in baseline.items()
                               if name.startswith("src/") and run["source_sha256"][name] != digest]
                assert differences == (["src/runtime/graph.rs"] if candidate == "empty-mark" and pos in [1, 2] else [])
        results = {}
        for key in cycles[0][0]["estimates_ns"]:
            values = {"a": [], "b": []}
            ratios = []
            for cycle in cycles:
                a = [point(cycle[i], key) for i in [0, 3]]
                b = [point(cycle[i], key) for i in [1, 2]]
                values["a"].extend(a)
                values["b"].extend(b)
                ratios.append(math.sqrt((b[0] / a[0]) * (b[1] / a[1])))
            results[key] = {
                "a_mean_ns": statistics.mean(values["a"]),
                "b_mean_ns": statistics.mean(values["b"]),
                "a_range_ns": [min(values["a"]), max(values["a"])],
                "b_range_ns": [min(values["b"]), max(values["b"])],
                "cycle_b_over_a": ratios,
                "paired_geomean_percent": 100 * (math.sqrt(math.prod(ratios)) - 1),
            }
        suites[suite] = {"inputs": [[run["label"] for run in cycle] for cycle in cycles], "results": results}
    return suites


def profile(label, old):
    run = load(label)
    assert run["profiling"] and not run["instrumented"] and not run["features"]
    symbols = []
    for line in run["perf_report"].splitlines():
        match = re.match(r"\s*(\d+\.\d+)%.*?\[\.\]\s+(.*)", line)
        if match:
            symbols.append({"exclusive_percent": float(match[1]), "symbol": match[2]})
    lines_path = LOG / "runs" / label / "perf-lines.txt"
    lines = lines_path.read_text() if lines_path.exists() else old.get("source_line_report")
    groups = {
        "marking": ["::drain_marks", "::mark_observers", "::invalidate_signal_observers"],
        "check_ascent": ["::check_computation_sources", "::update_if_necessary"],
        "owners": ["::prepare_execution"],
        "evaluation_guards": ["::enter_evaluation", "EvaluationGuard"],
        "reconciliation": ["::commit_sources", "::reconcile_sources"],
        "visible_value_retirement": ["::release_value_slot", "::clear_value"],
        "panic_merge": ["::merge_panic"],
        "mixed_memo_execution": ["::update_memo", "resource_boundary::<reactive::runtime::memos::Execution"],
    }
    grouped = {name: sum(row["exclusive_percent"] for row in symbols
                         if any(marker in row["symbol"] for marker in markers))
               for name, markers in groups.items()}
    return {"input": label, "symbols": symbols, "source_line_report": lines,
            "exclusive_symbol_groups_percent": grouped,
            "header": run["perf_report"].split("# Overhead")[0]}


def main():
    destination = LOG / "bottleneck-analysis.json"
    old = json.loads(destination.read_text()) if destination.exists() else {}
    profiles = {}
    labels = {"chain": "bottleneck-current-chain-perf", "cut": "bottleneck-current-cut-perf"}
    for kind in ["no-drop", "drop"]:
        for mode in ["unconditional", "equal", "changed"]:
            suffix = "-confirm" if (kind, mode) == ("drop", "changed") else ""
            labels[f"{kind}/{mode}"] = f"bottleneck-current-write-{kind}-{mode}{suffix}-perf"
    source = load("bottleneck-current-c1-p1-a-chain")["source_sha256"]
    for name, label in labels.items():
        run = load(label)
        assert all(run["source_sha256"][path] == digest for path, digest in source.items() if path.startswith("src/"))
        profiles[name] = profile(label, old.get("profiles", {}).get(name, {}))
        assert "Total Lost Samples: 0" in profiles[name]["header"], label
    memory = load("bottleneck-current-memory")
    assert memory["instrumented"]
    rows = [row for row in memory["metrics"]["rows"] if row["scenario"] in ["chain_work", "isolated_writes"]]
    for row in rows:
        assert row["steady_state"]["allocations"] == row["steady_state"]["reallocations"] == 0
        assert row["after_runtime_drop"]["net_requested_bytes"] == 0
        if row["scenario"] == "chain_work":
            work, iterations = row["work"]["work"], row["iterations"]
            assert work["source_checks"] == row["size"] * iterations
            assert work["memo_executions"] == (1 if row["equality_cut"] else row["size"]) * iterations
            assert work["effect_executions"] == (0 if row["equality_cut"] else 1) * iterations
    inline_memory = load("bottleneck-inline-memory")
    result = {"inline_reads_abba": abba("bottleneck-current", "inline"),
              "empty_mark_abba": abba("bottleneck-empty-mark", "empty-mark"),
              "profiles": profiles, "memory_input": memory["label"], "memory": rows,
              "inline_memory_input": inline_memory["label"],
              "inline_chain_memory": [row for row in inline_memory["metrics"]["rows"] if row["scenario"] == "chain_work"],
              "excluded": {"bottleneck-current-write-drop-changed-perf": "132 lost samples; replaced by confirm run",
                           "bottleneck-current-writes-perf": "aggregate across all modes, not used for per-mode attribution"},
              "limitations": ["Two ABBA cycles per candidate, not a statistical proof; per-run Criterion CIs remain in inputs",
                              "Fixed benchmark order within each block; ABBA does not remove all cache/thermal/code-layout effects",
                              "Exclusive symbols include inlined work, not isolated stage durations or removable overhead",
                              "Phase timers are not additive and include clock overhead",
                              "Empty Drop isolates needs_drop guard paths, not realistic destructor costs",
                              "Unconditional publication still calls a protected false comparator",
                              "No runtime candidate adopted; unsafe and protections unchanged"]}
    destination.write_text(json.dumps(result, indent=2) + "\n")
    for candidate in ["inline_reads_abba", "empty_mark_abba"]:
        print(candidate)
        for suite in ["chain", "runtime"]:
            for key, value in result[candidate][suite]["results"].items():
                print(key, round(value["a_mean_ns"], 3), round(value["b_mean_ns"], 3),
                      round(value["paired_geomean_percent"], 2),
                      [round(100 * (r - 1), 2) for r in value["cycle_b_over_a"]])
    print(f"Saved {destination}")


if __name__ == "__main__":
    main()
