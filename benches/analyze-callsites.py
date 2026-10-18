#!/usr/bin/env python3
"""Disjoint instruction/callsite accounting and diagnostic Session entry costs."""
import collections
import hashlib
import json
from pathlib import Path
import re
import statistics
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'
PROJECT = '/experiments/typed-runtime/src/'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(label):
    return json.loads((LOG / (label + '.json')).read_text())


def canonical(name):
    return re.sub(r"'\d+$", '', name)


def parse(path):
    names, objects = {}, {}
    own, calls, edges, pcs = (collections.Counter() for _ in range(4))
    caller = callee = obj = None
    position = [0, 0]
    call = False
    text = path.read_text()
    assert 'positions: instr line\n' in text and 'events: Ir\n' in text
    for line in text.splitlines():
        match = re.fullmatch(r'(c?fn|c?ob)=\((\d+)\)(?: (.*))?', line)
        if match:
            tag, ident, name = match[1], int(match[2]), match[3]
            table = names if tag.endswith('fn') else objects
            if name is not None:
                table[ident] = name
            if tag == 'fn':
                caller = ident
            elif tag == 'cfn':
                callee = ident
            elif tag == 'ob':
                obj = ident
        elif line.startswith('calls='):
            calls[caller, callee] += int(line.split()[0].split('=')[1])
            call = True
        elif re.match(r'^[+\-*\d]', line):
            fields = line.split()
            assert len(fields) == 3
            for i, value in enumerate(fields[:2]):
                if value != '*':
                    position[i] = position[i] + int(value, 0) if value[0] in '+-' else int(value, 0)
            cost = int(fields[2])
            if call:
                edges[caller, callee] += cost
                call = False
            else:
                own[caller] += cost
                pcs[caller, obj, position[0]] += cost
    summary = int(re.search(r'^summary: (\d+)$', text, re.M)[1])
    assert sum(own.values()) == sum(pcs.values()) == summary and not call
    return {'names': names, 'objects': objects, 'own': own, 'calls': calls, 'edges': edges, 'pcs': pcs}


def stacks_for(binary, addresses):
    command = ['addr2line', '-C', '-a', '-f', '-i', '-e', str(binary)]
    text = subprocess.check_output(command, input=''.join(hex(a) + '\n' for a in sorted(addresses)), text=True)
    result = {}
    address, lines = None, []

    def finish():
        if address is not None:
            assert len(lines) % 2 == 0
            frames = []
            for i in range(0, len(lines), 2):
                match = re.match(r'^(.*):(\d+|\?)(?: \(discriminator .*\))?$', lines[i + 1])
                frames.append({'function': lines[i], 'file': match[1] if match else lines[i + 1],
                               'line': int(match[2]) if match and match[2] != '?' else 0})
            result[address] = frames

    for line in text.splitlines():
        if re.fullmatch(r'0x[0-9a-f]+', line):
            finish()
            address, lines = int(line, 16), []
        else:
            lines.append(line)
    finish()
    assert set(result) == addresses
    return result, text


def location(group, frame):
    path = frame['file']
    if path.startswith(str(ROOT) + '/'):
        path = str(Path(path).relative_to(ROOT))
    return f"{group}|{path}|{frame['line']}"


