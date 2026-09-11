import json, os
g = json.load(open(os.path.expanduser("~/.fno/graph.json")))
nodes = g["entries"]
byid = {n["id"]: n for n in nodes}
epic = byid["NODEID"]
kids = epic.get("children") or []
print("NODEID status:", epic.get("status"), "| children:", len(kids))
print()
ids = []
for k in kids:
    ids.append(k["id"] if isinstance(k, dict) else k)
for k in ids:
    n = byid.get(k)
    if not n:
        print("  %s  MISSING from graph" % k)
        continue
    print("  %s  %-12s %-3s %-7s | %s" % (
        n["id"], n.get("status"), str(n.get("priority")),
        str(n.get("difficulty")), (n.get("title") or "")[:58]))
