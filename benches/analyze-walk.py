#!/usr/bin/env python3
"""Verify the pre-change walk experiment and print costs, not causal claims."""
import argparse
import hashlib
import json
from pathlib import Path
import statistics

ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    parser.add_argument('--abort-label')
    args = parser.parse_args()
    path = ROOT / 'benches/.log' / (args.label + '.json')
    result = json.loads(path.read_text())
    assert all(digest(ROOT / p) == h for p, h in result['source_sha256'].items())
    for build in result['builds'].values():
        assert digest(Path(build['path'])) == build['sha256']
        for artifact in build['artifacts']:
            name = artifact.get('target', {}).get('name')
            if name in ['reactive', 'typed_runtime_prototype']:
                assert artifact.get('features', []) == [], (name, artifact.get('features'))
    assert len(result['schedule']) == 48 and len(result['runs']) == 720
    for row in result['runs']:
        assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
        assert row['iterations'] == result['iterations']
    print('Actual panic strategies:', {profile: sorted({r['panic'] for r in result['runs'] if r['profile'] == profile})
                                       for profile in result['builds']})
    print('Release means, microseconds/write:')
    for mode, row in result['summary']['release'].items():
        print(mode, {shape: round(row['means_ns'][f'{shape}/256']/1000, 3)
                     for shape in ['chain', 'cut', 'diamond', 'fan_out', 'conditional']})
        print('  fits ns/node, intercept ns:', {shape: (round(fit['ns_per_node'], 2), round(fit['intercept_ns'], 2))
                                               for shape, fit in row['fits'].items()})
    pooled = {}
    if len({b['sha256'] for b in result['builds'].values()}) == 1:
        for mode in result['summary']['release']:
            pooled[mode] = {}
            for shape in ['chain', 'cut', 'diamond', 'fan_out', 'conditional']:
                rows = [r for r in result['runs'] if r['mode'] == mode and r['shape'] == shape]
                sizes = sorted({r['size'] for r in rows})
                means = {n: statistics.mean(r['ns_per_write'] for r in rows if r['size'] == n) for n in sizes}
                slope, intercept = statistics.linear_regression(sizes, [means[n] for n in sizes])
                samples = [r['ns_per_write'] for r in rows if r['size'] == 256]
                assert len(samples) == 8
                pooled[mode][shape] = dict(means_ns=means, ns_per_node=slope, intercept_ns=intercept,
                    residuals_ns={n: means[n]-(slope*n+intercept) for n in sizes},
                    samples_256_ns=samples, range_256_percent=100*(max(samples)-min(samples))/statistics.mean(samples))
    counters = {}
    for run in result['counters']:
        assert run['exit_code'] == 0, run['stderr']
        parsed = {}
        for line in run['perf'].splitlines():
            fields = line.split(';')
            if len(fields) > 2 and fields[2].endswith(':u'):
                parsed[fields[2]] = float(fields[0].replace(',', ''))
        counters[(run['mode'], run['shape'], run['iterations'])] = parsed
    rates = {}
    if counters:
        assert len(counters) == 24
        for mode in result['summary']['release']:
            rates[mode] = {}
            for shape in ['chain', 'cut']:
                before, after = [counters[(mode, shape, n)] for n in [2000, 6000]]
                rates[mode][shape] = {event: (after[event]-before[event])/(4000*256) for event in after}
                assert rates[mode][shape]['instructions:u'] > 0
        print('Differential counters/node:', json.dumps(rates, indent=2))
    abort = None
    if args.abort_label:
        abort = json.loads(path.with_name(args.abort_label + '.json').read_text())
        assert abort['source_sha256'] == result['source_sha256']
        assert abort['binaries']['unwind'] == result['builds']['release']
        assert len(abort['runs']) == 64 and abort['rustflags'] == '-C panic=abort'
        for build in abort['binaries'].values():
            assert digest(Path(build['path'])) == build['sha256']
        for row in abort['runs']:
            assert row['panic'] == row['profile']
            assert not row['root_diagnostics'] and not row['root_verification'] and row['work'] is None
        for artifact in abort['build_artifacts']:
            if artifact.get('target', {}).get('name') in ['reactive', 'typed_runtime_prototype']:
                assert artifact.get('features', []) == []
        build_log = ROOT / 'benches/.log/runs' / args.abort_label / 'build.log'
        for target in ['typed_runtime_prototype', 'walk_diagnostic']:
            assert any(f'--crate-name {target} ' in line and '-C panic=abort' in line
                       for line in build_log.read_text().splitlines())
        print('Verified abort/unwind means:', json.dumps(abort['means_ns'], indent=2))
    output = path.with_name(args.label + '-analysis.json')
    output.write_text(json.dumps(dict(summary=result['summary'], pooled_identical_binary=pooled,
                                      abort=abort, counters_per_node=rates,
                                      analysis_driver_sha256=digest(Path(__file__)),
                                      source_sha256=result['source_sha256'], limits=[
        'Fixed-work, four observations per profile/mode/size, not Criterion confidence intervals',
        'Chain depth cap requires 384 instead of 1024; shallow fixtures include 1024',
        'Counter deltas subtract process setup approximately; one pair, not repeated confidence bounds',
        'FullFrames first Chain/256 observation is an outlier; small Chain differences are not established gains',
        'No runtime optimization or causal ablation in this checkpoint']), indent=2) + '\n')
    print('Verified', output)


if __name__ == '__main__':
    main()
