#!/usr/bin/env python3
"""Disjoint final PC accounting, current Chain-cut, and matched Sycamore symbols."""
import collections
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'
spec = importlib.util.spec_from_file_location('old', ROOT / 'benches/analyze-callsites.py')
old = importlib.util.module_from_spec(spec)
spec.loader.exec_module(old)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def classify(root, frames):
    # Scan inline frames INSIDE OUT. push/pop are now inlined under next;
    # choosing next first by method name would absorb their cost twice/mislabel it.
    for frame in frames:
        function = frame['function']
        match = re.match(r'^<typed_runtime_prototype::fused::(Session|Context)>::(\w+)', function)
        if match:
            return old.location(('walk.' if match[1] == 'Session' else 'context.') + match[2], frame)
        for marker, group in [
            ('typed_runtime_prototype::record_resolved_read', 'read.tracking'),
            ('typed_runtime_prototype::record_read', 'read.tracking'),
            ('<typed_runtime_prototype::Runtime>::read_copy', 'read.copy'),
            ('<typed_runtime_prototype::Runtime>::reconcile', 'reconcile'),
            ('<typed_runtime_prototype::Runtime>::prepare_logical_execution', 'prepare.logical'),
            ('<typed_runtime_prototype::Runtime>::prepare_execution', 'prepare.physical'),
            ('typed_runtime_prototype::execute::<', 'typed.completion'),
            ('typed_runtime_prototype::execute<', 'typed.completion'),
            ('typed_runtime_prototype::fused::update', 'session.entry_exit'),
        ]:
            if function.startswith(marker):
                return old.location(group, frame)
        if re.match(r'^(?:<)?sycamore_reactive::', function):
            for marker, group in [
                ('>::run_node_update', 'sycamore.run_node_update'),
                ('>::dispose_children', 'sycamore.dispose_children'),
                ('>::dfs', 'sycamore.dfs'),
                ('>::propagate_updates', 'sycamore.propagate_updates'),
                ('>::propagate_node_updates', 'sycamore.propagate_node_updates'),
                ('>::mark_dependents_dirty', 'sycamore.mark_dependents_dirty'),
                ('>::create_dependency_link', 'sycamore.create_dependency_link'),
                ('>::with_untracked', 'sycamore.read_untracked'),
                ('>::track', 'sycamore.track'),
                ('>::notify', 'sycamore.notify'),
                ('memos::create_selector_with', 'sycamore.memo_callback'),
            ]:
                if marker in function:
                    return old.location(group, frame)
            # An inline helper/type mentioning Sycamore is not itself a
            # propagation callsite. Keep scanning for its enclosing operation.
    sycamore = [f for f in frames if re.match(r'^(?:<)?sycamore_reactive::', f['function'])]
    if sycamore:
        return old.location('sycamore.other:' + sycamore[0]['function'], sycamore[0])
    project = [f for f in frames if '/experiments/typed-runtime/src/' in f['file'] or '/benches/' in f['file']]
    group = 'physical:' + root
    return old.location(group, project[0] if project else {'file': '[unmapped-project]', 'line': 0})


def slopes(costs):
    keys = set().union(*costs.values())
    return {key: old.fit([(size, counter[key]) for size, counter in costs.items()]) for key in sorted(keys)}


