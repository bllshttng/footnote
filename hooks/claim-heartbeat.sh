#!/usr/bin/env bash
# hooks/claim-heartbeat.sh - PostToolUse: renew this session's node:<id> claim
# TTL while the owning session is actively working (x-a166, Facet A).
#
# The node claim is anchored to a transient init PID (dead seconds after init)
# plus a fixed 2h TTL that nothing renews, so a genuinely-live session (attended
# or --bg) loses its claim at the 2h mark and the 5-min active dispatcher then
# re-spawns a worker for work already in flight. This hook is the missing
# heartbeat: an active session's tool calls keep its claim alive; a truly idle
# session (no tool calls for a full TTL) still lapses - the correct "abandoned"
# signal that frees the slot.
#
# Holder-gated + throttled:
#   - refresh ONLY when this session is the recorded claim holder (never revive
#     or steal another session's claim - that is a split-brain, Domain Pitfall).
#   - at most once per THROTTLE window (a stamp-file mtime gate makes almost
#     every tool call a cheap stat+exit; only an aging claim shells `fno`).
#
# Never fails the tool call: silent no-op on not-holder / throttled / no
# manifest; a refresh error logs to stderr and still exits 0. Touches the claim
# lockfile only (via `fno claim refresh`) - never the immutable manifest.
# `refresh_claim` takes the same per-key recovery mutex as `reap`/`acquire`
# (closes a resurrection race - see core.py), so on rare contention with a
# concurrent reap sweep or acquire on the SAME key this call can wait up to
# ~5s before returning; the stamp-file throttle above keeps that window rare.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Refresh at most once per THROTTLE seconds of activity. Well under the claim's
# default 2h TTL, so an actively-working session stays LIVE with wide margin.
# ponytail: a plain stamp-mtime throttle, not half-life arithmetic - refresh is
# idempotent and only extends, so "at most once per 20 min while active" is both
# correct and the cheapest thing that keeps a live claim from lapsing.
THROTTLE="${FNO_CLAIM_HEARTBEAT_THROTTLE:-1200}"  # 20 min

# Separate from claim TTL: a 30s stamp stays fresh inside the 120s peer window.
LIVE_THROTTLE=30

# Re-arm to the node claim's canonical 2h window. `fno claim refresh` with no
# --ttl defaults to MIN_TTL (1 min) and does NOT guard against shortening, so an
# omitted ttl would SHRINK the very claim we mean to keep alive. 2h matches the
# init acquire window; refreshing to now+2h always extends a live (<=2h-left)
# claim, so the "only ever extends" invariant holds.
HEARTBEAT_TTL="${FNO_CLAIM_HEARTBEAT_TTL:-2h}"

# Resolve the project dir. Claude Code runs the hook from the project root; also
# honor a cwd on stdin JSON and $CLAUDE_PROJECT_DIR. The manifest is per-worktree.
STDIN="$(cat 2>/dev/null || true)"
CWD=""
CUR_CLAUDE_SID=""
CUR_CODEX_THREAD_ID="${CODEX_THREAD_ID:-}"
HOOK_SESSION_ID=""
IS_CODEX_HOOK=0
_ENV_CODEX_COMPACT="${CUR_CODEX_THREAD_ID//[[:space:]]/}"
if [[ "${FNO_PLATFORM:-}" == "codex" || -n "${CODEX_PLUGIN_ROOT:-}" \
      || -n "$_ENV_CODEX_COMPACT" ]]; then
  IS_CODEX_HOOK=1
fi
if [[ -n "$STDIN" ]] && command -v jq >/dev/null 2>&1; then
  CWD="$(printf '%s' "$STDIN" | jq -r '.cwd // empty' 2>/dev/null)"
  HOOK_SESSION_ID="$(printf '%s' "$STDIN" \
    | jq -r 'if (.session_id? | type) == "string" then .session_id else empty end' \
      2>/dev/null)"
fi
if [[ "$IS_CODEX_HOOK" -eq 1 ]]; then
  [[ -n "$_ENV_CODEX_COMPACT" ]] || CUR_CODEX_THREAD_ID="$HOOK_SESSION_ID"
else
  CUR_CLAUDE_SID="$HOOK_SESSION_ID"
fi
[[ -z "$CWD" ]] && CWD="${CLAUDE_PROJECT_DIR:-$PWD}"

# Stamp before the manifest/holder gates: non-owner activity is still overlap.
# Git's administrative directory is physical-worktree-specific even when a
# Claude WorktreeCreate path has shared the checkout's whole .fno directory.
LIVE_SESSION_ID="$CUR_CLAUDE_SID"
[[ "$IS_CODEX_HOOK" -eq 1 ]] && LIVE_SESSION_ID="$CUR_CODEX_THREAD_ID"
live_helper="$SCRIPT_DIR/helpers/worktree-live-peers.sh"
LIVE_DIR="$(bash "$live_helper" --live-dir "$CWD" </dev/null 2>/dev/null || true)"
if [[ -n "$LIVE_DIR" && "$LIVE_SESSION_ID" =~ ^[A-Za-z0-9_-]+$ ]]; then
  live_stamp="$LIVE_DIR/$LIVE_SESSION_ID"
  live_now="$(date +%s 2>/dev/null || echo 0)"
  live_mtime="$(stat -c %Y "$live_stamp" 2>/dev/null || stat -f %m "$live_stamp" 2>/dev/null || echo 0)"
  if (( live_now <= 0 || live_mtime <= 0 || live_now - live_mtime >= LIVE_THROTTLE )); then
    mkdir -p "${live_stamp%/*}" 2>/dev/null && touch "$live_stamp" 2>/dev/null || true
  fi
