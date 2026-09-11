import json

SHAS = ("5a22d327", "75ef49a4", "0797f4cc", "4b15e0dd")
KEEP = ("verdict", "reviewer", "attestation_origin", "reviewed_sha", "sha",
        "branch", "pr", "pr_number", "reviewer_context", "name",
        "session_id", "author_session_id", "freshness")

rows = 0
hits = 0
with open("~/.fno/events.jsonl") as fh:
    for line in fh:
        if not any(s in line for s in SHAS):
            continue
        rows += 1
        if "attest" not in line and "review" not in line:
            continue
        try:
            d = json.loads(line)
        except Exception:
            continue
        p = d.get("payload") or d
        if not isinstance(p, dict):
            continue
        keep = {k: v for k, v in p.items() if k in KEEP}
        if not keep:
            continue
        hits += 1
        print(d.get("ts", "")[:19], d.get("type"), json.dumps(keep))

print("\nscanned %d rows mentioning those shas, %d carried review/attest fields" % (rows, hits))
assert rows, "PROBE-FAIL: no rows matched the shas at all; the grep found 35 so this is a parse fault"
