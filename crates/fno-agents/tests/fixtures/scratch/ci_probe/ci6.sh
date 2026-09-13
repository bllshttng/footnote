#!/usr/bin/env bash
# dedupe check-runs by name, keep the LATEST attempt (highest id)
roll() {
python3 -c "
import json,sys
d=json.load(sys.stdin)
latest={}
for r in d.get('check_runs',[]):
    n=r['name']
    if n not in latest or r['id']>latest[n]['id']:
        latest[n]=r
rs=list(latest.values())
pend=[r['name'] for r in rs if r['status']!='completed']
fail=[r['name'] for r in rs if r.get('conclusion') in ('failure','timed_out','cancelled','action_required')]
print('distinct=%d pending=%d failing=%d'%(len(rs),len(pend),len(fail)))
if fail: print('FAILING:', fail)
if pend: print('PENDING:', pend)
"
}
echo "== 1576 head be27e6b2 (deduped)"
gh api "repos/bllshttng/footnote/commits/be27e6b23168baef50d3853d9df1f990566432b5/check-runs?per_page=100" | roll
echo "== 1562 head 5286bc41 (deduped)"
gh api "repos/bllshttng/footnote/commits/5286bc41fe4b8126300747681be77a62aa1e09f5/check-runs?per_page=100" | roll
