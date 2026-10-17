#!/usr/bin/env python3
"""Verify the ordered gate interpreter checkpoint without historical approvals."""
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


def counts(text):
    return list(map(int, re.findall(r'test result: ok\. (\d+) passed;', text)))


def main():
    real = load('ordered-v2-balanced')
    sources = dict(real['source_sha256'])

    def verify(run):
        for path, digest in run['source_sha256'].items():
            assert sha(ROOT / path) == digest, path
            assert sources.get(path, digest) == digest, path
            sources[path] = digest

    verify(real)
    binary = real['binary']
    assert sha(Path(binary['path'])) == binary['sha256']
    timing = {}
    for suffix in ['balanced', 'sham']:
        run = load('ordered-v2-' + suffix)
        verify(run)
        assert run['binary'] == binary
        assert run['modes'] == {'f': 'full', 'l': 'ordered', 't': 'local'}
        assert run['sham'] == (suffix == 'sham')
        assert len(run['runs']) == 12 and run['schedule'] == list('flttlf') * 2
        for block, variant in zip(run['runs'], run['schedule']):
            assert block['variant'] == variant and len(block['estimates']) == 7
            assert block['evaluation'] == ('full' if run['sham'] else run['modes'][variant])
        timing[suffix] = run['summary']
    directory = LOG / 'runs/ordered-v2-validation'
    native = {}
    for mode in ['full', 'light', 'topological', 'shared', 'shared-dirty',
                 'shared-opt', 'shared-dirty-opt', 'global', 'global-dirty', 'local', 'local-dirty', 'ordered']:
        name = mode + '-all.log'
        assert counts((directory / name).read_text()) == [246, 0, 2, 13, 15, 9, 10, 201, 2, 8, 12, 0]
        native[name] = sha(directory / name)
    for mode in ['full', 'local', 'ordered']:
        name = mode + '-default.log'
        assert counts((directory / name).read_text()) == [240, 0, 2, 13, 15, 9, 10, 174, 0, 8, 12, 0]
        native[name] = sha(directory / name)
    assert 'Finished' in (directory / 'clippy.log').read_text()
    miri = {}
    for case, expected in [('ordered', [6, 0]), ('planning', [4, 0])]:
        run = load('ordered-v2-miri-' + case)
        verify(run)
        assert run['sources_unchanged'] and run['exit_code'] == 0 and not run['timed_out']
        assert run['evaluation'] == 'ordered' and run['miriflags'] == '-Zmiri-env-forward=TYPED_EVALUATION'
        assert counts(run['stdout']) == expected
        miri[case] = expected
    integration = directory / 'miri-integrations.log'
    assert counts(integration.read_text()) == [2, 13, 9, 10]
    miri['integrations'] = dict(counts=[2, 13, 9, 10], sha256=sha(integration))
    memory, work = {}, {}
    for mode in ['full', 'ordered', 'local']:
        for kind, target in [('memory', memory), ('work', work)]:
            run = load(f'ordered-v2-{mode}-{kind}')
            verify(run)
            target[mode] = run['metrics']
        for row in memory[mode]['rows']:
            if row['engine'] in ['reactive', 'typed_research']:
                assert (row['steady_allocations'], row['steady_reallocations'], row['after_drop_net_bytes']) == (0, 0, 0)
        assert all(r['after_drop_net_bytes'] == 0 for r in memory[mode]['owner_resources'])
    assert [r['work'] for r in work['ordered']['rows']] == [r['work'] for r in work['full']['rows']]
    profiles = {}
    profile_binary = None
    for mode in ['full', 'ordered']:
        run = load('ordered-v2-profile-' + mode)
        verify(run)
        assert (run['profile'], run['rustflags'], run['cpu'], run['seconds'], run['frequency'], run['call_graph']) == (
            'profiling', '-C force-frame-pointers=yes', 2, 15, 499, 'fp')
        assert run['evaluation'] == mode and len(run['runs']) == 1
        if profile_binary is None:
            profile_binary = run['binary']
        assert run['binary'] == profile_binary and sha(Path(profile_binary['path'])) == profile_binary['sha256']
        assert 'Total Lost Samples: 0' in run['runs'][0]['self_report']
        path = LOG / 'runs' / run['label']
        assert sha(path / 'typed_research.perf.data') == run['runs'][0]['trace_sha256']
        assert sha(ROOT / run['runs'][0]['svg']) == run['runs'][0]['svg_sha256']
        profiles[mode] = run
    output = dict(source_sha256=sources, binary=binary, native=native, miri=miri,
                  timing=timing, memory=memory, work=work, profiles=profiles,
                  limits=['No promotion: slower than FullFrames on six fixtures',
                          'Gate authorization still requires logical depth/return state',
                          'Directed Miri, not independent unsafe review/all ports/full model',
                          'Sham local-label cut drift 7.35%; small cut claims limited',
                          'No new paired reference-engine timing for OrderedDemand'])
    (LOG / 'ordered-v2-analysis.json').write_text(json.dumps(output, indent=2) + '\n')
    print('Verified ordered gate interpreter checkpoint')


if __name__ == '__main__':
    main()
