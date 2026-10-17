#!/usr/bin/env python3
"""Verify instruction-local correction evidence, including same-binary controls."""
import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'

def load(name):
    return json.loads((LOG / (name + '.json')).read_text())

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def counts(text):
    return list(map(int, re.findall(r'test result: ok\. (\d+) passed;', text)))

def main():
    result = load('local-plan-v1-validation')
    source = result['source_sha256']
    for path, digest in source.items():
        assert sha(ROOT / path) == digest, path
    directory = LOG / 'runs/local-plan-v1-validation'
    assert len(result['native']) == 14
    for name, expected in result['native'].items():
        assert counts((directory / name).read_text()) == expected
    assert 'Finished' in (directory / 'clippy.log').read_text()
    for mode in ['full', 'local', 'local-dirty']:
        for kind in ['memory', 'work']:
            run = load(f'local-plan-v1-{mode}-{kind}')
            for path, digest in run['source_sha256'].items():
                assert source[path] == digest, path
        metrics = result['memory'][mode]
        for row in metrics['rows']:
            if row['engine'] in ['reactive', 'typed_research']:
                assert (row['steady_allocations'], row['steady_reallocations'],
                        row['after_drop_net_bytes']) == (0, 0, 0)
        assert all(row['after_drop_net_bytes'] == 0 for row in metrics['owner_resources'])
    for mode in ['local', 'local-dirty']:
        assert [r['work'] for r in result['work'][mode]['rows']] == [r['work'] for r in result['work']['full']['rows']]
    timing, binary = {}, None
    for control, suffix in [('full', 'balanced'), ('global', 'balanced'), ('full', 'sham')]:
        label = f'local-plan-v1-{control}-{suffix}'
        run = load(label)
        assert run['sham'] == (suffix == 'sham')
        assert run['modes'] == {'f': control, 'l': 'local', 't': 'local-dirty'}
        assert run['schedule'] == list('flttlf') * 2 and len(run['runs']) == 12
        assert len(run['summary']) == 7
        for path, digest in run['source_sha256'].items():
            assert source[path] == digest, path
        if binary is None:
            binary = run['binary']
        assert run['binary'] == binary and sha(Path(binary['path'])) == binary['sha256']
        for block, variant in zip(run['runs'], run['schedule']):
            assert block['variant'] == variant
            assert block['evaluation'] == (control if run['sham'] else run['modes'][variant])
        timing[label] = run['summary']
        for fixture, row in run['summary'].items():
            print(label, fixture, {m: round(row[m + '_paired_percent'], 2)
                                  for m in ['local', 'local-dirty']})
    miri = {}
    for case, expected in [('local', [5, 0]), ('global', [7, 0]), ('shared', [9, 0])]:
        run = load('local-plan-v1-miri-' + case)
        assert run['exit_code'] == 0 and not run['timed_out'] and run['sources_unchanged']
        assert run['evaluation'] == 'local'
        assert run['miriflags'] == '-Zmiri-env-forward=TYPED_EVALUATION'
        assert counts(run['stdout']) == expected
        for path, digest in run['source_sha256'].items():
            assert source[path] == digest, path
        miri[case] = expected
    for mode in ['local', 'dirty']:
        path = directory / f'miri-{mode}-integrations.log'
        assert counts(path.read_text()) == [2, 13, 9, 10]
        miri[mode + '_integrations'] = {'counts': [2, 13, 9, 10], 'sha256': sha(path)}
    result.update(timing=timing, binary=binary, miri=miri,
                  timing_status='Completed paired full/global controls and sham',
                  miri_status='Passed directed unit cases and normal-build integrations')
    (LOG / 'local-plan-v1-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print('Verified local-plan correction checkpoint')

if __name__ == '__main__':
    main()
