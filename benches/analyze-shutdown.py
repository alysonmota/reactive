#!/usr/bin/env python3
"""Preserve estimates/CIs and work/allocation counts for whole-forest shutdown."""
import json
from pathlib import Path

LOG = Path(__file__).resolve().parent / ".log"


def load(label):
    value = json.loads((LOG / "reports" / f"{label}.json").read_text())
    assert value["status_code"] == 0, label
    return value


def estimate(value):
    return value.get("slope") or value["mean"]


def main():
    labels = ["shutdown-before-repeat-runtime", "shutdown-after-runtime", "shutdown-repeat-runtime"]
    reports = [load(label) for label in labels]
    before, after, repeat = [report["estimates_ns"] for report in reports]
    assert before.keys() == after.keys() == repeat.keys()
    assert all(not report["features"] and not report["instrumented"] for report in reports)
    assert len({report["benchmark_cpu"] for report in reports}) == 1
    for report in reports:
        for path, digest in reports[0]["source_sha256"].items():
            if path.startswith("benches/") or path in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"]:
                assert report["source_sha256"][path] == digest, path
    rows = {}
    for key in before:
        old, new, again = [estimate(values[key]) for values in [before, after, repeat]]
        rows[key] = {"before_ns": old, "after_ns": new, "repeat_ns": again,
                     "repeat_change_percent": 100 * (again["point_estimate"] / old["point_estimate"] - 1)}
    memory_labels = ["shutdown-before-memory", "shutdown-after-memory"]
    memory = []
    old_rows, new_rows = [load(label)["metrics"]["rows"] for label in memory_labels]
    for old, new in zip(old_rows, new_rows, strict=True):
        assert old["scenario"] == new["scenario"]
        if old["scenario"] != "root_shutdown":
            continue
        assert old["shape"] == new["shape"] and old["width"] == new["width"]
        work = new["work"]["work"]
        assert new["work"]["nodes"] == new["work"]["edges"] == 0
        assert work["mark_visits"] == work["state_changes"] == work["tickets_enqueued"] == 0
        assert work["edges_removed"] == old["work"]["work"]["edges_removed"]
        assert new["after_runtime_drop"]["net_requested_bytes"] == 0
        memory.append({"before": old, "after": new})
    result = {"timing_inputs": labels, "estimates": rows, "memory_inputs": memory_labels, "memory": memory,
              "excluded_timing": "shutdown-before-runtime: timing and allocator executable ran concurrently on CPU 2; replaced by isolated baseline rerun",
              "baseline_rerun": "Restored root-by-root shutdown; factored subtree helpers retained, traversal starts with capacity 1 as original vec![root]",
              "limitations": ["One machine, short exploratory runs, not ABBA", "Fan-in 4096 regresses in repeat; no universal latency improvement", "Whole-forest traversal adds O(owners) temporary IDs; callbacks/destructors/allocator/queues retain their own costs", "No new Sycamore/Leptos ranking"]}
    path = LOG / "shutdown-analysis.json"
    path.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
