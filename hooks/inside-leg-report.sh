#!/usr/bin/env bash
# hooks/inside-leg-report.sh -- the inside leg (inside-out E3.2).
#
# A per-turn hook that pushes structured agent state OUTWARD so a grid pane badge
# is fact, not a scrape guess. Wired to two Claude Code events in hooks.json:
#   UserPromptSubmit -> state=working   (the turn started)
#   Stop             -> state=done       (the turn finished)
# The desired state is the first argument ($1). `blocked` is in the contract but
# has no natural Claude Code hook trigger yet (no Notification event wired), so
# E3.2 emits working/done only; a future permission/idle trigger can push blocked
# through the same verb.
#
# Chain: this hook -> `fno agents report` (the thin verb) -> agent.report RPC ->
# the daemon STORES the latest state on the matching claude row. The match keys
# on the daemon-pinned session_id (the same uuid Claude Code passes here), so a
# pane reports under the id E1 recorded.
#
# Fire-and-forget by design:
#   - It NEVER blocks or reds a turn (UserPromptSubmit/Stop are non-blocking; this
#     script always exits 0).
#   - The verb sends to an ALREADY-RUNNING daemon and never boots one, so for a
#     plain claude session with no daemon (the common non-grid case) the report
#     is a cheap no-op. When a daemon IS up but this session is not a registered
#     pane, the daemon drops the report as unknown_session -- the daemon is the
#     filter, so this hook needs no "am I a grid pane?" gate.
# ponytail: fires for every claude session; the daemon-presence + unknown-session
# drop is the gate. A per-session opt-in lands if the dropped-report noise ever
# matters.

set -uo pipefail

STATE="${1:-working}"
case "$STATE" in
  working | blocked | done) ;;
  *) STATE="working" ;;
esac

INPUT=$(cat)

# One python process extracts the session id and a millisecond-resolution seq.
# Millisecond (not second) resolution matters: the working/done pair of one fast
# turn must get strictly increasing seqs, else the daemon's `seq <= last_seq`
# drop would discard the `done`. A missing/garbled session id -> silent exit 0.
PARSED=$(python3 -c '
import sys, json, time
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
sid = d.get("session_id") or ""
if not sid:
    sys.exit(0)
print(f"{sid}\t{int(time.time() * 1000)}")
' <<<"$INPUT" 2>/dev/null) || exit 0
[[ -z "$PARSED" ]] && exit 0

SESSION_ID="${PARSED%%$'\t'*}"
SEQ="${PARSED##*$'\t'}"
[[ -z "$SESSION_ID" || -z "$SEQ" ]] && exit 0

# Resolve the fno-agents binary, most-local first (mirrors target-stop-hook.sh).
REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
BIN=""
if [[ -n "${FNO_AGENTS_BIN:-}" ]] && [[ -x "${FNO_AGENTS_BIN}" ]]; then
  BIN="$FNO_AGENTS_BIN"
elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/release/fno-agents" ]]; then
  BIN="${REPO_ROOT}/crates/fno-agents/target/release/fno-agents"
elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents" ]]; then
  BIN="${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
elif command -v fno-agents >/dev/null 2>&1; then
  BIN=$(command -v fno-agents)
fi
# No binary -> nothing to report to; stay silent (the inside leg is best-effort).
[[ -z "$BIN" ]] && exit 0

"$BIN" report \
  --session-id "$SESSION_ID" \
  --seq "$SEQ" \
  --state "$STATE" \
  >/dev/null 2>&1 || true

exit 0
