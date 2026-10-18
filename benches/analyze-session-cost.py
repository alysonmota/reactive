#!/usr/bin/env python3
"""Verify isolated session/read ablations and exact-region execute accounting."""
import collections
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(label):
    return json.loads((LOG / (label + '.json')).read_text())


def parse(path):
    names, files = {}, {}
    own, calls, edges = collections.Counter(), collections.Counter(), collections.Counter()
    instructions = collections.Counter()
    caller = callee = None
    call = False
    pos = [0, 0]
    for line in path.read_text().splitlines():
        m = re.fullmatch(r'(c?fn)=\((\d+)\)(?: (.*))?', line)
        if m:
            ident = int(m[2])
            if m[3] is not None:
                names[ident] = m[3]
            if m[1] == 'fn':
                caller = ident
            else:
                callee = ident
        elif line.startswith('calls='):
            calls[callee] += int(line.split()[0].split('=')[1])
            call = True
        elif re.match(r'^[+\-*\d]', line):
            fields = line.split()
            for i, value in enumerate(fields[:-1]):
                if value != '*':
                    pos[i] = pos[i] + int(value, 0) if value[0] in '+-' else int(value, 0)
            cost = int(fields[-1])
            if call:
                edges[caller, callee] += cost
                call = False
            else:
                own[caller] += cost
                instructions[caller, pos[0]] += cost
    summary = int(re.search(r'^summary: (\d+)$', path.read_text(), re.M)[1])
    assert sum(own.values()) == summary and not call
    return names, own, calls, edges, instructions


