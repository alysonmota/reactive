#!/usr/bin/env python3
"""Generate a versionable derived summary, never outside benches/.log."""
import json
from pathlib import Path

LOG = Path(__file__).resolve().parent / ".log"


def load(label):
    return json.loads((LOG / "reports" / f"{label}.json").read_text())


def estimate(values):
    # Prefer Criterion's regression estimator when available; preserve CIs.
    return values.get("slope", values["mean"])


def main():
    before = load("baseline-complete")["estimates_ns"]
    after = load("final-comparison")["estimates_ns"]
    repeat = load("final-repeat")["estimates_ns"]
    comparisons = {}
    for key in sorted(before.keys() & after.keys() & repeat.keys()):
        old, new, again = (estimate(data[key]) for data in [before, after, repeat])
        comparisons[key] = {"baseline_ns": old, "final_ns": new, "repeat_ns": again,
                            "change_percent": 100 * (new["point_estimate"] / old["point_estimate"] - 1),
                            "repeat_change_percent": 100 * (again["point_estimate"] / old["point_estimate"] - 1)}
    thresholds = {n: load(f"fanin-{n}")["estimates_ns"] for n in [16, 32, 64, 128]}
    sweeps = {key: {str(n): estimate(data[key]) for n, data in thresholds.items()} for key in sorted(thresholds[16])}
    memory = load("memory-final")["metrics"]
    profile = load("profile-chain-full")
    document = {
        "inputs": ["baseline-complete", "final-comparison", "final-repeat", "runtime-before", "runtime-after", "memory-before", "memory-final", "fanin-16", "fanin-32", "fanin-64", "fanin-128", "pending-count", "direct-effect", "profile-chain-full"],
        "comparisons": comparisons, "threshold_sweep_ns": sweeps,
        "runtime_phases_ns": load("runtime-after")["estimates_ns"],
        "pending_count_black_box_ns": load("pending-count")["estimates_ns"],
        "direct_effect_ns": load("direct-effect")["estimates_ns"],
        "memory": memory,
        "effective_benchmark_build": load("final-comparison").get("build"),
        "profile_samples": profile.get("perf_report", "").splitlines()[:45],
        "limitations": ["Short exploratory runs, one machine, no isolated CPU/governor changes", "Fan-out varies between runs; improvements are not universal", "Clock-pair cost is included in iter_custom cases", "Layout experiment is a surrogate, not a modified runtime", "Requested allocator bytes are not RSS", "Initial timeout baseline and smoke-only profile-chain are excluded"],
    }
    path = LOG / "analysis.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
