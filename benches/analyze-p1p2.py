#!/usr/bin/env python3
"""Validate P0 provenance and summarize the P1/P2 semantic-subset experiment."""
import hashlib
import json
import argparse
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"


def load(label):
    return json.loads((LOG / f"{label}.json").read_text())


def main():
    parser=argparse.ArgumentParser(__doc__)
    parser.add_argument("--check-current",action="store_true",help="Require the historical prototype to match current experimental sources")
    args=parser.parse_args()
    final, memory, work, sham, baseline, matrix = [load(label) for label in [
        "p1p2-final", "p1p2-final-memory", "p1p2-final-work", "p1p2-sham", "p1p2-frozen-baseline", "p1p2-final-matrix"]]
    frozen = load("p1p2-baseline")
    bundle = load("p1p2-frozen-source")["sources"]
    for path, sha in frozen["source_sha256"].items():
        assert hashlib.sha256(bundle[path].encode()).hexdigest() == sha
        if path.startswith("src/"):
            assert final["source_sha256"][path] == sha
            assert hashlib.sha256((ROOT / path).read_bytes()).hexdigest() == sha
    for run in [final, memory, work, sham, matrix]:
        for path, sha in final["source_sha256"].items():
            if path.startswith(("src/", "experiments/")):
                assert run["source_sha256"][path] == sha
                if args.check_current or path.startswith("src/"):
                    assert hashlib.sha256((ROOT / path).read_bytes()).hexdigest() == sha
    assert len(baseline["runs"][0]["estimates"]) == 45
    assert len(matrix["runs"][0]["estimates"]) == 64
    assert [r["variant"] for r in final["runs"]] == list("abbaabba")
    assert [r["variant"] for r in sham["runs"]] == list("abbaabba")
    assert final["binary"] == matrix["binary"] == sham["binary"]
    destination = LOG / "p1p2-analysis.json"
    previous = load("p1p2-analysis") if destination.exists() else {}
    rows = []
    for shape in ["chain", "diamond", "fan_out", "conditional"]:
        a = next(r for r in memory["metrics"]["rows"] if r["shape"] == shape and r["engine"] == "reactive")
        b = next(r for r in memory["metrics"]["rows"] if r["shape"] == shape and r["engine"] == "typed_research")
        assert a["effect_calls_including_warmup"] == b["effect_calls_including_warmup"]
        for r in [a,b]:
            assert (r["steady_allocations"],r["steady_reallocations"],r["after_drop_net_bytes"]) == (0,0,0)
        aw = next(r["work"] for r in work["metrics"]["rows"] if r["shape"] == shape and r["engine"] == "reactive")
        bw = next(r["work"] for r in work["metrics"]["rows"] if r["shape"] == shape and r["engine"] == "typed_research")
        for field in ["memo_executions", "effect_executions", "source_checks"]:
            assert aw[field] == bw[field]
        rows.append({"shape": shape, "baseline_bytes":a["live_requested_bytes"], "prototype_bytes":b["live_requested_bytes"],
                     "memory_percent":100*(b["live_requested_bytes"]/a["live_requested_bytes"]-1),
                     "baseline_work":aw,"prototype_work":bw})
    miri = previous.get("miri", "")
    path = Path("/tmp/opencode/p1p2-final-miri.log")
    if path.exists():
        miri = path.read_text()
    assert "10 passed; 0 failed" in miri
    build = previous.get("build_metadata", {})
    path = LOG / "runs/p1p2-final/1-a/criterion/build.json"
    if path.exists():
        build = json.loads(path.read_text())
    assert build["panic_unwind"] and not build["diagnostics"] and not build["verification"]
    result = {"status":"Research prototype retained, NOT production equivalent",
              "inputs":[r["label"] for r in [final,memory,work,sham,baseline,matrix]],
              "performance":final["summary"], "sham":sham["summary"], "memory_and_work":rows,
              "layout_bytes":memory["metrics"]["prototype_layout"], "build_metadata":build, "miri":miri,
              "limitations":["No explicit owners/cleanups, writable effects, batching or budgets; not an isolated representation-only A/B",
                             "Marking work differs; memo/effect execution counts and source checks match on measured fixtures",
                             "One CPU/machine and two ABBA cycles; sham Diamond varied -5.13%, so small deltas are inconclusive",
                             "Memory is requested live bytes after warmup, not RSS or peak; reference TLS may retain allocations",
                             "Intermediate pilot/confirmation/matrix/perf artifacts do not describe the final revision",
                             "Fresh 45-case baseline and 64-case matrix are short runs, not full ABBA for every size"]}
    destination.write_text(json.dumps(result,indent=2)+"\n")
    for family,row in final["summary"].items():
        print(family, {k:round(v/1000,3) for k,v in row["means_ns"].items()}, round(row["paired_percent"],2))
    for row in rows:
        print("memory",row["shape"],row["baseline_bytes"],row["prototype_bytes"],round(row["memory_percent"],2))
    print(destination)


if __name__ == "__main__":
    main()
