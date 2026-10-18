#!/usr/bin/env python3
"""Bounded balanced full/topological comparison, one effective-abort executable."""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import statistics
import subprocess

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
spec=importlib.util.spec_from_file_location('runner',ROOT/'benches/run-p1p2.py')
runner=importlib.util.module_from_spec(spec);spec.loader.exec_module(runner)

def main():
    p=argparse.ArgumentParser(__doc__);p.add_argument('label');p.add_argument('--cpu',type=int,default=2)
    p.add_argument('--sham',action='store_true',help='Run FullFrames under both labels to measure noise')
    a=p.parse_args();assert a.label and all(c.isalnum() or c in '-_' for c in a.label)
    directory=LOG/'runs'/a.label;directory.mkdir(parents=True,exist_ok=False)
    env=dict(os.environ,CARGO_TARGET_DIR='/tmp/opencode/reactive-p1p2-lab-target',REACTIVE_TYPED_PROTOTYPE='1')
    source={str(p.relative_to(ROOT)):runner.digest(p) for pattern in ['src/**/*.rs','experiments/**/*.rs','benches/**/*.rs','tests/p1p2*.rs','tests/p1p2_support/**/*.rs','Cargo.toml','Cargo.lock','experiments/**/Cargo.toml','benches/compare-evaluation.py','benches/run-p1p2.py'] for p in ROOT.glob(pattern)}
    runner.lifecycle.STEP_TIMEOUT=240
    binary,identity=runner.build('comparison',directory,a.label,env)
    result={'label':a.label,'sham':a.sham,'binary':identity,'source_sha256':source,'cpu':a.cpu,'rustc':subprocess.check_output(['rustc','-Vv'],text=True),'runs':[],'schedule':list('fttf')*3,'limitations':['CPU-pinned balanced same-executable timing; not independent safety review','All policies retain production semantics; only typed engine timed','Historical reference times are not paired against this experiment']}
    pattern=r'((chain|diamond|fan_out|conditional)_write_to_observed|p1p2_cut)/typed_research/256$|contract_(owner_resources|writable_effects)/typed_research/8$'
    modes={'f':'full','t':'topological'}
    result['panic']='abort';result['rustflags']=env.get('RUSTFLAGS');result['encoded_rustflags']=env.get('CARGO_ENCODED_RUSTFLAGS')
    result['modes']=modes
    for mode in modes.values():
        env['TYPED_EVALUATION']=mode
        with (directory/f'smoke-{mode}.log').open('w') as out:
            runner.lifecycle.run_step(['taskset','-c',str(a.cpu),str(binary),'--test'],cwd=ROOT,env=env,stdout=out,stderr=subprocess.STDOUT,check=True)
    for i,variant in enumerate(result['schedule']):
        block=directory/f'{i+1}-{variant}';block.mkdir();env['TYPED_EVALUATION']=modes['f'] if a.sham else modes[variant]
        env['REACTIVE_BENCH_LOG']=str(block/'criterion')
        assert runner.digest(binary)==identity['sha256']
        assert all(runner.digest(ROOT/p)==h for p,h in source.items())
        command=['taskset','-c',str(a.cpu),str(binary),'--bench','--noplot','--sample-size','30','--warm-up-time','0.3','--measurement-time','0.7',pattern]
        print(f'{a.label}: block {i+1}/12 {env["TYPED_EVALUATION"]}',flush=True)
        with (block/'stdout.log').open('w') as out:
            runner.lifecycle.run_step(command,cwd=ROOT,env=env,stdout=out,stderr=subprocess.STDOUT,check=True)
        estimates={str(p.parent.parent.relative_to(block/'criterion')):json.loads(p.read_text()) for p in (block/'criterion').glob('**/new/estimates.json')}
        assert len(estimates)==7,estimates.keys()
        result['runs'].append({'variant':variant,'evaluation':env['TYPED_EVALUATION'],'command':command,'estimates':estimates})
        (LOG/(a.label+'.json')).write_text(json.dumps(result,indent=2)+'\n')
    summary={}
    for key in result['runs'][0]['estimates']:
        def point(i):
            d=result['runs'][i]['estimates'][key];return (d.get('slope') or d['mean'])['point_estimate']
        row={'means_ns':{m:statistics.mean(point(i) for i,r in enumerate(result['runs']) if r['variant']==v) for v,m in modes.items()}}
        for mode,pair in [(modes['t'],(1,2))]:
            ratios=[math.sqrt(point(start+pair[0])*point(start+pair[1])/point(start)/point(start+3)) for start in [0,4,8]]
            row[mode+'_cycle_percent']=[100*(r-1) for r in ratios]
            row[mode+'_paired_percent']=100*(math.prod(ratios)**(1/len(ratios))-1)
        summary[key]=row
    result['summary']=summary
    assert runner.digest(binary)==identity['sha256']
    assert all(runner.digest(ROOT/p)==h for p,h in source.items())
    (LOG/(a.label+'.json')).write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(summary,indent=2),flush=True)
if __name__=='__main__':main()
