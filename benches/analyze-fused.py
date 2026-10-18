#!/usr/bin/env python3
"""Verify sealed control/current fused evidence, not historical source approval."""
import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(label):
    return json.loads((LOG / (label + '.json')).read_text())


def current(hashes):
    for path, digest in hashes.items():
        assert sha(ROOT / path) == digest, path


def main():
    profiles = {}
    for label in ['fused-control', 'fused-stage-a', 'fused-stage-b']:
        data = load(label)
        directory = LOG / 'runs' / label
        bundle = json.loads((directory / 'source-bundle.json').read_text())
        for path, digest in data['source_sha256'].items():
            assert hashlib.sha256(bundle[path].encode()).hexdigest() == digest, path
        if label == 'fused-stage-b':
            # Source approval concerns Rust and Cargo files. Collector scripts
            # may evolve independently after an already sealed collection.
            current({p: h for p, h in data['source_sha256'].items() if not p.endswith('.py')})
            control = json.loads((LOG / 'runs/fused-control/source-bundle.json').read_text())
            assert control['experiments/typed-runtime/src/evaluation.rs'] == (ROOT / 'experiments/typed-runtime/src/evaluation.rs').read_text()
        assert data['rustflags'] == '-C panic=abort'
        assert len(data['runs']) == 36
        for row in data['runs']:
            assert row['panic'] == 'abort'
            assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
            symbol = ('reactive_sycamore_walk_' if row['mode'] == 'sycamore' else 'reactive_walk_') + row['phase']
            assert '--toggle-collect=' + symbol in row['command']
            trace = directory / f"{row['mode']}-{row['shape']}-{row['size']}-{row['phase']}.callgrind"
            assert sha(trace) == row['trace_sha256']
            instructions = int(re.search(r'^summary: (\d+)$', trace.read_text(), re.M)[1])
            assert instructions == row['instructions']
            fit = data['fits'][row['mode']][row['shape']][row['phase']]
            assert abs(row['instructions_per_write'] - fit['instructions_per_node'] * row['size'] - fit['fixed_instructions']) < 1e-6
        if 'sealed_binary' in data:
            identity = data['sealed_binary']
            assert sha(Path(identity['path'])) == identity['sha256'] == data['binary']['sha256']
        for annotation in data.get('annotations', []):
            assert sha(Path(annotation['path'])) == annotation['sha256']
        profiles[label] = data['fits']

    timing = load('fused-stage-b-abba')
    assert timing['panic'] == timing['baseline']['panic'] == 'abort'
    current(timing['source_sha256'])
    for identity in [timing['candidate'], timing['baseline']['binary']]:
        assert sha(Path(identity['path'])) == identity['sha256']
    assert timing['schedule'] == list('abba') * 2
    assert set(timing['runs']) == {'full', 'topological'}
    for runs in timing['runs'].values():
        assert len(runs) == 8

    native = load('fused-b-validation')
    assert native['unchanged']
    current(native['source_sha256'])
    validation = []
    for run in native['runs']:
        assert run['exit_code'] == 0
        text = Path(run['log']).read_text()
        if 'sha256' in run:
            assert sha(Path(run['log'])) == run['sha256']
            counts = list(map(int, re.findall(r'test result: ok\. (\d+) passed;', text)))
            assert counts and sum(counts) > 400
            validation.append({'mode': run['mode'], 'features': run['features'], 'passed_counts': counts})
        else:
            assert '-D' in run['command'] and 'warnings' in run['command']
            assert 'Finished' in text and 'error:' not in text

    miri = []
    for mode in ['full', 'topological']:
        for case in ['fused', 'planning', 'audit', 'production']:
            data = load(f'fused-b-miri-{mode}-{case}')
            assert data['exit_code'] == 0 and data['sources_unchanged'] and not data['timed_out']
            assert data['evaluation'] == mode and data['case'] == case
            current(data['source_sha256'])
            counts = list(map(int, re.findall(r'test result: ok\. (\d+) passed;', data['stdout'])))
            assert sum(counts) > 0
            miri.append({'mode': mode, 'case': case, 'passed_counts': counts})
    result = {'profiles': profiles, 'timing': timing['summary'], 'native': validation, 'miri': miri,
              'limits': ['Directed Miri, not full model/all-ports coverage or independent unsafe review',
                         'Profiling instructions are not native timing', 'No default change or promotion']}
    (LOG / 'fused-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
