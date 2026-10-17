#!/usr/bin/env python3
"""Verify selectable-evaluator timing, memory, work and directed validation."""
import hashlib
import json
from pathlib import Path
import re

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
def load(label):return json.loads((LOG/(label+'.json')).read_text())
def sha(path):return hashlib.sha256(path.read_bytes()).hexdigest()
def main():
    timing=load('evaluation-v1-balanced');sham=load('evaluation-v1-sham')
    assert timing['source_sha256']==sham['source_sha256']
    assert timing['binary']==sham['binary']
    assert timing['schedule']==sham['schedule']==list('flttlf')*2
    for p,h in timing['source_sha256'].items():assert sha(ROOT/p)==h,p
    before=load('typed-opt-v3-source')['measured_source_sha256']
    for p,h in before.items():
        if p.startswith('src/') or p=='experiments/typed-runtime/src/evaluation.rs':
            assert timing['source_sha256'][p]==h,p
    for data in [timing,sham]:
        assert len(data['runs'])==12
        assert [r['variant'] for r in data['runs']]==data['schedule']
        assert sha(Path(data['binary']['path']))==data['binary']['sha256']
    miri=[];memory={};work={};validation=[]
    for mode in ['full','light','topological']:
        run=load('evaluation-v1-miri-'+mode)
        assert run['evaluation']==mode and run['exit_code']==0 and run['sources_unchanged'] and not run['timed_out']
        assert run['miriflags']=='-Zmiri-env-forward=TYPED_EVALUATION'
        assert re.search(r'test result: ok\. 80 passed;',run['stdout']),mode
        for p,h in run['source_sha256'].items():assert sha(ROOT/p)==h,p
        miri.append({'mode':mode,'unit_tests':80,'manifest_sha256':sha(LOG/('evaluation-v1-miri-'+mode+'.json'))})
        for features,counts in [('all',[246,0,2,13,15,9,10,157,8,12,0]),('default',[240,0,2,13,15,9,10,152,8,12,0])]:
            p=LOG/'runs/evaluation-v1-validation'/f'{mode}-{features}.log';text=p.read_text()
            assert list(map(int,re.findall(r'test result: ok\. (\d+) passed;',text)))==counts
            validation.append({'mode':mode,'features':features,'sha256':sha(p),'passed_counts':counts})
        if mode!='full':
            p=LOG/'runs/evaluation-v1-validation'/f'miri-{mode}-integrations.log'
            assert list(map(int,re.findall(r'test result: ok\. (\d+) passed;',p.read_text())))==[2,13,9,10]
            validation.append({'mode':mode,'check':'normal-build Miri integrations','sha256':sha(p),'passed_counts':[2,13,9,10]})
        mem=load(f'evaluation-v1-{mode}-memory');w=load(f'evaluation-v1-{mode}-work')
        for data in [mem,w]:
            assert data['evaluation']==mode
            assert all(timing['source_sha256'][p]==h for p,h in data['source_sha256'].items())
        memory[mode]=mem['metrics'];work[mode]=w['metrics']
        for row in mem['metrics']['rows']:
            if row['engine'] in ['reactive','typed_research']:
                assert (row['steady_allocations'],row['steady_reallocations'],row['after_drop_net_bytes'])==(0,0,0)
        assert all(row['after_drop_net_bytes']==0 for row in mem['metrics']['owner_resources'])
    for mode in ['light','topological']:
        for row in work[mode]['rows']:
            ref=next(r for r in work['full']['rows'] if (r['engine'],r['shape'])==(row['engine'],row['shape']))
            assert row['work']==ref['work']
    clippy=LOG/'runs/evaluation-v1-validation/clippy.log'
    assert 'Finished' in clippy.read_text() and 'error:' not in clippy.read_text()
    validation.append({'check':'Clippy workspace all-targets all-features -D warnings','sha256':sha(clippy)})
    result={'source_sha256':timing['source_sha256'],'binary':timing['binary'],'timing':timing['summary'],'sham':sham['summary'],
            'memory':memory,'work':work,'miri':miri,'validation':validation,
            'limits':['Experimental only; default remains FullFrames','Directed Miri excludes production ports and three large conformance cases',
                      'Full randomized 32x1000 model passed native in every mode, not final-source Miri',
                      'No independent unsafe review or promotion','Same logical graph work, different protocol/kernel batching',
                      'No new reference-engine paired timings; historical timing is not causal evidence']}
    (LOG/'evaluation-v1-analysis.json').write_text(json.dumps(result,indent=2)+'\n')
    for key,row in result['timing'].items():
        print(key,{m:round(ns/1000,3) for m,ns in row['means_ns'].items()},'light/full',round(row['light_paired_percent'],2),'topological/full',round(row['topological_paired_percent'],2))
    print('Verified 240 directed Miri unit executions, 68 normal-build Miri integration executions and six native workspace suites')
if __name__=='__main__':main()
