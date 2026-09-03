#!/usr/bin/env bash
# Regression: a Codex rollout basename must resolve the active target and run
# loop-check, producing a positive named marker instead of the silent visitor
# allow.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="$ROOT/hooks/target-stop-hook.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
OWNER="$TMP/owner"
HOME_DIR="$TMP/home"
STUB="$TMP/fno-agents"
STATE="$OWNER/.fno/target-state.md"
TRANSCRIPT="$TMP/rollout-2026-09-03T17-00-00-01a06844-c5e1-7e30-b198-f89b798ed1a2.jsonl"
RESOLVER_ID="$TMP/resolver-id"
EVENTS="$TMP/events.jsonl"
SESSION_ID="01a06844-c5e1-7e30-b198-f89b798ed1a2"

mkdir -p "$REPO" "$HOME_DIR/.fno"
git -C "$REPO" init -q -b feature
git -C "$REPO" config user.email test@example.com
git -C "$REPO" config user.name Test
printf 'seed\n' > "$REPO/seed"
git -C "$REPO" add seed
git -C "$REPO" commit -qm seed
git -C "$REPO" worktree add -q "$OWNER" -b owner
mkdir -p "$OWNER/.fno"
cat > "$STATE" <<STATE_EOF
---
fno_id: target-run
harness_session_id: $SESSION_ID
target_claim_key: "node:target-run"
target_claim_holder: "target-session:test"
target_claim_ttl: "2h"
---
STATE_EOF
printf '%s\n' '{"message":{"role":"assistant","content":"still working"}}' > "$TRANSCRIPT"

cat > "$STUB" <<'STUB_EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  manifest-for-session)
    id="${3:-}"
    printf '%s\n' "$id" > "$RESOLVER_ID"
    if [[ "$id" == "$SESSION_ID" ]]; then
      printf '%s\n' "$STATE"
      exit 0
    fi
    exit 1
    ;;
  loop-check)
    printf '%s\n' '{"type":"loop_check","data":{"signal":"codex_target_stop_reinvoke"}}' >> "$EVENTS"
    printf '%s\n' '{"decision":"block","message":"codex_target_stop_reinvoke"}'
    ;;
  *)
    exit 2
    ;;
esac
STUB_EOF
chmod +x "$STUB"

set +e
OUT=$(cd "$REPO" && \
  env -u CODEX_THREAD_ID -u CODEX_SESSION_ID -u CLAUDE_CODE_SESSION_ID \
      -u CLAUDECODE_SESSION_ID -u GEMINI_SESSION_ID -u OPENCODE_SESSION_ID \
      HOME="$HOME_DIR" FNO_AGENTS_BIN="$STUB" CLAUDECODE=0 \
      RESOLVER_ID="$RESOLVER_ID" EVENTS="$EVENTS" STATE="$STATE" \
      SESSION_ID="$SESSION_ID" bash "$HOOK" \
      <<< "{\"transcript_path\":\"$TRANSCRIPT\"}" 2>&1)
RC=$?
set -e

[[ "$RC" -eq 2 ]] || { echo "FAIL: expected Codex block rc=2, got $RC" >&2; exit 1; }
[[ "$(<"$RESOLVER_ID")" == "$SESSION_ID" ]] || {
  echo "FAIL: resolver saw $(<"$RESOLVER_ID"), expected bare session UUID" >&2
  exit 1
}

python3 - "$EVENTS" "$OUT" <<'PY'
import json
import sys

events_path, hook_output = sys.argv[1:]
rows = [json.loads(line) for line in open(events_path, encoding="utf-8")]
assert any(
    row.get("type") == "loop_check"
    and row.get("data", {}).get("signal") == "codex_target_stop_reinvoke"
    for row in rows
), rows
assert "codex_target_stop_reinvoke" in hook_output, hook_output
print("PASS: codex rollout reaches loop-check with named marker")
PY
