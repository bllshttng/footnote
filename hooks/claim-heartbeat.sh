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
# Never blocks the tool call: silent no-op on not-holder / throttled / no
# manifest; a refresh error logs to stderr and still exits 0. Touches the claim
# lockfile only (via `fno claim refresh`) - never the immutable manifest.
# `refresh_claim` takes the same per-key recovery mutex as `reap`/`acquire`
# (closes a resurrection race - see core.py), so on rare contention with a
# concurrent reap sweep or acquire on the SAME key this call can wait up to
# ~25s before returning (ACQUIRE_MAX_ATTEMPTS=5 retries x the mutex's own
# 5s wait each, core.py) rather than failing fast. The stamp-file throttle
# above keeps that window rare, but "rare" is not "never" - a PostToolUse
# hook that can hang the tool call for 25s once in a while still breaks the
# "NEVER blocks" contract this file promises, so the call itself is bounded
# below with the shared `with_timeout` (scripts/lib/with-timeout.sh): a
# refresh that would otherwise wait out the mutex instead times out and logs,
# same non-fatal outcome as any other refresh failure.
#
# REFRESH_TIMEOUT (5s default) is deliberately shorter than refresh_claim's
# own ~25s graceful contention-exhaustion budget above: this hook only cares
# about bounding wall-clock, not about letting refresh_claim's own
# ClaimContended path run to completion, so the external kill firing first is
# the intended outcome under real contention, not a bug. It also means a kill
# CAN land while refresh_claim holds the recovery mutex (Python's default
# SIGTERM disposition does not run `finally`), orphaning `.recovery.d`. That
# window is narrow - only the fast read+atomic-rewrite after the mutex is
# already acquired, not the (much longer) wait to acquire it - and self-heals
# via the existing corpse-steal path (`steal_if_stale`, mutex.py,
# STALE_MUTEX_STEAL_S=120s), the same bounded-recovery mechanism every other
# stale mutex in this file relies on. Narrowing that window further would
# mean either raising REFRESH_TIMEOUT back toward 25s+ (reintroducing the
# hang this bound exists to prevent) or making the CLI's own SIGTERM handling
# interruption-safe globally - a materially larger change than this hook's
# scope.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"
WITH_TIMEOUT_LIB="$PLUGIN_ROOT/scripts/lib/with-timeout.sh"
# Unlike the three documented silent-no-op paths below (not-holder,
# throttled, no manifest - each a legitimate "nothing to do" state), a
# missing/unreadable/broken with-timeout.sh is an infrastructure fault: every
# future refresh on every tool call would silently no-op forever with zero
# diagnostic trail otherwise (AGENTS.md pitfall: never let an absence read
# the same as "correctly did nothing").
if [[ ! -r "$WITH_TIMEOUT_LIB" ]]; then
  echo "claim-heartbeat: $WITH_TIMEOUT_LIB missing or unreadable; refresh skipped" >&2
  exit 0
fi
# shellcheck source=scripts/lib/with-timeout.sh
if ! source "$WITH_TIMEOUT_LIB"; then
  echo "claim-heartbeat: $WITH_TIMEOUT_LIB failed to load (syntax error?); refresh skipped" >&2
  exit 0
fi

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

# A raw `gh pr create` bypasses the fno PR path, but PostToolUse still observes
# its successful URL. Bind only one unambiguous GitHub PR URL; the binder then
# verifies the current branch names exactly one real graph node. Every refusal
# is non-fatal and leaves the graph unchanged.
TOOL_NAME=""
TOOL_COMMAND=""
if [[ -n "$STDIN" ]] && command -v jq >/dev/null 2>&1; then
  TOOL_NAME="$(printf '%s' "$STDIN" | jq -r '.tool_name // empty' 2>/dev/null)"
  TOOL_COMMAND="$(printf '%s' "$STDIN" \
    | jq -r '.tool_input.command // .tool_input.cmd // empty' 2>/dev/null)"
fi
if [[ "$TOOL_NAME" =~ ^(Bash|Shell|exec_command)$ \
      && "$TOOL_COMMAND" == *gh* && "$TOOL_COMMAND" == *pr* \
      && "$TOOL_COMMAND" == *create* ]]; then
  _BARE_GH_CREATE=0
  _GH_ATTRIBUTION="other"
  _GH_ATTRIBUTION_RC=0
  if command -v python3 >/dev/null 2>&1; then
    _GH_ATTRIBUTION_CODE='
