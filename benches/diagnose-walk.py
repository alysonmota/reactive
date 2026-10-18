#!/usr/bin/env python3
"""Serial, fixed-work walk audit before changing evaluator implementations."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--cpu', type=int, default=2)
    parser.add_argument('--iterations', type=int, default=3000)
    parser.add_argument('--counters', action='store_true')
    parser.add_argument('--historical-unwind-audit', action='store_true',
                        help='Only for replay in the archived pre-removal source tree')
    args = parser.parse_args()
    if not args.historical_unwind_audit:
        parser.error('Historical unwind audit: replay the frozen source bundle explicitly; use profile-walk-phases.py or compare-evaluation.py for the current abort baseline')
    assert args.label and all(c.isalnum() or c in '-_' for c in args.label)
    directory = ROOT / 'benches/.log/runs' / args.label
    directory.mkdir(parents=True, exist_ok=False)
    sources = {str(p.relative_to(ROOT)): digest(p) for pattern in [
        'src/**/*.rs', 'experiments/**/*.rs', 'benches/**/*.rs', '**/Cargo.toml',
        'Cargo.lock', 'benches/diagnose-walk.py'] for p in ROOT.glob(pattern)
        if 'references' not in p.parts}
    env = dict(os.environ, CARGO_TARGET_DIR='/tmp/opencode/reactive-walk-audit-target')
    for key in ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'REACTIVE_CHAIN_INVESTIGATION']:
        env.pop(key, None)
    result = dict(source_sha256=sources, cpu=args.cpu, iterations=args.iterations,
                  rustc=subprocess.check_output(['rustc', '-Vv'], text=True), builds={}, runs=[], counters=[])
    destination = ROOT / 'benches/.log' / (args.label + '.json')

    def save():
        destination.write_text(json.dumps(result, indent=2) + '\n')

    for profile in ['release', 'bench-abort']:
        command = ['cargo', 'bench', '--locked', '--no-default-features', '--profile', profile,
                   '--bench', 'walk_diagnostic', '--no-run', '--message-format=json', '-vv']
        build = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, check=True, timeout=300)
        (directory / (profile + '-build.jsonl')).write_text(build.stdout)
        (directory / (profile + '-build.log')).write_text(build.stderr)
        events = [json.loads(line) for line in build.stdout.splitlines() if line.startswith('{')]
        artifact = [e for e in events if e.get('reason') == 'compiler-artifact'
                    and e.get('target', {}).get('name') == 'walk_diagnostic' and e.get('executable')][-1]
        binary = Path(artifact['executable'])
        result['builds'][profile] = dict(path=str(binary), sha256=digest(binary), artifacts=events)
        save()
    # Two symmetric cycles, all modes/references in the same binary per profile.
    modes = ['full', 'light', 'topological', 'local', 'local-dirty', 'sycamore']
    fixtures = [(shape, size) for shape in ['chain', 'cut'] for size in [64, 256, 384]]
    fixtures += [(shape, size) for shape in ['diamond', 'fan_out', 'conditional'] for size in [64, 256, 1024]]
    schedule = [(p, m) for p in result['builds'] for m in modes]
    result['schedule'] = schedule + schedule[::-1] + schedule + schedule[::-1]
    for block, (profile, mode) in enumerate(result['schedule']):
        print(f'{args.label}: {block + 1}/{len(result["schedule"])} {profile}/{mode}', flush=True)
        binary = Path(result['builds'][profile]['path'])
        assert digest(binary) == result['builds'][profile]['sha256']
        env['TYPED_EVALUATION'] = mode if mode != 'sycamore' else 'full'
        for shape, size in fixtures:
            command = ['taskset', '-c', str(args.cpu), str(binary), 'sycamore' if mode == 'sycamore' else 'typed', shape, str(size), str(args.iterations)]
            run = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, check=True, timeout=60)
            row = json.loads(run.stdout)
            assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
            result['runs'].append(dict(block=block, profile=profile, mode=mode, command=command, **row))
        save()
    if args.counters:
        binary = result['builds']['release']['path']
        for mode in modes:
            env['TYPED_EVALUATION'] = mode if mode != 'sycamore' else 'full'
            for shape in ['chain', 'cut']:
                for iterations in [2000, 6000]:
                    output = directory / f'perf-{mode}-{shape}-{iterations}.csv'
                    command = ['taskset', '-c', str(args.cpu), 'perf', 'stat', '-x', ';', '-o', str(output),
                               '-e', 'instructions:u,cycles:u,L1-dcache-loads:u,L1-dcache-load-misses:u', '--',
                               binary, 'sycamore' if mode == 'sycamore' else 'typed', shape, '256', str(iterations)]
                    run = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, timeout=90)
                    result['counters'].append(dict(mode=mode, shape=shape, iterations=iterations, command=command,
                                                   exit_code=run.returncode, stdout=run.stdout, stderr=run.stderr,
                                                   perf=output.read_text() if output.exists() else ''))
                    save()
    summary = {}
    for profile in result['builds']:
        summary[profile] = {}
        for mode in modes:
            rows = [r for r in result['runs'] if r['profile'] == profile and r['mode'] == mode]
            means = {f'{shape}/{size}': statistics.mean(r['ns_per_write'] for r in rows if r['shape'] == shape and r['size'] == size)
                     for shape, size in fixtures}
            fits = {}
            for shape in ['chain', 'cut', 'diamond', 'fan_out', 'conditional']:
                points = [(n, means[f'{shape}/{n}']) for s, n in fixtures if s == shape]
                slope, intercept = statistics.linear_regression(*zip(*points))
                residuals = [y - (intercept + slope*x) for x, y in points]
                fits[shape] = dict(ns_per_node=slope, intercept_ns=intercept, residuals_ns=residuals)
            summary[profile][mode] = dict(means_ns=means, fits=fits)
    result['summary'] = summary
    assert all(digest(ROOT / p) == h for p, h in sources.items())
    save()
    print(destination)


if __name__ == '__main__':
    main()
