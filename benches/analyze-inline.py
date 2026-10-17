#!/usr/bin/env python3
"""Summarize paired Vec/SmallVec runs, retaining Criterion confidence intervals."""
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
    pairs = {}
    for suite in ["comparison-1", "comparison-2", "fanin", "memory"]:
        baseline_label = f"inline-vec-{suite}"
        candidate_label = f"inline-smallvec-{suite}"
        baseline, candidate = load(baseline_label), load(candidate_label)
        assert baseline["source_sha256"] == candidate["source_sha256"], "Paired runs must use identical sources"
        assert "experimental-inline-reads" not in baseline["features"]
        assert "experimental-inline-reads" in candidate["features"]
        pair = {"baseline": baseline_label, "candidate": candidate_label, "estimates": {}}
        assert baseline["estimates_ns"].keys() == candidate["estimates_ns"].keys()
        for key, value in baseline["estimates_ns"].items():
            old, new = estimate(value), estimate(candidate["estimates_ns"][key])
            pair["estimates"][key] = {
                "vec_ns": old, "smallvec_ns": new,
                "change_percent": 100 * (new["point_estimate"] / old["point_estimate"] - 1),
            }
        if suite == "memory":
            pair["rows"] = []
            for old, new in zip(baseline["metrics"]["rows"], candidate["metrics"]["rows"], strict=True):
                assert old["scenario"] == new["scenario"]
                if "steady_work" in old:
                    assert old["steady_work"]["work"] == new["steady_work"]["work"], "Observable work must remain identical"
                pair["rows"].append({"vec": old, "smallvec": new})
        pairs[suite] = pair
    document = {
        "feature": "experimental-inline-reads", "inline_capacity": 2,
        "decision": "Keep opt-in; no default promotion: timing gains inconsistent and larger nodes increase retained memory",
        "pairs": pairs,
        "limitations": ["One machine, short runs, no CPU isolation/governor change", "Memory instrumentation uses diagnostics in both builds", "SmallVec inline capacity is part of arena bytes, not additional heap bytes", "Not a hot/cold layout experiment or evidence for other buffers/capacities"],
    }
    path = LOG / "inline-analysis.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
