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
# NEVER blocks the tool call: silent no-op on not-holder / throttled / no
# manifest; a refresh error logs to stderr and still exits 0. Touches the claim
# lockfile only (via `fno claim refresh`) - never the immutable manifest.

set -uo pipefail

# Refresh at most once per THROTTLE seconds of activity. Well under the claim's
# default 2h TTL, so an actively-working session stays LIVE with wide margin.
# ponytail: a plain stamp-mtime throttle, not half-life arithmetic - refresh is
# idempotent and only extends, so "at most once per 20 min while active" is both
# correct and the cheapest thing that keeps a live claim from lapsing.
THROTTLE="${FNO_CLAIM_HEARTBEAT_THROTTLE:-1200}"  # 20 min

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
if [[ -n "$STDIN" ]] && command -v jq >/dev/null 2>&1; then
  CWD="$(printf '%s' "$STDIN" | jq -r '.cwd // empty' 2>/dev/null)"
fi
[[ -z "$CWD" ]] && CWD="${CLAUDE_PROJECT_DIR:-$PWD}"

MANIFEST="$CWD/.fno/target-state.md"
[[ -f "$MANIFEST" ]] || exit 0   # no target session here -> nothing to refresh

# graph_node_id lives in the manifest BODY; session_id in the frontmatter.
NODE_ID="$(sed -n 's/^[[:space:]]*graph_node_id:[[:space:]]*//p' "$MANIFEST" | head -1 | tr -d "\"'")"
[[ -n "$NODE_ID" && "$NODE_ID" != "null" ]] || exit 0
SESSION_ID="$(sed -n 's/^[[:space:]]*session_id:[[:space:]]*//p' "$MANIFEST" | head -1 | tr -d "\"'")"
[[ -n "$SESSION_ID" ]] || exit 0

# Throttle: skip when the stamp is younger than THROTTLE seconds.
STAMP="$CWD/.fno/.claim-heartbeat.stamp"
if [[ -f "$STAMP" ]]; then
  now="$(date +%s 2>/dev/null || echo 0)"
  mtime="$(stat -f %m "$STAMP" 2>/dev/null || stat -c %Y "$STAMP" 2>/dev/null || echo 0)"
  (( now > 0 && mtime > 0 && now - mtime < THROTTLE )) && exit 0
fi

command -v fno >/dev/null 2>&1 || exit 0   # no CLI -> silent no-op

# Holder gate: refresh ONLY our own claim. A different holder (or no live claim)
# stamps and returns so we do not re-probe on every tool call.
HOLDER="$(fno claim status "node:$NODE_ID" --json 2>/dev/null | jq -r '.holder // empty' 2>/dev/null)"
if [[ "$HOLDER" != "target-session:$SESSION_ID" ]]; then
  touch "$STAMP" 2>/dev/null || true
  exit 0
fi

# We hold it: renew the TTL. Best-effort - a failure logs but never blocks.
if ! fno claim refresh "node:$NODE_ID" --holder "target-session:$SESSION_ID" --ttl "$HEARTBEAT_TTL" >/dev/null 2>&1; then
  echo "claim-heartbeat: refresh failed for node:$NODE_ID (non-fatal)" >&2
fi
touch "$STAMP" 2>/dev/null || true
exit 0
