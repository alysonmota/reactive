#!/usr/bin/env python3
"""Analyze stable-prefix optimization against its preserved release baseline."""
import json
from pathlib import Path

LOG = Path(__file__).resolve().parent / ".log"


def load(label):
    report = json.loads((LOG / "reports" / f"{label}.json").read_text())
    assert report["status_code"] == 0, label
    return report


def estimate(value):
    return value.get("slope", value["mean"])


def main():
    document = {"optimization": "Stable source prefix before dedup; lazy hash catch-up without new storage fields", "suites": {}}
    for suite in ["fanin", "comparison"]:
        labels = [f"prefix-{stage}-{suite}" for stage in ["before", "after", "repeat"]]
        reports = [load(label) for label in labels]
        before, after, repeat = [r["estimates_ns"] for r in reports]
        assert before.keys() == after.keys() == repeat.keys()
        for report in reports:
            assert not report["instrumented"] and not report["features"]
            assert report["benchmark_cpu"] == reports[0]["benchmark_cpu"]
            for path, digest in reports[0]["source_sha256"].items():
                if path.startswith("benches/") or path in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"]:
                    assert report["source_sha256"][path] == digest, path
        rows = {}
        for key in before:
            old, new, again = [estimate(values[key]) for values in [before, after, repeat]]
            rows[key] = {
                "before_ns": old, "after_ns": new, "repeat_ns": again,
                "change_percent": 100 * (new["point_estimate"] / old["point_estimate"] - 1),
                "repeat_change_percent": 100 * (again["point_estimate"] / old["point_estimate"] - 1),
            }
        document["suites"][suite] = {"inputs": labels, "estimates": rows}
    before, after = [load(f"prefix-{stage}-memory")["metrics"]["rows"] for stage in ["before", "after"]]
    memory = []
    for old, new in zip(before, after, strict=True):
        assert old["scenario"] == new["scenario"]
        if "steady_work" in old:
            assert old["steady_work"]["work"] == new["steady_work"]["work"]
            assert old["steady_work"]["node_size"] == new["steady_work"]["node_size"]
        memory.append({"before": old, "after": new})
    document["memory"] = {"inputs": ["prefix-before-memory", "prefix-after-memory"], "rows": memory}
    document["limitations"] = [
        "One baseline and two candidate runs, not an ABBA experiment; one machine, no CPU isolation/governor changes",
        "Short exploratory windows; clock-pair cost included in fan-in measurements",
        "Regressions exist for minimal stable graphs and some large reordered/nested graphs",
        "No new profiling; scalar unobserved reads do not exercise this fast path",
    ]
    path = LOG / "prefix-analysis.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
