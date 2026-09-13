import json, subprocess, os

out = subprocess.run(['fno', 'agents', 'list', '--json'], capture_output=True, text=True, timeout=240).stdout
try:
    rows = json.loads(out)
except Exception:
    rows = json.loads(subprocess.run(['fno', 'agents', 'list'], capture_output=True,
                                     text=True, timeout=240).stdout)
if isinstance(rows, dict):
    rows = rows.get('agents') or rows.get('sessions') or rows.get('rows') or []

for name in ('t-e882-identity', 't-0961-verbfix'):
    r = next((x for x in rows if (x.get('name') or '') == name), None)
    if not r:
        print('%s: NOT FOUND' % name)
        continue
    print('%-18s node=%-10s status=%-9s provider=%s' % (
        name, r.get('node'), r.get('status'), r.get('provider')))

print()
print('claims for NODEID:')
d = '~/.fno/claims'
hits = [f for f in os.listdir(d) if 'e882' in f]
print('  ', hits or '(none)')
