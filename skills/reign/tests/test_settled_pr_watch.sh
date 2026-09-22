#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WATCH="$HERE/../scripts/settled-pr-watch.sh"
[[ -x "$WATCH" ]] || { echo "FAIL: settled-pr-watch.sh is missing" >&2; exit 1; }
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/state"

export WATCH_STATE="$tmp/state"
export MAIL_LOG="$tmp/mail.log"
export CALL_LOG="$tmp/calls.log"
export COURT_JSON="$tmp/court.json"
export EVENT_JSON="$tmp/events.json"
export PR_JSON="$tmp/pr.json"
export WATCH_MODE=ok

cat > "$tmp/bin/fno" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CALL_LOG"
case "$*" in
  "agents court --nodes --json")
    [[ "$WATCH_MODE" == fail-court ]] && exit 1
    cat "$COURT_JSON"
    ;;
  "doctor event find pr_nudge_escalated --since 15m --json")
    [[ "$WATCH_MODE" == fail-events ]] && exit 1
    cat "$EVENT_JSON"
    ;;
  "do pr status 42")
    cat "$PR_JSON"
    ;;
  agents\ mail\ send\ --to-king\ *)
    printf '%s\n' "$*" >> "$MAIL_LOG"
    ;;
  *)
    echo "unexpected fno call: $*" >&2
    exit 1
    ;;
esac
EOF
cat > "$tmp/bin/fno-agents" <<'EOF'
#!/usr/bin/env bash
if [[ "$*" == "state path" ]]; then
  printf '%s\n' "$WATCH_STATE"
  exit 0
fi
echo "unexpected fno-agents call: $*" >&2
exit 1
EOF
chmod +x "$tmp/bin/fno" "$tmp/bin/fno-agents"

cat > "$COURT_JSON" <<'EOF'
{"crowns":[{"manifest_session":"king-1","scope_nodes":{"nodes":[{"id":"x-1","owned":true,"pr_number":42}]}}]}
EOF
cat > "$EVENT_JSON" <<'EOF'
{"matches":[{"data":{"node":"x-1","pr":42}}],"unreadable_files":[]}
EOF
cat > "$PR_JSON" <<'EOF'
{"ready":true,"ready_blockers":[]}
EOF

positive="$(PATH="$tmp/bin:$PATH" FNO_REIGN_WATCH_DIR="$WATCH_STATE" "$WATCH" scope-1 king-1 0)"
grep -q '^watch matched PR #42 on x-1$' <<<"$positive"
grep -q 'PR #42 on x-1 is ready with no blockers; take the merge lever\.' "$MAIL_LOG"

: > "$CALL_LOG"
export WATCH_MODE=fail-court
if PATH="$tmp/bin:$PATH" FNO_REIGN_WATCH_DIR="$WATCH_STATE" "$WATCH" scope-1 king-1 0; then
  echo "expected court failure" >&2
  exit 1
fi
grep -q 'reign watch probe failed:' "$MAIL_LOG"

: > "$CALL_LOG"
cat > "$EVENT_JSON" <<'EOF'
{"matches":[],"unreadable_files":[{"path":"events.jsonl","error":"permission denied"}]}
EOF
if PATH="$tmp/bin:$PATH" FNO_REIGN_WATCH_DIR="$WATCH_STATE" "$WATCH" scope-1 king-1 0; then
  echo "expected unreadable event failure" >&2
  exit 1
fi
grep -q 'reign watch probe failed: unreadable_files=1' "$MAIL_LOG"
cat > "$EVENT_JSON" <<'EOF'
{"matches":[{"data":{"node":"x-1","pr":42}}],"unreadable_files":[]}
EOF

: > "$CALL_LOG"
export WATCH_MODE=ok
sleep 60 & holder=$!
mkdir "$WATCH_STATE/scope-1.pid.lock"
printf '%s\n' "$holder" > "$WATCH_STATE/scope-1.pid.lock/pid"
live="$(PATH="$tmp/bin:$PATH" FNO_REIGN_WATCH_DIR="$WATCH_STATE" "$WATCH" scope-1 king-1 0)"
kill "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true
grep -q "^watch live pid $holder$" <<<"$live"
[[ ! -s "$CALL_LOG" ]]

echo "test_settled_pr_watch: ok"