def classify(root, frames):
    project = [f for f in frames if PROJECT in f['file'] and f['line']]
    # Assign EACH instruction once to its innermost logical evaluator method,
    # not simultaneously to next, tick, run, update and their inline helpers.
    for method in ['next', 'push', 'pop', 'enter']:
        candidates = [f for f in project if re.match(r'^<typed_runtime_prototype::(?:fused|evaluation)::Session>::' + method + r'(?:$|::)', f['function'])]
        if candidates:
            return location('evaluator.' + method, candidates[0])
    for method in ['capture', 'validation', 'restore']:
        candidates = [f for f in project if f['function'].startswith('<typed_runtime_prototype::fused::Context>::' + method)]
        if candidates:
            return location('context.' + method, candidates[0])
    for method in ['execute', 'tick', 'run']:
        candidates = [f for f in project if re.match(r'^<typed_runtime_prototype::(?:fused|evaluation)::Session>::' + method + r'(?:$|::)', f['function'])]
        if candidates:
            return location('evaluator.' + method, candidates[0])
    for marker, group in [('typed_runtime_prototype::record_resolved_read', 'record_read'),
                          ('typed_runtime_prototype::record_read', 'record_read'),
                          ('<typed_runtime_prototype::Runtime>::read_copy', 'read_copy'),
                          ('<typed_runtime_prototype::Runtime>::reconcile', 'reconcile'),
                          ('<typed_runtime_prototype::Runtime>::prepare_logical_execution', 'prepare.logical'),
                          ('<typed_runtime_prototype::Runtime>::prepare_execution', 'prepare.physical')]:
        candidates = [f for f in project if f['function'].startswith(marker)]
        if candidates:
            return location(group, candidates[-1])
    candidates = [f for f in project if re.match(r'typed_runtime_prototype::execute(?:::)?<', f['function'])]
    if candidates:
        return location('typed_execute', candidates[-1])
    candidates = [f for f in project if re.match(r'^typed_runtime_prototype::(?:fused|evaluation)::update(?:$|::)', f['function'])]
    if candidates:
        return location('session.entry_exit', candidates[0])
    # Retain undecodable prologues/epilogues explicitly; do not redistribute
    # them among source lines based on the desired explanation.
    if 'fused::Session>::push' in root:
        group = 'evaluator.push.unmapped'
    elif 'fused::Session>::pop' in root:
        group = 'evaluator.pop.unmapped'
    elif root == '<typed_runtime_prototype::Runtime>::update':
        group = 'update.other'
    elif '::execute::<' in root:
        group = 'typed_execute.unmapped'
    elif '>::prepare_logical_execution' in root:
        group = 'prepare.logical.unmapped'
    elif '::PartialEq>::eq' in root:
        group = 'comparator.body'
    elif ' as walk_diagnostic::adapters::Engine>::' in root:
        group = 'fixture.callback'
    else:
        group = root
    frame = project[-1] if project else {'file': '[no-project-callsite]', 'line': 0}
    return location(group, frame)


def fit(rows):
    slope, intercept = statistics.linear_regression([n for n, _ in rows], [v for _, v in rows])
    residual = max(abs(v - slope * n - intercept) for n, v in rows)
    return {'slope': slope, 'intercept': intercept, 'max_residual': residual}


