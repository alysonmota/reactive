#!/usr/bin/env python3
"""Paired release fixed-work timings, independent of profiling/Callgrind."""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'
spec = importlib.util.spec_from_file_location('runner', ROOT / 'benches/run-p1p2.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--control')
    parser.add_argument('--cpu', type=int, default=2)
    parser.add_argument('--iterations', type=int, default=5000)
    args = parser.parse_args()
    assert args.label and all(c.isalnum() or c in '-_' for c in args.label)
    directory = LOG / 'runs' / (args.label + '-time')
    directory.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, CARGO_TARGET_DIR='/tmp/opencode/reactive-hypothesis-time',
               REACTIVE_TYPED_PROTOTYPE='1')
    for key in ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'REACTIVE_CHAIN_INVESTIGATION']:
        env.pop(key, None)
    sources = {str(p.relative_to(ROOT)): digest(p) for pattern in ['src/**/*.rs', 'experiments/**/*.rs',
               'benches/**/*.rs', 'Cargo.toml', 'Cargo.lock', 'experiments/**/Cargo.toml'] for p in ROOT.glob(pattern)}
    runner.lifecycle.STEP_TIMEOUT = 360
    binary, _ = runner.build('walk_diagnostic', directory, args.label, env)
    sealed = directory / 'sealed-binary'
    shutil.copyfile(binary, sealed)
    sealed.chmod(0o755)
    binaries = {'variant': {'path': str(sealed.resolve()), 'sha256': digest(sealed)}}
    result = {'profile': 'bench-abort', 'rustflags': env['RUSTFLAGS'], 'cpu': args.cpu,
              'source_sha256': sources, 'binary': binaries['variant'], 'runs': [],
              'rustc': subprocess.check_output(['rustc', '-Vv'], text=True),
              'method': 'fixed-work N=256, 5000 writes, ABBAABBA per mode/fixture; not Criterion',
              'limits': ['No physical isolation or significance test', 'Do not mix profiling with release timing']}
    if args.control:
        control = json.loads((LOG / (args.control + '-time.json')).read_text())
        assert control['profile'] == result['profile'] and control['rustflags'] == result['rustflags']
        assert control['rustc'] == result['rustc']
        binaries['control'] = control['binary']
        result['control'] = args.control
        gate = json.loads((LOG / (args.label + '-validation.json')).read_text())
        assert gate['complete']
        for path, expected in gate['source_sha256'].items():
            assert sources[path] == expected
    else:
        binaries['control'] = binaries['variant']
    for identity in binaries.values():
        assert digest(Path(identity['path'])) == identity['sha256']
    for mode in ['full', 'topological']:
        env['TYPED_EVALUATION'] = mode
        for shape in ['chain', 'cut', 'diamond', 'fan_out', 'conditional']:
            schedule = ['control', 'variant', 'variant', 'control'] * 2 if args.control else ['variant']
            for i, arm in enumerate(schedule):
                identity = binaries[arm]
                command = ['taskset', '-c', str(args.cpu), identity['path'], 'typed', shape, '256', str(args.iterations)]
                run = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, check=True, timeout=60)
                row = json.loads(run.stdout)
                assert row['panic'] == 'abort' and not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
                result['runs'].append({'mode': mode, 'shape': shape, 'arm': arm, 'command': command, 'probe': row})
                assert digest(Path(identity['path'])) == identity['sha256']
    result['summary'] = {}
    if args.control:
        for mode in ['full', 'topological']:
            for shape in ['chain', 'cut', 'diamond', 'fan_out', 'conditional']:
                rows = [r for r in result['runs'] if r['mode'] == mode and r['shape'] == shape]
                means = {arm: statistics.mean(r['probe']['ns_per_write'] for r in rows if r['arm'] == arm) for arm in binaries}
                cycles = []
                for start in [0, 4]:
                    block = rows[start:start + 4]
                    gm = {arm: math.exp(statistics.mean(math.log(r['probe']['ns_per_write']) for r in block if r['arm'] == arm)) for arm in binaries}
                    cycles.append(gm['variant'] / gm['control'])
                result['summary'][mode + '-' + shape] = {'means_ns': means, 'cycle_ratios': cycles,
                                                         'paired_ratio': math.sqrt(math.prod(cycles))}
    assert all(digest(ROOT / path) == expected for path, expected in sources.items())
    (LOG / (args.label + '-time.json')).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result['summary'], indent=2))


if __name__ == '__main__':
    main()
