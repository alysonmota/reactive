#!/usr/bin/env python3
"""Validate audit-v3 provenance and consolidate independent timing/alloc/work."""
import argparse
import hashlib
import json
import re
from pathlib import Path

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
def read(label):return json.loads((LOG/(label+'.json')).read_text())
def sha(path):return hashlib.sha256(path.read_bytes()).hexdigest()

def main():
    p=argparse.ArgumentParser(__doc__);p.add_argument('--prefix',default='p1p2-audit-v3-safe');p.add_argument('--miri-prefix',default='p1p2-audit-v3');p.add_argument('--allow-incomplete-miri',action='store_true');a=p.parse_args()
    final,memory,work,sham=[read(a.prefix+'-'+s) for s in ['final','memory','work','sham']]
    for run in [memory,work,sham]:assert run['source_sha256']==final['source_sha256'],run['label']
    for path,h in final['source_sha256'].items():assert sha(ROOT/path)==h,path
    for path,h in read('p1p2-baseline')['source_sha256'].items():
        if path.startswith('src/'):assert final['source_sha256'][path]==h,path
    assert final['binary']==sham['binary']
    for run in [final,sham]:assert [r['variant'] for r in run['runs']]==list('abbaabba')
    rows=[]
    for shape in ['chain','diamond','fan_out','conditional']:
        def row(run,engine):return next(r for r in run['metrics']['rows'] if r['engine']==engine and r['shape']==shape)
        prod,typed=[row(memory,e) for e in ['reactive','typed_research']]
        pw,tw=[row(work,e)['work'] for e in ['reactive','typed_research']]
        for r in [prod,typed]:assert (r['steady_allocations'],r['steady_reallocations'],r['after_drop_net_bytes'])==(0,0,0)
        for f in ['memo_executions','effect_executions','source_checks']:assert pw[f]==tw[f]
        assert prod['effect_calls_including_warmup']==typed['effect_calls_including_warmup']
        rows.append({'shape':shape,'production_bytes':prod['live_requested_bytes'],'typed_bytes':typed['live_requested_bytes'],'memory_percent':100*(typed['live_requested_bytes']/prod['live_requested_bytes']-1),'production_work':pw,'typed_work':tw})
    for r in memory['metrics']['owner_resources']:assert r['after_drop_net_bytes']==0 and r['cleanups']==1160
    miri=[];missing=[];retries=[]
    expected=['api','audit','conformance','sources','reuse']+[f'model-corrected-{s:02}-{s+3:02}' for s in range(1,33,4)]
    attestation=read('p1p2-audit-v3-miri-guard-attestation')
    frozen=read('p1p2-audit-v3-pre-guard-source')
    # Recompute the exact cfg-only attestation, never trust its JSON alone.
    old=frozen['sources'][attestation['file']];current=(ROOT/attestation['file']).read_text();attribute='#[cfg(any(test, feature = "verification"))]'
    lines=lambda s:[l.strip() for l in s.splitlines() if l.strip()]
    assert old.count(attribute)==3 and attribute not in current
    assert lines(old.replace(attribute,''))==lines(current)
    assert hashlib.sha256(old.encode()).hexdigest()==attestation['old_sha256']
    assert sha(ROOT/attestation['file'])==attestation['new_sha256']
    seeds=[]
    for suffix in expected:
        path=LOG/f'{a.miri_prefix}-miri-{suffix}.json'
        if not path.exists():missing.append(suffix);continue
        original=json.loads(path.read_text())
        retry=path.with_stem(path.stem+'-retry')
        if original['exit_code']!=0 and retry.exists():
            retries.append({'label':original['label'],'exit_code':original['exit_code'],'elapsed_seconds':original['elapsed_seconds'],'metadata_sha256':sha(path),'retry_label':retry.stem})
            path=retry
        result=json.loads(path.read_text());passed=result['exit_code']==0 and result['sources_unchanged'] and not result['timed_out']
        expected_flags='-Zmiri-env-forward=TYPED_MODEL_SEED_START -Zmiri-env-forward=TYPED_MODEL_SEED_END' if result['case']=='model' else None
        assert result['miriflags']==expected_flags
        for path,h in result['source_sha256'].items():
            if sha(ROOT/path)==h:continue
            if path==attestation['file']:
                assert h==attestation['old_sha256']
            elif path=='benches/run-typed-miri.py':
                # Earlier successful non-sharded runs used the snapshotted
                # runner, before adding explicit environment forwarding.
                assert result['case']!='model' and h==frozen['source_sha256'][path]
            else:raise AssertionError(path)
        if not passed:missing.append(suffix)
        count=sum(int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',result['stdout']))
        if passed:
            assert count=={'api':19,'audit':32,'conformance':13,'sources':1,'reuse':1,'model':1}[result['case']]
        if passed and result['case']=='model':
            assert result['observed_seeds']==list(range(result['seed_range'][0],result['seed_range'][1]+1))
            seeds.extend(result['observed_seeds'])
        miri.append({'label':result['label'],'passed':passed,'test_executions':count,'seed_range':result['seed_range'],'elapsed_seconds':result['elapsed_seconds'],'metadata_sha256':sha(LOG/(result['label']+'.json'))})
    complete=not missing and sorted(seeds)==list(range(1,33))
    validation=read('p1p2-audit-v3-validation')
    assert validation['status']=='passed'
    for check in validation['checks']:
        assert hashlib.sha256(check['stdout'].encode()).hexdigest()==check['sha256']
        assert 'error:' not in check['stdout'] and 'FAILED' not in check['stdout']
    allocator_log=next(c['stdout'] for c in validation['checks'] if c['log'].endswith('miri-allocator-reentry.log'))
    assert '1 passed; 0 failed' in allocator_log
    if not a.allow_incomplete_miri:assert complete,missing
    build_path=LOG/'runs'/final['label']/'1-a/criterion/build.json'
    build=json.loads(build_path.read_text()) if build_path.exists() else read(a.prefix+'-analysis')['build']
    assert build['panic_unwind'] and not build['diagnostics'] and not build['verification']
    coverage=read('production-coverage')
    assert len(coverage)==246 and all('classification' in r for r in coverage)
    matrix_path=LOG/(a.prefix+'-matrix.json');matrix=None
    if matrix_path.exists():
        matrix=json.loads(matrix_path.read_text());assert matrix['source_sha256']==final['source_sha256'];assert matrix['binary']==final['binary'];assert len(matrix['runs'][0]['estimates'])==68
    result={'status':'Experimental; no promotion or equivalence proof','inputs':[r['label'] for r in [final,memory,work,sham]],'timing':final['summary'],'sham':sham['summary'],'memory_and_work':rows,'owner_resources':memory['metrics']['owner_resources'],'layout':memory['metrics']['prototype_layout'],'build':build,'validation_sha256':sha(LOG/'p1p2-audit-v3-validation.json'),'miri':miri,'miri_retried_attempts':retries,'miri_guard_attestation':attestation,'miri_complete':complete,'miri_missing_or_failed':missing,'matrix_cases':68 if matrix else None,'coverage_counts':{c:sum(r['classification']==c for r in coverage) for c in sorted({r['classification'] for r in coverage})},'limitations':['Grouped contract evidence is not a direct port of all 246 original test bodies','Source review and Miri are not a formal or independent soundness proof','Unknown secondary panic payloads may be retained defensively','Marking work differs between integrated backends','Reference caches and intentional resource allocations preclude blanket zero-allocation/leak claims','No API drop-in or production complexity parity claim']}
    (LOG/(a.prefix+'-analysis.json')).write_text(json.dumps(result,indent=2)+'\n')
    for family,r in result['timing'].items():print(family,{e:round(ns/1000,3) for e,ns in r['means_ns'].items()},round(r['paired_percent'],2))
    print('Miri complete:',complete,'missing:',missing)
    print('Memory:',[(r['shape'],r['production_bytes'],r['typed_bytes']) for r in rows])
if __name__=='__main__':main()
