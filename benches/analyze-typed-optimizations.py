#!/usr/bin/env python3
"""Validate optimization checkpoint; no reuse of old Miri/performance claims."""
import hashlib
import json
from pathlib import Path
import re

ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
def read(name):return json.loads((LOG/(name+'.json')).read_text())
def sha(path):return hashlib.sha256(path.read_bytes()).hexdigest()

def main():
    final,memory,work=[read('typed-opt-v3-'+s) for s in ['final','memory','work']]
    for run in [memory,work]:assert run['source_sha256']==final['source_sha256']
    for path,digest in final['source_sha256'].items():assert sha(ROOT/path)==digest,path
    for path,digest in read('p1p2-baseline')['source_sha256'].items():
        if path.startswith('src/'):assert final['source_sha256'][path]==digest,path
    assert [r['variant'] for r in final['runs']]==list('abbaabba')
    miri=[]
    for case,count in [('api',19),('audit',32),('conformance',13),('optimization',11),('frames',1)]:
        run=read('typed-opt-v3-miri-'+case)
        assert run['exit_code']==0 and run['sources_unchanged'] and not run['timed_out']
        assert run['miriflags'] is None
        assert sum(map(int,re.findall(r'test result: ok\. (\d+) passed;',run['stdout'])))==count
        for path,digest in run['source_sha256'].items():assert sha(ROOT/path)==digest,path
        miri.append({'case':case,'tests':count,'elapsed_seconds':run['elapsed_seconds'],'manifest_sha256':sha(LOG/(run['label']+'.json'))})
    validation=read('typed-opt-v3-validation')
    assert validation['passed'] and validation['source_sha256']==final['source_sha256']
    for check in validation['checks']:assert sha(ROOT/check['path'])==check['sha256']
    timeout=read('typed-opt-v3-miri-sources')
    assert timeout['timed_out'] and timeout['exit_code']==124 and timeout['sources_unchanged']
    previous=read('p1p2-audit-v3-safe-memory')['metrics']
    rows=[]
    for row in memory['metrics']['rows']:
        if row['engine'] not in ['typed_research','reactive']:continue
        assert (row['steady_allocations'],row['steady_reallocations'],row['after_drop_net_bytes'])==(0,0,0)
        if row['engine']=='reactive':continue
        old=next(r for r in previous['rows'] if (r['engine'],r['shape'])==(row['engine'],row['shape']))
        rows.append({'shape':row['shape'],'previous_bytes':old['live_requested_bytes'],'current_bytes':row['live_requested_bytes'],'delta_percent':100*(row['live_requested_bytes']/old['live_requested_bytes']-1)})
    oldwork=read('p1p2-audit-v3-safe-work')['metrics']['rows']
    for row in work['metrics']['rows']:
        if row['engine']!='typed_research':continue
        old=next(r for r in oldwork if (r['engine'],r['shape'])==(row['engine'],row['shape']))
        assert row['work']==old['work'],row['shape']
    resources=memory['metrics']['owner_resources']
    assert all(row['after_drop_net_bytes']==0 for row in resources)
    assert len({row['cleanups'] for row in resources})==1
    result={'source_sha256':final['source_sha256'],'timing':final['summary'],'memory_vs_previous_checkpoint':rows,
            'miri':miri,'validation_sha256':sha(LOG/'typed-opt-v3-validation.json'),
            'miri_sources_timeout_manifest_sha256':sha(LOG/'typed-opt-v3-miri-sources.json'),
            'resource_memory':resources,
            'limits':['Experimental backend only; no promotion','Combined revision, not four isolated causal ablations',
                      'Old release timing is historical, not paired pre/post optimization evidence',
                      'Three large original conformance cases lack completed Miri on this revision; sources timed out at 300 seconds',
                      'The independent 32x1000 model passed native; directed Miri is not a proof',
                      'Dynamic callback reads can still nest Rust calls','Chain retained memory increased because of reusable frames']}
    (LOG/'typed-opt-v3-analysis.json').write_text(json.dumps(result,indent=2)+'\n')
    for name,row in result['timing'].items():print(name,round(row['paired_percent'],2),{k:round(v/1000,3) for k,v in row['means_ns'].items()})
    print('Memory changes:',rows)
    print('Directed Miri:',sum(r['tests'] for r in miri),'unit tests, plus integrations in validation manifest')
if __name__=='__main__':main()
