#!/usr/bin/env python3
"""Validate and consolidate the reintegrated-contract checkpoint (no new runs)."""
import argparse
import hashlib
import json
from pathlib import Path

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/"benches/.log"


def read(label):return json.loads((LOG/f"{label}.json").read_text())


def main():
    parser=argparse.ArgumentParser(__doc__)
    parser.add_argument("--prefix",default="p1p2-contract-v2")
    parser.add_argument("--miri-log",type=Path,default=Path("/tmp/opencode/p1p2-contract-v2-miri.log"))
    args=parser.parse_args()
    final,memory,work,sham,matrix,retention=[read(args.prefix+"-"+suffix) for suffix in ["final","memory","work","sham","matrix","retention"]]
    frozen=read("p1p2-baseline")
    pre_contract=read("p1p2-pre-contract-source")
    for path,sha in frozen["source_sha256"].items():
        if path.startswith("src/"):
            assert final["source_sha256"][path]==sha
    for run in [final,memory,work,sham,matrix]:
        assert run["source_sha256"]==final["source_sha256"],run["label"]
    for path,sha in final["source_sha256"].items():
        assert hashlib.sha256((ROOT/path).read_bytes()).hexdigest()==sha,path
    for path,sha in pre_contract["sha256"].items():
        assert retention["control_source_sha256"][path]==sha,path
    assert retention["binaries"]["b"]==final["binary"]==sham["binary"]==matrix["binary"]
    for run in [final,sham,retention]:
        assert [r["variant"] for r in run["runs"]]==list("abbaabba")
    assert len(matrix["runs"][0]["estimates"])==68
    rows=[]
    for shape in ["chain","diamond","fan_out","conditional"]:
        a,b=[next(r for r in memory["metrics"]["rows"] if r["engine"]==engine and r["shape"]==shape) for engine in ["reactive","typed_research"]]
        aw,bw=[next(r["work"] for r in work["metrics"]["rows"] if r["engine"]==engine and r["shape"]==shape) for engine in ["reactive","typed_research"]]
        for row in [a,b]:assert (row["steady_allocations"],row["steady_reallocations"],row["after_drop_net_bytes"])==(0,0,0)
        for field in ["memo_executions","effect_executions","source_checks"]:assert aw[field]==bw[field]
        assert a["effect_calls_including_warmup"]==b["effect_calls_including_warmup"]
        rows.append({"shape":shape,"production_bytes":a["live_requested_bytes"],"typed_bytes":b["live_requested_bytes"],
                     "memory_percent":100*(b["live_requested_bytes"]/a["live_requested_bytes"]-1),"production_work":aw,"typed_work":bw})
    for row in memory["metrics"]["owner_resources"]:
        assert row["after_drop_net_bytes"]==0 and row["cleanups"]==1160
    destination=LOG/(args.prefix+"-analysis.json")
    previous=json.loads(destination.read_text()) if destination.exists() else {}
    miri=args.miri_log.read_text() if args.miri_log.exists() else previous.get("miri","")
    for count in [7,9,10]:assert f"{count} passed; 0 failed" in miri
    build_path=LOG/"runs"/final["label"]/"1-a/criterion/build.json"
    build=json.loads(build_path.read_text()) if build_path.exists() else previous.get("build_metadata",{})
    assert build["panic_unwind"] and not build["diagnostics"] and not build["verification"]
    result={"status":"Retain as experimental; do not promote","inputs":[run["label"] for run in [final,memory,work,sham,matrix,retention]],
            "performance":final["summary"],"versus_pre_contract":retention["summary"],"sham":sham["summary"],
            "memory_and_work":rows,"owner_resources":memory["metrics"]["owner_resources"],"layout":memory["metrics"]["prototype_layout"],
            "miri":miri,"build_metadata":build,
            "limitations":["Core contracts reintegrated, but not full public API/error/diagnostic conformance or a soundness proof",
                           "Marking work differs; measured memo/effect execution and source-check counts match",
                           "Two ABBA cycles on one CPU; short sham, no aggregate confidence interval",
                           "Retention compares integrated revisions and different executables, not additive contract costs",
                           "Owner-resource fixture allocates by design; zero recurring allocations applies only to scalar graphs",
                           "API gaps include custom comparators, update_cloned/conflict protocol, complete failure history and diagnostic snapshots",
                           "Individual node disposal still searches its owner's node list; no claim of production complexity parity",
                           "Miri directed to 26 tests, not the full runtime/model/conformance suite"]}
    destination.write_text(json.dumps(result,indent=2)+"\n")
    for family,row in final["summary"].items():print(family,{k:round(v/1000,3) for k,v in row["means_ns"].items()},round(row["paired_percent"],2))
    for row in rows:print("memory",row["shape"],row["production_bytes"],row["typed_bytes"],round(row["memory_percent"],2))
    print(destination)


if __name__=="__main__":main()
