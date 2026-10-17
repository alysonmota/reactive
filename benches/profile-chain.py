#!/usr/bin/env python3
"""Bounded fixture profiling; raw traces ignored, summary/provenance versionable.

Sampling percentages describe CPU samples, not isolated feature costs or
baseline timings. Criterion --profile-time avoids its statistical analysis.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--build-json', type=Path, required=True)
    parser.add_argument('--engines', nargs='+', default=['reactive', 'typed_research', 'sycamore', 'leptos'],
                        choices=['reactive', 'typed_research', 'sycamore', 'leptos'])
    parser.add_argument('--seconds', type=int, default=10)
    parser.add_argument('--frequency', type=int, default=199)
    parser.add_argument('--size', type=int, default=256, choices=[8, 64, 256])
    parser.add_argument('--fixture', choices=['chain_write_to_observed', 'diamond_write_to_observed',
                                            'conditional_write_to_observed', 'p1p2_cut'],
                        default='chain_write_to_observed')
    parser.add_argument('--cpu', type=int, default=2)
    parser.add_argument('--call-graph', choices=['fp', 'dwarf,65528'], default='fp')
    args = parser.parse_args()
    assert args.label and all(c.isalnum() or c in '-_' for c in args.label)
    assert 1 <= args.seconds <= 60 and 1 <= args.frequency <= 999
    directory = LOG / 'runs' / args.label
    directory.mkdir(parents=True, exist_ok=False)
    artifacts = [json.loads(line) for line in args.build_json.read_text().splitlines()]
    binaries = [a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact'
                and a.get('target', {}).get('name') == 'comparison' and a.get('executable')]
    assert len(binaries) == 1
    binary = Path(binaries[0])
    sources = {str(p.relative_to(ROOT)): digest(p)
               for pattern in ['src/**/*.rs', 'experiments/**/*.rs', 'benches/**/*.rs',
                               'Cargo.toml', 'Cargo.lock', 'experiments/**/Cargo.toml',
                               '.cargo/config.toml', 'benches/profile-chain.py']
               for p in ROOT.glob(pattern)}
    result = {'label': args.label, 'binary': {'path': str(binary), 'sha256': digest(binary)},
              'source_sha256': sources, 'profile': 'profiling', 'rustflags': os.environ.get('RUSTFLAGS'),
               'system': platform.uname()._asdict(), 'cpu': args.cpu, 'size': args.size,
                'fixture': args.fixture,
               'evaluation': os.environ.get('TYPED_EVALUATION', 'full'),
              'seconds': args.seconds, 'frequency': args.frequency, 'event': 'cycles:u',
              'call_graph': args.call_graph, 'runs': [],
              'limitations': ['CPU sampling, not baseline timing or an isolated ablation',
                              'Deep recursive stacks may exceed captured user stack or unwind limits',
                              'Inclusive recursive frame percentages cannot be summed',
                              'Record includes fixture preflights, setup and teardown; profile-time dominates but does not exclude them']}
    env = dict(os.environ, REACTIVE_TYPED_PROTOTYPE='1', DEBUGINFOD_URLS='')
    env.pop('REACTIVE_CHAIN_INVESTIGATION', None)
    commands = []

    def run(command, output, timeout=180):
        commands.append(command)
        with output.open('w') as out, output.with_suffix(output.suffix + '.stderr').open('w') as err:
            subprocess.run(command, cwd=ROOT, env=env, stdout=out, stderr=err,
                           timeout=timeout, check=True)

    for engine in args.engines:
        print(f'{args.label}: {engine}/{args.size}, {args.seconds}s, {args.frequency}Hz', flush=True)
        trace = directory / f'{engine}.perf.data'
        fixture = f'^{args.fixture}/{engine}/{args.size}$'
        start = time.monotonic()
        run(['perf', 'record', '--no-inherit', '--mmap-pages', '128', '-e', 'cycles:u', '-F', str(args.frequency), '--call-graph', args.call_graph,
             '-o', str(trace), '--', 'taskset', '-c', str(args.cpu), str(binary),
             '--bench', '--noplot', '--profile-time', str(args.seconds), fixture], directory / f'{engine}.record.log')
        run(['perf', 'report', '-i', str(trace), '--stdio', '--no-children', '--percent-limit', '0.1',
             '--sort', 'symbol', '--call-graph', 'none'], directory / f'{engine}.self.txt')
        run(['perf', 'report', '-i', str(trace), '--stdio', '--no-children', '--percent-limit', '0.2',
             '--sort', 'srcline', '--call-graph', 'none'], directory / f'{engine}.lines.txt')
        run(['perf', 'script', '-i', str(trace), '--max-stack', '1024',
             '-F', 'comm,pid,time,event,ip,sym,dso'], directory / f'{engine}.stacks.txt')
        folded = directory / f'{engine}.folded'
        run(['inferno-collapse-perf', str(directory / f'{engine}.stacks.txt')], folded)
        run(['inferno-flamegraph', '--title', f'{args.fixture}/{args.size} {engine} — cycles:u',
             '--countname', 'samples', '--deterministic', str(folded)], directory / f'{engine}.svg')
        result['runs'].append({'engine': engine, 'elapsed_seconds': time.monotonic() - start,
                               'record_log': (directory / f'{engine}.record.log').read_text(),
                               'record_stderr': (directory / f'{engine}.record.log.stderr').read_text(),
                               'self_report': (directory / f'{engine}.self.txt').read_text(),
                               'line_report': (directory / f'{engine}.lines.txt').read_text(),
                               'svg': str((directory / f'{engine}.svg').relative_to(ROOT)),
                               'svg_sha256': digest(directory / f'{engine}.svg'),
                               'trace_sha256': digest(trace)})
        assert digest(binary) == result['binary']['sha256']
        assert all(digest(ROOT / p) == h for p, h in sources.items())
        result['commands'] = commands
        (LOG / f'{args.label}.json').write_text(json.dumps(result, indent=2) + '\n')
    print(LOG / f'{args.label}.json', flush=True)


if __name__ == '__main__':
    main()