def main():
    data = old.load('final-callsite')
    profile = old.load('hypothesis-final')
    directory = LOG / 'runs/final-callsite'
    binary = Path(data['binary']['path'])
    assert digest(binary) == data['binary']['sha256'] == profile['sealed_binary']['sha256']
    assert data['panic'] == 'abort' and profile['rustflags'] == '-C panic=abort'
    assert data['collector_sha256'] == digest(ROOT / 'benches/profile-final-callsites.py')
    assert len(data['runs']) == 48 and data['source_sha256'] == profile['source_sha256']
    bundle = json.loads((directory / 'source-bundle.json').read_text())
    for path, expected in data['source_sha256'].items():
        assert hashlib.sha256(bundle[path].encode()).hexdigest() == expected
        if path.endswith(('.rs', '.toml', '.lock')):
            assert digest(ROOT / path) == expected, path
    parsed, addresses = {}, set()
    for row in data['runs']:
        assert row['probe']['panic'] == 'abort' and row['probe']['phase_probe']
        assert not row['probe']['root_diagnostics'] and not row['probe']['root_verification'] and row['probe']['work'] is None
        symbol = ('reactive_sycamore_walk_' if row['mode'] == 'sycamore' else 'reactive_walk_') + row['phase']
        assert '--toggle-collect=' + symbol in row['command'] and '--dump-instr=yes' in row['command']
        path = ROOT / row['trace']
        assert digest(path) == row['trace_sha256']
        trace = old.parse(path)
        assert sum(trace['own'].values()) == row['instructions']
        key = row['mode'], row['shape'], row['phase'], row['size'], row['iterations']
        assert key not in parsed
        parsed[key] = trace
        addresses.update(address for (_, obj, address), count in trace['pcs'].items()
                         if count and trace['objects'][obj] == str(binary))
    stacks, text = old.stacks_for(binary, addresses)
    (directory / 'inline-stacks.log').write_text(text)
    (directory / 'inline-stacks.json').write_text(json.dumps({hex(pc): stack for pc, stack in stacks.items()}, indent=2) + '\n')
    assembly_text = subprocess.check_output(['objdump', '-d', '-C', '--no-show-raw-insn', str(binary)], text=True)
    (directory / 'disassembly.log').write_text(assembly_text)
    assembly = {}
    for line in assembly_text.splitlines():
        match = re.match(r'^\s*([0-9a-f]+):\s+(.*)$', line)
        if match:
            assembly[int(match[1], 16)] = match[2]
    # Archive the ACTUAL dependency sources named by DWARF, not a possibly
    # different reference clone. Include standard-library/helpers when available.
    files = {f['file'] for stack in stacks.values() for f in stack if f['line'] and Path(f['file']).is_file()}
    dependency = {path: Path(path).read_text() for path in sorted(files) if not path.startswith(str(ROOT) + '/')}
    lock = tomllib.loads((ROOT / 'Cargo.lock').read_text())
    package = next(p for p in lock['package'] if p['name'] == 'sycamore-reactive' and p['version'] == '0.9.4')
    sycamore_sources = [Path(path) for path in dependency if '/sycamore-reactive-0.9.4/src/' in path]
    assert len(sycamore_sources) >= 4
    for path in sycamore_sources:
        root = path.parent.parent
        archive = root.parents[2] / 'cache' / root.parent.name / (root.name + '.crate')
        assert digest(archive) == package['checksum']
        with tarfile.open(archive) as crate:
            member = crate.extractfile(root.name + '/' + str(path.relative_to(root)))
            assert member is not None
            assert path.read_bytes() == member.read(), path
    (directory / 'dependency-source-bundle.json').write_text(json.dumps(dependency) + '\n')
    dependency_sha = {path: hashlib.sha256(text.encode()).hexdigest() for path, text in dependency.items()}
    sites, physical, groups = {}, {}, {}
    for key, trace in parsed.items():
        counter, funcs = collections.Counter(), collections.Counter()
        detail = []
        for (fn, obj, pc), cost in trace['pcs'].items():
            if not cost:
                continue
            name = old.canonical(trace['names'][fn])
            same_object = trace['objects'][obj] == str(binary)
            stack = stacks[pc] if same_object else []
            site = classify(name, stack) if same_object else 'external:' + name + '|[external]|0'
            counter[site] += cost
            funcs[name] += cost
            if key[3:] == (256, 200):
                detail.append({'address': hex(pc), 'physical_function': name, 'Ir': cost,
                               'Ir_per_write': cost / 200, 'callsite': site,
                               'asm': assembly.get(pc) if same_object else None,
                               'inline_stack': stack})
        assert sum(counter.values()) == sum(funcs.values()) == sum(trace['own'].values())
        sites[key] = counter
        physical[key] = funcs
        groups[key] = collections.Counter()
        for site, cost in counter.items():
            groups[key][site.split('|')[0]] += cost
        if key[3:] == (256, 200):
            assert sum(row['Ir'] for row in detail) == sum(counter.values())
            (directory / f'instructions-{key[0]}-{key[1]}-{key[2]}-256.json').write_text(json.dumps(detail, indent=2) + '\n')
    fits = {'callsite': {}, 'physical': {}, 'group': {}}
    calls = {}
    for mode in ['topological', 'sycamore']:
        for shape in ['chain', 'cut']:
            for phase in ['set', 'flush']:
                fixture = mode + '-' + shape + '-' + phase
                for kind, counts in [('callsite', sites), ('physical', physical), ('group', groups)]:
                    costs = {n: collections.Counter({k: v / 200 for k, v in counts[mode, shape, phase, n, 200].items()}) for n in [64, 256, 384]}
                    fits[kind][fixture] = slopes(costs)
                    assert all(fit['max_residual'] < 1e-6 for fit in fits[kind][fixture].values())
                    for n in [64, 256, 384]:
                        before, after = counts[mode, shape, phase, n, 200], counts[mode, shape, phase, n, 400]
                        assert all(after[k] == 2 * before[k] for k in before.keys() | after.keys())
                # Calls include setup even with collect disabled: subtract warmup.
                calls[fixture] = {}
                for n in [64, 256, 384]:
                    before = old.call_counts(parsed[mode, shape, phase, n, 200])
                    after = old.call_counts(parsed[mode, shape, phase, n, 400])
                    calls[fixture][n] = {fn: (after[fn] - before[fn]) / 200 for fn in before.keys() | after.keys() if after[fn] != before[fn]}
    totals = {fixture: {part: sum(fit[part] for fit in mapping.values()) for part in ['slope', 'intercept']}
              for fixture, mapping in fits['group'].items()}
    for shape in ['chain', 'cut']:
        for phase in ['set', 'flush']:
            expected = profile['fits']['topological'][shape][phase]
            got = totals['topological-' + shape + '-' + phase]
            assert abs(got['slope'] - expected['instructions_per_node']) < 1e-6
            assert abs(got['intercept'] - expected['fixed_instructions']) < 1e-6
    assert abs(totals['topological-cut-flush']['slope'] - 272) < 1e-6
    assert abs(totals['topological-chain-flush']['slope'] - 1064) < 1e-6
    assert totals['sycamore-chain-set']['slope'] == 744 and totals['sycamore-cut-set']['slope'] == 111
    differences = {}
    for kind, mapping in fits.items():
        differences[kind] = {}
        for mode in ['topological', 'sycamore']:
            phase = 'set' if mode == 'sycamore' else 'flush'
            chain, cut = mapping[mode + '-chain-' + phase], mapping[mode + '-cut-' + phase]
            differences[kind][mode] = {name: chain.get(name, {}).get('slope', 0) - cut.get(name, {}).get('slope', 0) for name in sorted(chain.keys() | cut.keys())}
            assert abs(sum(differences[kind][mode].values()) - (792 if mode == 'topological' else 633)) < 1e-6
    blocks = {}
    for mode, delta in differences['physical'].items():
        costs = collections.Counter()
        for name, cost in delta.items():
            if abs(cost) < 1e-6:
                continue
            if mode == 'topological':
                if name == '<typed_runtime_prototype::Runtime>::update' or 'fused::Session>::' in name:
                    group = 'evaluator_execute_resume'
                elif name == 'typed_runtime_prototype::execute::<u64>' or ('>::graph::<' in name and 'typed_runtime_prototype::execute<' in name):
                    group = 'typed_completion_and_graph_helpers'
                elif any(marker in name for marker in ['>::read_copy', '::record_resolved_read', '>::notify']):
                    group = 'clean_read_tracking_notify'
                elif '>::reconcile' in name:
                    group = 'reconcile'
                elif '>::prepare_logical_execution' in name:
                    group = 'logical_prepare'
                elif ' as walk_diagnostic::adapters::Engine>::' in name:
                    group = 'fixture_callback'
                elif '::PartialEq>::eq' in name:
                    group = 'comparator_body'
                else:
                    raise AssertionError((mode, name, cost))
            else:
                if '>::run_node_update' in name:
                    group = 'run_node_update'
                elif '>::dispose_children' in name:
                    group = 'dispose_children'
                elif '>::create_dependency_link' in name:
                    group = 'create_dependency_link'
                elif '>::mark_dependents_dirty' in name:
                    group = 'mark_dependents_dirty'
                elif '>::with_untracked' in name or '::SycamoreView as ' in name:
                    group = 'read_and_track'
                elif name.startswith('sycamore_reactive::memos::create_selector_with'):
                    group = 'memo_callback_comparison_publication'
                elif '::any::Any>::type_id' in name:
                    group = 'type_id_body'
                elif ' as walk_diagnostic::adapters::Engine>::set' in name:
                    group = 'propagation_scan_inlined_in_set'
                else:
                    raise AssertionError((mode, name, cost))
            costs[group] += cost
        blocks[mode] = dict(costs)
        assert abs(sum(costs.values()) - (792 if mode == 'topological' else 633)) < 1e-6
    cut_next = collections.Counter()
    next_ranges = {
        'frame_loop_certificate': {150, 154, 155, 158, 159, 163, 165},
        'source_access_revision_and_gate': {171, 172, 175, 177, 178, 186, 187, 190, 199, 203, 204, 207},
        'memo_state_initialized': {182, 183},
        'descent_callsite': {214, 215},
        'mark_clean': {221},
    }
    for site, fit in fits['callsite']['topological-cut-flush'].items():
        group, file, line = site.split('|')
        if group != 'walk.next' or abs(fit['slope']) < 1e-6:
            continue
        block = 'unmapped_line'
        if int(line):
            assert file == 'experiments/typed-runtime/src/fused.rs'
            found = [key for key, lines in next_ranges.items() if int(line) in lines]
            assert len(found) == 1, site
            block = found[0]
        cut_next[block] += fit['slope']
    assert abs(sum(cut_next.values()) - 181) < 1e-6
    artifacts = {str(path.relative_to(ROOT)): digest(path) for path in directory.iterdir() if path.name in [
        'inline-stacks.log', 'inline-stacks.json', 'disassembly.log', 'dependency-source-bundle.json'] or path.name.startswith('instructions-')}
    result = {'binary': data['binary'], 'totals': totals, 'fits': fits, 'chain_minus_cut': differences,
              'analyzer_sha256': digest(Path(__file__)),
              'parser_sha256': digest(ROOT / 'benches/analyze-callsites.py'),
              'sycamore_registry_package_checksum': package['checksum'],
              'chain_minus_cut_exclusive_blocks': blocks, 'cut_next_source_blocks': dict(cut_next),
              'chain_gap_bridge': {'cut_gap': 330 - 111, 'additional_execution_gap': 792 - 633, 'total_gap': 1122 - 744},
              'differential_calls_per_write_whole_loop': calls, 'dependency_source_sha256': dependency_sha,
              'artifacts': artifacts, 'limits': ['DWARF attribution is not a causal optimization estimate',
                  'One assignment per exclusive PC; no sums of overlapping inclusive callsites',
                  'Only slopes comparable per memo; intercepts retained separately',
                  'Symbols reveal mechanisms, not the irreducible price of contracts',
                  'No runtime change, new timing or IPC/cache causal claim']}
    (LOG / 'final-callsite-analysis.json').write_text(json.dumps(result, indent=2) + '\n')
    print('TOTALS', json.dumps(totals, indent=2))
    for fixture in ['topological-cut-flush', 'sycamore-cut-set']:
        print(fixture, {k: v['slope'] for k, v in fits['group'][fixture].items() if abs(v['slope']) > 1e-6})
    for mode in ['topological', 'sycamore']:
        print('CHAIN-CUT', mode, {k: v for k, v in differences['group'][mode].items() if abs(v) > 1e-6})


if __name__ == '__main__':
    main()
