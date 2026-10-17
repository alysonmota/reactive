#!/usr/bin/env python3
"""Bounded ABBA: frozen pre-contract prototype versus reintegrated typed core.

The frozen source bundle is used, not a mutable shared-target cache. This
comparison measures the integrated revision, NOT an additive contract cost.
"""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import statistics
import subprocess

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/"benches/.log"
spec=importlib.util.spec_from_file_location("runner",ROOT/"benches/run-p1p2.py")
runner=importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def main():
    p=argparse.ArgumentParser(__doc__)
    p.add_argument("label")
    p.add_argument("--candidate",default="p1p2-contract-final")
    p.add_argument("--cpu",type=int,default=2)
    args=p.parse_args()
    assert args.label and all(c.isalnum() or c in "-_" for c in args.label)
    root=LOG/"runs"/args.label
    root.mkdir(parents=True,exist_ok=False)
    package=Path("/tmp/opencode")/(args.label+"-control")
    package.mkdir(exist_ok=False)
    frozen=json.loads((LOG/"p1p2-pre-contract-source.json").read_text())
    baseline=json.loads((LOG/"p1p2-frozen-source.json").read_text())["sources"]
    sources={k:v for k,v in baseline.items() if k.startswith(("src/",".cargo/"))}
    sources.update(frozen["sources"])
    for name,text in sources.items():
        path=Path(name)
        assert not path.is_absolute() and ".." not in path.parts
        path=package/path
        path.parent.mkdir(parents=True,exist_ok=True)
        path.write_text(text)
    candidate=json.loads((LOG/f"{args.candidate}.json").read_text())
    for name,sha in candidate["source_sha256"].items():
        assert runner.digest(ROOT/name)==sha,name
    for name,sha in candidate["source_sha256"].items():
        if name.startswith("src/"):assert runner.digest(package/name)==sha,name
    env=dict(os.environ,REACTIVE_TYPED_PROTOTYPE="1",CARGO_TARGET_DIR="/tmp/opencode/reactive-p1p2-pre-contract-target")
    runner.lifecycle.STEP_TIMEOUT=240
    old,old_binary=runner.build("comparison",root,args.label,env,package)
    new=Path(candidate["binary"]["path"])
    binaries={"a":old_binary,"b":candidate["binary"]}
    result={"label":args.label,"candidate":args.candidate,"binaries":binaries,"cpu":args.cpu,
            "control_source_sha256":{name:hashlib.sha256(text.encode()).hexdigest() for name,text in sources.items()},
            "driver_source":Path(__file__).read_text(),"runs":[],"rustc":subprocess.check_output(["rustc","-Vv"],text=True),
            "limitations":"Different integrated revisions/binaries; this is not an isolated additive ownership cost or full conformance proof"}
    destination=LOG/f"{args.label}.json"
    for index,variant in enumerate("abbaabba"):
        directory=root/f"{index+1}-{variant}"
        directory.mkdir()
        env["REACTIVE_BENCH_LOG"]=str(directory/"criterion")
        binary=old if variant=="a" else new
        assert runner.digest(binary)==binaries[variant]["sha256"]
        for name,sha in candidate["source_sha256"].items():assert runner.digest(ROOT/name)==sha,name
        pattern=r"((chain|diamond|fan_out|conditional)_write_to_observed|p1p2_cut)/(typed_research|sycamore|leptos)/256$"
        command=["taskset","-c",str(args.cpu),str(binary),"--bench","--noplot","--sample-size","30","--warm-up-time","0.3","--measurement-time","0.7",pattern]
        with (directory/"stdout.log").open("w") as out:
            runner.lifecycle.run_step(command,cwd=package if variant=="a" else ROOT,env=env,stdout=out,stderr=subprocess.STDOUT,check=True)
        estimates={str(path.parent.parent.relative_to(directory/"criterion")):json.loads(path.read_text()) for path in (directory/"criterion").glob("**/new/estimates.json")}
        assert len(estimates)==15
        result["runs"].append({"variant":variant,"command":command,"estimates":estimates})
        destination.write_text(json.dumps(result,indent=2)+"\n")
    summary={}
    for family in ["chain_write_to_observed","diamond_write_to_observed","fan_out_write_to_observed","conditional_write_to_observed","p1p2_cut"]:
        key=f"{family}/typed_research/256" if family!="p1p2_cut" else "p1p2_cut_typed_research_256"
        def point(run):
            d=run["estimates"][key]
            return (d.get("slope")or d["mean"])["point_estimate"]
        cycles=[]
        for start in [0,4]:
            a,b,c,d=map(point,result["runs"][start:start+4])
            cycles.append(math.sqrt(b*c/a/d))
        summary[family]={"old_ns":statistics.mean(point(r) for r in result["runs"] if r["variant"]=="a"),
                         "new_ns":statistics.mean(point(r) for r in result["runs"] if r["variant"]=="b"),
                         "cycle_percent":[100*(r-1) for r in cycles],"paired_percent":100*(math.sqrt(math.prod(cycles))-1)}
    result["summary"]=summary
    destination.write_text(json.dumps(result,indent=2)+"\n")
    print(json.dumps(summary,indent=2),flush=True)
    print(destination,flush=True)


if __name__=="__main__":main()
