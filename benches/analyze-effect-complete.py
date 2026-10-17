#!/usr/bin/env python3
"""Compare completion fixtures without equating flush timing to isolated completion."""
import json
from pathlib import Path

LOG = Path(__file__).resolve().parent / ".log"


def main():
    labels = [f"effect-complete-{stage}-runtime" for stage in ["before", "after", "fused", "repeat"]]
    reports = [json.loads((LOG / "reports" / f"{label}.json").read_text()) for label in labels]
    assert all(report["status_code"] == 0 and not report["features"] and not report["instrumented"] for report in reports)
    assert len({report["benchmark_cpu"] for report in reports}) == 1
    for report in reports:
        for path, digest in reports[0]["source_sha256"].items():
            if path.startswith("benches/") or path in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"]:
                assert report["source_sha256"][path] == digest, path
        assert report["estimates_ns"].keys() == reports[0]["estimates_ns"].keys()
    rows = {}
    for key in reports[0]["estimates_ns"]:
        estimates = [(report["estimates_ns"][key].get("slope") or report["estimates_ns"][key]["mean"]) for report in reports]
        rows[key] = dict(zip(["before_ns", "initial_fusion_ns", "fused_no_self_copy_ns", "repeat_ns"], estimates, strict=True))
        rows[key]["repeat_change_percent"] = 100 * (estimates[-1]["point_estimate"] / estimates[0]["point_estimate"] - 1)
    result = {"inputs": labels, "estimates": rows, "limitations": [
        "Measures flush, including callbacks/scheduling, not isolated complete_effect",
        "O(k + m - p) remains: suffix unlinking is necessary with bidirectional observer vectors",
        "One machine, not ABBA; repeat uses 50 samples/0.3s warmup/1s measurement versus 30/0.2s/0.5s",
        "Small timing differences do not prove significance or universal speedup",
        "Initial fusion with redundant self-copies replaced by no-self-copy variant"]}
    path = LOG / "effect-complete-analysis.json"
    path.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
