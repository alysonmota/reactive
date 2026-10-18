#!/usr/bin/env python3
"""Verify prepare variants and exact reapplication of the outlined variant."""
import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(label):
    return json.loads((LOG / (label + '.json')).read_text())


def main():
    labels = ['session-final', 'logical-prepare', 'logical-prepare-inline', 'logical-prepare-outlined']
    bundles, profiles, manifests = {}, {}, {}
    for label in labels:
        data = load(label)
        manifests[label] = data
        directory = LOG / 'runs' / label
        bundle = json.loads((directory / 'source-bundle.json').read_text())
        for path, expected in data['source_sha256'].items():
            assert hashlib.sha256(bundle[path].encode()).hexdigest() == expected, path
            if label == 'logical-prepare-outlined':
                assert digest(ROOT / path) == expected, path
        bundles[label] = bundle
        identity = data['sealed_binary']
        assert digest(Path(identity['path'])) == identity['sha256'] == data['binary']['sha256']
        assert data['rustflags'] == '-C panic=abort' and len(data['runs']) == 48
        for row in data['runs']:
            assert row['panic'] == 'abort' and row['phase_probe']
            assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
            assert '--toggle-collect=reactive_walk_' + row['phase'] in row['command']
            trace = directory / f"{row['mode']}-{row['shape']}-{row['size']}-{row['phase']}.callgrind"
            assert digest(trace) == row['trace_sha256']
            total = int(re.search(r'^summary: (\d+)$', trace.read_text(), re.M)[1])
            assert total == row['instructions']
            fit = data['fits'][row['mode']][row['shape']][row['phase']]
            assert abs(total / row['iterations'] - row['size'] * fit['instructions_per_node'] - fit['fixed_instructions']) < 1e-6
        profiles[label] = data['fits']
    base = bundles['session-final']
    changed = [path for path in base if path.endswith('.rs') and base[path] != bundles['logical-prepare'][path]]
    assert set(changed) == {'experiments/typed-runtime/src/ownership.rs', 'experiments/typed-runtime/src/fused.rs'}
    ownership = 'experiments/typed-runtime/src/ownership.rs'
    for path in base:
        if not path.endswith('.rs'):
            continue
        inline = bundles['logical-prepare-inline'][path]
        outlined = bundles['logical-prepare-outlined'][path]
        if path == ownership:
            assert inline.replace('#[inline(always)]\nfn defer_execution', 'fn defer_execution') == bundles['logical-prepare'][path]
            assert outlined.replace('    #[inline(never)]\n    pub(super) fn prepare_logical_execution',
                                    '    pub(super) fn prepare_logical_execution') == inline
        else:
            assert inline == outlined == bundles['logical-prepare'][path]

    validation = []
    for mode in ['full', 'topological']:
        passed = 0
        for case in ['fused', 'planning', 'api', 'audit', 'optimization']:
            label = f'logical-prepare-miri-{mode}-{case}'
            data = load(label)
            assert data['exit_code'] == 0 and data['sources_unchanged'] and not data['timed_out']
            for path, expected in data['source_sha256'].items():
                if path.endswith('.rs'):
                    assert manifests['logical-prepare']['source_sha256'][path] == expected, path
            passed += sum(map(int, re.findall(r'test result: ok\. (\d+) passed;', data['stdout'])))
        assert passed == 70
        validation.append({'variant': 'logical-prepare', 'mode': mode, 'miri_passed': passed})
    native = load('logical-prepare-native')
    for path, expected in native['source_sha256'].items():
        assert manifests['logical-prepare']['source_sha256'][path] == expected, path
    for run in native['runs']:
        assert run['exit_code'] == 0 and digest(Path(run['log'])) == run['sha256']
    for variant in ['inline', 'outlined']:
        data = load(f'logical-prepare-{variant}-preflight')
        for path, expected in data['source_sha256'].items():
            assert manifests[f'logical-prepare-{variant}']['source_sha256'][path] == expected, path
        for run in data['runs']:
            assert run['exit_code'] == 0 and digest(Path(run['log'])) == run['sha256']
        for mode in ['full', 'topological']:
            passed = sum(run.get('passed', 0) for run in data['runs'] if run['mode'] == mode)
            assert passed == 7
            validation.append({'variant': variant, 'mode': mode, 'miri_passed': passed})
    result = {'decision': 'Outlined variant reapplied; source byte-identical to logical-prepare-outlined',
              'fits': profiles, 'validation': validation,
              'limits': ['No native timing for rejected variants', 'Directed Miri only',
                         'Codegen attribution is not proof of a cache/register/branch cause']}
    (LOG / 'logical-prepare-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
