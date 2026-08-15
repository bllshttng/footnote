#!/usr/bin/env bash
# Regression check for scripts/lib/worktree-status.py's registry cross-reference.
# Plain assert-based bash: build a scratch repo with two worktrees, point the
# helper at a synthetic registry.json marking one "live" and one "exited", and
# assert the correct row is labeled in both text and --json output.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../scripts/lib" && pwd)"
SCRATCH="$(mktemp -d)"
SCRATCH="$(cd "$SCRATCH" && pwd -P)"  # resolve symlinks (e.g. macOS /var -> /private/var) to match git's own path reporting
trap 'rm -rf "$SCRATCH"' EXIT

REPO="$SCRATCH/repo"
mkdir -p "$REPO"
git -C "$REPO" init -q
git -C "$REPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init

LIVE_WT="$SCRATCH/live-wt"
BUSY_WT="$SCRATCH/busy-wt"
EXITED_WT="$SCRATCH/exited-wt"
git -C "$REPO" worktree add -q -b live-branch "$LIVE_WT" >/dev/null
git -C "$REPO" worktree add -q -b busy-branch "$BUSY_WT" >/dev/null
git -C "$REPO" worktree add -q -b exited-branch "$EXITED_WT" >/dev/null

REGISTRY="$SCRATCH/registry.json"
cat > "$REGISTRY" <<JSON
{"schema_version": 1, "agents": [
  {"name": "test-live", "cwd": "$LIVE_WT", "status": "live", "last_reconciled_at": "2026-08-15T00:00:00Z"},
  {"name": "test-busy", "cwd": "$BUSY_WT", "status": "busy", "last_reconciled_at": "2026-08-15T00:00:00Z"},
  {"name": "test-exited", "cwd": "$EXITED_WT", "status": "exited", "exited_at": "2026-08-14T00:00:00Z"}
]}
JSON

export WORKTREE_STATUS_REGISTRY="$REGISTRY"

TEXT_OUT=$(python3 "$LIB_DIR/worktree-status.py" --repo "$REPO")
echo "$TEXT_OUT" | grep -q "live-branch.*live:test-live" \
    || { echo "FAIL: live worktree not labeled live:test-live in text output" >&2; echo "$TEXT_OUT" >&2; exit 1; }
# A session mid-turn sits in "busy", not "live" - the common case, not the
# edge case. It must still be reported as live, not read as dead.
echo "$TEXT_OUT" | grep -q "busy-branch.*live:test-busy" \
    || { echo "FAIL: busy worktree not labeled live:test-busy in text output" >&2; echo "$TEXT_OUT" >&2; exit 1; }
echo "$TEXT_OUT" | grep -q "exited-branch.*exited:test-exited" \
    || { echo "FAIL: exited worktree not labeled exited:test-exited in text output" >&2; echo "$TEXT_OUT" >&2; exit 1; }

JSON_OUT=$(python3 "$LIB_DIR/worktree-status.py" --repo "$REPO" --json)
python3 - "$JSON_OUT" <<'PY'
import json, sys
data = json.loads(sys.argv[1])
by_branch = {r["branch"]: r for r in data["worktrees"]}
assert by_branch["live-branch"]["target"] == "live:test-live", by_branch["live-branch"]
assert by_branch["busy-branch"]["target"] == "live:test-busy", by_branch["busy-branch"]
assert by_branch["exited-branch"]["target"] == "exited:test-exited", by_branch["exited-branch"]
s = data["summary"]
assert s["live"] == 2, s
assert s["dead"] == 1, s
assert s["total"] == s["live"] + s["dead"] + s["no_session"], s
PY

echo "PASS: worktree-status.py classifies live vs exited correctly"
