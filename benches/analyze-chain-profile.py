#!/usr/bin/env python3
"""Summarize self CPU shares and flamegraph quality without summing recursion."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import subprocess
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--label', default='chain-flamegraph-fp-v3')
    args = parser.parse_args()
    source = LOG / f'{args.label}.json'
    data = json.loads(source.read_text())
    assert len(data['runs']) == 4
    assert data['rustflags'] == '-C force-frame-pointers=yes'
    assert data['call_graph'] == 'fp'
    for path, digest in data['source_sha256'].items():
        assert sha(ROOT / path) == digest, path
    assert sha(Path(data['binary']['path'])) == data['binary']['sha256']
    rows = []
    for run in data['runs']:
        engine = run['engine']
        directory = LOG / 'runs' / args.label
        assert sha(directory / f'{engine}.perf.data') == run['trace_sha256']
        report = run['self_report']
        assert '# Total Lost Samples: 0' in report
        assert not re.search(r'lost \d+ chunks', run['record_stderr'])
        symbols = []
        for line in report.splitlines():
            match = re.match(r'\s*([\d.]+)%\s+\[.\]\s+(.*?)\s+-\s+-\s*$', line)
            if match:
                symbols.append({'self_percent': float(match[1]), 'symbol': match[2]})
        total = unknown = full_root = 0
        depths = Counter()
        leaves = Counter()
        for line in (directory / f'{engine}.folded').read_text().splitlines():
            stack, weight = line.rsplit(' ', 1)
            count = int(weight)
            frames = stack.split(';')
            total += count
            depths[len(frames)] += count
            unknown += count * any('unknown' in frame for frame in frames)
            full_root += count * ('_start' in frames)
            leaves[frames[-1]] += count
        assert total >= 3000, (engine, total)
        assert sha(ROOT / run['svg']) == run['svg_sha256']
        ET.parse(ROOT / run['svg'])
        rows.append({'engine': engine, 'self_symbols': symbols,
                     'folded_samples': total, 'samples_with_unknown_frames': unknown,
                     'samples_containing_start': full_root,
                     'start_coverage_percent': 100 * full_root / total,
                     'max_expanded_depth': max(depths),
                     'top_folded_leaves': leaves.most_common(15),
                     'svg': run['svg'], 'svg_sha256': run['svg_sha256']})
    result = {'input': source.name, 'input_sha256': sha(source), 'rows': rows,
              'fixture': data.get('fixture', 'chain_write_to_observed'),
              'perf': subprocess.check_output(['perf', '--version'], text=True).strip(),
              'rustc': subprocess.check_output(['rustc', '-Vv'], text=True),
              'kernel_max_stack': int(Path('/proc/sys/kernel/perf_event_max_stack').read_text()),
              'interpretation': ['Self percentages are period-weighted cycles:u; folded widths count samples.',
                                 'Inlining creates more displayed frames than the kernel physical-frame limit.',
                                 'Missing _start means incomplete root visibility, not necessarily a missing hot leaf.',
                                 'Recursive inclusive widths cannot be added or interpreted as removable cost.',
                                 'LTO may merge identical functions: a cut closure name does not mean the wrong fixture ran.']}
    (LOG / f'{args.label}-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    for row in rows:
        print(row['engine'], 'samples', row['folded_samples'], 'root coverage', round(row['start_coverage_percent'], 1), '%')
        for symbol in row['self_symbols'][:8]:
            print(f'  {symbol["self_percent"]:5.2f}% {symbol["symbol"]}')


if __name__ == '__main__':
    main()
