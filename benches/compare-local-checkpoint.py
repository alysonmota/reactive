#!/usr/bin/env python3
"""Balanced sealed-before/current-after release comparison; no source mixing."""
import hashlib
import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import shutil

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
spec=importlib.util.spec_from_file_location('runner',ROOT/'benches/run-p1p2.py')
runner=importlib.util.module_from_spec(spec);spec.loader.exec_module(runner)

def main():
    parser=argparse.ArgumentParser(__doc__)
    parser.add_argument('--label',required=True)
    parser.add_argument('--baseline',type=Path,required=True)
    parser.add_argument('--modes',nargs='+',default=['full','topological'])
    args=parser.parse_args()
    assert args.label and all(c.isalnum() or c in '-_' for c in args.label)
    directory=LOG/'runs'/args.label;directory.mkdir(parents=True,exist_ok=False)
    baseline=json.loads(args.baseline.read_text())
    old_identity=baseline.get('candidate') or baseline['binary']
    old=Path(old_identity['path']);assert runner.digest(old)==old_identity['sha256']
    assert baseline.get('panic')=='abort','Only effective-abort baselines may be compared'
    env=dict(os.environ,CARGO_TARGET_DIR='/tmp/opencode/reactive-p1p2-lab-target',REACTIVE_TYPED_PROTOTYPE='1')
    runner.lifecycle.STEP_TIMEOUT=240
    new,identity=runner.build('comparison',directory,args.label,env)
    sealed=directory/'sealed-binary';shutil.copyfile(new,sealed);sealed.chmod(0o755)
    new=sealed;identity={'path':str(sealed.resolve()),'sha256':runner.digest(sealed)}
    source={str(p.relative_to(ROOT)):runner.digest(p) for pattern in ['src/**/*.rs','experiments/**/*.rs','benches/**/*.rs','tests/**/*.rs','Cargo.toml','Cargo.lock','experiments/**/Cargo.toml','benches/compare-local-checkpoint.py'] for p in ROOT.glob(pattern)}
    (directory/'source-bundle.json').write_text(json.dumps({p:(ROOT/p).read_text() for p in source}))
    result={'baseline':baseline,'candidate':identity,'source_sha256':source,'panic':'abort','rustflags':env.get('RUSTFLAGS'),'cpu':2,'schedule':list('abba')*2,'runs':{},'summary':{},'rustc':subprocess.check_output(['rustc','-Vv'],text=True),'limits':['Different sealed executables; paired before/after bundle, not per-function cost','Release without diagnostics; CPU pinned, not physical isolation','No reference-engine timing']}
    pattern=r'((chain|diamond|fan_out|conditional)_write_to_observed|p1p2_cut)/typed_research/256$|contract_(owner_resources|writable_effects)/typed_research/8$'
    for mode in args.modes:
        env['TYPED_EVALUATION']=mode;runs=[]
        for i,variant in enumerate(result['schedule']):
            block=directory/f'{mode}-{i+1}-{variant}';block.mkdir()
            env['REACTIVE_BENCH_LOG']=str(block/'criterion')
            executable=old if variant=='a' else new
            command=['taskset','-c','2',str(executable),'--bench','--noplot','--sample-size','30','--warm-up-time','0.3','--measurement-time','0.7',pattern]
            print(mode,i+1,variant,flush=True)
            with (block/'stdout.log').open('w') as out:
                runner.lifecycle.run_step(command,cwd=ROOT,env=env,stdout=out,stderr=subprocess.STDOUT,check=True)
            estimates={str(p.parent.parent.relative_to(block/'criterion')):json.loads(p.read_text()) for p in (block/'criterion').glob('**/new/estimates.json')}
            assert len(estimates)==7
            runs.append({'variant':variant,'command':command,'estimates':estimates})
            assert runner.digest(old)==old_identity['sha256'] and runner.digest(new)==identity['sha256']
            assert all(runner.digest(ROOT/p)==h for p,h in source.items())
        result['runs'][mode]=runs
        summary={}
        for fixture in runs[0]['estimates']:
            def point(i):
                e=runs[i]['estimates'][fixture];return (e.get('slope') or e['mean'])['point_estimate']
            ratios=[math.sqrt(point(s+1)*point(s+2)/point(s)/point(s+3)) for s in [0,4]]
            summary[fixture]={'before_ns':statistics.mean(point(i) for i in [0,3,4,7]),'after_ns':statistics.mean(point(i) for i in [1,2,5,6]),'cycle_percent':[100*(r-1) for r in ratios],'paired_percent':100*(math.sqrt(math.prod(ratios))-1)}
        result['summary'][mode]=summary
        (LOG/(args.label+'.json')).write_text(json.dumps(result,indent=2)+'\n')
        print(mode,json.dumps(summary,indent=2),flush=True)

if __name__=='__main__':main()
