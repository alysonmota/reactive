#!/usr/bin/env python3
"""Summarize the rejected callback-free empty-owner execution candidate."""
import difflib
import importlib.util
import json
import math
from pathlib import Path
import statistics

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"
spec = importlib.util.spec_from_file_location("helpers", ROOT / "benches/analyze-bottlenecks.py")
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)


def production(text):
    return text.split("\n#[cfg(test)]\nmod tests {", 1)[0]


def main():
    destination = LOG / "empty-owner-analysis.json"
    previous = json.loads(destination.read_text()) if destination.exists() else {}
    baseline = Path("/tmp/opencode/reactive-empty-owner-baseline")
    candidate = Path("/tmp/opencode/reactive-empty-owner-probe")
    patch = previous.get("production_patch")
    if baseline.exists() and candidate.exists():
        patch = "".join("".join(difflib.unified_diff(
            (production((baseline / p).read_text()) + "\n").splitlines(True),
            (production((candidate / p).read_text()) + "\n").splitlines(True),
            fromfile=f"a/{p}", tofile=f"b/{p}")) for p in ["src/runtime/memos.rs", "src/runtime/ownership.rs"])
        for path in ROOT.glob("src/**/*.rs"):
            assert production(path.read_text()) == production((baseline / path.relative_to(ROOT)).read_text()), path
    assert patch
    series = {}
    for label, suite in [("chain", "chain"), ("confirm", "chain"), ("graphs", "comparison"), ("resources", "runtime")]:
        cycles = [[helpers.load(f"owner-fused-{label}-c{c}-p{p}-{v}-{suite}")
                   for p, v in enumerate("abba", 1)] for c in [1, 2]]
        reference = cycles[0][0]["source_sha256"]
        for cycle in cycles:
            for pos, run in enumerate(cycle):
                assert not run["features"] and not run["instrumented"] and not run["profiling"]
                changed = {p for p, digest in reference.items() if p.startswith("src/") and run["source_sha256"][p] != digest}
                assert changed == ({"src/runtime/memos.rs", "src/runtime/ownership.rs"} if pos in [1, 2] else set())
                assert run["source_sha256"] == cycles[0][pos]["source_sha256"]
        rows = {}
        for key in cycles[0][0]["estimates_ns"]:
            a = [helpers.point(c[i], key) for c in cycles for i in [0, 3]]
            b = [helpers.point(c[i], key) for c in cycles for i in [1, 2]]
            ratios = [math.sqrt(helpers.point(c[1], key) * helpers.point(c[2], key)
                                / helpers.point(c[0], key) / helpers.point(c[3], key)) for c in cycles]
            rows[key] = {"a_mean_ns": statistics.mean(a), "b_mean_ns": statistics.mean(b),
                         "cycle_percent": [100 * (r - 1) for r in ratios],
                         "paired_geomean_percent": 100 * (math.sqrt(math.prod(ratios)) - 1)}
        series[label] = {"inputs": [[r["label"] for r in c] for c in cycles], "results": rows}
    a = helpers.load("owner-unfused-memory-final")["metrics"]["rows"]
    b = helpers.load("owner-fused-memory")["metrics"]["rows"]
    assert len(a) == len(b)
    for left, right in zip(a, b):
        # Memory runs have identical source-independent work/allocator results.
        assert left == right, (left.get("scenario"), left.get("width"), left.get("size"))
    result = {"decision": "Rejected; production restored", "production_patch": patch,
              "series": series, "memory_inputs": ["owner-unfused-memory-final", "owner-fused-memory"],
              "identical_memory_rows": len(a),
              "memo_owner_memory": [r for r in b if r["scenario"] == "memo_owner_execution"],
              "limitations": ["One machine, CPU 2, two ABBA cycles per series; descriptive paired ratios, not aggregate CIs",
                              "Resource attachment excluded from timing/allocator windows; disposal and callbacks included",
                              "Disk quota interrupted resource build before timing; retry used experiment-isolated A/B targets"]}
    destination.write_text(json.dumps(result, indent=2) + "\n")
    for name, data in series.items():
        for key, row in data["results"].items():
            if name != "graphs" or "/reactive/" in key:
                print(name, key, round(row["paired_geomean_percent"], 2))
    print(f"Identical allocator/work scenarios: {len(a)}; saved {destination}")


if __name__ == "__main__":
    main()
