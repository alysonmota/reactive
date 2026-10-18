#!/usr/bin/env python3
"""Matched current policies/references, same release binary and scalar fixtures."""
import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import shutil

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'
spec = importlib.util.spec_from_file_location('runner', ROOT / 'benches/run-p1p2.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
FAMILIES = ['chain_write_to_observed', 'diamond_write_to_observed',
            'fan_out_write_to_observed', 'conditional_write_to_observed', 'p1p2_cut']
MODES = {'production': ('reactive', 'full'), 'full': ('typed_research', 'full'),
          'topological': ('typed_research', 'topological'),
         'sycamore': ('sycamore', 'full'), 'leptos': ('leptos', 'full')}


def key(family, engine):
    return f'p1p2_cut_{engine}_256' if family == 'p1p2_cut' else f'{family}/{engine}/256'


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--cpu', type=int, default=2)
    args = parser.parse_args()
    assert args.label and all(c.isalnum() or c in '-_' for c in args.label)
    directory = LOG / 'runs' / args.label
    directory.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, CARGO_TARGET_DIR='/tmp/opencode/reactive-p1p2-lab-target',
               REACTIVE_TYPED_PROTOTYPE='1', TYPED_EVALUATION='full')
    env.pop('RUSTFLAGS', None)
    env.pop('REACTIVE_CHAIN_INVESTIGATION', None)
    runner.lifecycle.STEP_TIMEOUT = 240
    binary, identity = runner.build('comparison', directory, args.label, env)
    sealed = directory / 'sealed-binary'
    shutil.copyfile(binary, sealed)
    sealed.chmod(0o755)
    binary = sealed.resolve()
    identity = {'path': str(binary), 'sha256': runner.digest(binary)}
    sources = {str(p.relative_to(ROOT)): runner.digest(p)
               for pattern in ['src/**/*.rs', 'experiments/**/*.rs', 'benches/**/*.rs',
                               'Cargo.toml', 'Cargo.lock', 'experiments/**/Cargo.toml',
                               'benches/compare-current-references.py', 'benches/run-p1p2.py']
               for p in ROOT.glob(pattern)}
    modes = list(MODES)
    cycle_width = len(modes) * 2
    schedule = (modes + list(reversed(modes))) * 2
    result = dict(binary=identity, source_sha256=sources, modes=MODES, schedule=schedule,
                   cpu=args.cpu, profile='bench-abort', panic='abort', rustflags=env.get('RUSTFLAGS'), diagnostics=False, size=256,
                  rustc=subprocess.check_output(['rustc', '-Vv'], text=True),
                  system=platform.uname()._asdict(), runs=[],
                  limits=['Five scalar fixtures, not complete API/contract equivalence',
                          'CPU pinned and runs serial, no physical isolation',
                          'Balanced placement, not a dedicated sham or statistical significance test',
                          'No historical timing combined with this series'])
    destination = LOG / (args.label + '.json')
    for mode in ['full', 'topological']:
        env['TYPED_EVALUATION'] = mode
        env['REACTIVE_BENCH_LOG'] = str(directory / ('smoke-' + mode))
        with (directory / ('smoke-' + mode + '.log')).open('w') as out:
            runner.lifecycle.run_step(['taskset', '-c', str(args.cpu), str(binary), '--test'],
                                      cwd=ROOT, env=env, stdout=out, stderr=subprocess.STDOUT, check=True)
    for i, label in enumerate(schedule, 1):
        engine, evaluation = MODES[label]
        env['TYPED_EVALUATION'] = evaluation
        block = directory / f'{i}-{label}'
        block.mkdir()
        env['REACTIVE_BENCH_LOG'] = str(block / 'criterion')
        pattern = rf'((chain|diamond|fan_out|conditional)_write_to_observed|p1p2_cut)/{engine}/256$'
        command = ['taskset', '-c', str(args.cpu), str(binary), '--bench', '--noplot',
                   '--sample-size', '30', '--warm-up-time', '0.3', '--measurement-time', '0.7', pattern]
        print(f'{args.label}: {i}/{len(schedule)} {label}', flush=True)
        with (block / 'stdout.log').open('w') as out:
            runner.lifecycle.run_step(command, cwd=ROOT, env=env, stdout=out,
                                      stderr=subprocess.STDOUT, check=True)
        estimates = {str(p.parent.parent.relative_to(block / 'criterion')): json.loads(p.read_text())
                     for p in (block / 'criterion').glob('**/new/estimates.json')}
        assert set(estimates) == {key(f, engine) for f in FAMILIES}
        result['runs'].append(dict(label=label, engine=engine, evaluation=evaluation,
                                   command=command, estimates=estimates))
        assert runner.digest(binary) == identity['sha256']
        assert all(runner.digest(ROOT / p) == h for p, h in sources.items())
        destination.write_text(json.dumps(result, indent=2) + '\n')
    summary = {}
    for family in FAMILIES:
        def point(run):
            e = run['estimates'][key(family, run['engine'])]
            return (e.get('slope') or e['mean'])['point_estimate']
        means = {m: statistics.mean(point(r) for r in result['runs'] if r['label'] == m) for m in modes}
        ratios = {}
        for mode in ['production', 'full', 'topological']:
            ratios[mode] = {}
            for reference in ['sycamore', 'leptos']:
                cycles = []
                for start in [0, cycle_width]:
                    cycle = result['runs'][start:start + cycle_width]
                    def gm(label):
                        return math.exp(statistics.mean(math.log(point(r)) for r in cycle if r['label'] == label))
                    cycles.append(gm(mode) / gm(reference))
                ratios[mode][reference] = dict(cycle_ratios=cycles, paired_ratio=math.sqrt(math.prod(cycles)))
        summary[family] = dict(means_ns=means, ratios=ratios)
    result['summary'] = summary
    destination.write_text(json.dumps(result, indent=2) + '\n')
    print(destination, flush=True)


if __name__ == '__main__':
    main()
