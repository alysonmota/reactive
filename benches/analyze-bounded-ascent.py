#!/usr/bin/env python3
"""Fail-closed evidence and exclusive PC comparison for the bounded ascent trial."""
import argparse
import collections
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'


def load(label):
    return json.loads((LOG / (label + '.json')).read_text())


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--callsites')
    args = parser.parse_args()
    assert re.fullmatch(r'[\w-]+', args.label)
    profile, gate = load(args.label), load(args.label + '-validation')
    control = load('hypothesis-final')
    assert gate['complete'] and profile['profile'] == 'profiling'
    assert profile['rustflags'] == control['rustflags'] == '-C panic=abort'
    for path, expected in gate['source_sha256'].items():
        assert profile['source_sha256'][path] == expected, path
    for row in gate['runs']:
        assert row['exit_code'] == 0 and row['passed'] > 0
        assert digest(ROOT / row['log']) == row['sha256']
    for mode in ['full', 'topological']:
        cases = [row for row in gate['runs'] if row['mode'] == mode and row['case']]
        assert sum(row['passed'] for row in cases) == 16
        assert sum('optimization_tests::ascent_' in row['case'] for row in cases) == 3
    binary = Path(profile['sealed_binary']['path'])
    assert digest(binary) == profile['sealed_binary']['sha256'] == profile['binary']['sha256']
    directory = LOG / 'runs' / args.label
    bundle = json.loads((directory / 'source-bundle.json').read_text())
    for path, expected in profile['source_sha256'].items():
        assert hashlib.sha256(bundle[path].encode()).hexdigest() == expected
    snapshot = json.loads((LOG / 'runs/ascent-control/manifest.json').read_text())
    assert snapshot['hypothesis-final'] == control['sealed_binary']
    assert digest(Path(control['sealed_binary']['path'])) == control['sealed_binary']['sha256']
    changed = [path for path, expected in snapshot['source_sha256'].items()
               if profile['source_sha256'][path] != expected]
    assert set(changed) <= {'experiments/typed-runtime/src/fused.rs',
                            'experiments/typed-runtime/src/optimization_tests.rs'}
    for row in profile['runs']:
        probe = row
        assert probe['panic'] == 'abort' and probe['phase_probe']
        assert not probe['root_diagnostics'] and not probe['root_verification'] and probe['work'] is None
        trace_arg = next(s for s in row['command'] if s.startswith('--callgrind-out-file='))
        trace_path = Path(trace_arg.split('=', 1)[1])
        assert digest(trace_path) == row['trace_sha256']
        assert int(re.search(r'^summary: (\d+)$', trace_path.read_text(), re.M)[1]) == row['instructions']
    costs, requirements = {}, {}
    for mode in ['full', 'topological']:
        for shape in ['chain', 'cut', 'diamond', 'fan_out']:
            for phase in ['set', 'flush']:
                before = control['fits'][mode][shape][phase]
                after = profile['fits'][mode][shape][phase]
                assert after['max_residual_instructions_per_write'] < 1e-6
                key = '-'.join([mode, shape, phase])
                costs[key] = {'control': before, 'variant': after,
                              'slope_delta': after['instructions_per_node'] - before['instructions_per_node']}
                if mode == 'topological':
                    limit = 240 if (shape, phase) == ('cut', 'flush') else before['instructions_per_node']
                    requirements[key] = after['instructions_per_node'] <= limit + 1e-6
    timing = load(args.label + '-time')
    assert timing['control'] == 'hypothesis-final'
    assert timing['profile'] == 'bench-abort' and timing['rustflags'] == '-C panic=abort'
    time_control = load('hypothesis-final-time')
    assert snapshot['hypothesis-final-time'] == time_control['binary']
    assert digest(Path(time_control['binary']['path'])) == time_control['binary']['sha256']
    assert timing['rustc'] == time_control['rustc']
    for path, expected in gate['source_sha256'].items():
        assert timing['source_sha256'][path] == expected
    assert digest(Path(timing['binary']['path'])) == timing['binary']['sha256']
    for row in timing['runs']:
        probe = row['probe']
        assert probe['panic'] == 'abort' and not probe['root_diagnostics'] and not probe['root_verification'] and probe['work'] is None
    result = {'label': args.label, 'analyzer_sha256': digest(Path(__file__)),
              'source_changes_from_control': changed, 'costs': costs,
              'instruction_requirements': requirements,
              'instruction_gate_passed': all(requirements.values()),
              'timing': timing['summary'],
              'timing_limits': 'Two ABBA blocks are descriptive, not a significance test; inspect consistency.',
              'current_source_matches_variant': all(digest(ROOT / p) == h for p, h in gate['source_sha256'].items())}
    if args.callsites:
        pc_data = load(args.callsites)
        assert pc_data['profile_manifest'] == args.label and pc_data['panic'] == 'abort'
        pc_binary = Path(pc_data['binary']['path'])
        assert digest(pc_binary) == pc_data['binary']['sha256'] == profile['sealed_binary']['sha256']
        assert pc_data['source_sha256'] == profile['source_sha256']
        assert pc_data['collector_sha256'] == digest(ROOT / 'benches/profile-callsites.py')
        attribution = module('final', 'benches/analyze-final-callsites.py')
        old = attribution.old
        traces, addresses = {}, set()
        for row in pc_data['runs']:
            assert row['mode'] == 'topological' and row['shape'] in ['cut', 'chain']
            assert '--toggle-collect=reactive_walk_flush' in row['command']
            assert row['probe']['panic'] == 'abort' and row['probe']['phase_probe']
            assert not row['probe']['root_diagnostics'] and not row['probe']['root_verification'] and row['probe']['work'] is None
            path = ROOT / row['trace']
            assert digest(path) == row['trace_sha256']
            trace = old.parse(path)
            assert sum(trace['own'].values()) == row['instructions']
            key = row['shape'], row['size'], row['iterations']
            assert key not in traces
            traces[key] = trace
            addresses.update(pc for (fn, obj, pc), cost in trace['pcs'].items()
                             if cost and trace['objects'][obj] == str(pc_binary))
        assert set(traces) == {(shape, n, it) for shape in ['cut', 'chain'] for n in [64, 256, 384] for it in [200, 400]}
        pc_dir = LOG / 'runs' / args.callsites
        stacks, text = old.stacks_for(pc_binary, addresses)
        (pc_dir / 'inline-stacks.log').write_text(text)
        assembly_text = subprocess.check_output(['objdump', '-d', '-C', '--no-show-raw-insn', str(pc_binary)], text=True)
        (pc_dir / 'disassembly.log').write_text(assembly_text)
        assembly = {}
        for line in assembly_text.splitlines():
            match = re.match(r'^\s*([0-9a-f]+):\s+(.*)$', line)
            if match:
                assembly[int(match[1], 16)] = match[2]
        counters = {}
        for key, trace in traces.items():
            counters[key] = collections.Counter()
            details = []
            for (fn, obj, pc), cost in trace['pcs'].items():
                if not cost:
                    continue
                name = old.canonical(trace['names'][fn])
                own_binary = trace['objects'][obj] == str(pc_binary)
                stack = stacks[pc] if own_binary else []
                site = attribution.classify(name, stack) if own_binary else 'external:' + name + '|[external]|0'
                counters[key][site] += cost
                if key[1:] == (256, 200):
                    details.append({'address': hex(pc), 'physical_function': name, 'Ir': cost,
                                    'Ir_per_write': cost / 200, 'callsite': site,
                                    'asm': assembly.get(pc) if own_binary else None, 'inline_stack': stack})
            assert sum(counters[key].values()) == sum(trace['own'].values())
            if details:
                (pc_dir / ('instructions-' + key[0] + '-256.json')).write_text(json.dumps(details, indent=2) + '\n')
        old_fits = load('final-callsite-analysis')['fits']['group']
        pc_result = {}
        for shape in ['cut', 'chain']:
            for n in [64, 256, 384]:
                a, b = counters[shape, n, 200], counters[shape, n, 400]
                assert all(b[site] == 2 * a[site] for site in a.keys() | b.keys())
            sites = set().union(*(counters[shape, n, 200] for n in [64, 256, 384]))
            fits = {site: old.fit([(n, counters[shape, n, 200][site] / 200) for n in [64, 256, 384]]) for site in sorted(sites)}
            assert all(fit['max_residual'] < 1e-6 for fit in fits.values())
            groups = collections.Counter()
            for site, fit in fits.items():
                groups[site.split('|')[0]] += fit['slope']
            assert abs(sum(groups.values()) - profile['fits']['topological'][shape]['flush']['instructions_per_node']) < 1e-6
            before = old_fits['topological-' + shape + '-flush']
            delta = {name: groups[name] - before.get(name, {}).get('slope', 0) for name in sorted(groups.keys() | before.keys())}
            pc_result[shape] = {'callsites': fits, 'group_slopes': dict(groups), 'group_deltas': delta}
        result['pc_attribution'] = pc_result
        result['pc_artifacts'] = {str(p.relative_to(ROOT)): digest(p) for p in pc_dir.iterdir()
                                  if p.name in ['inline-stacks.log', 'disassembly.log'] or p.name.startswith('instructions-')}
    (LOG / (args.label + '-analysis.json')).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({'instruction_gate_passed': result['instruction_gate_passed'],
                      'instruction_requirements': requirements,
                      'flush_slopes': {k: v['variant']['instructions_per_node'] for k, v in costs.items() if k.endswith('flush')},
                      'paired_ratios': {k: v['paired_ratio'] for k, v in timing['summary'].items()}}, indent=2))


if __name__ == '__main__':
    main()
