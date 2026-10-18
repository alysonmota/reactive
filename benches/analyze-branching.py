#!/usr/bin/env python3
"""Verify exact branching traces and rank real functions, not DWARF file rows."""
import collections
import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def functions(path):
    names = {}
    own = collections.Counter()
    edges = collections.Counter()
    caller = callee = None
    call = False
    for line in path.read_text().splitlines():
        match = re.fullmatch(r'(c?fn)=\((\d+)\)(?: (.*))?', line)
        if match:
            ident = int(match[2])
            if match[3] is not None:
                names[ident] = match[3]
            if match[1] == 'fn':
                caller = ident
            else:
                callee = ident
        elif line.startswith('calls='):
            call = True
        elif re.match(r'^[+\-*\d]', line):
            fields = line.split()
            assert len(fields) == 2, 'Expected positions: line and one event: Ir'
            count = int(fields[1])
            if call:
                edges[caller, callee] += count
                call = False
            else:
                own[caller] += count
    assert not call
    summary = int(re.search(r'^summary: (\d+)$', path.read_text(), re.M)[1])
    assert sum(own.values()) == summary
    result = collections.defaultdict(lambda: {'self': 0, 'inclusive': 0})
    for ident in set(own) | {caller for caller, _ in edges}:
        name = names[ident]
        result[name]['self'] += own[ident]
        result[name]['inclusive'] += own[ident] + sum(cost for (caller, _), cost in edges.items() if caller == ident)
    return dict(result)


def main():
    label = 'branching-current'
    data = json.loads((LOG / (label + '.json')).read_text())
    directory = LOG / 'runs' / label
    assert len(data['runs']) == 48
    for path, expected in data['source_sha256'].items():
        assert digest(ROOT / path) == expected, path
    assert data['rustflags'] == '-C panic=abort'
    assert digest(Path(data['sealed_binary']['path'])) == data['sealed_binary']['sha256'] == data['binary']['sha256']
    ranking = {}
    for row in data['runs']:
        assert row['panic'] == 'abort' and row['phase_probe']
        assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
        assert '--toggle-collect=reactive_walk_' + row['phase'] in row['command']
        path = directory / f"{row['mode']}-{row['shape']}-{row['size']}-{row['phase']}.callgrind"
        assert digest(path) == row['trace_sha256']
        total = int(re.search(r'^summary: (\d+)$', path.read_text(), re.M)[1])
        assert total == row['instructions']
        fit = data['fits'][row['mode']][row['shape']][row['phase']]
        assert abs(total / row['iterations'] - fit['instructions_per_node'] * row['size'] - fit['fixed_instructions']) < 1e-6
        if row['phase'] == 'flush' and row['size'] == 256:
            denominator = row['iterations'] * row['size']
            ranking[row['mode'] + '-' + row['shape']] = {
                name: {metric: cost / denominator for metric, cost in costs.items()}
                for name, costs in functions(path).items()
            }
    for annotation in data['annotations']:
        assert digest(Path(annotation['path'])) == annotation['sha256']
    miri = []
    for mode in ['full', 'topological']:
        for case, expected_count in [('fused', 4), ('planning', 4), ('audit', 32)]:
            run_label = f'fused-b-miri-full-{case}' if mode == 'full' else f'branching-miri-topological-{case}'
            run = json.loads((LOG / (run_label + '.json')).read_text())
            assert run['exit_code'] == 0 and run['sources_unchanged'] and not run['timed_out']
            assert run['evaluation'] == mode
            for path, expected in run['source_sha256'].items():
                assert digest(ROOT / path) == expected, path
            counts = list(map(int, re.findall(r'test result: ok\. (\d+) passed;', run['stdout'])))
            assert sum(counts) == expected_count
            miri.append({'label': run_label, 'passed': sum(counts), 'sha256': digest(LOG / (run_label + '.json'))})
    ports = json.loads((LOG / 'fused-b-miri-full-production.json').read_text())
    assert ports['exit_code'] == 124 and ports['timed_out'] and ports['sources_unchanged']
    # Execution and graph functions must not silently absorb the evaluator
    # delta. Compare exclusive counts; inclusive costs overlap, especially
    # Diamond where the aggregator executes nested branch sessions.
    deltas = {}
    for shape in ['chain', 'cut', 'diamond', 'fan_out']:
        full = ranking['full-' + shape]
        demand = ranking['topological-' + shape]
        deltas[shape] = sorted([
            {'function': name, 'full': full.get(name, {}).get('self', 0),
             'demand': demand.get(name, {}).get('self', 0),
             'delta': demand.get(name, {}).get('self', 0) - full.get(name, {}).get('self', 0)}
            for name in full.keys() | demand.keys()
            if abs(demand.get(name, {}).get('self', 0) - full.get(name, {}).get('self', 0)) > 1e-9
        ], key=lambda row: abs(row['delta']), reverse=True)
        for row in deltas[shape]:
            assert any(s in row['function'] for s in ['::Runtime>::update', '::evaluation::Session>', '::fused::Session>']), row
    result = {'fits': data['fits'], 'functions_per_scale_unit': ranking, 'exclusive_deltas': deltas, 'miri': miri,
              'production_ports_miri': 'Full timeout after 600s; not approved; Demand ports not run',
              'limits': ['Inclusive rankings overlap; do not sum them',
                         'N means branches in Diamond (N+1 memos, 1 effect), and memo/effect pairs in Fan-out',
                         'Native perf counters not collected; no IPC/cache causal claim']}
    (LOG / 'branching-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    for shape, rows in deltas.items():
        print(shape, rows)


if __name__ == '__main__':
    main()
