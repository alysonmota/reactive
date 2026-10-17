#!/usr/bin/env python3
"""Bounded, selectable Miri jobs with source hashes and explicit model shards."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
CASES={
 'api':['api_tests::'], 'audit':['audit_tests::'],
 'sources':['conformance::individual_source_disposals_keep_indices_release_values_and_reconcile_once','--','--exact'],
 'reuse':['conformance::repeated_create_dispose_without_flush_reuses_slots_and_drops_all_captures','--','--exact'],
 'model':['conformance::randomized_reads_writes_branches_flush_and_disposal_match_reference_model','--','--exact','--nocapture'],
 'conformance':['conformance::','--','--skip','individual_source_disposals_keep_indices_release_values_and_reconcile_once','--skip','repeated_create_dispose_without_flush_reuses_slots_and_drops_all_captures','--skip','randomized_reads_writes_branches_flush_and_disposal_match_reference_model'],
 'production':['production_ports::'],
}
def hashes():
    paths=list((ROOT/'experiments/typed-runtime').rglob('*.rs'))+[ROOT/'experiments/typed-runtime/Cargo.toml',ROOT/'Cargo.toml',ROOT/'Cargo.lock',Path(__file__)]
    return {str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}
def main():
    p=argparse.ArgumentParser(__doc__);p.add_argument('label');p.add_argument('--case',choices=CASES,required=True);p.add_argument('--cpu',type=int,default=3);p.add_argument('--timeout',type=int,default=600);p.add_argument('--seed-start',type=int,default=1);p.add_argument('--seed-end',type=int,default=32);a=p.parse_args()
    assert a.label and all(c.isalnum() or c in '-_' for c in a.label)
    assert 1<=a.seed_start<=a.seed_end<=32 and a.timeout>0
    directory=LOG/'runs'/a.label;directory.mkdir(parents=True,exist_ok=False)
    env=dict(os.environ);env.pop('MIRIFLAGS',None)
    # Miri isolation hides host environment variables unless explicitly
    # forwarded. Check actual seed progress, not merely the requested range.
    flags='-Zmiri-env-forward=TYPED_MODEL_SEED_START -Zmiri-env-forward=TYPED_MODEL_SEED_END'
    if a.case=='model':env['MIRIFLAGS']=flags
    env['TYPED_MODEL_SEED_START']=str(a.seed_start);env['TYPED_MODEL_SEED_END']=str(a.seed_end)
    command=['taskset','-c',str(a.cpu),'cargo','+nightly-2026-08-29','miri','test','--locked','--profile','fast-dev','-p','typed-runtime-prototype','--all-features']+CASES[a.case]
    before=hashes();start=time.monotonic()
    print(f'{a.label}: {a.case}, cpu={a.cpu}, timeout={a.timeout}s, seeds={a.seed_start}..{a.seed_end}',flush=True)
    timed_out=False
    with (directory/'stdout.log').open('w') as out:
        proc=subprocess.Popen(command,cwd=ROOT,env=env,stdout=out,stderr=subprocess.STDOUT,start_new_session=True)
        try:code=proc.wait(timeout=a.timeout)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid,signal.SIGKILL);proc.wait();code=124;timed_out=True
    import re
    text=(directory/'stdout.log').read_text();unchanged=before==hashes()
    actual=[int(n) for n in re.findall(r'model seed (\d+)/32: 1000 operations',text)]
    if code==0 and a.case=='model' and actual!=list(range(a.seed_start,a.seed_end+1)):code=126
    result={'label':a.label,'case':a.case,'command':command,'cpu':a.cpu,'seed_range':[a.seed_start,a.seed_end] if a.case=='model' else None,'observed_seeds':actual,'timeout_seconds':a.timeout,'elapsed_seconds':time.monotonic()-start,'exit_code':code,'timed_out':timed_out,'sources_unchanged':unchanged,'source_sha256':before,'miriflags':env.get('MIRIFLAGS'),'stdout':text}
    (LOG/(a.label+'.json')).write_text(json.dumps(result,indent=2)+'\n')
    print(f'{a.label}: exit={code}, unchanged={unchanged}, elapsed={result["elapsed_seconds"]:.1f}s',flush=True)
    raise SystemExit(code if unchanged else 125)
if __name__=='__main__':main()
