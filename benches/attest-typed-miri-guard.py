#!/usr/bin/env python3
"""Attest the sole post-Miri delta: remove cfg from an existing safety guard.

The test/verification program already included all three guarded items. This
comparison is deliberately narrow, not a general source-normalization bypass.
"""
import hashlib
import json
from pathlib import Path
ROOT=Path(__file__).resolve().parent.parent
LOG=ROOT/'benches/.log'
def digest(text):return hashlib.sha256(text.encode()).hexdigest()
def lines(text):return [line.strip() for line in text.splitlines() if line.strip()]
def main():
    old=json.loads((LOG/'p1p2-audit-v3-pre-guard-source.json').read_text())
    path='experiments/typed-runtime/src/lib.rs'
    original=old['sources'][path];current=(ROOT/path).read_text()
    attribute='#[cfg(any(test, feature = "verification"))]'
    assert original.count(attribute)==3
    assert attribute not in current
    assert lines(original.replace(attribute,''))==lines(current),'Unexpected code change after Miri snapshot'
    for p,text in old['sources'].items():
        if p!=path and not p.endswith('.py'):assert (ROOT/p).read_text()==text,p
    result={'old_sha256':digest(original),'new_sha256':digest(current),'file':path,
        'delta':'Exactly three cfg(test or verification) attributes removed: Inner.kernel_active, initialization and graph borrow guard.',
        'unchanged_configurations':['cfg(test)','feature=verification'],
        'changed_configuration':'Normal dependency builds now have the same mandatory reentry guard.',
        'limitation':'This does not attest any other source change or replace normal-build allocator reentry tests.'}
    (LOG/'p1p2-audit-v3-miri-guard-attestation.json').write_text(json.dumps(result,indent=2)+'\n')
    print('Exact cfg-only guard delta attested.')
if __name__=='__main__':main()
