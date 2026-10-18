#!/usr/bin/env python3
"""Abort-only, exact Callgrind regions for set and flush, not whole-process deltas."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import shutil

ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--iterations', type=int, default=200)
    parser.add_argument('--modes', nargs='+', choices=['full', 'topological', 'sycamore'], default=['full', 'topological', 'sycamore'])
    parser.add_argument('--shapes', nargs='+', choices=['chain', 'cut', 'diamond', 'fan_out', 'conditional'], default=['chain', 'cut'])
    parser.add_argument('--sizes', nargs='+', type=int, default=[64, 256, 384])
    args = parser.parse_args()
    assert args.iterations > 0 and args.label and all(c.isalnum() or c in '-_' for c in args.label)
    assert len(set(args.sizes)) >= 3 and min(args.sizes) > 0
    directory = ROOT / 'benches/.log/runs' / args.label
    directory.mkdir(parents=True, exist_ok=False)
    paths = [p for pattern in ['src/**/*.rs', 'experiments/**/*.rs', 'benches/**/*.rs',
                               'Cargo.toml', 'Cargo.lock', 'experiments/**/Cargo.toml'] for p in ROOT.glob(pattern)]
    paths.append(Path(__file__).resolve())
    sources = {str(p.relative_to(ROOT)): digest(p) for p in paths}
    (directory / 'source-bundle.json').write_text(json.dumps({str(p.relative_to(ROOT)): p.read_text() for p in paths}))
    env = dict(os.environ, CARGO_TARGET_DIR='/tmp/opencode/reactive-walk-phase-target', RUSTFLAGS='-C panic=abort')
    env.pop('CARGO_ENCODED_RUSTFLAGS', None)
    build = subprocess.run(['cargo', 'bench', '--locked', '--profile', 'profiling', '--no-default-features',
                            '--bench', 'walk_diagnostic', '--no-run', '--message-format=json', '-vv'],
                           cwd=ROOT, env=env, capture_output=True, text=True, check=True, timeout=360)
    (directory / 'build.jsonl').write_text(build.stdout)
    (directory / 'build.log').write_text(build.stderr)
    artifacts = [json.loads(s) for s in build.stdout.splitlines() if s.startswith('{')]
    binary = Path([e['executable'] for e in artifacts if e.get('reason') == 'compiler-artifact'
                   and e.get('target', {}).get('name') == 'walk_diagnostic' and e.get('executable')][-1])
    symbols = subprocess.check_output(['nm', '-C', str(binary)], text=True)
    assert ' reactive_walk_set\n' in symbols and ' reactive_walk_flush\n' in symbols
    for a in artifacts:
        if a.get('target', {}).get('name') in ['reactive', 'typed_runtime_prototype']:
            assert a.get('features', []) == []
    result = dict(profile='profiling', rustflags=env['RUSTFLAGS'], source_sha256=sources,
                   binary=dict(path=str(binary), sha256=digest(binary)), artifacts=artifacts, runs=[])
    sealed = directory / 'sealed-binary'
    shutil.copyfile(binary, sealed)
    sealed.chmod(0o755)
    result['sealed_binary'] = dict(path=str(sealed.resolve()), sha256=digest(sealed))
    binary = sealed.resolve()
    output = ROOT / 'benches/.log' / (args.label + '.json')
    for mode in args.modes:
        env['TYPED_EVALUATION'] = mode if mode != 'sycamore' else 'full'
        for shape in args.shapes:
            for size in args.sizes:
                for phase in ['set', 'flush']:
                    name = f'{mode}-{shape}-{size}-{phase}'
                    trace = directory / (name + '.callgrind')
                    symbol = ('reactive_sycamore_walk_' if mode == 'sycamore' else 'reactive_walk_') + phase
                    command = ['valgrind', '--tool=callgrind', '--collect-atstart=no',
                               '--toggle-collect=' + symbol,
                               '--callgrind-out-file=' + str(trace), str(binary), 'sycamore' if mode == 'sycamore' else 'typed', shape,
                               str(size), str(args.iterations), 'phases']
                    run = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, check=True, timeout=120)
                    (directory / (name + '.log')).write_text(run.stderr)
                    row = json.loads(run.stdout)
                    assert row['panic'] == 'abort' and row['phase_probe']
                    assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
                    text = trace.read_text()
                    assert re.search(r'^events: Ir$', text, re.M)
                    instructions = int(re.search(r'^summary: (\d+)$', text, re.M)[1])
                    assert instructions > 0 or (mode == 'sycamore' and phase == 'flush')
                    result['runs'].append(dict(mode=mode, phase=phase, command=command, instructions=instructions,
                        instructions_per_write=instructions/args.iterations,
                        instructions_per_node=instructions/(args.iterations*size), trace_sha256=digest(trace), **row))
                    output.write_text(json.dumps(result, indent=2) + '\n')
                    print(name, round(instructions/(args.iterations*size), 2), 'instructions/node', flush=True)
    result['fits'] = {}
    for mode in args.modes:
        result['fits'][mode] = {}
        for shape in args.shapes:
            result['fits'][mode][shape] = {}
            for phase in ['set', 'flush']:
                rows = [r for r in result['runs'] if r['mode'] == mode and r['shape'] == shape and r['phase'] == phase]
                slope, intercept = statistics.linear_regression([r['size'] for r in rows], [r['instructions_per_write'] for r in rows])
                residual = max(abs(r['instructions_per_write'] - slope*r['size'] - intercept) for r in rows)
                result['fits'][mode][shape][phase] = dict(instructions_per_node=slope, fixed_instructions=intercept,
                    max_residual_instructions_per_write=residual)
    assert all(digest(ROOT / p) == h for p, h in sources.items())
    output.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
