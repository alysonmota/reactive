#!/usr/bin/env python3
"""Analyze individual/bulk source disposal including deferred reconciliation."""
import json
from pathlib import Path

LOG = Path(__file__).resolve().parent / ".log"


def load(label):
    data = json.loads((LOG / "reports" / f"{label}.json").read_text())
    assert data["status_code"] == 0, label
    return data


def estimate(value):
    # Long baseline samples can select flat sampling (slope is then null).
    return value.get("slope") or value["mean"]


def main():
    document = {"optimization": "Keep ID-only source tombstones at fixed indices; reconcile once instead of compacting after each disposal", "suites": {}}
    for suite in ["runtime", "comparison"]:
        labels = [f"disposal-{stage}-{suite}" for stage in ["before", "after", "repeat"]]
        reports = [load(label) for label in labels]
        before, after, repeat = [report["estimates_ns"] for report in reports]
        assert before.keys() == after.keys() == repeat.keys()
        assert all(not report["features"] and not report["instrumented"] for report in reports)
        assert len({report["benchmark_cpu"] for report in reports}) == 1
        for report in reports:
            for path, digest in reports[0]["source_sha256"].items():
                # Memory-only JSON fields changed to expose tombstones; that
                # executable is not linked into either timing suite.
                if (path.startswith("benches/") and path != "benches/memory.rs") or path in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"]:
                    assert report["source_sha256"][path] == digest, path
        rows = {}
        for key in before:
            old, new, again = [estimate(values[key]) for values in [before, after, repeat]]
            rows[key] = {"before_ns": old, "after_ns": new, "repeat_ns": again,
                         "repeat_speedup": old["point_estimate"] / again["point_estimate"],
                         "repeat_change_percent": 100 * (again["point_estimate"] / old["point_estimate"] - 1)}
        document["suites"][suite] = {"inputs": labels, "estimates": rows}
    before, after = [load(label)["metrics"]["rows"] for label in ["disposal-before-memory", "disposal-final-memory"]]
    memory = []
    for old, new in zip(before, after, strict=True):
        assert old["scenario"] == new["scenario"]
        if old["scenario"].startswith("source_disposal"):
            assert new["disposed_work"]["edges"] == 0
            assert new["disposed_work"]["source_slots"] == new["width"]
            assert new["disposed_work"]["source_tombstones"] == new["width"]
            assert new["reconciled_work"]["source_tombstones"] == 0
            assert new["disposed_work"]["work"]["edges_removed"] == old["disposed_work"]["work"]["edges_removed"] == new["width"]
            assert new["reconciled_work"]["node_size"] == old["reconciled_work"]["node_size"]
            assert new["after_dispose"]["net_requested_bytes"] == old["after_dispose"]["net_requested_bytes"]
        field = "steady_work" if "steady_work" in old else "work" if "work" in old else None
        if field:
            assert old[field]["work"] == new[field]["work"]
            assert old["steady_state"] == new["steady_state"]
        memory.append({"before": old, "after": new})
    document["memory"] = {"inputs": ["disposal-before-memory", "disposal-final-memory"], "rows": memory}
    document["limitations"] = ["One baseline and two candidate runs, not ABBA; one machine", "Flat sampling uses mean where slope is null; preserve original CIs", "Total O(k) describes this source-disposal fixture without intervening reader executions; callbacks/allocators and unrelated invalidation have their own cost", "Physical ID tombstones can persist until reader evaluation/disposal; no payload retention", "Bulk 4096 barely changes; some non-disposal benchmarks regress"]
    path = LOG / "disposal-analysis.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
