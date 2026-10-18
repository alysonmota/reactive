#!/usr/bin/env python3
"""Preserve release assembly of boundary/phase paths under benches/.log."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "benches/.log"
MARKERS = ["resource_boundary", "set_phase", "notify_if_safe", "update_memo", "NotificationBoundary"]


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("label")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", args.label):
        parser.error("invalid label")
    directory = LOG / "runs" / args.label
    directory.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ)
    if 'CARGO_ENCODED_RUSTFLAGS' in env:
        env['CARGO_ENCODED_RUSTFLAGS'] += '\x1f-C\x1fpanic=abort'
    else:
        env['RUSTFLAGS'] = (env.get('RUSTFLAGS', '') + ' -C panic=abort').strip()
    command = ["cargo", "bench", "--locked", "--profile", "profiling", "--no-default-features", "--bench", "comparison", "--no-run", "--message-format=json"]
    build = subprocess.run(command, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (directory / "build.log").write_text(build.stdout)
    build.check_returncode()
    artifacts = []
    for line in build.stdout.splitlines():
        try:
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == "comparison" and message.get("executable"):
                artifacts.append(message["executable"])
        except json.JSONDecodeError:
            pass
    assert artifacts, "missing benchmark executable"
    executable = artifacts[-1]
    assembly_command = ["objdump", "-Cd", "--no-show-raw-insn", executable]
    assembly = subprocess.run(assembly_command, text=True, stdout=subprocess.PIPE, check=True).stdout
    (directory / "assembly.txt").write_text(assembly)
    symbols = subprocess.run(["nm", "-S", "-C", "--defined-only", executable], text=True, stdout=subprocess.PIPE, check=True).stdout
    selected = [line for line in symbols.splitlines() if any(marker in line for marker in MARKERS)]
    sections = []
    for section in assembly.split("\n\n"):
        header = section.splitlines()[0] if section else ""
        if re.match(r"^[0-9a-f]+ <", header) and any(marker in header for marker in MARKERS):
            sections.append(section)
    (directory / "boundary-assembly.txt").write_text("\n\n".join(sections) + "\n")
    files = sorted(ROOT.glob("src/**/*.rs")) + [ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / ".cargo/config.toml"]
    report = {
        "label": args.label, "command": command, "assembly_command": assembly_command,
        "profile": "release", "features": [], "strip": "none", "debug": "line-tables-only",
        "source_sha256": {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in files},
        "rustc": subprocess.run(["rustc", "-Vv"], text=True, stdout=subprocess.PIPE, check=True).stdout,
        "symbols": selected, "sections": [{"symbol": section.splitlines()[0], "assembly": section} for section in sections],
        "note": "LTO/inlining may hide standalone symbols; symbol size and assembly are not exclusive runtime cost",
    }
    reports = LOG / "reports"
    reports.mkdir(exist_ok=True)
    (reports / f"{args.label}.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Assembly saved: benches/.log/runs/{args.label}; report: benches/.log/reports/{args.label}.json")


if __name__ == "__main__":
    main()