def inspect(label):
    data = load(label)
    profile = load(data['profile_manifest'])
    assert profile['profile'] == 'profiling' and profile['rustflags'] == '-C panic=abort'
    assert data['source_sha256'] == profile['source_sha256']
    assert data['binary']['sha256'] == profile['sealed_binary']['sha256']
    assert data['collector_sha256'] == digest(ROOT / 'benches/profile-callsites.py')
    binary = Path(data['binary']['path'])
    assert digest(binary) == data['binary']['sha256'] and data['panic'] == 'abort'
    assert len(data['runs']) == (48 if label == 'callsite-current' else 24)
    bundle = json.loads((LOG / 'runs' / label / 'source-bundle.json').read_text())
    for path, expected in data['source_sha256'].items():
        assert hashlib.sha256(bundle[path].encode()).hexdigest() == expected, path
    parsed, addresses = {}, set()
    for row in data['runs']:
        path = ROOT / row['trace']
        assert digest(path) == row['trace_sha256']
        assert '--dump-instr=yes' in row['command'] and '--toggle-collect=reactive_walk_flush' in row['command']
        assert row['probe']['panic'] == 'abort' and not row['probe']['root_diagnostics'] and not row['probe']['root_verification'] and row['probe']['work'] is None
        assert row['probe']['shape'] == row['shape'] and row['probe']['size'] == row['size'] and row['probe']['iterations'] == row['iterations']
        trace = parse(path)
        assert sum(trace['own'].values()) == row['instructions']
        key = row['mode'], row['shape'], row['size'], row['iterations']
        assert key not in parsed
        parsed[key] = trace
        for (_, obj, address), count in trace['pcs'].items():
            if count and trace['objects'][obj] == str(binary):
                addresses.add(address)
    stacks, text = stacks_for(binary, addresses)
    directory = LOG / 'runs' / label
    (directory / 'inline-stacks.log').write_text(text)
    (directory / 'inline-stacks.json').write_text(json.dumps({hex(a): s for a, s in stacks.items()}, indent=2) + '\n')
    disassembly = subprocess.check_output(['objdump', '-d', '-C', '--no-show-raw-insn', str(binary)], text=True)
    (directory / 'disassembly.log').write_text(disassembly)
    assembly = {}
    for line in disassembly.splitlines():
        match = re.match(r'^\s*([0-9a-f]+):\s+(.*)$', line)
        if match:
            assembly[int(match[1], 16)] = match[2]
    costs = {}
    for key, trace in parsed.items():
        counter = collections.Counter()
        detailed = []
        for (fn, obj, address), count in trace['pcs'].items():
            if not count:
                continue
            root = canonical(trace['names'][fn])
            if trace['objects'][obj] == str(binary):
                site = classify(root, stacks[address])
            else:
                site = 'external:' + root + '|[external-object]|0'
            counter[site] += count
            if key[2:] == (256, 200):
                detailed.append({'physical_function': root, 'address': hex(address), 'Ir': count,
                                 'Ir_per_write': count / 200, 'callsite': site,
                                 'asm': assembly.get(address) if trace['objects'][obj] == str(binary) else None})
        assert sum(counter.values()) == sum(trace['own'].values())
        costs[key] = counter
        if detailed:
            assert sum(row['Ir'] for row in detailed) == sum(counter.values())
            (directory / f'instructions-{key[0]}-{key[1]}-256.json').write_text(json.dumps(detailed, indent=2) + '\n')
    # Collection boundaries count Ir only in the measured loop. Calls also
    # include warmup; use the 400-200 differential for all invocation counts.
    for mode, shape, size, iterations in parsed:
        if iterations != 200:
            continue
        before, after = costs[mode, shape, size, 200], costs[mode, shape, size, 400]
        assert before.keys() == after.keys()
        assert all(after[k] == 2 * before[k] for k in before)
    return data, parsed, costs


def call_counts(trace):
    counter = collections.Counter()
    for (_, callee), value in trace['calls'].items():
        counter[canonical(trace['names'][callee])] += value
    return counter


