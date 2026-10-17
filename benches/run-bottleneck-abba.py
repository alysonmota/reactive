#!/usr/bin/env python3
"""Alternating isolated targets: A=default, B=experimental-inline-reads."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("label")
    parser.add_argument("--cpu", type=int, default=2)
    parser.add_argument("--cycles", type=int, default=2)
    parser.add_argument("--chain-only", action="store_true", help="Skip isolated-write timings")
    parser.add_argument("--suite", choices=["chain", "comparison", "runtime"], default="chain")
    parser.add_argument("--filter", help="Override the primary suite filter")
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--warmup", type=float, default=0.3)
    parser.add_argument("--measurement", type=float, default=1)
    parser.add_argument("--target-prefix", help="Reuse this experiment's isolated A/B targets across suites")
    parser.add_argument("--rebuild-package", action="store_true", help="Clean reactive artifacts before each variant build when switching source copies")
    parser.add_argument("--baseline-directory", type=Path, help="Use a preserved source baseline as A")
    parser.add_argument("--candidate-directory", type=Path,
                        help="Use a source candidate instead of inline-reads")
    args = parser.parse_args()
    if args.cycles < 1:
        parser.error("cycles must be positive")
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", args.label):
        parser.error("invalid label")
    if args.target_prefix and not re.fullmatch(r"[a-zA-Z0-9_-]+", args.target_prefix):
        parser.error("invalid target prefix")
    roots = {"a": args.baseline_directory.resolve() if args.baseline_directory else ROOT,
             "b": args.candidate_directory.resolve() if args.candidate_directory else ROOT}
    def environment(variant):
        return dict(os.environ, CARGO_TARGET_DIR=f"/tmp/opencode/reactive-{args.target_prefix or args.label}-{variant}-target")
    # Finish both builds before measuring: no compiler activity between blocks.
    builds = {}
    for variant in "ab":
        env = environment(variant)
        if args.rebuild_package:
            subprocess.run(["cargo", "clean", "--profile", "release", "--package", "reactive"],
                           cwd=roots[variant], env=env, check=True)
        command = ["cargo", "bench", "--locked", "--profile", "release",
                   "--no-default-features", "--bench", "runtime" if args.suite == "runtime" else "comparison", "--no-run", "--message-format=json"]
        if not args.chain_only:
            command += ["--bench", "runtime"]
        if variant == "b" and not args.candidate_directory:
            command += ["--features", "experimental-inline-reads"]
        result = subprocess.run(command, cwd=roots[variant], env=env, stdout=subprocess.PIPE, text=True, check=True)
        executables = {}
        for line in result.stdout.splitlines():
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact" and message.get("executable"):
                path = Path(message["executable"])
                executables[str(path)] = hashlib.sha256(path.read_bytes()).hexdigest()
        assert executables, "no benchmark binary reported"
        builds[variant] = {"root": str(roots[variant]), "rebuild_package": args.rebuild_package, "executables": executables}
    build_report = ROOT / "benches/.log/reports" / f"{args.label}-builds.json"
    build_report.parent.mkdir(parents=True, exist_ok=True)
    build_report.write_text(json.dumps(builds, indent=2) + "\n")
    for cycle in range(args.cycles):
        for position, variant in enumerate("abba"):
            env = environment(variant)
            pattern = args.filter or (r"(chain_scaling/.*/256$|chain_phases/.*/256$)" if args.suite == "chain" else None)
            suites = [(args.suite, pattern)]
            if not args.chain_only:
                suites.append(("runtime", "isolated_writes/"))
            for suite, pattern in suites:
                label = f"{args.label}-c{cycle + 1}-p{position + 1}-{variant}-{suite}"
                command = [sys.executable, str(roots[variant] / "benches/run.py"), label,
                           "--suite", suite, "--cpu", str(args.cpu),
                           "--samples", str(args.samples), "--warmup", str(args.warmup), "--measurement", str(args.measurement)]
                if pattern:
                    command += ["--filter", pattern]
                if variant == "b" and not args.candidate_directory:
                    command.append("--inline-reads")
                subprocess.run(command, cwd=roots[variant], env=env, check=True)


if __name__ == "__main__":
    main()
