#!/usr/bin/env python3
"""Verify executable shared-plan evidence without borrowing historical timings."""
import hashlib
import json
from pathlib import Path
import re

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
def load(label):return json.loads((LOG/(label+'.json')).read_text())
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def counts(text):return list(map(int,re.findall(r'test result: ok\. (\d+) passed;',text)))
def main():
    timing=load('shared-plan-v1-balanced');sham=load('shared-plan-v1-sham')
    assert timing['source_sha256']==sham['source_sha256'] and timing['binary']==sham['binary']
    assert timing['modes']==sham['modes']=={'f':'full','l':'shared','t':'shared-dirty'}
    assert timing['schedule']==sham['schedule']==list('flttlf')*2
    for path,digest in timing['source_sha256'].items():assert sha(ROOT/path)==digest,path
    assert sha(Path(timing['binary']['path']))==timing['binary']['sha256']
    for run in [timing,sham]:
        assert len(run['runs'])==12 and [r['variant'] for r in run['runs']]==run['schedule']
        assert all(r['evaluation']==('full' if run['sham'] else run['modes'][r['variant']]) for r in run['runs'])
    old=load('topology-shadow-v1-source')['source_sha256']
    for path,digest in old.items():
        if path.startswith('src/') or path=='experiments/typed-runtime/src/evaluation.rs':assert sha(ROOT/path)==digest,path
    validation=[];miri=[];memory={};work={}
    for mode in ['full','light','topological','shared','shared-dirty']:
        path=LOG/'runs/shared-plan-v1-validation'/f'{mode}-all.log'
        assert counts(path.read_text())==[246,0,2,13,15,9,10,175,2,8,12,0]
        validation.append({'mode':mode,'features':'all','sha256':sha(path)})
    for mode in ['full','shared','shared-dirty']:
        path=LOG/'runs/shared-plan-v1-validation'/f'{mode}-default.log'
        assert counts(path.read_text())==[240,0,2,13,15,9,10,158,0,8,12,0]
        validation.append({'mode':mode,'features':'default','sha256':sha(path)})
        mem=load(f'shared-plan-v1-{mode}-memory');w=load(f'shared-plan-v1-{mode}-work')
        for run in [mem,w]:
            assert run['evaluation']==mode
            for path,digest in run['source_sha256'].items():assert timing['source_sha256'][path]==digest,path
        memory[mode]=mem['metrics'];work[mode]=w['metrics']
        for row in mem['metrics']['rows']:
            if row['engine'] in ['reactive','typed_research']:assert (row['steady_allocations'],row['steady_reallocations'],row['after_drop_net_bytes'])==(0,0,0)
        assert all(r['after_drop_net_bytes']==0 for r in mem['metrics']['owner_resources'])
    for mode,label in [('shared','shared'),('shared-dirty','dirty')]:
        run=load('shared-plan-v1-miri-'+label)
        assert run['evaluation']==mode and run['exit_code']==0 and run['sources_unchanged'] and not run['timed_out']
        assert run['miriflags']=='-Zmiri-env-forward=TYPED_EVALUATION'
        assert counts(run['stdout'])==[98,2]
        for path,digest in run['source_sha256'].items():assert sha(ROOT/path)==digest,path
        miri.append({'mode':mode,'counts':[98,2],'manifest_sha256':sha(LOG/('shared-plan-v1-miri-'+label+'.json'))})
        path=LOG/'runs/shared-plan-v1-validation'/f'miri-{label}-integrations.log'
        assert counts(path.read_text())==[2,13,9,10]
        miri.append({'mode':mode,'normal_integration_counts':[2,13,9,10],'sha256':sha(path)})
    for mode in ['shared','shared-dirty']:
        for row in work[mode]['rows']:
            control=next(c for c in work['full']['rows'] if (c['shape'],c['engine'])==(row['shape'],row['engine']))
            assert row['work']==control['work']
    probe=load('shared-plan-v1-two-effects')
    assert probe['rows']==[{'mode':'full','source_checks':2,'memo_executions':1,'effect_executions':2},{'mode':'shared','source_checks':2,'memo_executions':1,'effect_executions':2},{'mode':'shared-dirty','source_checks':1,'memo_executions':1,'effect_executions':2}]
    clippy=LOG/'runs/shared-plan-v1-validation/clippy.log';assert 'Finished' in clippy.read_text() and 'error:' not in clippy.read_text()
    result={'source_sha256':timing['source_sha256'],'binary':timing['binary'],'timing':timing['summary'],'sham':sham['summary'],'memory':memory,'work':work,'miri':miri,'native':validation,'clippy_sha256':sha(clippy),'two_effects_probe':probe,
            'limits':['Opt-in only; FullFrames remains default','Transitive invalidation deliberately retained; not immediate-only Sycamore propagation','Directed Miri excludes 74 direct ports and three large conformance cases','Full 32x1000 model passed native in all five modes','No new paired reference-engine timing or independent unsafe review']}
    (LOG/'shared-plan-v1-analysis.json').write_text(json.dumps(result,indent=2)+'\n')
    for name,row in result['timing'].items():print(name,{m:round(n/1000,3) for m,n in row['means_ns'].items()},'shared/full',round(row['shared_paired_percent'],2),'dirty/full',round(row['shared-dirty_paired_percent'],2))
    print('Verified 196 directed Miri unit executions, 4 package integration executions and 68 root normal-build integrations')
if __name__=='__main__':main()
