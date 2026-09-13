import datetime, glob, os

sid = "SESSIONID"
now = datetime.datetime.now(datetime.timezone.utc)

# transcript mtime is the decisive liveness proof; print UTC explicitly rather
# than hand-appending Z to a local stamp.
pats = [
    os.path.expanduser(f"~/.claude/projects/*/{sid}.jsonl"),
    os.path.expanduser(f"~/.claude/projects/**/{sid}.jsonl"),
]
found = []
for p in pats:
    found.extend(glob.glob(p, recursive=True))
found = sorted(set(found))
print("transcripts found:", len(found))
for f in found:
    m = datetime.datetime.fromtimestamp(os.path.getmtime(f), datetime.timezone.utc)
    print(f"  age_seconds={int((now-m).total_seconds())}  mtime_utc={m.isoformat()}")
    print(f"  path={f}")

# POSITIVE CONTROL: my own transcript must be found and be very fresh. If the
# glob cannot find a session I know is live, a zero above means nothing.
me = "SESSIONID"
mine = glob.glob(os.path.expanduser(f"~/.claude/projects/**/{me}.jsonl"), recursive=True)
print("CONTROL own transcript found:", len(mine))
for f in mine:
    m = datetime.datetime.fromtimestamp(os.path.getmtime(f), datetime.timezone.utc)
    print(f"  CONTROL age_seconds={int((now-m).total_seconds())}")