import shlex
import sys

source = sys.argv[1].replace("\\\r\n", "").replace("\\\n", "")
try:
    lexer = shlex.shlex(source, posix=True, punctuation_chars=";&|()`\n")
    lexer.whitespace = " \t\r"
    lexer.whitespace_split = True
    lexer.commenters = ""
    tokens = list(lexer)
except ValueError:
    print("invalid")
    raise SystemExit(0)

def is_create_at(index):
    return tokens[index:index + 3] == ["gh", "pr", "create"]

positions = [i for i in range(len(tokens) - 2) if is_create_at(i)]
operators = {
    token for token in tokens
    if token and all(char in ";&|()`\n" for char in token)
}
if positions == [0] and not operators:
    print("bare")
elif positions:
    print("compound")
else:
    print("other")
'
    _GH_ATTRIBUTION="$(with_timeout "${FNO_PR_ATTRIBUTION_TIMEOUT:-2}" \
      python3 -c "$_GH_ATTRIBUTION_CODE" "$TOOL_COMMAND")"
    _GH_ATTRIBUTION_RC=$?
  else
    _GH_ATTRIBUTION_RC=127
  fi
  if [[ "$_GH_ATTRIBUTION_RC" -ne 0 ]]; then
    echo "claim-heartbeat: gh pr create attribution unavailable; binding skipped" >&2
  elif [[ "$_GH_ATTRIBUTION" == bare ]]; then
    _BARE_GH_CREATE=1
  elif [[ "$_GH_ATTRIBUTION" == compound || "$_GH_ATTRIBUTION" == invalid ]]; then
    echo "claim-heartbeat: gh pr create not attributable ($_GH_ATTRIBUTION command); binding skipped" >&2
  fi
  if [[ "$_BARE_GH_CREATE" -eq 1 ]] && ! command -v fno >/dev/null 2>&1; then
    echo "claim-heartbeat: gh pr create observed but fno is unavailable; binding skipped" >&2
  elif [[ "$_BARE_GH_CREATE" -eq 1 ]]; then
    _CREATED_PR_FAILED="$(printf '%s' "$STDIN" | jq -r '
      if (.tool_response | type) == "object" and (
        ((.tool_response.is_error // .tool_response.isError // false) == true) or
        (((.tool_response.exit_code // .tool_response.exitCode // 0) | tonumber? // 0) != 0)
      ) then 1 else 0 end' 2>/dev/null)"
    _CREATED_PR_URLS="$(printf '%s' "$STDIN" | jq -r '
      [.tool_response | .. | strings
        | scan("https://github\\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/pull/[0-9]+")]
      | unique | .[]' 2>/dev/null)"
    _CREATED_PR_URL_COUNT="$(printf '%s\n' "$_CREATED_PR_URLS" | awk 'NF { n++ } END { print n+0 }')"
    if [[ "$_CREATED_PR_FAILED" != 0 ]]; then
      echo "claim-heartbeat: gh pr create failed; binding skipped" >&2
    elif [[ "$_CREATED_PR_URL_COUNT" -eq 0 ]]; then
      echo "claim-heartbeat: gh pr create returned no PR URL; binding skipped" >&2
    elif [[ "$_CREATED_PR_URL_COUNT" -ne 1 ]]; then
      echo "claim-heartbeat: gh pr create returned ambiguous PR URLs; binding skipped" >&2
    else
      _BIND_OWNER="$CUR_CLAUDE_SID"
      [[ "$IS_CODEX_HOOK" -eq 1 ]] && _BIND_OWNER="$CUR_CODEX_THREAD_ID"
      if [[ -z "$_BIND_OWNER" && "${FNO_NODE_CLAIM_HOLDER:-}" == spawn-handover:* ]]; then
        _BIND_OWNER="${FNO_NODE_CLAIM_HOLDER#spawn-handover:}"
      fi
      _bind_args=(pr bind-created --url "$_CREATED_PR_URLS" --repo "$CWD")
      [[ -n "$_BIND_OWNER" ]] && _bind_args+=(--owner "$_BIND_OWNER")
      _BIND_MANUAL="fno pr bind-created --url $_CREATED_PR_URLS --repo $CWD"
      [[ -n "$_BIND_OWNER" ]] && _BIND_MANUAL="$_BIND_MANUAL --owner $_BIND_OWNER"
      _BIND_ATTEMPT=0
      _BIND_RC=1
      _BIND_OUTPUT=""
      _BIND_TIMEOUT="${FNO_PR_BIND_CREATED_TIMEOUT:-2}"
      while [[ "$_BIND_ATTEMPT" -lt 2 && "$_BIND_RC" -ne 0 ]]; do
        _BIND_ATTEMPT=$((_BIND_ATTEMPT + 1))
        _BIND_OUTPUT="$(with_timeout "$_BIND_TIMEOUT" \
          fno "${_bind_args[@]}" 2>&1)"
        _BIND_RC=$?
      done
      if [[ "$_BIND_RC" -ne 0 ]]; then
        if [[ "$_BIND_RC" -eq 124 ]]; then
          _BIND_REASON="timed out"
        else
          _BIND_REASON="$(printf '%s' "$_BIND_OUTPUT" \
            | jq -r '.refusal // .error // empty' 2>/dev/null)"
          [[ -n "$_BIND_REASON" ]] || _BIND_REASON="$(printf '%s' "$_BIND_OUTPUT" | head -1)"
          [[ -n "$_BIND_REASON" ]] || _BIND_REASON="binder exited $_BIND_RC"
        fi
        echo "claim-heartbeat: PR binding failed after 2 attempts: $_BIND_REASON; run: $_BIND_MANUAL" >&2
      fi
    fi
  fi
fi

# Before target init there is no manifest, but an explicit spawn already owns
# a 15-minute handover lease. Keep only that exact holder alive while its worker
# is producing tool activity. `refresh` can extend; it cannot acquire or steal.
_HANDOVER_NODE="${FNO_NODE:-}"
_HANDOVER_HOLDER="${FNO_NODE_CLAIM_HOLDER:-}"
if [[ "$_HANDOVER_NODE" =~ ^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$ \
      && "$_HANDOVER_HOLDER" == spawn-handover:* \
      && "$_HANDOVER_HOLDER" != "spawn-handover:" ]] \
      && command -v fno >/dev/null 2>&1; then
  _HANDOVER_STAMP="$CWD/.fno/.claim-handover-heartbeat.stamp"
  _HANDOVER_THROTTLE="${FNO_CLAIM_HANDOVER_HEARTBEAT_THROTTLE:-300}"
  _handover_due=1
  if [[ -f "$_HANDOVER_STAMP" ]]; then
    _handover_now="$(date +%s 2>/dev/null || echo 0)"
    _handover_mtime="$(stat -c %Y "$_HANDOVER_STAMP" 2>/dev/null || stat -f %m "$_HANDOVER_STAMP" 2>/dev/null || echo 0)"
    (( _handover_now > 0 && _handover_mtime > 0 \
       && _handover_now - _handover_mtime < _HANDOVER_THROTTLE )) && _handover_due=0
  fi
  if [[ "$_handover_due" -eq 1 ]]; then
    _HANDOVER_STATUS="$(with_timeout "${FNO_CLAIM_HEARTBEAT_STATUS_TIMEOUT:-5}" \
      fno claim status "node:$_HANDOVER_NODE" --json --no-roster 2>/dev/null)"
    [[ -n "$_HANDOVER_STATUS" ]] || _HANDOVER_STATUS="$(with_timeout \
      "${FNO_CLAIM_HEARTBEAT_STATUS_TIMEOUT:-5}" \
      fno claim status "node:$_HANDOVER_NODE" --json 2>/dev/null)"
    _HANDOVER_STATUS_VALID="$(printf '%s' "$_HANDOVER_STATUS" | jq -r '
      if (type == "object") and
         (.state == "free" or .state == "live" or .state == "suspect" or
          .state == "stale" or .state == "corrupted") and
         ((.holder == null) or (.holder | type) == "string")
      then 1 else 0 end' 2>/dev/null)"
    _RECORDED_HANDOVER_STATE="$(printf '%s' "$_HANDOVER_STATUS" \
      | jq -r '.state // empty' 2>/dev/null)"
    _RECORDED_HANDOVER_HOLDER="$(printf '%s' "$_HANDOVER_STATUS" \
      | jq -r '.holder // empty' 2>/dev/null)"
    if [[ "$_HANDOVER_STATUS_VALID" != 1 ]]; then
      echo "claim-heartbeat: handover claim status unreadable for node:$_HANDOVER_NODE; refresh remains due" >&2
    # The short-lived spawner normally exits before init, so its unexpired
    # handover becomes SUSPECT. The TTL still protects it and permits renewal.
    elif [[ ( "$_RECORDED_HANDOVER_STATE" == live \
              || "$_RECORDED_HANDOVER_STATE" == suspect ) \
            && "$_RECORDED_HANDOVER_HOLDER" == "$_HANDOVER_HOLDER" ]]; then
      if with_timeout "${FNO_CLAIM_HEARTBEAT_REFRESH_TIMEOUT:-5}" \
        fno claim refresh "node:$_HANDOVER_NODE" --holder "$_HANDOVER_HOLDER" \
        --ttl "${FNO_CLAIM_HANDOVER_TTL:-15m}" >/dev/null 2>&1; then
        touch "$_HANDOVER_STAMP" 2>/dev/null || true
      else
        echo "claim-heartbeat: handover refresh failed for node:$_HANDOVER_NODE; refresh remains due" >&2
      fi
    else
      # A complete status answer positively proves there is nothing this holder
      # may refresh. Throttle that safe no-op; only an instrument failure stays due.
      touch "$_HANDOVER_STAMP" 2>/dev/null || true
    fi
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
# --no-roster: this runs on tool calls and reads .holder ONLY. The roster
# cross-check shells out to the harness, and it fires on exactly the branch this
# gate hits when a claim has lapsed, so leaving it on would tax every tool call
# to compute a field discarded on the next line.
#
# The flag is NEW, and this hook runs against the DEPLOYED fno, which lags the
# source. An older binary rejects the unknown option, exits 2 with empty stdout,
# and the gate below then reads "not our claim" and returns - so a live session
# stops refreshing and its claim expires underneath it. Fall back to the
# unflagged call, which every version understands, and pay the cross-check
# rather than lose the heartbeat.
# Keyed on EMPTY OUTPUT, not on the exit code. `fno claim status` happens to
# exit 0 for every state today, but its own docstring says the exit code
# reflects state, so an `||` here would one day run BOTH calls and print two
# JSON objects. `jq .holder` then emits two lines, the gate below never matches,
# and a live session silently stops refreshing its claim.
# BOUNDED, like the refresh below it. This runs on the PostToolUse path, and the
# unflagged fallback pays the roster cross-check - a full `claude agents --json
# --all` enumeration - so an unbounded call would hang a tool call on a slow
# fleet. A bound that fires reads as no claim, which only skips one heartbeat.
# A kill also trips the empty-output retry below, so the worst case is two
# bounded calls rather than one; the alternative is a hook that hangs.
_STATUS_TIMEOUT="${FNO_CLAIM_HEARTBEAT_STATUS_TIMEOUT:-5}"
_status_json() {
  local out
  out="$(with_timeout "$_STATUS_TIMEOUT" \
    fno claim status "node:$NODE_ID" --json --no-roster 2>/dev/null)"
  [ -n "$out" ] || out="$(with_timeout "$_STATUS_TIMEOUT" \
    fno claim status "node:$NODE_ID" --json 2>/dev/null)"
  printf '%s' "$out"
}
HOLDER="$(_status_json | jq -r '.holder // empty' 2>/dev/null)"
if [[ "$HOLDER" != "$CLAIM_HOLDER" ]]; then
  touch "$STAMP" 2>/dev/null || true
  exit 0
fi

# We hold it: renew the TTL. Best-effort - a failure (including a bound
# firing on held-mutex contention) logs but never blocks the tool call past
# REFRESH_TIMEOUT.
REFRESH_TIMEOUT="${FNO_CLAIM_HEARTBEAT_REFRESH_TIMEOUT:-5}"
if ! with_timeout "$REFRESH_TIMEOUT" \
    fno claim refresh "node:$NODE_ID" --holder "$CLAIM_HOLDER" --ttl "$HEARTBEAT_TTL" \
    >/dev/null 2>&1; then
  echo "claim-heartbeat: refresh failed or timed out for node:$NODE_ID (non-fatal)" >&2
fi
touch "$STAMP" 2>/dev/null || true
exit 0
