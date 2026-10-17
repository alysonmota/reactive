#!/usr/bin/env python3
"""Preserve accepted and rejected boundary experiments, including assembly."""
import json
from pathlib import Path

LOG = Path(__file__).resolve().parent / ".log"


def load(label):
    data = json.loads((LOG / "reports" / f"{label}.json").read_text())
    if "status_code" in data:
        assert data["status_code"] == 0, label
    return data


def estimate(value):
    return value.get("slope", value["mean"])


def compare(baseline_label, labels):
    baseline = load(baseline_label)
    rows = {}
    for label in labels:
        candidate = load(label)
        assert not candidate["instrumented"] and not candidate["features"]
        assert candidate["benchmark_cpu"] == baseline["benchmark_cpu"]
        assert candidate["estimates_ns"].keys() == baseline["estimates_ns"].keys()
        for path, digest in baseline["source_sha256"].items():
            if path.startswith("benches/") or path in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"]:
                assert candidate["source_sha256"][path] == digest, path
        rows[label] = {}
        for key in baseline["estimates_ns"]:
            old, new = estimate(baseline["estimates_ns"][key]), estimate(candidate["estimates_ns"][key])
            rows[label][key] = {"baseline_ns": old, "candidate_ns": new,
                                "change_percent": 100 * (new["point_estimate"] / old["point_estimate"] - 1)}
    return {"baseline": baseline_label, "candidates": rows}


def main():
    document = {
        "accepted": "Keep boundary depth/catches, skip notification attempt on nested exit; remove duplicate check immediately before set_phase",
        "rejected": "Fused phase/capture, including forced inline variant: no reliable improvement in large chains",
        "comparison": compare("boundary-before-comparison", ["boundary-after-comparison", "boundary-group-repeat", "boundary-inline-comparison", "boundary-minimal-comparison", "boundary-minimal-repeat"]),
        "checks": compare("boundary-before-checks", ["boundary-after-checks", "boundary-minimal-checks"]),
    }
    memory = []
    old_rows, new_rows = [load(label)["metrics"]["rows"] for label in ["boundary-before-memory", "boundary-minimal-memory"]]
    for old, new in zip(old_rows, new_rows, strict=True):
        assert old["scenario"] == new["scenario"]
        field = "steady_work" if "steady_work" in old else "work" if "work" in old else None
        if field:
            assert old[field]["work"] == new[field]["work"]
            assert old[field]["node_size"] == new[field]["node_size"]
            assert old["steady_state"] == new["steady_state"]
        memory.append({"baseline": old, "accepted": new})
    document["memory"] = memory
    document["assembly"] = {label: load(label)["symbols"] for label in ["boundary-before-assembly", "boundary-after-assembly", "boundary-minimal-assembly"]}
    document["limitations"] = ["One baseline, two accepted comparison runs, one accepted checks run; not ABBA", "Single machine, no CPU isolation/governor changes", "Symbol sizes are not exclusive timing and assembly builds use separate symbol flags", "No cold splitting adopted; no new CPU sampling; other crates not rerun", "Changes in scalar reads/writes do not establish causal benefit from memo transitions"]
    path = LOG / "boundary-analysis.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
