#!/usr/bin/env python3
"""Source-matched native and directed Miri gates BEFORE kernel measurements."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / 'benches/.log'
CASES = [
    'api_tests::disposal_and_slot_reuse_during_edit_never_access_old_cell',
    'audit_tests::provisional_reads_survive_self_disposal_growth_and_slot_type_reuse',
    'audit_tests::failed_cleanup_leaves_no_execution_owner_backlink_or_resurrected_callback',
    'audit_tests::fault_injected_stale_memo_read_and_clean_source_overflow_do_not_publish',
    'audit_tests::memo_revision_overflow_keeps_current_slot_and_drops_only_candidate',
    'audit_tests::reconcile_updated_prefix_then_divergent_tail_repairs_backlinks',
    'optimization_tests::committed_chain_reuses_iterative_frames_and_respects_logical_depth_limit',
    'audit_tests::nested_dirty_memo_restores_phase_after_success_error_unwind_and_cleanup_failure',
    'audit_tests::reconcile_rechecks_source_freshness_after_comparator_fault_injection',
    'fused::tests::',
]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('label')
    args = parser.parse_args()
    assert re.fullmatch(r'[\w-]+', args.label)
    directory = LOG / 'runs' / (args.label + '-validation')
    directory.mkdir(parents=True, exist_ok=False)
    paths = [p for pattern in ['src/**/*.rs', 'experiments/**/*.rs', 'benches/**/*.rs',
                               'Cargo.toml', 'Cargo.lock', 'experiments/**/Cargo.toml'] for p in ROOT.glob(pattern)]
    hashes = {str(p.relative_to(ROOT)): digest(p) for p in paths}
    result = {'source_sha256': hashes, 'runs': [], 'complete': False}
    destination = LOG / (args.label + '-validation.json')
    for mode in ['full', 'topological']:
        env = dict(os.environ, TYPED_EVALUATION=mode)
        commands = [('native', ['cargo', 'test', '--locked', '--profile', 'fast-dev',
                                '--workspace', '--all-features', '--quiet'], None)]
        for i, case in enumerate(CASES):
            command = ['cargo', '+nightly-2026-08-29', 'miri', 'test', '--locked', '--profile',
                       'fast-dev', '-p', 'typed-runtime-prototype', '--all-features', case]
            if not case.endswith('::'):
                command += ['--', '--exact']
            commands.append((f'miri-{i}', command, case))
        for name, command, case in commands:
            path = directory / f'{mode}-{name}.log'
            with path.open('w') as output:
                run = subprocess.run(command, cwd=ROOT, env=env, stdout=output,
                                     stderr=subprocess.STDOUT, timeout=600)
            counts = list(map(int, re.findall(r'test result: ok\. (\d+) passed;', path.read_text())))
            row = {'mode': mode, 'case': case, 'command': command, 'exit_code': run.returncode,
                   'passed': sum(counts), 'log': str(path.relative_to(ROOT)), 'sha256': digest(path)}
            result['runs'].append(row)
            destination.write_text(json.dumps(result, indent=2) + '\n')
            assert run.returncode == 0 and sum(counts) > 0, row
            if case:
                assert sum(counts) == (4 if case.endswith('::') else 1), row
            print(mode, name, sum(counts), 'passed', flush=True)
    assert all(digest(ROOT / p) == h for p, h in hashes.items())
    result['complete'] = True
    destination.write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
