#!/usr/bin/env python3
"""Verify the profiled local-plan repair checkpoint without reusing v1 approval."""
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


def verify_sources(sources, run):
    for path, digest in run['source_sha256'].items():
        assert sha(ROOT / path) == digest, path
        if path in sources:
            assert sources[path] == digest, path
        else:
            sources[path] = digest


def main():
    abba = load('local-plan-v4-abba')
    sources = abba['source_sha256']
    for path, digest in sources.items():
        assert sha(ROOT / path) == digest, path
    assert sha(Path(abba['baseline']['binary']['path'])) == abba['baseline']['binary']['sha256']
    assert sha(Path(abba['candidate']['path'])) == abba['candidate']['sha256']
    assert abba['schedule'] == list('abba') * 2
    for mode, runs in abba['runs'].items():
        assert mode in ['local', 'local-dirty'] and len(runs) == 8
        assert [r['variant'] for r in runs] == abba['schedule']
        assert all(len(r['estimates']) == 7 for r in runs)
    validation = LOG / 'runs/local-plan-v4-validation'
    native = {}
    for mode in ['full', 'light', 'topological', 'shared', 'shared-dirty',
                 'shared-opt', 'shared-dirty-opt', 'global', 'global-dirty', 'local', 'local-dirty']:
        name = mode + '-all.log'
        native[name] = counts((validation / name).read_text())
        assert native[name] == [246, 0, 2, 13, 15, 9, 10, 189, 2, 8, 12, 0]
    for mode in ['full', 'local', 'local-dirty']:
        name = mode + '-default.log'
        native[name] = counts((validation / name).read_text())
        assert native[name] == [240, 0, 2, 13, 15, 9, 10, 165, 0, 8, 12, 0]
    assert 'Finished' in (validation / 'clippy.log').read_text()
    artifacts = {}
    for name in ['full-balanced', 'full-sham', 'isolated-sham']:
        run = load('local-plan-v4-' + name)
        assert run['schedule'] == list('flttlf') * 2 and len(run['runs']) == 12
        assert run['binary'] == abba['candidate'] and len(run['summary']) == 7
        verify_sources(sources, run)
        artifacts[name] = run['summary']
    miri = {}
    for case, expected in [('local', [6, 0]), ('global', [7, 0]), ('shared', [9, 0])]:
        run = load('local-plan-v4-miri-' + case)
        assert run['exit_code'] == 0 and not run['timed_out'] and run['sources_unchanged']
        assert run['evaluation'] == 'local'
        assert run['miriflags'] == '-Zmiri-env-forward=TYPED_EVALUATION'
        assert counts(run['stdout']) == expected
        verify_sources(sources, run)
        miri[case] = expected
    for mode in ['local', 'local-dirty']:
        path = validation / ('miri-' + mode + '-integrations.log')
        assert counts(path.read_text()) == [2, 13, 9, 10]
        miri[mode + '-integrations'] = {'counts': [2, 13, 9, 10], 'sha256': sha(path)}
    memory, work = {}, {}
    for mode in ['full', 'local', 'local-dirty']:
        for kind, target in [('memory', memory), ('work', work)]:
            run = load(f'local-plan-v4-{mode}-{kind}')
            verify_sources(sources, run)
            target[mode] = run['metrics']
        for row in memory[mode]['rows']:
            if row['engine'] in ['reactive', 'typed_research']:
                assert (row['steady_allocations'], row['steady_reallocations'],
                        row['after_drop_net_bytes']) == (0, 0, 0)
        assert all(r['after_drop_net_bytes'] == 0 for r in memory[mode]['owner_resources'])
    for mode in ['local', 'local-dirty']:
        assert [r['work'] for r in work[mode]['rows']] == [r['work'] for r in work['full']['rows']]
    profile = load('local-plan-diagnosis-v4-local-chain')
    assert profile['rustflags'] == '-C force-frame-pointers=yes'
    verify_sources(sources, profile)
    result = dict(source_sha256=sources, native=native, miri=miri, memory=memory,
                  work=work, timing=artifacts, before_after=abba['summary'],
                  candidate=abba['candidate'], profile=profile,
                  limits=['No promotion: residual regressions against FullFrames',
                          'Small deltas not resolved by shams; no physical isolation',
                          'Directed Miri, not all ports/full model or independent unsafe review',
                          'No new reference-engine timing; before/after is a bundled correction'])
    (LOG / 'local-plan-v4-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print('Verified profiled local-plan v4 checkpoint')


if __name__ == '__main__':
    main()
