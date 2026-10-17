#!/usr/bin/env python3
"""Compare Clean-source validation with its preserved release baseline."""
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
    document = {"optimization": "Validate Clean/Idle initialized value sources and revision in one short borrow", "suites": {}}
    for suite in ["checks", "comparison"]:
        labels = [f"clean-{stage}-{suite}" for stage in ["before", "after", "repeat"]]
        reports = [load(label) for label in labels]
        before, after, repeat = [report["estimates_ns"] for report in reports]
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
    memory_reports = [load(f"clean-{stage}-memory") for stage in ["before", "after"]]
    assert memory_reports[0]["features"] == memory_reports[1]["features"] == ["diagnostics"]
    memory = []
    for old, new in zip(*(report["metrics"]["rows"] for report in memory_reports), strict=True):
        assert old["scenario"] == new["scenario"]
        field = "steady_work" if "steady_work" in old else "work" if "work" in old else None
        if field:
            assert old[field]["work"] == new[field]["work"], old["scenario"]
            assert old[field]["node_size"] == new[field]["node_size"]
            assert old["steady_state"] == new["steady_state"]
        memory.append({"before": old, "after": new})
    document["memory"] = {"inputs": ["clean-before-memory", "clean-after-memory"], "rows": memory}
    document["limitations"] = [
        "One baseline and two candidate runs, not ABBA; one machine, no governor/isolation changes",
        "Short windows; clock-pair cost included in iter_custom",
        "Diamonds 256 do not improve; isolated writes show variation/regression not directly explained by this path",
        "Comparison filtered to reactive; no new CPU profile or other-crate measurements",
    ]
    path = LOG / "clean-analysis.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
