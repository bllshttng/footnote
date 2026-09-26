#!/usr/bin/env bash
# Regression: a Codex rollout basename must resolve the active target and run
# the decision, producing a positive named marker instead of the silent visitor
# allow. The resolver is native (`hook stop`), so the contract is
# pinned on the REAL binary: the rollout basename's stripped uuid resolves the
# owner worktree's manifest and the fire is judged, and a miss names every id
# tried, uuid suffix first.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="$ROOT/hooks/target-stop-hook.sh"
# Same resolution order as the wrapper: env, release, debug. A sibling leg
# of the packet may have cleaned the target dir between provisioning and this
# run: rebuild the debug binary quietly rather than fail on an artifact the
# environment is documented to provide.
BIN="${FNO_AGENTS_BIN:-}"
if [[ -z "$BIN" ]]; then
    for candidate in "$ROOT/crates/fno-agents/target/release/fno-agents" \
        "$ROOT/crates/fno-agents/target/debug/fno-agents"; do
        [[ -x "$candidate" ]] && BIN="$candidate" && break
    done
fi
if [[ -z "$BIN" ]] || [[ ! -x "$BIN" ]]; then
    (cd "$ROOT/crates/fno-agents" && cargo build --bin fno-agents >/dev/null 2>&1)
    BIN="$ROOT/crates/fno-agents/target/debug/fno-agents"
fi
[[ -x "$BIN" ]] || { echo "FAIL: fno-agents binary missing at $BIN" >&2; exit 1; }
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
OWNER="$TMP/owner"
HOME_DIR="$TMP/home"
SPACE="$TMP/space"
STATE="$OWNER/.fno/target-state.md"
TRANSCRIPT="$TMP/rollout-2026-09-03T17-00-00-01a06844-c5e1-7e30-b198-f89b798ed1a2.jsonl"
EVENTS="$SPACE/events.jsonl"
SESSION_ID="01a06844-c5e1-7e30-b198-f89b798ed1a2"

mkdir -p "$REPO" "$HOME_DIR" "$SPACE"
git -C "$REPO" init -q -b feature
git -C "$REPO" config user.email test@example.com
git -C "$REPO" config user.name Test
printf 'seed\n' > "$REPO/seed"
git -C "$REPO" add seed
git -C "$REPO" commit -qm seed
mkdir -p "$OWNER"
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

set +e
OUT=$(cd "$REPO" && \
  env -u CODEX_THREAD_ID -u CODEX_SESSION_ID -u CLAUDE_CODE_SESSION_ID \
      -u CLAUDECODE_SESSION_ID -u GEMINI_SESSION_ID -u OPENCODE_SESSION_ID \
      HOME="$HOME_DIR" FNO_AGENTS_BIN="$BIN" FNO_EVENTS_PATH="$EVENTS" \
      FNO_SPACES_DIR="$SPACE/spaces" CLAUDECODE=0 \
      bash "$HOOK" \
      <<< "{\"transcript_path\":\"$TRANSCRIPT\",\"cwd\":\"$REPO\"}" 2>&1)
RC=$?
set -e

# The rollout basename's bare uuid resolves the OWNER worktree's manifest, so
# the fire is JUDGED: the continue block, not the silent visitor allow.
[[ "$RC" -eq 2 ]] || { echo "FAIL: expected Codex block rc=2, got $RC: $OUT" >&2; exit 1; }
echo "$OUT" | grep -q "continue working" || {
  echo "FAIL: expected the decision's continue block, got: $OUT" >&2
  exit 1
}

# A MISS names every id tried, with the bare uuid the resolver stripped from
# the rollout basename FIRST in the list (id-major order, most authoritative
# first).
set +e
MISS=$(cd "$REPO" && \
  env -u CODEX_THREAD_ID -u CODEX_SESSION_ID -u CLAUDE_CODE_SESSION_ID \
      -u CLAUDECODE_SESSION_ID -u GEMINI_SESSION_ID -u OPENCODE_SESSION_ID \
      HOME="$HOME_DIR" FNO_AGENTS_BIN="$BIN" FNO_EVENTS_PATH="$EVENTS" \
      FNO_SPACES_DIR="$SPACE/spaces" CLAUDECODE=0 \
      bash "$HOOK" \
      <<< "{\"transcript_path\":\"$TMP/rollout-2026-09-03T17-00-00-99999999-aaaa-bbbb-cccc-dddddddddddd.jsonl\"}" 2>&1)
set -e
MISS_UUID="99999999-aaaa-bbbb-cccc-dddddddddddd"
echo "$MISS" | grep -q "visitor allowed (tried: $MISS_UUID rollout-" || {
  echo "FAIL: the miss diagnostic must list the stripped uuid first, got: $MISS" >&2
  exit 1
}

echo "PASS: codex rollout reaches the decision (owner judged, miss names the ids)"
