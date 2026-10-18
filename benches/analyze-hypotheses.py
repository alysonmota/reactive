#!/usr/bin/env python3
"""Verify isolated hypothesis gates, binaries, source snapshots and paired times."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import statistics

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'
spec = importlib.util.spec_from_file_location('session', ROOT / 'benches/analyze-session-cost.py')
session = importlib.util.module_from_spec(spec)
spec.loader.exec_module(session)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(label):
    return json.loads((LOG / (label + '.json')).read_text())


def verify(label, control):
    profile = load(label)
    timing = load(label + '-time')
    gate = load(label + '-validation')
    assert gate['complete'] and profile['profile'] == 'profiling' and profile['rustflags'] == '-C panic=abort'
    assert timing['profile'] == 'bench-abort' and timing['rustflags'] == '-C panic=abort'
    native_control = load(timing['control'] + '-time')
    assert digest(Path(native_control['binary']['path'])) == native_control['binary']['sha256']
    assert timing['rustc'] == native_control['rustc'] == control['rustc']
    assert digest(Path(timing['binary']['path'])) == timing['binary']['sha256']
    assert digest(Path(profile['sealed_binary']['path'])) == profile['sealed_binary']['sha256'] == profile['binary']['sha256']
    bundle = json.loads((LOG / 'runs' / label / 'source-bundle.json').read_text())
    for path, expected in profile['source_sha256'].items():
        assert hashlib.sha256(bundle[path].encode()).hexdigest() == expected, (label, path)
    for path, expected in gate['source_sha256'].items():
        assert profile['source_sha256'][path] == timing['source_sha256'][path] == expected
    miri = {}
    for mode in ['full', 'topological']:
        cases = [r for r in gate['runs'] if r['mode'] == mode and r['case']]
        assert len(cases) >= 7
        assert any('provisional_reads' in r['case'] for r in cases)
        assert any('failed_cleanup' in r['case'] for r in cases)
        assert any('disposal_and_slot_reuse' in r['case'] for r in cases)
        miri[mode] = sum(r['passed'] for r in cases)
    for run in gate['runs']:
        assert run['exit_code'] == 0 and run['passed'] > 0 and digest(ROOT / run['log']) == run['sha256']
    assert len(profile['runs']) == 48
    for row in profile['runs']:
        assert row['panic'] == 'abort' and row['phase_probe']
        assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
        path = LOG / 'runs' / label / f"{row['mode']}-{row['shape']}-{row['size']}-{row['phase']}.callgrind"
        assert digest(path) == row['trace_sha256']
        _, own, _, _, _ = session.parse(path)
        assert sum(own.values()) == row['instructions']
        fit = profile['fits'][row['mode']][row['shape']][row['phase']]
        assert abs(row['instructions_per_write'] - row['size'] * fit['instructions_per_node'] - fit['fixed_instructions']) < 1e-6
    assert len(timing['runs']) == 80
    for mode in ['full', 'topological']:
        for shape in ['chain', 'cut', 'diamond', 'fan_out', 'conditional']:
            rows = [r for r in timing['runs'] if r['mode'] == mode and r['shape'] == shape]
            assert [r['arm'] for r in rows] == ['control', 'variant', 'variant', 'control'] * 2
            for row in rows:
                probe = row['probe']
                assert probe['panic'] == 'abort' and probe['size'] == 256
                assert not probe['root_diagnostics'] and not probe['root_verification'] and probe['work'] is None
                identity = native_control['binary'] if row['arm'] == 'control' else timing['binary']
                assert row['command'][3] == identity['path']
            for arm in ['control', 'variant']:
                mean = statistics.mean(r['probe']['ns_per_write'] for r in rows if r['arm'] == arm)
                assert abs(mean - timing['summary'][mode + '-' + shape]['means_ns'][arm]) < 1e-6
    changed = [p for p, h in control['source_sha256'].items() if p.endswith('.rs') and profile['source_sha256'][p] != h]
    return {'fits': profile['fits'], 'native_timing': timing['summary'], 'miri_passed': miri,
            'changed_rust_sources_vs_control': changed, 'binary': profile['sealed_binary'],
            'timing_binary': timing['binary'], 'native_control_label': timing['control']}


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('labels', nargs='*', default=[
        'hypothesis-' + suffix for suffix in [
            'alive-preflight', 'execute-header', 'prepared-descriptor', 'read-hoist-live',
            'action-error', 'source-fields', 'split-borrows', 'descent-certificate',
            'depth-deferred', 'caps-context', 'validation-context', 'graph-frames',
            'tight-walk', 'direct-dirty-ready', 'adapter-borrowed-ready', 'lazy-capture',
            'combined', 'combined-dirty', 'final']])
    args = parser.parse_args()
    control = load('hypothesis-control-time')
    assert digest(Path(control['binary']['path'])) == control['binary']['sha256']
    result = {'control': 'kernel-final-directed', 'native_control': control['binary'], 'variants': {},
              'limits': ['Timings are paired release fixed-work, not Criterion/reference timings',
                         'Four samples/arm; no significance or physical isolation claim',
                         'No codegen/cache/spill causality inferred from source size',
                         'Individual hypotheses use original control; lazy capture uses adapter control',
                         'Combined-dirty measures marginal effect against combined, not original']}
    for label in args.labels:
        assert re.fullmatch(r'[\w-]+', label)
        result['variants'][label] = verify(label, control)
        row = result['variants'][label]
        print(label, 'Demand flush', [row['fits']['topological'][s]['flush']['instructions_per_node'] for s in ['chain', 'cut', 'diamond', 'fan_out']],
              'Demand time ratios', [round(row['native_timing']['topological-' + s]['paired_ratio'], 4) for s in ['chain', 'cut', 'diamond', 'fan_out', 'conditional']])
    if 'hypothesis-final' in args.labels:
        final = load('hypothesis-final')
        for path, expected in final['source_sha256'].items():
            if path.endswith(('.rs', '.toml', '.lock')):
                assert digest(ROOT / path) == expected, path
        for path, expected in control['source_sha256'].items():
            if (path.startswith('src/') and path.endswith('.rs')) or path in [
                'experiments/typed-runtime/src/ownership.rs',
                'experiments/typed-runtime/src/evaluation.rs']:
                assert final['source_sha256'][path] == expected, path
        checks = load('hypothesis-final-checks')
        for path, expected in checks['source_sha256'].items():
            assert digest(ROOT / path) == expected
        for run in checks['runs']:
            assert run['exit_code'] == 0 and digest(ROOT / run['log']) == run['sha256']
        result['final_checks_manifest_sha256'] = digest(LOG / 'hypothesis-final-checks.json')
        references = load('current-hypothesis-references')
        assert references['profile'] == 'bench-abort' and references['rustflags'] == '-C panic=abort'
        assert references['panic'] == 'abort' and len(references['runs']) == 20
        assert digest(Path(references['binary']['path'])) == references['binary']['sha256']
        for path, expected in references['source_sha256'].items():
            if path.endswith('.rs'):
                assert expected == final['source_sha256'][path] == digest(ROOT / path)
        result['current_reference_times'] = references['summary']
    rejected = load('hypothesis-preflight-certificate-validation')
    assert not rejected['complete'] and rejected['runs'][-1]['exit_code'] != 0
    path = ROOT / rejected['runs'][-1]['log']
    assert digest(path) == rejected['runs'][-1]['sha256']
    assert 'left: Clean' in path.read_text() and 'right: Check' in path.read_text()
    directory = LOG / 'runs/hypothesis-preflight-certificate-validation'
    bundle = json.loads((directory / 'source-bundle.json').read_text())
    for path, text in bundle.items():
        assert hashlib.sha256(text.encode()).hexdigest() == rejected['source_sha256'][path]
    topological = directory / 'topological-counterexample.log'
    assert 'left: Clean' in topological.read_text() and 'right: Check' in topological.read_text()
    result['counterexample_sha256'] = {str(path.relative_to(ROOT)): digest(path) for path in [
        directory / 'source-bundle.json', topological]}
    assert not (LOG / 'hypothesis-preflight-certificate.json').exists()
    assert not (LOG / 'hypothesis-read-hoist.json').exists()
    result['rejected_before_measurement'] = ['hypothesis-preflight-certificate', 'hypothesis-read-hoist']
    (LOG / 'hypothesis-analysis.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
