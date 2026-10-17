#!/usr/bin/env python3
"""Verify stage-8 checkpoint evidence; never accept a pending or drifting run."""
import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'

def load(name):
    return json.loads((LOG / (name + '.json')).read_text())

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def counts(text):
    return list(map(int, re.findall(r'test result: ok\. (\d+) passed;', text)))

def main():
    result = load('shared-plan-opt-v1-validation')
    for path, sha in result['source_sha256'].items():
        assert digest(ROOT / path) == sha, path
    timing = {}
    for control, suffix in [('full', 'repeat'), ('shared', 'balanced'),
                            ('shared-dirty', 'balanced'), ('shared', 'sham')]:
        label = f'shared-plan-opt-v1-{control}-{suffix}'
        run = load(label)
        assert run['modes'] == {'f': control, 'l': 'shared-opt', 't': 'shared-dirty-opt'}
        assert run['schedule'] == list('flttlf') * 2
        assert len(run['runs']) == 12 and len(run['summary']) == 7
        assert run['sham'] == (suffix == 'sham')
        for path, sha in run['source_sha256'].items():
            assert result['source_sha256'][path] == sha, path
        assert digest(Path(run['binary']['path'])) == run['binary']['sha256']
        for block, variant in zip(run['runs'], run['schedule']):
            assert block['variant'] == variant
            assert block['evaluation'] == (control if run['sham'] else run['modes'][variant])
        timing[label] = run['summary']
        for fixture, row in run['summary'].items():
            print(label, fixture, {m: round(row[m + '_paired_percent'], 2)
                                   for m in ['shared-opt', 'shared-dirty-opt']})
    miri = {}
    for label, mode in [('shared', 'shared-opt'), ('dirty', 'shared-dirty-opt')]:
        run = load('shared-plan-opt-v1-miri-' + label)
        assert run['exit_code'] == 0 and not run['timed_out'] and run['sources_unchanged']
        assert run['evaluation'] == mode and counts(run['stdout']) == [99, 2, 0]
        for path, sha in run['source_sha256'].items():
            assert result['source_sha256'][path] == sha, path
        path = LOG / 'runs/shared-plan-opt-v1-validation' / f'miri-{label}-integrations.log'
        assert counts(path.read_text()) == [2, 13, 9, 10]
        miri[mode] = {'directed_unit': 99, 'package_integrations': 2,
                      'root_integrations': 34, 'normal_log_sha256': digest(path)}
    result.update(timing=timing, miri=miri, miri_status='Passed directed checkpoint runs',
                  timing_status='Completed; exploratory full-balanced explicitly excluded')
    (LOG / 'shared-plan-opt-v1-analysis.json').write_text(json.dumps(result, indent=2) + '\n')

if __name__ == '__main__':
    main()