def main():
    current, parsed, costs = inspect('callsite-current')
    for path, expected in current['source_sha256'].items():
        if path.endswith('.rs'):
            assert digest(ROOT / path) == expected, path
    profile = load('logical-prepare-outlined')
    assert current['binary']['sha256'] == profile['sealed_binary']['sha256']
    for row in current['runs']:
        expected = profile['fits'][row['mode']][row['shape']]['flush']
        assert abs(row['instructions'] / row['iterations'] - expected['instructions_per_node'] * row['size'] - expected['fixed_instructions']) < 1e-6
    fits, groups, physical = {}, {}, {}
    for mode in ['full', 'topological']:
        for shape in ['chain', 'cut', 'diamond', 'fan_out']:
            keys = set().union(*(costs[mode, shape, n, 200] for n in [64, 256, 384]))
            fits[mode, shape] = {key: fit([(n, costs[mode, shape, n, 200][key] / 200) for n in [64, 256, 384]]) for key in keys}
            group_costs = {n: collections.Counter() for n in [64, 256, 384]}
            for n in group_costs:
                for key, cost in costs[mode, shape, n, 200].items():
                    group_costs[n][key.split('|')[0]] += cost / 200
            names = set().union(*group_costs.values())
            groups[mode, shape] = {key: fit([(n, group_costs[n][key]) for n in group_costs]) for key in names}
            by_function = {}
            for n in [64, 256, 384]:
                trace = parsed[mode, shape, n, 200]
                counter = collections.Counter()
                for fn, cost in trace['own'].items():
                    counter[canonical(trace['names'][fn])] += cost / 200
                by_function[n] = counter
            names = set().union(*by_function.values())
            physical[mode, shape] = {name: fit([(n, by_function[n][name]) for n in by_function]) for name in names}
    cut = fits['topological', 'cut']
    chain = fits['topological', 'chain']
    delta = {key: chain.get(key, {}).get('slope', 0) - cut.get(key, {}).get('slope', 0) for key in chain.keys() | cut.keys()}
    assert abs(sum(v['slope'] for v in cut.values()) - 330) < 1e-6
    assert abs(sum(v['slope'] for v in chain.values()) - 1209) < 1e-6
    assert abs(sum(delta.values()) - 879) < 1e-6
    physical_delta = {name: physical['topological', 'chain'].get(name, {}).get('slope', 0) - physical['topological', 'cut'].get(name, {}).get('slope', 0)
                      for name in physical['topological', 'chain'].keys() | physical['topological', 'cut'].keys()}
    completion = collections.Counter()
    for name, cost in physical_delta.items():
        if abs(cost) < 1e-8:
            continue
        if name == '<typed_runtime_prototype::Runtime>::update' or 'fused::Session>::' in name:
            block = 'evaluator_resume_and_execute'
        elif name == 'typed_runtime_prototype::execute::<u64>' or ('>::graph::<' in name and 'typed_runtime_prototype::execute<' in name):
            block = 'typed_completion_and_graph_helpers'
        elif any(marker in name for marker in ['>::read_copy', '::record_resolved_read', '>::notify']):
            block = 'clean_read_tracking_notify'
        elif '>::reconcile' in name:
            block = 'reconcile'
        elif '>::prepare_logical_execution' in name:
            block = 'logical_prepare'
        elif ' as walk_diagnostic::adapters::Engine>::' in name:
            block = 'callback_adapter'
        elif '::PartialEq>::eq' in name:
            block = 'comparator_body'
        else:
            raise AssertionError(('Unclassified exclusive differential', name, cost))
        completion[block] += cost
    assert abs(sum(completion.values()) - 879) < 1e-6

    probe, probe_parsed, probe_costs = inspect('callsite-entry-probe')
    probe_bundle = json.loads((LOG / 'runs/callsite-entry-probe/source-bundle.json').read_text())
    current_bundle = json.loads((LOG / 'runs/callsite-current/source-bundle.json').read_text())
    for path, text in current_bundle.items():
        if not path.endswith('.rs'):
            continue
        actual = probe_bundle[path]
        if path.endswith('/fused.rs') or path.endswith('/evaluation.rs'):
            actual = actual.replace('#[inline(never)]\npub(super) fn update(', 'pub(super) fn update(')
            actual = actual.replace('    #[inline(never)]\n    fn run(', '    fn run(')
        assert actual == text, path
    entries = []
    for mode, module in [('full', 'evaluation'), ('topological', 'fused')]:
        entry_name = f'typed_runtime_prototype::{module}::update'
        run_name = f'<typed_runtime_prototype::{module}::Session>::run'
        for shape in ['diamond', 'fan_out']:
            for n in [64, 256, 384]:
                first = probe_parsed[mode, shape, n, 200]
                second = probe_parsed[mode, shape, n, 400]
                calls = (call_counts(second)[entry_name] - call_counts(first)[entry_name]) / 200
                assert calls == n
                own = sum(cost for fn, cost in first['own'].items() if canonical(first['names'][fn]) == entry_name)
                other_calls = sum(cost for (caller, callee), cost in first['edges'].items()
                                  if canonical(first['names'][caller]) == entry_name and canonical(first['names'][callee]) != run_name)
                run_calls = sum(cost for (caller, callee), cost in first['edges'].items()
                                if canonical(first['names'][caller]) == entry_name and canonical(first['names'][callee]) == run_name)
                assert calls > 0 and run_calls > 0
                details_path = LOG / 'runs/callsite-entry-probe' / f'instructions-{mode}-{shape}-256.json'
                parts = collections.Counter()
                if n == 256:
                    for instruction in json.loads(details_path.read_text()):
                        if instruction['physical_function'] == entry_name:
                            parts[instruction['callsite'].split('|')[0]] += instruction['Ir'] / (200 * calls)
                    assert abs(sum(parts.values()) - own / (200 * calls)) < 1e-6
                entries.append({'mode': mode, 'shape': shape, 'size': n, 'sessions_per_write': calls,
                                'entry_exit_self_per_session': own / (200 * calls),
                                'entry_exit_including_other_helpers_per_session': (own + other_calls) / (200 * calls),
                                'entry_parts_per_session': dict(parts),
                                'sum_run_edges_inclusive_non_additive_per_write': run_calls / 200})
    probe_totals = {}
    for mode in ['full', 'topological']:
        for shape in ['diamond', 'fan_out']:
            probe_totals[mode + '-' + shape] = fit([(n, sum(probe_costs[mode, shape, n, 200].values()) / 200) for n in [64, 256, 384]])
    artifacts = []
    for label in ['callsite-current', 'callsite-entry-probe']:
        directory = LOG / 'runs' / label
        for path in [directory / 'inline-stacks.log', directory / 'inline-stacks.json', directory / 'disassembly.log', *directory.glob('instructions-*.json')]:
            artifacts.append({'path': str(path.relative_to(ROOT)), 'sha256': digest(path)})
    # Native reference timing already exists for this exact Rust source. Do not
    # relabel the older "no timing yet" checkpoint as the current measurement.
    timing = load('current-outlined-references')
    for path, expected in timing['source_sha256'].items():
        if path.endswith('.rs'):
            assert digest(ROOT / path) == expected, path
    assert digest(Path(timing['binary']['path'])) == timing['binary']['sha256']
    validation = load('callsite-validation')
    assert validation['unchanged']
    for path, expected in validation['source_sha256'].items():
        assert digest(ROOT / path) == expected, path
    for run in validation['runs']:
        assert run['exit_code'] == 0 and digest(ROOT / run['log']) == run['sha256']
    result = {'current_binary': current['binary'], 'probe_binary': probe['binary'], 'artifacts': artifacts,
              'current_native_reference_timing_manifest': 'current-outlined-references',
              'validation_manifest_sha256': digest(LOG / 'callsite-validation.json'),
              'exclusive_chain_minus_cut_blocks': dict(completion),
              'cut_callsites': dict(sorted(cut.items(), key=lambda item: item[1]['slope'], reverse=True)),
              'chain_minus_cut': dict(sorted(delta.items(), key=lambda item: item[1], reverse=True)),
              'group_fits': {mode + '-' + shape: value for (mode, shape), value in groups.items()},
              'all_callsite_fits': {mode + '-' + shape: value for (mode, shape), value in fits.items()},
              'physical_function_fits': {mode + '-' + shape: value for (mode, shape), value in physical.items()},
              'session_entry_probe': entries,
              'probe_total_fits_not_a_baseline': probe_totals,
              'limits': ['Each instruction assigned once; no inclusive sums across nested callsites',
                         'DWARF callsites are attribution, not causal optimization estimates',
                         'noinline update/run probe changes codegen; not a replacement timing/cost baseline',
                         'Summed run inclusive edges overlap in nested sessions; entry cost excludes ALL run edges',
                         'No runtime change retained; no IPC/cache/latency causal claim']}
    (LOG / 'callsite-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print('CUT GROUPS', json.dumps(groups['topological', 'cut'], indent=2))
    print('DELTA GROUPS', json.dumps({k: groups['topological', 'chain'].get(k, {}).get('slope', 0) - groups['topological', 'cut'].get(k, {}).get('slope', 0)
                                      for k in groups['topological', 'chain'].keys() | groups['topological', 'cut'].keys()}, indent=2))
    print('ENTRIES', json.dumps(entries, indent=2))


if __name__ == '__main__':
    main()
