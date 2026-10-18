#!/usr/bin/env python3
"""Verify isolated kernel variants, safety gates, fits and retained composition."""
import collections
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import statistics

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'
LABELS = ['logical-prepare-outlined', 'kernel-publication', 'kernel-reconcile',
          'kernel-execute', 'kernel-execute-unit', 'kernel-combined', 'kernel-final', 'kernel-final-directed']
spec = importlib.util.spec_from_file_location('session', ROOT / 'benches/analyze-session-cost.py')
session = importlib.util.module_from_spec(spec)
spec.loader.exec_module(session)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(label):
    return json.loads((LOG / (label + '.json')).read_text())


def compact(text):
    return re.sub(r'\s+', '', text)


def section(text, start, end):
    return text[text.index(start):text.index(end)]


def main():
    profiles, bundles, validation, exclusive = {}, {}, {}, {}
    for label in LABELS:
        data = load(label)
        profiles[label] = data
        directory = LOG / 'runs' / label
        bundle = json.loads((directory / 'source-bundle.json').read_text())
        bundles[label] = bundle
        for path, expected in data['source_sha256'].items():
            assert hashlib.sha256(bundle[path].encode()).hexdigest() == expected, (label, path)
            if label == LABELS[-1] and path.endswith(('.rs', '.toml', '.lock')):
                assert digest(ROOT / path) == expected, path
        assert data['profile'] == 'profiling' and data['rustflags'] == '-C panic=abort'
        assert digest(Path(data['sealed_binary']['path'])) == data['sealed_binary']['sha256'] == data['binary']['sha256']
        assert len(data['runs']) == 48
        function_rows = collections.defaultdict(list)
        for row in data['runs']:
            assert row['panic'] == 'abort' and row['phase_probe']
            assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
            assert '--toggle-collect=reactive_walk_' + row['phase'] in row['command']
            trace = directory / f"{row['mode']}-{row['shape']}-{row['size']}-{row['phase']}.callgrind"
            assert digest(trace) == row['trace_sha256']
            names, own, _, _, _ = session.parse(trace)
            assert sum(own.values()) == row['instructions']
            fit = data['fits'][row['mode']][row['shape']][row['phase']]
            assert abs(row['instructions'] / row['iterations'] - row['size'] * fit['instructions_per_node'] - fit['fixed_instructions']) < 1e-6
            if row['phase'] == 'flush':
                counter = collections.Counter()
                for fn, value in own.items():
                    counter[re.sub(r"'\d+$", '', names[fn])] += value / row['iterations']
                function_rows[row['mode'], row['shape']].append((row['size'], counter))
        exclusive[label] = {}
        for (mode, shape), rows in function_rows.items():
            functions = set().union(*(counter for _, counter in rows))
            slopes = {fn: statistics.linear_regression([n for n, _ in rows], [c[fn] for _, c in rows]).slope for fn in functions}
            assert abs(sum(slopes.values()) - data['fits'][mode][shape]['flush']['instructions_per_node']) < 1e-6
            exclusive[label][mode + '-' + shape] = slopes
        if label != LABELS[0]:
            gate = load(label + '-validation')
            assert gate['complete']
            for path, expected in gate['source_sha256'].items():
                assert data['source_sha256'][path] == expected, (label, path)
            passed = collections.Counter()
            for run in gate['runs']:
                assert run['exit_code'] == 0 and run['passed'] > 0
                assert digest(ROOT / run['log']) == run['sha256']
                if run['case']:
                    passed[run['mode']] += run['passed']
            assert set(passed) == {'full', 'topological'}
            assert all(v == (9 if label == 'kernel-publication' else 10) for v in passed.values())
            validation[label] = dict(passed)

    base = bundles[LABELS[0]]
    lib = 'experiments/typed-runtime/src/lib.rs'
    fused = 'experiments/typed-runtime/src/fused.rs'
    audit = 'experiments/typed-runtime/src/audit_tests.rs'
    for label in LABELS[1:]:
        changed = {path for path in base if path.endswith('.rs') and base[path] != bundles[label][path]}
        assert changed == {lib, audit, *([fused] if label in ['kernel-execute', 'kernel-execute-unit'] else [])}, (label, changed)
    reconcile = ('    fn reconcile(', '    pub fn dispose<')
    execute = ('unsafe fn execute<T:', 'unsafe fn destroy<T>')
    pub_text = bundles['kernel-publication'][lib]
    rec_text = bundles['kernel-reconcile'][lib]
    assert pub_text.replace(section(pub_text, *execute), section(base[lib], *execute)) == base[lib]
    assert rec_text.replace(section(rec_text, *reconcile), section(base[lib], *reconcile)) == base[lib]
    expected = pub_text.replace(section(pub_text, *reconcile), section(rec_text, *reconcile))
    assert bundles['kernel-combined'][lib] == expected
    for label in ['kernel-final', 'kernel-final-directed']:
        assert compact(bundles[label][lib]) == compact(expected)
        assert bundles[label][fused] == base[fused]
        assert bundles[label]['experiments/typed-runtime/src/ownership.rs'] == base['experiments/typed-runtime/src/ownership.rs']
    for mode in ['full', 'topological']:
        for shape in ['chain', 'cut', 'diamond', 'fan_out']:
            assert profiles['kernel-final']['fits'][mode][shape] == profiles['kernel-combined']['fits'][mode][shape]
            assert profiles['kernel-final-directed']['fits'][mode][shape] == profiles['kernel-final']['fits'][mode][shape]

    deltas = {}
    for label in LABELS[1:]:
        deltas[label] = {}
        for fixture, slopes in exclusive[label].items():
            control = exclusive[LABELS[0]][fixture]
            deltas[label][fixture] = {fn: slopes.get(fn, 0) - control.get(fn, 0) for fn in slopes.keys() | control.keys()
                                     if abs(slopes.get(fn, 0) - control.get(fn, 0)) > 1e-6}
    checks = load('kernel-final-checks')
    for path, expected in checks['source_sha256'].items():
        assert digest(ROOT / path) == expected
    assert {r['name'] for r in checks['runs']} == {'full', 'topological', 'clippy', 'fmt', 'diff'}
    for run in checks['runs']:
        assert run['exit_code'] == 0 and digest(ROOT / run['log']) == run['sha256']
    timing = load('current-kernel-references')
    assert len(timing['runs']) == 20 and timing['profile'] == 'bench-abort'
    assert timing['panic'] == 'abort' and timing['rustflags'] == '-C panic=abort'
    assert digest(Path(timing['binary']['path'])) == timing['binary']['sha256']
    for path, expected in timing['source_sha256'].items():
        if path.endswith('.rs'):
            assert expected == profiles[LABELS[-1]]['source_sha256'][path] == digest(ROOT / path)
    result = {'retained': ['kernel-publication', 'kernel-reconcile'],
              'not_retained': ['kernel-execute', 'kernel-execute-unit'],
              'current': LABELS[-1], 'validation_miri_passed_per_mode': validation,
              'final_checks_manifest_sha256': digest(LOG / 'kernel-final-checks.json'),
              'current_native_reference_timing_manifest': 'current-kernel-references',
              'fits': {label: data['fits'] for label, data in profiles.items()},
              'exclusive_function_slope_deltas': deltas,
              'limits': ['Current reference timing is not a paired before/after ablation', 'Directed Miri, not a full Miri/unsafe approval',
                         'Function-exclusive attribution does not establish cache/spill/latency causality']}
    (LOG / 'kernel-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    for label in LABELS:
        print(label, {m: {s: profiles[label]['fits'][m][s]['flush']['instructions_per_node'] for s in ['chain', 'cut', 'diamond', 'fan_out']} for m in ['full', 'topological']})


if __name__ == '__main__':
    main()
