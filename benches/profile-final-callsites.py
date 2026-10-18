#!/usr/bin/env python3
"""PC dumps of final Demand and Sycamore, matched sealed binary, set AND flush."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    label = 'final-callsite'
    profile = json.loads((LOG / 'hypothesis-final.json').read_text())
    assert profile['profile'] == 'profiling' and profile['rustflags'] == '-C panic=abort'
    identity = profile['sealed_binary']
    assert digest(Path(identity['path'])) == identity['sha256'] == profile['binary']['sha256']
    for path, expected in profile['source_sha256'].items():
        if path.endswith(('.rs', '.toml', '.lock')):
            assert digest(ROOT / path) == expected, path
    directory = LOG / 'runs' / label
    directory.mkdir(parents=True, exist_ok=False)
    binary = directory / 'sealed-binary'
    shutil.copyfile(identity['path'], binary)
    binary.chmod(0o755)
    shutil.copyfile(LOG / 'runs/hypothesis-final/source-bundle.json', directory / 'source-bundle.json')
    result = {'label': label, 'profile_manifest': 'hypothesis-final',
              'binary': {'path': str(binary.resolve()), 'sha256': digest(binary)},
              'source_sha256': profile['source_sha256'], 'collector_sha256': digest(Path(__file__)),
              'panic': 'abort', 'runs': []}
    for mode in ['topological', 'sycamore']:
        for shape in ['chain', 'cut']:
            for phase in ['set', 'flush']:
                symbol = ('reactive_sycamore_walk_' if mode == 'sycamore' else 'reactive_walk_') + phase
                for size in [64, 256, 384]:
                    for iterations in [200, 400]:
                        name = f'{mode}-{shape}-{phase}-{size}-{iterations}'
                        trace = directory / (name + '.callgrind')
                        command = ['valgrind', '--tool=callgrind', '--dump-instr=yes', '--collect-atstart=no',
                                   '--toggle-collect=' + symbol, '--callgrind-out-file=' + str(trace),
                                   str(binary.resolve()), 'sycamore' if mode == 'sycamore' else 'typed',
                                   shape, str(size), str(iterations), 'phases']
                        run = subprocess.run(command, cwd=ROOT, env=dict(os.environ, TYPED_EVALUATION='topological'),
                                             capture_output=True, text=True, check=True, timeout=180)
                        (directory / (name + '.log')).write_text(run.stderr)
                        probe = json.loads(run.stdout)
                        assert probe['panic'] == 'abort' and probe['phase_probe']
                        assert not probe['root_diagnostics'] and not probe['root_verification'] and probe['work'] is None
                        text = trace.read_text()
                        assert re.search(r'^positions: instr line$', text, re.M) and re.search(r'^events: Ir$', text, re.M)
                        total = int(re.search(r'^summary: (\d+)$', text, re.M)[1])
                        result['runs'].append({'mode': mode, 'shape': shape, 'phase': phase, 'size': size,
                                               'iterations': iterations, 'command': command,
                                               'trace': str(trace.relative_to(ROOT)), 'trace_sha256': digest(trace),
                                               'instructions': total, 'probe': probe})
                        (LOG / (label + '.json')).write_text(json.dumps(result, indent=2) + '\n')
                        print(name, total / iterations, 'Ir/write', flush=True)
    assert digest(binary) == identity['sha256']
    for path, expected in profile['source_sha256'].items():
        if path.endswith('.rs'):
            assert digest(ROOT / path) == expected, path


if __name__ == '__main__':
    main()