def main():
    profiles = {}
    bundles = {}
    for label in ['branching-current', 'session-drop', 'resolved-read', 'session-final']:
        data = load(label)
        directory = LOG / 'runs' / label
        bundle = json.loads((directory / 'source-bundle.json').read_text())
        for path, expected in data['source_sha256'].items():
            assert hashlib.sha256(bundle[path].encode()).hexdigest() == expected, path
            if label == 'session-final':
                assert sha(ROOT / path) == expected, path
        bundles[label] = bundle
        identity = data['sealed_binary']
        assert sha(Path(identity['path'])) == identity['sha256'] == data['binary']['sha256']
        assert data['rustflags'] == '-C panic=abort' and len(data['runs']) == 48
        for row in data['runs']:
            assert row['panic'] == 'abort' and row['phase_probe']
            assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
            assert '--toggle-collect=reactive_walk_' + row['phase'] in row['command']
            trace = directory / f"{row['mode']}-{row['shape']}-{row['size']}-{row['phase']}.callgrind"
            assert sha(trace) == row['trace_sha256']
            total = int(re.search(r'^summary: (\d+)$', trace.read_text(), re.M)[1])
            assert total == row['instructions']
            fit = data['fits'][row['mode']][row['shape']][row['phase']]
            assert abs(total / row['iterations'] - row['size'] * fit['instructions_per_node'] - fit['fixed_instructions']) < 1e-6
        profiles[label] = data['fits']
    # Each standalone ablation changes exactly one runtime file. The final
    # bundle combines their exact file contents, not a different implementation.
    base = bundles['branching-current']
    for label, changed in [('session-drop', 'fused.rs'), ('resolved-read', 'lib.rs')]:
        assert [p for p in base if p.endswith('.rs') and base[p] != bundles[label][p]] == ['experiments/typed-runtime/src/' + changed]
    for path in base:
        if path.endswith('.rs'):
            source = 'session-drop' if path.endswith('/fused.rs') else 'resolved-read'
            assert bundles['session-final'][path] == bundles[source][path], path

    timing = load('session-final-abba')
    assert timing['panic'] == timing['baseline']['panic'] == 'abort'
    for identity in [timing['candidate'], timing['baseline']['candidate']]:
        assert sha(Path(identity['path'])) == identity['sha256']
    for path, expected in timing['source_sha256'].items():
        assert sha(ROOT / path) == expected, path
    assert timing['schedule'] == list('abba') * 2
    assert set(timing['runs']) == {'full', 'topological'}
    assert all(len(runs) == 8 for runs in timing['runs'].values())

    counts = load('session-step-counts')
    assert sha(Path(counts['binary']['path'])) == counts['binary']['sha256']
    probe = load('session-step-probe')
    assert counts['binary'] == probe['sealed_binary']
    probe_bundle = json.loads((LOG / 'runs/session-step-probe/source-bundle.json').read_text())
    for path, expected in probe['source_sha256'].items():
        assert hashlib.sha256(probe_bundle[path].encode()).hexdigest() == expected, path
    path = 'experiments/typed-runtime/src/fused.rs'
    assert probe_bundle[path].replace('    #[inline(never)]\n    fn next(', '    fn next(') == base[path]
    parsed_counts = {}
    for row in counts['runs']:
        trace = Path(row['trace'])
        assert sha(trace) == row['sha256'] and row['probe']['panic'] == 'abort'
        names, _, calls, _, _ = parse(trace)
        parsed_counts[row['shape'], row['iterations']] = {names[n]: value for n, value in calls.items()}
    transitions = {}
    for shape in ['chain', 'cut']:
        before, after = parsed_counts[shape, 200], parsed_counts[shape, 400]
        transitions[shape] = {name: (after[name] - before.get(name, 0)) / 200
                              for name in after if 'fused::Session>::' in name}
        assert transitions[shape] == counts[shape]
        assert sorted(transitions[shape].values()) == [257, 257, 513]

    control = load('session-cost-control')
    assert sha(Path(control['control']['path'])) == control['control']['sha256']
    row = next(r for r in control['runs'] if r['probe']['shape'] == 'chain' and r['probe']['iterations'] == 200)
    trace = Path(row['trace'])
    assert sha(trace) == row['sha256']
    names, own, _, edges, instructions = parse(trace)
    execute = next(n for n, name in names.items() if name == 'typed_runtime_prototype::execute::<u64>')
    assert own[execute] / 51200 == 223
    assert (own[execute] + sum(v for (caller, _), v in edges.items() if caller == execute)) / 51200 == 509
    addresses = {addr: cost for (fn, addr), cost in instructions.items() if fn == execute}
    stacks_text = subprocess.check_output(['addr2line', '-C', '-a', '-f', '-i', '-e', control['control']['path']],
                                         input=''.join(hex(addr) + '\n' for addr in addresses), text=True)
    stacks = {}
    addr = None
    for line in stacks_text.splitlines():
        if re.fullmatch('0x[0-9a-f]+', line):
            addr = int(line, 16)
            stacks[addr] = []
        else:
            stacks[addr].append(line)
    line_costs = collections.Counter()
    for addr, cost in addresses.items():
        lines = [int(m[1]) for frame in stacks[addr]
                 if (m := re.search(r'/experiments/typed-runtime/src/lib.rs:(\d+)', frame)) and 1224 <= int(m[1]) <= 1321]
        line_costs[lines[-1] if lines else 0] += cost
    assert sum(line_costs.values()) == own[execute]

    native = load('session-final-validation')
    assert native['unchanged']
    for path, expected in native['source_sha256'].items():
        assert sha(ROOT / path) == expected, path
    for run in native['runs']:
        assert run['exit_code'] == 0 and sha(Path(run['log'])) == run['sha256']
    miri = []
    for mode in ['full', 'topological']:
        for case in ['fused', 'planning', 'api', 'audit']:
            data = load(f'session-final-miri-{mode}-{case}')
            assert data['exit_code'] == 0 and data['sources_unchanged'] and not data['timed_out']
            for path, expected in data['source_sha256'].items():
                assert sha(ROOT / path) == expected, path
            passed = sum(map(int, re.findall(r'test result: ok\. (\d+) passed;', data['stdout'])))
            assert passed > 0
            miri.append({'mode': mode, 'case': case, 'passed': passed})
    result = {'fits': profiles, 'transitions_per_write_at_256': transitions,
              'control_execute_self_per_memo_by_callsite': {line: cost / 51200 for line, cost in sorted(line_costs.items())},
              'miri': miri, 'timing': timing['summary'], 'limits': ['noinline probe is for call counts ONLY', 'Callsite DWARF attribution is not a causal ablation',
                                      'No changes to generation protocol, evaluator walk, reconcile or prepare_execution',
                                      'Directed Miri, not full-model/all-ports proof or independent unsafe review']}
    (LOG / 'session-cost-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
