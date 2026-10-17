#!/usr/bin/env python3
"""Validate and summarize Check entry/finish experiments and their ABBA controls."""
import difflib
import hashlib
import importlib.util
import json
import math
from pathlib import Path
import statistics

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"
spec = importlib.util.spec_from_file_location("bottlenecks", ROOT / "benches/analyze-bottlenecks.py")
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)


def summarize(prefix, suite):
    cycles = [[helpers.load(f"{prefix}-c{c}-p{p}-{v}-{suite}")
               for p, v in enumerate("abba", 1)] for c in [1, 2]]
    first = cycles[0][0]
    changed = {"src/runtime.rs", "src/runtime/memos.rs"}
    for cycle in cycles:
        for position, run in enumerate(cycle):
            assert not run["features"] and not run["instrumented"] and not run["profiling"]
            differences = {name for name, digest in first["source_sha256"].items()
                           if (name.startswith("src/") or name in ["Cargo.toml", "Cargo.lock"])
                           and run["source_sha256"][name] != digest}
            assert differences == (changed if position in [1, 2] else set())
            assert run["source_sha256"] == cycles[0][position]["source_sha256"]
    rows = {}
    for key in first["estimates_ns"]:
        values = {v: [helpers.point(cycle[i], key) for cycle in cycles for i in indexes]
                  for v, indexes in [("a", [0, 3]), ("b", [1, 2])]}
        ratios = [math.sqrt(helpers.point(cycle[1], key) * helpers.point(cycle[2], key)
                            / helpers.point(cycle[0], key) / helpers.point(cycle[3], key))
                  for cycle in cycles]
        rows[key] = {"a_mean_ns": statistics.mean(values["a"]), "b_mean_ns": statistics.mean(values["b"]),
                     "a_range_ns": [min(values["a"]), max(values["a"])],
                     "b_range_ns": [min(values["b"]), max(values["b"])],
                     "cycle_percent": [100 * (r - 1) for r in ratios],
                     "paired_geomean_percent": 100 * (math.sqrt(math.prod(ratios)) - 1)}
    return {"inputs": [[r["label"] for r in cycle] for cycle in cycles], "results": rows}


def main():
    destination = LOG / "check-optimization-analysis.json"
    old = json.loads(destination.read_text()) if destination.exists() else {}
    patches = dict(old.get("candidate_patches", {}))
    baseline = Path("/tmp/opencode/reactive-check-baseline")
    for name, candidate in [("finish_rejected", Path("/tmp/opencode/reactive-check-finish-candidate")),
                            ("entry_selected", ROOT)]:
        expected = helpers.load("check-entry-comparison-c1-p2-b-comparison" if name == "entry_selected"
                                else "check-finish-c1-p2-b-chain")["source_sha256"]
        paths = ["src/runtime.rs", "src/runtime/memos.rs"]
        if baseline.exists() and all((candidate / p).exists() and
                                    hashlib.sha256((candidate / p).read_bytes()).hexdigest() == expected[p]
                                    for p in paths):
            patches[name] = "".join("".join(difflib.unified_diff(
                (baseline / p).read_text().splitlines(keepends=True),
                (candidate / p).read_text().splitlines(keepends=True), fromfile=f"a/{p}", tofile=f"b/{p}")) for p in paths)
        assert name in patches, f"missing preserved source patch: {name}"
    memory = helpers.load("check-entry-memory")
    previous_memory = helpers.load("bottleneck-current-memory")
    rows = [r for r in memory["metrics"]["rows"] if r["scenario"] == "chain_work"]
    for row in rows:
        previous = next(r for r in previous_memory["metrics"]["rows"]
                        if r["scenario"] == "chain_work" and (r["size"], r["equality_cut"]) == (row["size"], row["equality_cut"]))
        assert row["work"] == previous["work"], "work/storage changed"
        assert row["after_warmup"] == previous["after_warmup"]
        assert row["steady_state"]["allocations"] == row["steady_state"]["reallocations"] == 0
        assert row["after_runtime_drop"]["net_requested_bytes"] == 0
    profiles = {name: helpers.profile(label, old.get("profiles", {}).get(name, {}))
                for name, label in [("full", "check-entry-full-perf"), ("cut", "check-entry-cut-perf")]}
    assert all("Total Lost Samples: 0" in p["header"] for p in profiles.values())
    result = {"finish_rejected": summarize("check-finish", "chain"),
              "entry_selected": summarize("check-entry", "chain"),
              "comparison_confirmation": summarize("check-entry-comparison", "comparison"),
              "candidate_patches": patches, "memory_input": memory["label"], "chain_memory": rows,
              "profiles": profiles,
              "limitations": ["Two ABBA cycles per series, one CPU/machine; Criterion CIs are per run",
                              "References varied even within ABBA; compiler layout and thermal effects remain",
                              "First entry series precedes added cfg(test) regressions; comparison uses final sources",
                              "Exclusive symbol changes include inlining/redistribution, not pure stage savings",
                              "No unsafe, recursion, scheduling or resource-guard removal in selected change"]}
    destination.write_text(json.dumps(result, indent=2) + "\n")
    for name in ["finish_rejected", "entry_selected", "comparison_confirmation"]:
        print(name)
        for key, row in result[name]["results"].items():
            if name != "comparison_confirmation" or "/256" in key:
                print(key, round(row["a_mean_ns"], 3), round(row["b_mean_ns"], 3),
                      round(row["paired_geomean_percent"], 2), [round(x, 2) for x in row["cycle_percent"]])
    print(f"Saved {destination}")


if __name__ == "__main__":
    main()
