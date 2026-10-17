#!/usr/bin/env python3
"""Verify final local fast-path/incremental-order evidence (v6)."""
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
    abba = load('local-plan-v6-structural-abba')
    sources = dict(abba['source_sha256'])

    def verify(run):
        for path, digest in run['source_sha256'].items():
            assert sha(ROOT / path) == digest, path
            assert sources.get(path, digest) == digest, path
            sources[path] = digest

    verify(abba)
    assert abba['schedule'] == list('abba') * 2
    assert set(abba['runs']) == {'local', 'local-dirty'}
    for mode, runs in abba['runs'].items():
        assert len(runs) == 8 and [r['variant'] for r in runs] == abba['schedule']
        assert all(len(r['estimates']) == 7 for r in runs)
    for binary in [abba['baseline']['binary'], abba['candidate']]:
        assert sha(Path(binary['path'])) == binary['sha256']
    directory = LOG / 'runs/local-plan-v6-validation'
    native = {}
    for features, modes, expected in [
            ('all', ['full', 'light', 'topological', 'shared', 'shared-dirty',
                     'shared-opt', 'shared-dirty-opt', 'global', 'global-dirty',
                     'local', 'local-dirty'], [246, 0, 2, 13, 15, 9, 10, 195, 2, 8, 12, 0]),
            ('default', ['full', 'local', 'local-dirty'],
             [240, 0, 2, 13, 15, 9, 10, 169, 0, 8, 12, 0])]:
        for mode in modes:
            path = directory / f'{mode}-{features}.log'
            assert counts(path.read_text()) == expected
            native[path.name] = dict(counts=expected, sha256=sha(path))
    assert 'Finished' in (directory / 'clippy.log').read_text()
    miri = {}
    for case, expected in [('local', [12, 0]), ('global', [7, 0]), ('shared', [9, 0])]:
        run = load('local-plan-v6-miri-' + case)
        verify(run)
        assert run['exit_code'] == 0 and not run['timed_out'] and run['sources_unchanged']
        assert run['evaluation'] == 'local' and run['miriflags'] == '-Zmiri-env-forward=TYPED_EVALUATION'
        assert counts(run['stdout']) == expected
        miri[case] = expected
    for mode in ['local', 'local-dirty']:
        path = directory / f'miri-{mode}-integrations.log'
        assert counts(path.read_text()) == [2, 13, 9, 10]
        miri[mode + '-integrations'] = dict(counts=[2, 13, 9, 10], sha256=sha(path))
    timing = {}
    for name in ['full-balanced', 'full-sham']:
        run = load('local-plan-v6-' + name)
        verify(run)
        assert run['binary'] == abba['candidate']
        assert len(run['runs']) == 12 and run['schedule'] == list('flttlf') * 2
        assert run['modes'] == {'f': 'full', 'l': 'local', 't': 'local-dirty'}
        assert run['sham'] == (name == 'full-sham')
        for block, variant in zip(run['runs'], run['schedule']):
            assert block['variant'] == variant
            assert block['evaluation'] == ('full' if run['sham'] else run['modes'][variant])
        timing[name] = run['summary']
    memory, work = {}, {}
    for mode in ['full', 'local', 'local-dirty']:
        for kind, target in [('memory', memory), ('work', work)]:
            run = load(f'local-plan-v6-{mode}-{kind}')
            verify(run)
            target[mode] = run['metrics']
        for row in memory[mode]['rows']:
            if row['engine'] in ['reactive', 'typed_research']:
                assert (row['steady_allocations'], row['steady_reallocations'], row['after_drop_net_bytes']) == (0, 0, 0)
        assert all(r['after_drop_net_bytes'] == 0 for r in memory[mode]['owner_resources'])
    for mode in ['local', 'local-dirty']:
        assert [r['work'] for r in work[mode]['rows']] == [r['work'] for r in work['full']['rows']]
    profiles = {}
    profile_binary = None
    for fixture in ['diamond', 'conditional']:
        for mode in ['full', 'local']:
            run = load(f'local-plan-v6-profile-{fixture}-{mode}')
            verify(run)
            assert (run['profile'], run['rustflags'], run['cpu'], run['seconds'], run['frequency'], run['call_graph']) == (
                'profiling', '-C force-frame-pointers=yes', 2, 15, 499, 'fp')
            assert run['evaluation'] == mode
            assert 'Total Lost Samples: 0' in run['runs'][0]['self_report']
            if profile_binary is None:
                profile_binary = run['binary']
            assert profile_binary == run['binary']
            assert sha(Path(profile_binary['path'])) == profile_binary['sha256']
            path = LOG / 'runs' / run['label']
            assert sha(path / 'typed_research.perf.data') == run['runs'][0]['trace_sha256']
            assert sha(ROOT / run['runs'][0]['svg']) == run['runs'][0]['svg_sha256']
            if mode == 'local':
                assert 'compile_local' not in (path / 'typed_research.folded').read_text()
            profiles[fixture + '-' + mode] = run
    result = dict(source_sha256=sources, candidate=abba['candidate'], native=native,
                  miri=miri, timing=timing, before_after=abba['summary'],
                  memory=memory, work=work, profiles=profiles,
                  limits=['No promotion: Diamond fast-path gain not demonstrated',
                          'Rank cache may visit a large ancestor segment on first multi-observer publication',
                          'Directed Miri, not full model/all ports or independent unsafe review',
                          'No new reference-engine timing; CPU pinning is not physical isolation'])
    (LOG / 'local-plan-v6-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print('Verified local fast-path and incremental ordering v6')


if __name__ == '__main__':
    main()
