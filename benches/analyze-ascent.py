#!/usr/bin/env python3
"""Preserve isolated ascent prototypes, paired timings and allocation costs."""
import difflib
import hashlib
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


def series(name, graphs=False):
    suite = "comparison" if graphs else "chain"
    prefix = f"ascent-{name}" + ("-graphs" if graphs else "")
    cycle_names = ["1", "2r" if name == "iterative" and graphs else "2"]
    cycles = [[helpers.load(f"{prefix}-c{c}-p{p}-{v}-{suite}") for p, v in enumerate("abba", 1)]
              for c in cycle_names]
    expected = {"src/runtime/memos.rs"}
    if name in ["scope", "compactscope"]:
        expected.add("src/runtime.rs")
    if name == "compactscope":
        expected.add("src/context.rs")
    baseline = cycles[0][0]["source_sha256"]
    for cycle in cycles:
        for pos, run in enumerate(cycle):
            assert not run["features"] and not run["instrumented"] and not run["profiling"]
            changed = {p for p, digest in baseline.items() if p.startswith("src/") and run["source_sha256"][p] != digest}
            assert changed == (expected if pos in [1, 2] else set())
            assert run["source_sha256"] == cycles[0][pos]["source_sha256"]
    rows = {}
    for key in cycles[0][0]["estimates_ns"]:
        a = [helpers.point(c[i], key) for c in cycles for i in [0, 3]]
        b = [helpers.point(c[i], key) for c in cycles for i in [1, 2]]
        ratios = [math.sqrt(helpers.point(c[1], key) * helpers.point(c[2], key)
                            / helpers.point(c[0], key) / helpers.point(c[3], key)) for c in cycles]
        rows[key] = {"a_mean_ns": statistics.mean(a), "b_mean_ns": statistics.mean(b),
                     "a_range_ns": [min(a), max(a)], "b_range_ns": [min(b), max(b)],
                     "cycle_percent": [100 * (r - 1) for r in ratios],
                     "paired_geomean_percent": 100 * (math.sqrt(math.prod(ratios)) - 1)}
    return {"inputs": [[r["label"] for r in c] for c in cycles], "results": rows}


def main():
    destination = LOG / "ascent-analysis.json"
    old = json.loads(destination.read_text()) if destination.exists() else {}
    baseline = Path("/tmp/opencode/reactive-ascent-baseline")
    patches = dict(old.get("production_patches", {}))
    results = {}
    for name in ["return", "scope", "compactscope", "iterative"]:
        candidate = Path(f"/tmp/opencode/reactive-ascent-{name}")
        if baseline.exists() and candidate.exists():
            patches[name] = "".join("".join(difflib.unified_diff(
                (production((baseline / p).read_text()) + "\n").splitlines(True),
                (production((candidate / p).read_text()) + "\n").splitlines(True),
                fromfile=f"a/{p}", tofile=f"b/{p}")) for p in ["src/runtime.rs", "src/runtime/memos.rs", "src/context.rs"])
        assert name in patches
        results[name] = {"chain": series(name), "graphs": series(name, graphs=True)}
    memory = helpers.load("ascent-iterative-memory")
    chain = [r for r in memory["metrics"]["rows"] if r["scenario"] == "chain_work"]
    baseline_memory = helpers.load("check-entry-memory")
    for row in chain:
        previous = next(r for r in baseline_memory["metrics"]["rows"] if r["scenario"] == "chain_work"
                        and (r["size"], r["equality_cut"]) == (row["size"], row["equality_cut"]))
        assert row["work"]["work"] == previous["work"]["work"], "logical work changed"
        assert row["after_runtime_drop"]["net_requested_bytes"] == 0
    # Native-only regressions were added after timing; verify final production,
    # not the cfg(test) suffix. Preserve hashes so future reruns can detect drift.
    production_hashes = old.get("production_sha256", {})
    if baseline.exists():
        production_hashes = {}
        for p in ROOT.glob("src/**/*.rs"):
            relative = p.relative_to(ROOT)
            reference = production((baseline / relative).read_text())
            assert production(p.read_text()) == reference, f"unexpected final production change: {relative}"
            production_hashes[str(relative)] = hashlib.sha256(reference.encode()).hexdigest()
    result = {"candidates": results, "production_patches": patches, "production_sha256": production_hashes,
              "iterative_memory_input": memory["label"], "iterative_chain_memory": chain,
              "excluded": ["ascent-iterative-graphs-c2-* and its final retry: command timed out, replaced by complete c2r ABBA"],
              "notes": ["No production candidate adopted", "Two ABBA cycles, fixed benchmark order, one CPU/machine",
                        "Scope wrapper alone measured 72 bytes, full guard 80; compact scope measured 64 and full guard 72",
                        "Iterative allocator benchmark only removes zero-allocation assertions; production sources are unchanged",
                        "Native-only layout/unwind tests were added to snapshots after timings; patches omit cfg(test) suffixes",
                        "CIs retained per run; paired cycle ratios are descriptive, not a combined statistical CI"]}
    destination.write_text(json.dumps(result, indent=2) + "\n")
    for name, data in results.items():
        print(name)
        for kind in ["chain", "graphs"]:
            for key, row in data[kind]["results"].items():
                if kind == "chain" or ("/reactive/" in key and "/256" in key):
                    print(key, round(row["a_mean_ns"], 2), round(row["b_mean_ns"], 2),
                          round(row["paired_geomean_percent"], 2), [round(x, 2) for x in row["cycle_percent"]])
    print(f"Saved {destination}")


if __name__ == "__main__":
    main()
