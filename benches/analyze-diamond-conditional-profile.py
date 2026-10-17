#!/usr/bin/env python3
"""Verify matched ABBA CPU profiles; shares are not baseline timings."""
import hashlib
import json
from pathlib import Path
import re
import statistics
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    binary = None
    sources = None
    results = {}
    for fixture in ['diamond', 'conditional']:
        runs = []
        for block, mode in enumerate(['full', 'local', 'local', 'full'], 1):
            label = f'dc-profile-v1-{fixture}-{block}-{mode}'
            manifest = LOG / (label + '.json')
            data = json.loads(manifest.read_text())
            assert data['evaluation'] == mode
            assert data['fixture'] == fixture + '_write_to_observed'
            assert (data['profile'], data['cpu'], data['seconds'], data['frequency'],
                    data['size'], data['event'], data['call_graph'], data['rustflags']) == (
                        'profiling', 2, 15, 499, 256, 'cycles:u', 'fp',
                        '-C force-frame-pointers=yes')
            if binary is None:
                binary, sources = data['binary'], data['source_sha256']
            assert binary == data['binary'] and sources == data['source_sha256']
            assert len(data['runs']) == 1
            run = data['runs'][0]
            assert run['engine'] == 'typed_research'
            directory = LOG / 'runs' / label
            assert sha(directory / 'typed_research.perf.data') == run['trace_sha256']
            assert sha(ROOT / run['svg']) == run['svg_sha256']
            assert 'Total Lost Samples: 0' in run['self_report']
            assert fixture + '_write_to_observed/typed_research/256' in run['record_log']
            shares = {}
            for line in run['self_report'].splitlines():
                match = re.match(r'\s*(\d+\.\d+)%\s+\[.\]\s+(.*?)\s+-\s+-\s*$', line)
                if match:
                    shares[match[2]] = float(match[1])
            assert len(shares) >= 10
            folded = directory / 'typed_research.folded'
            samples = 0
            compile_samples = 0
            stack_examples = []
            for line in folded.read_text().splitlines():
                stack, n = line.rsplit(' ', 1)
                n = int(n)
                samples += n
                # Count a sample ONCE, even if an inline/recursive frame repeats.
                if 'compile_local' in stack.split(';'):
                    compile_samples += n
                frames = stack.split(';')
                if mode == 'local' and len(stack_examples) < 3 and (
                        frames[-1] == 'begin' and 'read_inner<' in stack
                        and 'update<true, true, true>' in frames):
                    stack_examples.append(dict(samples=n, tail=frames[-7:]))
            assert samples > 5000
            annotations = {str(p.relative_to(ROOT)): sha(p)
                           for p in directory.glob('*.annotate.txt')}
            runs.append(dict(mode=mode, manifest=str(manifest.relative_to(ROOT)),
                             manifest_sha256=sha(manifest), self_percent=shares,
                             folded_samples=samples,
                             compile_stack_samples=compile_samples,
                             nested_read_session_examples=stack_examples,
                             folded_sha256=sha(folded), annotations=annotations))
        summary = {}
        for mode in ['full', 'local']:
            selected = [r for r in runs if r['mode'] == mode]
            symbols = set().union(*(r['self_percent'] for r in selected))
            summary[mode] = {symbol: dict(
                mean=statistics.mean(r['self_percent'].get(symbol, 0) for r in selected),
                observations=[r['self_percent'].get(symbol, 0) for r in selected])
                for symbol in sorted(symbols)}
        results[fixture] = dict(runs=runs, summary=summary)
        for mode, rows in summary.items():
            print(fixture, mode)
            for symbol, values in sorted(rows.items(), key=lambda item: -item[1]['mean'])[:12]:
                print(f"  {values['mean']:.2f}% {symbol}")
    assert sha(Path(binary['path'])) == binary['sha256']
    for path, digest in sources.items():
        assert sha(ROOT / path) == digest, path
    # Runtime source is the same as the previous v4 checkpoint. The profiling
    # helper changed to select these fixtures and explicitly record the strategy.
    old = json.loads((LOG / 'local-plan-v4-analysis.json').read_text())
    for path, digest in sources.items():
        if path.endswith('.rs'):
            assert old['source_sha256'][path] == digest, path
    build = LOG / 'runs/diamond-conditional-profile-v1/build.jsonl'
    output = dict(binary=binary, source_sha256=sources, results=results,
                  build_json_sha256=sha(build),
                  analyzer_sha256=sha(Path(__file__)),
                  rustc=subprocess.check_output(['rustc', '-Vv'], text=True),
                  perf=subprocess.check_output(['perf', '--version'], text=True),
                  limits=['CPU sample shares, not time/operation or causal ablation',
                          'Self symbol reports include inlined callees; do not sum inclusive recursion',
                          'Record includes preflights, construction, destruction',
                          'Pinned CPU, serial runs; no physical machine isolation',
                          'No runtime change and no new native/Miri approval claimed'])
    (LOG / 'diamond-conditional-profile-v1-analysis.json').write_text(
        json.dumps(output, indent=2) + '\n')
    print('Verified Diamond/Conditional matched profiles')


if __name__ == '__main__':
    main()
