#!/usr/bin/env python3
"""Force and verify abort for harness=false; profile names alone are not proof."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('baseline')
    parser.add_argument('label')
    parser.add_argument('--historical-unwind-audit', action='store_true',
                        help='Explicitly opt into the old unwind-versus-abort experiment')
    args = parser.parse_args()
    if not args.historical_unwind_audit:
        parser.error('Historical unwind comparison: current baseline collectors require effective abort')
    assert args.label and all(c.isalnum() or c in '-_' for c in args.label)
    log = ROOT / 'benches/.log'
    baseline = json.loads((log / (args.baseline + '.json')).read_text())
    sources = baseline['source_sha256']
    assert all(sha(ROOT / p) == h for p, h in sources.items())
    directory = log / 'runs' / args.label
    directory.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, CARGO_TARGET_DIR='/tmp/opencode/reactive-walk-abort-target', RUSTFLAGS='-C panic=abort')
    env.pop('CARGO_ENCODED_RUSTFLAGS', None)
    command = ['cargo', 'bench', '--locked', '--no-default-features', '--profile', 'bench-abort',
               '--bench', 'walk_diagnostic', '--no-run', '--message-format=json', '-vv']
    build = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, check=True, timeout=300)
    (directory / 'build.jsonl').write_text(build.stdout)
    (directory / 'build.log').write_text(build.stderr)
    events = [json.loads(s) for s in build.stdout.splitlines() if s.startswith('{')]
    binary = [e['executable'] for e in events if e.get('reason') == 'compiler-artifact'
              and e.get('target', {}).get('name') == 'walk_diagnostic' and e.get('executable')][-1]
    binaries = {'unwind': baseline['builds']['release'], 'abort': dict(path=binary, sha256=sha(binary))}
    pairs = [(p, mode) for p in binaries for mode in ['full', 'light', 'topological', 'sycamore']]
    schedule = (pairs + pairs[::-1])*2
    result = dict(source_sha256=sources, binaries=binaries, schedule=schedule, runs=[], cpu=baseline['cpu'],
                  rustflags=env['RUSTFLAGS'], build_artifacts=events)
    for i, (panic, mode) in enumerate(schedule):
        env['TYPED_EVALUATION'] = mode if mode != 'sycamore' else 'full'
        binary = binaries[panic]['path']
        assert sha(binary) == binaries[panic]['sha256']
        for shape in ['chain', 'cut']:
            command = ['taskset', '-c', str(baseline['cpu']), binary,
                       'sycamore' if mode == 'sycamore' else 'typed', shape, '256', '10000']
            row = json.loads(subprocess.check_output(command, cwd=ROOT, env=env, text=True, timeout=60))
            assert row['panic'] == panic
            assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
            result['runs'].append(dict(block=i, profile=panic, mode=mode, command=command, **row))
        print(f'{args.label}: {i+1}/{len(schedule)} {panic}/{mode}', flush=True)
    result['means_ns'] = {panic: {mode: {shape: statistics.mean(r['ns_per_write'] for r in result['runs']
        if r['profile'] == panic and r['mode'] == mode and r['shape'] == shape) for shape in ['chain', 'cut']}
        for mode in ['full', 'light', 'topological', 'sycamore']} for panic in binaries}
    assert all(sha(ROOT / p) == h for p, h in sources.items())
    (log / (args.label + '.json')).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result['means_ns'], indent=2))


if __name__ == '__main__':
    main()
