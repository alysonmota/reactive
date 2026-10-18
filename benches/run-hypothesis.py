#!/usr/bin/env python3
"""Fail-closed sequence: native+Miri, profiling instruction slopes, release time."""
import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--control', default='hypothesis-control')
    args = parser.parse_args()
    commands = [
        ['cargo', 'fmt', '--all', '--', '--check'],
        [sys.executable, 'benches/validate-kernel-ablation.py', args.label],
        [sys.executable, 'benches/profile-walk-phases.py', args.label, '--modes', 'full', 'topological',
         '--shapes', 'chain', 'cut', 'diamond', 'fan_out'],
        [sys.executable, 'benches/time-hypothesis.py', args.label, '--control', args.control],
    ]
    for command in commands:
        subprocess.run(command, cwd=ROOT, check=True)


if __name__ == '__main__':
    main()
