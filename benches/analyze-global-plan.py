#!/usr/bin/env python3
"""Verify executable global-plan evidence without borrowing prior checkpoints."""
import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'

def load(label):
    return json.loads((LOG / (label + '.json')).read_text())

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def counts(text):
    return list(map(int, re.findall(r'test result: ok\. (\d+) passed;', text)))

def main():
    result = load('global-plan-v1-validation')
    source = result['source_sha256']
    for path, digest in source.items():
        assert sha(ROOT / path) == digest, path
    directory = LOG / 'runs/global-plan-v1-validation'
    assert len(result['native']) == 12
    for name, expected in result['native'].items():
        assert counts((directory / name).read_text()) == expected
    assert 'Finished' in (directory / 'clippy.log').read_text()
    memory, work = {}, {}
    for mode in ['full', 'global', 'global-dirty']:
        m = load(f'global-plan-v1-{mode}-memory')
        w = load(f'global-plan-v1-{mode}-work')
        for run in [m, w]:
            assert run['evaluation'] == mode
            for path, digest in run['source_sha256'].items():
                assert source[path] == digest, path
        memory[mode] = m['metrics']
        work[mode] = w['metrics']
        for row in m['metrics']['rows']:
            if row['engine'] in ['reactive', 'typed_research']:
                assert (row['steady_allocations'], row['steady_reallocations'],
                        row['after_drop_net_bytes']) == (0, 0, 0)
        assert all(row['after_drop_net_bytes'] == 0 for row in m['metrics']['owner_resources'])
    for mode in ['global', 'global-dirty']:
        assert [r['work'] for r in work[mode]['rows']] == [r['work'] for r in work['full']['rows']]
    timing = {}
    binary = None
    for label, sham in [('global-plan-v1-balanced', False), ('global-plan-v1-sham', True)]:
        run = load(label)
        assert run['sham'] == sham and run['modes'] == {'f': 'full', 'l': 'global', 't': 'global-dirty'}
        assert run['schedule'] == list('flttlf') * 2 and len(run['runs']) == 12
        assert len(run['summary']) == 7
        for path, digest in run['source_sha256'].items():
            assert source[path] == digest, path
        if binary is None:
            binary = run['binary']
        assert run['binary'] == binary
        assert sha(Path(binary['path'])) == binary['sha256']
        for block, variant in zip(run['runs'], run['schedule']):
            assert block['variant'] == variant
            assert block['evaluation'] == ('full' if sham else run['modes'][variant])
        timing[label] = run['summary']
        for fixture, row in run['summary'].items():
            print(label, fixture, {mode: round(row[mode + '_paired_percent'], 2)
                                  for mode in ['global', 'global-dirty']})
    miri = {}
    for case, expected in [('global', [7, 0]), ('shared', [9, 0])]:
        run = load('global-plan-v1-miri-' + case)
        assert run['exit_code'] == 0 and run['sources_unchanged'] and not run['timed_out']
        assert run['miriflags'] == '-Zmiri-env-forward=TYPED_EVALUATION'
        assert counts(run['stdout']) == expected
        for path, digest in run['source_sha256'].items():
            assert source[path] == digest, path
        miri[case] = expected
    for mode in ['global', 'dirty']:
        path = directory / f'miri-{mode}-integrations.log'
        assert counts(path.read_text()) == [2, 13, 9, 10]
        miri[mode + '_normal_integrations'] = {'counts': [2, 13, 9, 10], 'sha256': sha(path)}
    result.update(memory=memory, work=work, timing=timing, binary=binary,
                  miri=miri, miri_integrations='Passed directed normal-build suites')
    (LOG / 'global-plan-v1-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print('Verified native, memory/work, balanced/sham and directed Miri evidence')

if __name__ == '__main__':
    main()
