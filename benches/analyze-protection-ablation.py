#!/usr/bin/env python3
"""Analyze contract-breaking ablations, never recommend them for adoption."""
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


def main():
    manifest = json.loads((LOG / "protection-ablation-manifest.json").read_text())
    for path, text in manifest["production_sources"].items():
        assert (ROOT / path).read_text() == text, f"Production changed: {path}"
    output = {"decision": "Ablation only; none eligible for production", "variants": {},
              "excluded": "All ablate-* runs without v2: shared-target stale binaries; do not interpret their deltas"}
    destination = LOG / "protection-ablation-analysis.json"
    previous = json.loads(destination.read_text()) if destination.exists() else {}
    negative = json.loads((LOG / "protection-ablation-negative.json").read_text())
    assert len(negative) == 10
    for name, result in negative.items():
        assert result["status"] == "completed" and result["exit_codes"] == {"control": 0, name: 101}
        result["evidence"] = previous.get("negative_checks", {}).get(name, {}).get("evidence", {})
        for copy in ["control", name]:
            path = Path(f"/tmp/opencode/reactive-protection-ablation/{name}-negative-{copy}.log")
            if path.exists():
                text = path.read_text()
                assert "running 1 test" in text
                assert ("1 passed; 0 failed" if copy == "control" else "0 passed; 1 failed") in text
                result["evidence"][copy] = text[text.index("running 1 test"):]
    output["negative_checks"] = negative
    output["graph_borrow_miri"] = previous.get("graph_borrow_miri", {})
    path = Path("/tmp/opencode/protection-ablation-miri.log")
    probe = Path("/tmp/opencode/reactive-protection-ablation/graph-borrow-checks/tests/ablation_valid.rs")
    if path.exists() and probe.exists():
        text = path.read_text()
        assert "1 passed; 0 failed" in text
        output["graph_borrow_miri"] = {"source": probe.read_text(), "log": text,
                                       "scope": "Valid chain/cut, dynamic fan-in and resources at sizes 8/16 only; not general safety"}
    control_memory = helpers.load("ablate-v2-control-memory")["metrics"]["rows"]
    for variant in manifest["variants"]:
        item = {"series": {}}
        for suite in ["chain", "comparison", "runtime"]:
            if variant in ["owner-renewal", "arena-generation-life", "combined-checks"] and suite == "runtime":
                continue
            prefix = f"ablate-v2-{variant}-{suite}"
            builds = json.loads((LOG / "reports" / f"{prefix}-builds.json").read_text())
            assert all(builds[v]["rebuild_package"] and builds[v]["executables"] for v in "ab")
            cycles = [[helpers.load(f"{prefix}-c{c}-p{p}-{v}-{suite}")
                       for p, v in enumerate("abba", 1)] for c in [1, 2]]
            changed_files = {line[6:] for line in manifest["variants"][variant].splitlines() if line.startswith("+++ b/")}
            control = cycles[0][0]["source_sha256"]
            for cycle in cycles:
                for pos, run in enumerate(cycle):
                    assert not run["features"] and not run["instrumented"] and not run["profiling"]
                    changes = {p for p, digest in control.items() if p.startswith("src/") and run["source_sha256"][p] != digest}
                    assert changes == (changed_files if pos in [1, 2] else set()), (variant, changes)
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
            item["series"][suite] = {"inputs": [[r["label"] for r in c] for c in cycles], "builds": builds, "results": rows}
        if variant not in ["owner-renewal", "arena-generation-life", "combined-checks", "graph-borrow-checks"]:
            memory = helpers.load(f"ablate-v2-{variant}-memory")["metrics"]["rows"]
            assert memory == control_memory, variant
            item["identical_memory_work_rows"] = len(memory)
        else:
            item["identical_memory_work_rows"] = None
        output["variants"][variant] = item
    destination.write_text(json.dumps(output, indent=2) + "\n")
    print("| Ablation | Full | Cut | Diamond | Conditional | Empty/32 | Cleanup/32 | Node/32 | Child/32 |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
    for variant, item in output["variants"].items():
        rows = {key: row for series in item["series"].values() for key, row in series["results"].items()}
        names = ["chain_phases_native_total_256", "chain_phases_equality_cut_256",
                 "diamond_write_to_observed/reactive/256", "conditional_write_to_observed/reactive/256"]
        names += [f"memo_owner_execution_{name}_32" for name in ["empty", "cleanup", "node_drop", "child_cleanup"]]
        values = [f'{rows[name]["paired_geomean_percent"]:+.2f}%' if name in rows else "N/A" for name in names]
        print("| " + " | ".join([variant] + values) + " |")


if __name__ == "__main__":
    main()
