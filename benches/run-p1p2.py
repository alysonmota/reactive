#!/usr/bin/env python3
"""P0: build once, hash the executable, run it directly (never cargo in timing).

All engines share fixtures. Reintegrated typed contracts still require full
conformance/audit before promotion. Raw logs stay ignored under runs/.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import math

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"
spec = importlib.util.spec_from_file_location("lifecycle", ROOT / "benches/protection-ablation.py")
lifecycle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lifecycle)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def build(bench, root, label, env, package_root=ROOT, work=False):
    with (root / f"build-{bench}.log").open("w") as stdout:
        command = ["cargo", "bench", "--locked", "--profile", "release", "--no-default-features",
                   "--bench", bench, "--no-run", "--message-format=json"]
        if work:
            command += ["--features", "diagnostics,typed-runtime-prototype/diagnostics"]
        lifecycle.run_step(command,
                           cwd=package_root, env=env, stdout=stdout, stderr=subprocess.STDOUT, check=True)
    artifacts = []
    for line in (root / f"build-{bench}.log").read_text().splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("reason") == "compiler-artifact" and event.get("target", {}).get("name") == bench and event.get("executable"):
            artifacts.append(Path(event["executable"]))
    assert artifacts
    executable = artifacts[-1]
    return executable, {"path": str(executable), "sha256": digest(executable)}


def main():
    p = argparse.ArgumentParser(__doc__)
    p.add_argument("label")
    p.add_argument("--mode", choices=["pilot", "full", "sham", "memory", "work", "baseline", "matrix"], default="pilot")
    p.add_argument("--cpu", type=int, default=2)
    p.add_argument("--contracts", action="store_true",help="Include owner-resource and writable-effect fixtures")
    p.add_argument('--evaluation', choices=['full','light','topological','shared','shared-dirty','shared-opt','shared-dirty-opt','global','global-dirty','local','local-dirty','ordered'], default='full')
    args = p.parse_args()
    assert args.label and all(c.isalnum() or c in "-_" for c in args.label)
    root = LOG / "runs" / args.label
    root.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, CARGO_TARGET_DIR="/tmp/opencode/reactive-p1p2-lab-target", REACTIVE_TYPED_PROTOTYPE="1")
    env['TYPED_EVALUATION']=args.evaluation
    package_root = ROOT
    if args.mode == "baseline":
        package_root = Path(json.loads((LOG / "p1p2-baseline.json").read_text())["directory"])
        if not package_root.exists():
            package_root.mkdir(parents=True, exist_ok=False)
            bundle = json.loads((LOG / "p1p2-frozen-source.json").read_text())["sources"]
            for name, text in bundle.items():
                path = Path(name)
                assert not path.is_absolute() and ".." not in path.parts
                destination = package_root / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(text)
        env["CARGO_TARGET_DIR"] = "/tmp/opencode/reactive-p1p2-frozen-target"
        env.pop("REACTIVE_TYPED_PROTOTYPE", None)
    lifecycle.STEP_TIMEOUT = 240
    source = {str(p.relative_to(package_root)): digest(p) for pattern in ["src/**/*.rs", "experiments/**/*.rs", "tests/p1p2*.rs", "tests/p1p2_support/**/*.rs", "benches/**/*.rs", "benches/run-p1p2.py", "experiments/**/Cargo.toml", "Cargo.toml", "Cargo.lock"] for p in package_root.glob(pattern)}
    bench = "p1p2_memory" if args.mode in ["memory", "work"] else "comparison"
    executable, binary = build(bench, root, args.label, env, package_root, args.mode == "work")
    metadata = {"label": args.label, "mode": args.mode, "binary": binary, "source_sha256": source,
                "cpu": args.cpu, "system": platform.uname()._asdict(),
                "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
                "cargo_home": os.environ.get("CARGO_HOME"), "runs": [],
                 "contracts":args.contracts,"evaluation":args.evaluation,"driver_source":Path(__file__).read_text(),
                "limitations": "Research core with reintegrated owners/cleanups, writable effects, rounds/budgets, batching and hooks; not full public API conformance"}
    destination = LOG / f"{args.label}.json"
    destination.write_text(json.dumps(metadata, indent=2) + "\n")
    if args.mode in ["memory", "work"]:
        env["REACTIVE_METRICS_LOG"] = str(root / "metrics.json")
        with (root / "stdout.log").open("w") as out:
            lifecycle.run_step(["taskset", "-c", str(args.cpu), str(executable)], cwd=ROOT, env=env, stdout=out, stderr=subprocess.STDOUT, check=True)
        metadata["metrics"] = json.loads((root / "metrics.json").read_text())
    else:
        # Smoke all adapters/fixtures with the exact executable to be timed.
        with (root / "smoke.log").open("w") as out:
            env["REACTIVE_BENCH_LOG"] = str(root / "smoke")
            lifecycle.run_step(["taskset", "-c", str(args.cpu), str(executable), "--test"], cwd=ROOT, env=env, stdout=out, stderr=subprocess.STDOUT, check=True)
        schedule = list("abbaabba") if args.mode in ["full", "sham"] else ["all"]
        for index, variant in enumerate(schedule):
            directory = root / f"{index + 1}-{variant}"
            directory.mkdir()
            engines = "reactive|typed_research|sycamore|leptos" if variant == "all" else ("reactive|sycamore|leptos" if variant == "a" else "typed_research|sycamore|leptos")
            if args.mode == "sham":
                engines = "reactive|sycamore|leptos"
            pattern = rf"((chain|diamond|fan_out|conditional)_write_to_observed|p1p2_cut)/({engines})/256$"
            if args.contracts:
                pattern=rf"({pattern})|((contract_owner_resources|contract_writable_effects)/({engines})/8$)"
            if args.mode in ["baseline", "matrix"]:
                pattern = ".*"
            env["REACTIVE_BENCH_LOG"] = str(directory / "criterion")
            assert digest(executable) == binary["sha256"]
            assert all(digest(package_root / p) == sha for p, sha in source.items()), "sources changed during series"
            command = ["taskset", "-c", str(args.cpu), str(executable), "--bench", "--noplot", "--sample-size", "30" if args.mode == "full" else "20",
                       "--warm-up-time", "0.3" if args.mode == "full" else "0.1", "--measurement-time", "0.7" if args.mode == "full" else "0.3", pattern]
            with (directory / "stdout.log").open("w") as out:
                lifecycle.run_step(command, cwd=ROOT, env=env, stdout=out, stderr=subprocess.STDOUT, check=True)
            estimates = {str(path.parent.parent.relative_to(directory / "criterion")): json.loads(path.read_text()) for path in (directory / "criterion").glob("**/new/estimates.json")}
            assert estimates
            metadata["runs"].append({"variant": variant, "command": command, "estimates": estimates})
            destination.write_text(json.dumps(metadata, indent=2) + "\n")
        if args.mode in ["full", "sham"]:
            rows = {}
            families=["chain_write_to_observed", "diamond_write_to_observed", "fan_out_write_to_observed", "conditional_write_to_observed", "p1p2_cut"]
            if args.contracts:families += ["contract_owner_resources","contract_writable_effects"]
            for family in families:
                values = {}
                candidate = "typed_research" if args.mode == "full" else "reactive"
                engines_for_family=["reactive",candidate] if family.startswith("contract_") else ["reactive",candidate,"sycamore","leptos"]
                def key_for(engine):
                    if family=="p1p2_cut":return f"p1p2_cut_{engine}_256"
                    if family.startswith("contract_"):return f"{family}_{engine}_8"
                    size=8 if family.startswith("contract_") else 256
                    return f"{family}/{engine}/{size}"
                for engine in dict.fromkeys(engines_for_family):
                    blocks = []
                    for run in metadata["runs"]:
                        key = key_for(engine)
                        if key in run["estimates"]:
                            data = run["estimates"][key]
                            blocks.append((data.get("slope") or data["mean"])["point_estimate"])
                    values[engine] = statistics.mean(blocks)
                ratios = []
                for start in [0,4]:
                    cycle=metadata["runs"][start:start+4]
                    def point(i, engine):
                        key=key_for(engine)
                        d=cycle[i]["estimates"][key]
                        return (d.get("slope") or d["mean"])["point_estimate"]
                    ratios.append(math.sqrt(point(1,candidate)*point(2,candidate)/point(0,"reactive")/point(3,"reactive")))
                rows[family] = {"means_ns": values, "cycle_percent": [100*(v-1) for v in ratios], "paired_percent":100*(math.sqrt(math.prod(ratios))-1)}
            metadata["summary"] = rows
    assert digest(executable) == binary["sha256"]
    destination.write_text(json.dumps(metadata, indent=2) + "\n")
    print(destination, flush=True)


if __name__ == "__main__":
    main()
