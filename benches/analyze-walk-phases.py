#!/usr/bin/env python3
"""Verify historical/final exact-region Callgrind and the final abort baseline."""
import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(name):
    return json.loads((LOG / (name + '.json')).read_text())


def main():
    phases = {}
    for label in ['walk-phases-before', 'walk-phases-final']:
        run = load(label)
        directory = LOG / 'runs' / label
        bundle = json.loads((directory / 'source-bundle.json').read_text())
        assert run['profile'] == 'profiling' and run['rustflags'] == '-C panic=abort'
        assert len(run['runs']) == 36
        assert all(hashlib.sha256(bundle[p].encode()).hexdigest() == h for p, h in run['source_sha256'].items())
        if label.endswith('final'):
            assert all(sha(ROOT / p) == h for p, h in run['source_sha256'].items())
        binary = run.get('sealed_binary', run['binary'])
        assert sha(Path(binary['path'])) == binary['sha256']
        for row in run['runs']:
            assert row['iterations'] == 200 and row['panic'] == 'abort'
            assert row['work'] is None and not row['root_diagnostics'] and not row['root_verification']
            name = f'{row["mode"]}-{row["shape"]}-{row["size"]}-{row["phase"]}'
            trace = directory / (name + '.callgrind')
            assert sha(trace) == row['trace_sha256']
            assert '--collect-atstart=no' in row['command']
            assert '--toggle-collect=reactive_walk_' + row['phase'] in row['command']
            text = trace.read_text()
            assert re.search(r'^events: Ir$', text, re.M)
            assert int(re.search(r'^summary: (\d+)$', text, re.M)[1]) == row['instructions']
            fit = run['fits'][row['mode']][row['shape']][row['phase']]
            assert abs(row['instructions_per_write'] - (fit['fixed_instructions'] + row['size']*fit['instructions_per_node'])) < 1e-6
        phases[label] = run
    timing = load('no-plan-abort-final')
    assert timing['panic'] == 'abort' and timing['rustflags'] == '-C panic=abort'
    assert timing['modes'] == {'f': 'full', 'l': 'light', 't': 'topological'}
    assert timing['schedule'] == list('flttlf')*2 and len(timing['runs']) == 12
    assert all(len(r['estimates']) == 7 for r in timing['runs'])
    assert sha(Path(timing['binary']['path'])) == timing['binary']['sha256']
    assert all(sha(ROOT / p) == h for p, h in timing['source_sha256'].items())
    native = {}
    directory = LOG / 'runs/no-plan-validation'
    for mode in ['full', 'light', 'topological']:
        for features, expected in [('all', [246, 0, 2, 13, 15, 9, 10, 167, 2, 8, 12, 0]),
                                   ('default', [240, 0, 2, 13, 15, 9, 10, 152, 0, 8, 12, 0])]:
            path = directory / f'{mode}-{features}-final.log'
            assert list(map(int, re.findall(r'test result: ok\. (\d+) passed', path.read_text()))) == expected
            native[path.name] = sha(path)
    assert 'Finished' in (directory / 'clippy-final-2.log').read_text()
    miri = {}
    for case, count in [('planning', 4), ('audit', 32)]:
        run = load('no-plan-miri-' + case)
        assert run['sources_unchanged'] and run['exit_code'] == 0 and not run['timed_out']
        assert run['evaluation'] == 'topological' and 'env-forward=TYPED_EVALUATION' in run['miriflags']
        assert all(sha(ROOT / p) == h for p, h in run['source_sha256'].items())
        assert list(map(int, re.findall(r'test result: ok\. (\d+) passed', run['stdout']))) == [count, 0]
        miri[case] = count
    (LOG / 'walk-phases-analysis.json').write_text(json.dumps(dict(phases=phases, timing=timing,
        native=native, miri=miri, limits=['Profiling instructions are not baseline wall-time evidence',
        'Final baseline timings compare remaining modes, not paired before/after removal speedups',
        'Directed Miri is not all ports/model or independent unsafe review']), indent=2) + '\n')
    print('Verified final phase attribution, abort baseline and validation')


if __name__ == '__main__':
    main()
