#!/usr/bin/env python3
"""Exact flush instruction dumps from a sealed effective-abort profiling binary."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--profile', default='logical-prepare-outlined')
    parser.add_argument('--modes', nargs='+', choices=['full', 'topological'], default=['full', 'topological'])
    parser.add_argument('--shapes', nargs='+', choices=['chain', 'cut', 'diamond', 'fan_out'], default=['chain', 'cut', 'diamond', 'fan_out'])
    parser.add_argument('--sizes', nargs='+', type=int, default=[64, 256, 384])
    parser.add_argument('--iterations', nargs='+', type=int, default=[200, 400])
    args = parser.parse_args()
    assert args.label and all(c.isalnum() or c in '-_' for c in args.label)
    assert min(args.sizes) > 0 and min(args.iterations) > 0
    profile = json.loads((LOG / (args.profile + '.json')).read_text())
    assert profile['profile'] == 'profiling' and profile['rustflags'] == '-C panic=abort'
    identity = profile['sealed_binary']
    assert digest(Path(identity['path'])) == identity['sha256'] == profile['binary']['sha256']
    for path, expected in profile['source_sha256'].items():
        if path.endswith('.rs') or path.endswith('.toml') or path.endswith('.lock'):
            assert digest(ROOT / path) == expected, path
    directory = LOG / 'runs' / args.label
    directory.mkdir(parents=True, exist_ok=False)
    binary = directory / 'sealed-binary'
    shutil.copyfile(identity['path'], binary)
    binary.chmod(0o755)
    shutil.copyfile(LOG / 'runs' / args.profile / 'source-bundle.json', directory / 'source-bundle.json')
    result = {'label': args.label, 'profile_manifest': args.profile,
              'binary': {'path': str(binary.resolve()), 'sha256': digest(binary)},
              'source_sha256': profile['source_sha256'], 'collector_sha256': digest(Path(__file__)),
              'panic': 'abort', 'runs': []}
    destination = LOG / (args.label + '.json')
    for mode in args.modes:
        for shape in args.shapes:
            for size in args.sizes:
                for iterations in args.iterations:
                    name = f'{mode}-{shape}-{size}-{iterations}'
                    trace = directory / (name + '.callgrind')
                    cmd = ['valgrind', '--tool=callgrind', '--dump-instr=yes', '--collect-atstart=no',
                           '--toggle-collect=reactive_walk_flush', '--callgrind-out-file=' + str(trace),
                           str(binary.resolve()), 'typed', shape, str(size), str(iterations), 'phases']
                    run = subprocess.run(cmd, cwd=ROOT, env=dict(os.environ, TYPED_EVALUATION=mode),
                                         capture_output=True, text=True, check=True, timeout=180)
                    (directory / (name + '.log')).write_text(run.stderr)
                    row = json.loads(run.stdout)
                    assert row['panic'] == 'abort' and row['phase_probe']
                    assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
                    text = trace.read_text()
                    assert re.search(r'^positions: instr line$', text, re.M)
                    assert re.search(r'^events: Ir$', text, re.M)
                    total = int(re.search(r'^summary: (\d+)$', text, re.M)[1])
                    result['runs'].append({'mode': mode, 'shape': shape, 'size': size, 'iterations': iterations,
                                           'command': cmd, 'trace': str(trace.relative_to(ROOT)),
                                           'trace_sha256': digest(trace), 'instructions': total, 'probe': row})
                    destination.write_text(json.dumps(result, indent=2) + '\n')
                    print(name, total / iterations, 'instructions/write', flush=True)
    assert digest(binary) == identity['sha256']
    for path, expected in profile['source_sha256'].items():
        if path.endswith('.rs'):
            assert digest(ROOT / path) == expected, path


if __name__ == '__main__':
    main()
