#!/usr/bin/env python3
"""Summarize controlled Chain optimization runs; never mix isolated targets."""
import json
from pathlib import Path

LOG = Path(__file__).resolve().parent / ".log"


def estimates(label):
    report = json.loads((LOG / "reports" / f"{label}.json").read_text())
    return {
        name: {"estimator": "slope" if value.get("slope") else "mean",
               **(value.get("slope") or value["mean"])}
        for name, value in report["estimates_ns"].items()
    }


def compare(before, after):
    baseline, candidate = estimates(before), estimates(after)
    return {
        "before": before,
        "after": after,
        "cases": {
            name: {
                "before_ns": baseline[name],
                "after_ns": candidate[name],
                "change_percent": 100 * (
                    candidate[name]["point_estimate"] /
                    baseline[name]["point_estimate"] - 1
                ),
            }
            for name in baseline
        },
    }


def main():
    result = {
        "method": "Exploratory sequential runs, CPU 2, release, default features; not ABBA. Negative change is faster.",
        "baseline": "Reconstructed original production paths in /tmp/opencode/reactive-chain-baseline; dedicated CARGO_TARGET_DIR=/tmp/opencode/reactive-chain-baseline-target.",
        "excluded": "chain-opt-isolated-* and original final/repeat confirmation pairs share a target directory; not used as confirmation evidence. Early candidate runs are exploratory only.",
        "comparisons": [
            compare(f"chain-opt-{baseline}-{suite}", f"chain-opt-retained-{suite}")
            for suite in ("chain", "snapshot")
            for baseline in ("controlled-base", "control-repeat")
        ],
        "memory": {},
    }
    for label in ("chain-opt-controlled-base-memory", "chain-opt-final-memory"):
        report = json.loads((LOG / "reports" / f"{label}.json").read_text())
        result["memory"][label] = [
            row for row in report["metrics"]["rows"]
            if row["scenario"] == "chain_work" and row["size"] == 256
        ]
    (LOG / "chain-optimization-analysis.json").write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