fi

MANIFEST="$CWD/.fno/target-state.md"
[[ -f "$MANIFEST" ]] || exit 0   # no target session here -> nothing to refresh

# graph_node_id lives in the manifest BODY; session_id in the frontmatter.
NODE_ID="$(sed -n 's/^[[:space:]]*graph_node_id:[[:space:]]*//p' "$MANIFEST" | head -1 | tr -d "\"'")"
[[ -n "$NODE_ID" && "$NODE_ID" != "null" ]] || exit 0
SESSION_ID="$(sed -n 's/^[[:space:]]*session_id:[[:space:]]*//p' "$MANIFEST" | head -1 | tr -d "\"'")"
[[ -n "$SESSION_ID" ]] || exit 0
CLAIM_HOLDER="$(sed -n 's/^[[:space:]]*target_claim_holder:[[:space:]]*//p' "$MANIFEST" | head -1 | tr -d "\"'")"
[[ -n "$CLAIM_HOLDER" && "$CLAIM_HOLDER" != "null" ]] \
  || CLAIM_HOLDER="target-session:$SESSION_ID"

# Identity gate (codex P1): prove the CURRENT running session owns this manifest,
# not a stale target-state.md a dead session left behind in this worktree. The
# manifest records the owner's Claude session uuid and Codex thread id at init;
# Claude Code passes its live uuid on stdin and Codex exports CODEX_THREAD_ID.
# A POSITIVE mismatch means a different session
# is sitting on a stale manifest whose session_id still matches an abandoned
# (stale/suspect) node claim - the holder gate below would then REVIVE that dead
# claim and block dispatch from reclaiming the node. This check is pid-independent
# (the claim's pid arm is unreliable by design - the whole reason this hook
# exists), so it cannot regress a dead-pid claim the way a state==live gate would.
# Claude keeps the legacy fail-open behavior when its identity is unavailable.
# Codex is detected independently from identity; CODEX_THREAD_ID wins, with a
# string stdin session_id as fallback. Codex fails CLOSED when either current
# identity or manifest owner is missing: a generic holder proves no ownership.
MANIFEST_CLAUDE_SID="$(sed -n 's/^[[:space:]]*claude_session_id:[[:space:]]*//p' "$MANIFEST" | head -1 | tr -d "\"'")"
_CUR_CODEX_COMPACT="${CUR_CODEX_THREAD_ID//[[:space:]]/}"
if [[ "$IS_CODEX_HOOK" -eq 0 && -n "$CUR_CLAUDE_SID" \
      && -n "$MANIFEST_CLAUDE_SID" && "$MANIFEST_CLAUDE_SID" != "null" \
      && "$CUR_CLAUDE_SID" != "$MANIFEST_CLAUDE_SID" ]]; then
  exit 0
fi
MANIFEST_CODEX_THREAD_ID="$(sed -n 's/^[[:space:]]*codex_thread_id:[[:space:]]*//p' "$MANIFEST" | head -1 | tr -d "\"'")"
[[ "$MANIFEST_CODEX_THREAD_ID" == "null" ]] && MANIFEST_CODEX_THREAD_ID=""
if [[ "$IS_CODEX_HOOK" -eq 1 ]]; then
  [[ -n "$_CUR_CODEX_COMPACT" ]] || exit 0
  [[ -n "$MANIFEST_CODEX_THREAD_ID" ]] || exit 0
  [[ "$CUR_CODEX_THREAD_ID" == "$MANIFEST_CODEX_THREAD_ID" ]] || exit 0
  if [[ "$CLAIM_HOLDER" != "target-session:$MANIFEST_CODEX_THREAD_ID" \
        && "$CLAIM_HOLDER" != "target-session:$SESSION_ID" ]]; then
    exit 0
  fi
fi

# Throttle: skip when the stamp is younger than THROTTLE seconds.
STAMP="$CWD/.fno/.claim-heartbeat.stamp"
if [[ -f "$STAMP" ]]; then
  now="$(date +%s 2>/dev/null || echo 0)"
  mtime="$(stat -c %Y "$STAMP" 2>/dev/null || stat -f %m "$STAMP" 2>/dev/null || echo 0)"
  (( now > 0 && mtime > 0 && now - mtime < THROTTLE )) && exit 0
fi

command -v fno >/dev/null 2>&1 || exit 0   # no CLI -> silent no-op

# Holder gate: refresh ONLY our own claim. A different holder (or no live claim)
# stamps and returns so we do not re-probe on every tool call.
HOLDER="$(fno claim status "node:$NODE_ID" --json 2>/dev/null | jq -r '.holder // empty' 2>/dev/null)"
if [[ "$HOLDER" != "$CLAIM_HOLDER" ]]; then
  touch "$STAMP" 2>/dev/null || true
  exit 0
fi

# We hold it: renew the TTL. Best-effort - a failure logs but never blocks.
if ! fno claim refresh "node:$NODE_ID" --holder "$CLAIM_HOLDER" --ttl "$HEARTBEAT_TTL" >/dev/null 2>&1; then
  echo "claim-heartbeat: refresh failed for node:$NODE_ID (non-fatal)" >&2
fi
touch "$STAMP" 2>/dev/null || true
exit 0
