#!/usr/bin/env python3
"""Reproducible runs. All metrics/artifacts stay under benches/.log."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"


def capture(command):
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    return {"command": command, "status": result.returncode, "output": result.stdout}


def reference_data():
    paths = {
        "reactively": ["packages/core/src/core.ts"],
        "sycamore-reactive": ["Cargo.toml", "src/root.rs", "src/memos.rs", "src/node.rs"],
        "leptos": ["Cargo.toml", "reactive_graph/src/graph/sets.rs", "reactive_graph/src/owner.rs"],
    }
    result = {}
    for name, selected in paths.items():
        directory = ROOT / "references" / name
        top = capture(["git", "-C", str(directory), "rev-parse", "--show-toplevel"])
        # A copied package may resolve to THIS repository. Never mislabel that
        # ancestor's commit as an independent reference revision.
        independent = top["status"] == 0 and Path(top["output"].strip()).resolve() == directory.resolve()
        result[name] = {
            "revision": capture(["git", "-C", str(directory), "rev-parse", "HEAD"]) if independent else None,
            "source_sha256": {p: hashlib.sha256((directory / p).read_bytes()).hexdigest() for p in selected if (directory / p).exists()},
        }
    return result


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("label", help="Unique run label (no overwrite)")
    parser.add_argument("--suite", choices=["comparison", "runtime", "memory", "chain"], default="comparison")
    parser.add_argument("--filter", default=None)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--warmup", type=float, default=0.3)
    parser.add_argument("--measurement", type=float, default=1.0)
    parser.add_argument("--cpu", type=int, help="Pin the entire run with taskset")
    parser.add_argument("--inline-reads", action="store_true", help="Enable experimental-inline-reads (default remains Vec)")
    parser.add_argument("--profile-seconds", type=int, help="Separate perf sampling run (requires --filter, no Criterion timing)")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", args.label):
        parser.error("label must contain only letters, digits, underscore or dash")
    if args.profile_seconds is not None and (not args.filter or args.suite == "memory"):
        parser.error("profiling requires a timing suite and an explicit filter")
    directory = LOG / "runs" / args.label
    directory.mkdir(parents=True, exist_ok=False)
    target = "comparison" if args.suite == "chain" else args.suite
    profile = "profiling" if args.profile_seconds is not None else "release"
    command = ["cargo", "bench", "--locked", "--profile", profile, "--no-default-features", "--bench", target]
    features = (["diagnostics"] if args.suite == "memory" else []) + (["experimental-inline-reads"] if args.inline_reads else [])
    if features:
        command += ["--features", ",".join(features)]
    if args.profile_seconds is not None:
        command += ["--no-run", "--message-format=json"]
    elif args.suite == "memory":
        pass
    else:
        command += ["--", "--noplot", "--nresamples", "10000", "--sample-size", str(args.samples), "--warm-up-time", str(args.warmup), "--measurement-time", str(args.measurement)]
        if args.filter:
            command += [args.filter]
    if args.cpu is not None and args.profile_seconds is None:
        command = ["taskset", "-c", str(args.cpu)] + command
    files = sorted(set(ROOT.glob("src/**/*.rs")) | set(ROOT.glob("benches/**/*.rs")) | {ROOT / "Cargo.toml", ROOT / "Cargo.lock", Path(__file__).resolve()} | {p for p in [ROOT / ".cargo/config.toml", ROOT / "rust-toolchain.toml"] if p.exists()})
    environment = dict(os.environ)
    if 'CARGO_ENCODED_RUSTFLAGS' in environment:
        if not environment['CARGO_ENCODED_RUSTFLAGS'].endswith('-C\x1fpanic=abort'):
            environment['CARGO_ENCODED_RUSTFLAGS'] += '\x1f-C\x1fpanic=abort'
    else:
        if not environment.get('RUSTFLAGS', '').rstrip().endswith('-C panic=abort'):
            environment['RUSTFLAGS'] = (environment.get('RUSTFLAGS', '') + ' -C panic=abort').strip()
    environment['REACTIVE_REQUIRE_ABORT'] = '1'
    if args.suite == "chain":
        environment["REACTIVE_CHAIN_INVESTIGATION"] = "1"
    environment["REACTIVE_BENCH_LOG"] = str(directory / "criterion")
    environment["REACTIVE_METRICS_LOG"] = str(directory / "metrics.json")
    metadata = {
        "label": args.label, "started": datetime.now(timezone.utc).isoformat(),
        "command": command, "profile": profile, "panic": "abort", "features": features, "instrumented": args.suite == "memory",
        "environment": {k: v for k, v in environment.items() if k in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "REACTIVE_REQUIRE_ABORT", "RUSTUP_TOOLCHAIN", "CARGO_HOME", "CARGO_TARGET_DIR", "REACTIVE_BENCH_LOG", "REACTIVE_METRICS_LOG", "REACTIVE_CHAIN_INVESTIGATION"] or k.startswith("CARGO_PROFILE_")},
        "source_sha256": {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in files},
        "rustc": capture(["rustc", "-Vv"]), "cargo": capture(["cargo", "-V"]),
        "cpu": capture(["lscpu"]), "system": capture(["uname", "-a"]),
        "revision": capture(["git", "rev-parse", "HEAD"]), "status": capture(["git", "status", "--short"]),
        "references": reference_data(),
        "affinity": sorted(os.sched_getaffinity(0)),
        "benchmark_cpu": args.cpu,
        "governors": {str(p): p.read_text().strip() for p in Path("/sys/devices/system/cpu").glob("cpu[0-9]*/cpufreq/scaling_governor")},
    }
    (directory / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"Running {args.label}: {' '.join(command)}", flush=True)
    with (directory / "stdout.log").open("w") as output:
        result = subprocess.run(command, cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT)
    if args.profile_seconds is not None and result.returncode == 0:
        artifacts = []
        for line in (directory / "stdout.log").read_text().splitlines():
            try:
                message = json.loads(line)
                if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == target and message.get("executable"):
                    artifacts.append(message["executable"])
            except json.JSONDecodeError:
                pass
        if not artifacts:
            raise RuntimeError("Cargo did not report a benchmark executable")
        perf_command = ["perf", "record", "-F", "997", "--call-graph", "dwarf", "-o", str(directory / "perf.data"), artifacts[-1], "--bench", "--noplot", "--profile-time", str(args.profile_seconds), args.filter]
        if args.cpu is not None:
            perf_command = ["taskset", "-c", str(args.cpu)] + perf_command
        metadata["perf_command"] = perf_command
        with (directory / "perf-record.log").open("w") as output:
            result = subprocess.run(perf_command, cwd=ROOT, env=environment, stdout=output, stderr=subprocess.STDOUT)
        if result.returncode == 0:
            with (directory / "perf-report.txt").open("w") as output:
                result = subprocess.run(["perf", "report", "--stdio", "--no-children", "-i", str(directory / "perf.data")], cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
    metadata["status_code"] = result.returncode
    metadata["finished"] = datetime.now(timezone.utc).isoformat()
    (directory / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    summaries = {}
    for path in sorted((directory / "criterion").glob("**/new/estimates.json")):
        data = json.loads(path.read_text())
        key = str(path.parent.parent.relative_to(directory / "criterion"))
        summaries[key] = {name: data[name] for name in ["mean", "median", "slope"] if name in data}
    report = {"label": args.label, "suite": args.suite, "environment": metadata["environment"], "command": command, "status_code": result.returncode,
              "started": metadata["started"], "finished": metadata["finished"],
              "instrumented": metadata["instrumented"], "profiling": args.profile_seconds is not None, "source_sha256": metadata["source_sha256"], "estimates_ns": summaries,
               "features": features, "benchmark_cpu": args.cpu,
               "rustc": metadata["rustc"]["output"], "cpu": metadata["cpu"]["output"], "affinity": metadata["affinity"], "governors": metadata["governors"]}
    if (directory / "criterion/build.json").exists():
        report["build"] = json.loads((directory / "criterion/build.json").read_text())
    if (directory / "perf-report.txt").exists():
        report["perf_command"] = metadata["perf_command"]
        report["perf_report"] = (directory / "perf-report.txt").read_text()
    if (directory / "metrics.json").exists():
        report["metrics"] = json.loads((directory / "metrics.json").read_text())
    reports = LOG / "reports"
    reports.mkdir(parents=True, exist_ok=True)
    (reports / f"{args.label}.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Exit {result.returncode}; logs: {directory.relative_to(ROOT)}; summary: benches/.log/reports/{args.label}.json", flush=True)
    if result.returncode:
        print((directory / "stdout.log").read_text()[-4000:], file=sys.stderr)
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
