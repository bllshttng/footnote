#!/usr/bin/env bash
# handoff.sh - Target session succession protocol helper.
#
# Executes the ordered 8-step handoff sequence (sec 2.1 of the design doc)
# as a single atomic invocation. The calling LLM session invokes this helper;
# the helper performs all state mutations; the LLM performs only step 9 (close).
#
# Usage:
#   handoff.sh --harness <harness> --model <model> [--account <id>]
#
# Output (one machine-parseable decision line on stdout):
#   delegated <node> child=<name> session=<sid> generation=<N>    exit 0
#   parked <node> reason="..."                                     exit 10
#   handoff-restore-failed <node> reason="..."                     exit 12
#
# Environment overrides (for testing):
#   FNO_DIR      override .fno/ dir (default: .fno relative to cwd)
#   HANDOFF_VERIFY_TIMEOUT   seconds to poll for child live status (default: 60)
#   HANDOFF_VERIFY_INTERVAL  poll interval in seconds (default: 5)
#
# Bash 3.2 compatible; set -uo pipefail (NOT -e: explicit error handling throughout).

set -uo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
_EXIT_PARKED=10
_EXIT_RESTORE_FAILED=12
_EXIT_USAGE=2

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
BOUNDARY="capability"
DEST_HARNESS=""
DEST_MODEL=""
DEST_ACCOUNT=""
DEST_DISPATCH_ACCOUNT=""
DEST_EFFORT=""

while [ $# -gt 0 ]; do
  case "$1" in
    --harness)
      DEST_HARNESS="${2:-}"
      shift 2
      ;;
    --model)
      DEST_MODEL="${2:-}"
      shift 2
      ;;
    --account)
      DEST_ACCOUNT="${2:-}"
      shift 2
      ;;
    --dispatch-account)
      DEST_DISPATCH_ACCOUNT="${2:-}"
      shift 2
      ;;
    --effort)
      DEST_EFFORT="${2:-}"
      shift 2
      ;;
    --boundary|--flags)
      echo "handoff: capability escalation requires --harness and --model; boundary triggers are retired" >&2
      exit "$_EXIT_USAGE"
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "handoff: unknown option: $1" >&2
      exit "$_EXIT_USAGE"
      ;;
    *)
      break
      ;;
  esac
done

if [ -z "$DEST_HARNESS" ] || [ -z "$DEST_MODEL" ]; then
  echo "handoff: capability escalation requires --harness and --model" >&2
  exit "$_EXIT_USAGE"
fi
if [ -n "$DEST_ACCOUNT" ] && [ -n "$DEST_DISPATCH_ACCOUNT" ]; then
  echo "handoff: pass only one of --account or --dispatch-account" >&2
  exit "$_EXIT_USAGE"
fi

# ---------------------------------------------------------------------------
# Dependencies guard
# ---------------------------------------------------------------------------
if ! command -v fno >/dev/null 2>&1; then
  echo "parked null reason=\"fno binary not found in PATH\"" >&1
  exit "$_EXIT_PARKED"
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "parked null reason=\"jq not found in PATH\"" >&1
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
FNO_DIR="${FNO_DIR:-.fno}"
STATE_FILE="$FNO_DIR/target-state.md"
EVENTS_FILE="$FNO_DIR/events.jsonl"
ARTIFACTS_DIR="$FNO_DIR/artifacts/handoff"

# Poll tuning (env-overridable for tests)
VERIFY_TIMEOUT="${HANDOFF_VERIFY_TIMEOUT:-60}"
VERIFY_INTERVAL="${HANDOFF_VERIFY_INTERVAL:-5}"

# ---------------------------------------------------------------------------
# Source config.sh (bundled sibling - skill-encapsulation: same directory)
# ---------------------------------------------------------------------------
_SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
_CONFIG_SH="$_SCRIPT_DIR/lib/config.sh"
_EVENTS_SH="$_SCRIPT_DIR/lib/events.sh"
# shellcheck source=lib/events.sh
source "$_EVENTS_SH" 2>/dev/null || true
if [ -f "$_CONFIG_SH" ]; then
  # Set LOCAL_SETTINGS relative to FNO_DIR so config works in sandbox
  LOCAL_SETTINGS="$FNO_DIR/config.toml"
  source "$_CONFIG_SH"
  HANDOFF_ENABLED=$(get_config "target.handoff.enabled" "true")
else
  HANDOFF_ENABLED="true"
fi

# ---------------------------------------------------------------------------
# Helper: emit an event to events.jsonl
# Accepts: emit_event <type> <json-data>
# fno doctor event emit is the PRIMARY writer (validates kind, takes the file lock).
# The bounded shell append is the FALLBACK only when fno exits nonzero
# (stale binary, unknown kind, daemon unavailable).
# The preflight at step 1 guarantees fno can emit `delegated`, so in the
# normal path fno always succeeds and the fallback never runs, preventing double-writes
# that would corrupt the generation count and lineage chain.
# ---------------------------------------------------------------------------
_emit_event() {
  local etype="$1"
  local edata="$2"
  local ts
  ts="$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)"
  # fno is primary; the bounded shell helper is fallback only when fno fails.
  # Always pass --source target so the envelope is correct even when
  # target-state.md has already been archived (step 4 happens before step 8).
  if ! fno doctor event emit --type "$etype" --data "$edata" --events "$EVENTS_FILE" \
       --source target >/dev/null 2>&1; then
    echo "handoff: WARN: fno doctor event emit failed for type '$etype'; using bounded shell fallback" >&2
    local line
    line=$(printf '{"ts":"%s","type":"%s","source":"target","data":%s}' "$ts" "$etype" "$edata")
    if ! declare -F _append_bounded_event >/dev/null 2>&1 \
       || ! _append_bounded_event target_handoff "$line" "$EVENTS_FILE"; then
      echo "handoff: WARN: bounded fallback append also failed for type '$etype'" >&2
    fi
  fi
}

# ---------------------------------------------------------------------------
# Helper: parse YAML frontmatter field from target-state.md
# Reads lines matching "^key: value" or "^key: \"value\""
# ---------------------------------------------------------------------------
_parse_manifest_field() {
  local file="$1" key="$2"
  set +o pipefail
  awk -v k="$key" '
    index($0, k ":") == 1 {
      sub("^" k ":[ \t]*", ""); gsub(/[\"'"'"']/, ""); sub(/\r$/, ""); print; exit
    }' "$file" 2>/dev/null || true
  set -o pipefail
}

# Parse a body-section field (a "key: value" line appended below the closing
# --- of the YAML frontmatter, e.g. graph_node_id / target_claim_*).
#
# Body-first with a whole-file fallback. The body fields read here have exactly
# one legitimate home (below the frontmatter), so we prefer the region AFTER
# the frontmatter close (the SECOND `^---` line). The close is found by LINE
# NUMBER, not a fence COUNT, so a stray `^---` later in the body (markdown
# rule, fenced code block, embedded YAML excerpt) is harmless - everything
# after the close is scanned regardless. Preferring the body also stops a
# frontmatter line from shadowing the field: `init-target-state.sh` writes the
# user's `input:` into frontmatter escaping only quotes (not newlines), so a
# multiline /target input carrying a `graph_node_id:` line would otherwise be
# read first (codex review, PR #531). When there is no second `^---`
# (unterminated frontmatter) or the field is absent from the body, we fall back
# to a whole-file scan so the ab-c2edd785 false-park modes stay fixed. Mirrors
# the placement-independent, shape-validated reader in
# cli/src/fno/cost/_register.py.
_parse_body_field() {
  local file="$1" key="$2" line raw close
  set +o pipefail
  # Line number of the frontmatter close (second `^---`), if the manifest has
  # one. `^---` matches a CRLF `---\r` line too (the CR trails the match).
  close="$(grep -n '^---' "$file" 2>/dev/null | sed -n '2p' | cut -d: -f1)"
  if [ -n "$close" ]; then
    # First "key:" line in the BODY (after the close). The ^[[:space:]]* anchor
    # tolerates indentation and excludes comment (# ...) / quote (> ...) lines.
    line="$(tail -n +"$((close + 1))" "$file" 2>/dev/null \
      | grep -E "^[[:space:]]*${key}:" | head -1 || true)"
  fi
  # Fallback: whole-file scan (unterminated frontmatter, or field not in body).
  if [ -z "${line:-}" ]; then
    line="$(grep -E "^[[:space:]]*${key}:" "$file" 2>/dev/null | head -1 || true)"
  fi
  set -o pipefail
  [ -n "$line" ] || return 0
  raw="${line#*"${key}:"}"          # strip up to and including the key
  raw="${raw//$'\r'/}"              # strip CR (CRLF manifests)
  raw="${raw#"${raw%%[![:space:]]*}"}"   # ltrim
  raw="${raw%"${raw##*[![:space:]]}"}"   # rtrim
  case "$raw" in                    # strip a single pair of surrounding quotes
    \"*\") raw="${raw#\"}"; raw="${raw%\"}" ;;
    \'*\') raw="${raw#\'}"; raw="${raw%\'}" ;;
  esac
  printf '%s' "$raw"
}

# Validate a captured graph_node_id against the canonical node-id shape
# (<prefix>-<4..8 lowercase hex>, legacy `ab-` or a configured prefix). A value
# that fails the shape - empty, the literal `null`, a markdown-prose mention
# like `ab-old (deprecated)`, or any CR/quote residue - is echoed back as empty
# so it routes to the genuine-missing park and is NEVER carried into the
# node:<id> claim lookup or the ${NODE_ID:3:8} child-name slice. Mirrors the
# _GRAPH_NODE_ID_SHAPE gate in cli/src/fno/cost/_register.py.
_validate_node_id() {
  local raw="$1"
  if printf '%s\n' "$raw" | grep -Eq '^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$'; then
    printf '%s' "$raw"
  fi
}

# ---------------------------------------------------------------------------
# Step 0: Read manifest
# ---------------------------------------------------------------------------
if [ ! -f "$STATE_FILE" ]; then
  echo "parked null reason=\"manifest $STATE_FILE not found\""
  exit "$_EXIT_PARKED"
fi

SESSION_ID="$(_parse_manifest_field "$STATE_FILE" "session_id")"
PLAN_PATH="$(_parse_manifest_field "$STATE_FILE" "plan_path")"
TARGET_SIZE="$(_parse_manifest_field "$STATE_FILE" "target_size")"
AUTO_MERGE_APPROVED="$(_parse_manifest_field "$STATE_FILE" "auto_merge_approved")"
NODE_ID="$(_validate_node_id "$(_parse_body_field "$STATE_FILE" "graph_node_id")")"
CLAIM_KEY="$(_parse_body_field "$STATE_FILE" "target_claim_key")"
CLAIM_HOLDER="$(_parse_body_field "$STATE_FILE" "target_claim_holder")"
_CLAIM_TTL_RAW="$(_parse_body_field "$STATE_FILE" "target_claim_ttl")"
CLAIM_TTL="${_CLAIM_TTL_RAW:-2h}"

if [ -z "$SESSION_ID" ]; then
  echo "parked null reason=\"manifest missing session_id\""
  exit "$_EXIT_PARKED"
fi

# Current manifests record the authoritative owner explicitly. Legacy
# manifests predate that body field and used the per-run session id directly.
[ -n "$CLAIM_HOLDER" ] || CLAIM_HOLDER="target-session:$SESSION_ID"

if [ -z "$NODE_ID" ] || [ "$NODE_ID" = "null" ]; then
  echo "parked ${SESSION_ID} reason=\"manifest missing graph_node_id\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 1: Preconditions (refuse = parked BEFORE any state mutation)
# ---------------------------------------------------------------------------

# Master switch check (x-aaaf wave 3 follow-up): config.autonomy.enabled
# outranks every spawner-specific gate, including this one, and is checked
# first for the same reason autonomy_master_enabled() checks it first in
# every Python resolver - a panic switch something else can bypass is not
# a panic switch.
AUTONOMY_ENABLED="true"
if declare -F get_config >/dev/null 2>&1; then
  AUTONOMY_ENABLED=$(get_config "autonomy.enabled" "true")
fi
case "$AUTONOMY_ENABLED" in
  true|True|TRUE|1|yes|on) ;;
  *)
    echo "parked $NODE_ID reason=\"config.autonomy.enabled is false\""
    exit "$_EXIT_PARKED"
    ;;
esac

# Config enabled check
if [ "$HANDOFF_ENABLED" != "true" ]; then
  echo "parked $NODE_ID reason=\"handoff disabled via config\""
  exit "$_EXIT_PARKED"
fi

# plan_path must be non-empty (AC1-EDGE)
if [ -z "$PLAN_PATH" ]; then
  echo "parked $NODE_ID reason=\"plan_path is empty; no re-entry point for successor\""
  exit "$_EXIT_PARKED"
fi

# plan file must exist
if [ ! -f "$PLAN_PATH" ]; then
  echo "parked $NODE_ID reason=\"plan file not found: $PLAN_PATH\""
  exit "$_EXIT_PARKED"
fi

# Is this plan dispatchable? `fno do plan rung` is the single readiness authority;
# this script does not classify statuses of its own. Exit 0 means dispatchable,
# and ANY non-zero exit parks - an unclassifiable plan, an unknown status word,
# or a missing `fno` (127) all land in the same fail-closed branch this block
# has always had. That policy is now named in fno.graph.ladder rather than
# implied by a case arm here.
set +o pipefail
_RUNG_OUT="$(fno do plan rung "$PLAN_PATH" 2>/dev/null)"
_RUNG_EC=$?
set -o pipefail
if [ "$_RUNG_EC" -ne 0 ]; then
  _RUNG="$(printf '%s' "$_RUNG_OUT" | sed -n 's/^rung=//p' | head -1)"
  if [ -z "$_RUNG" ]; then
    # No `rung=` line at all: the verb did not answer. An installed fno older
    # than this verb exits 2 the same way a usage error does, so name the
    # likely cause - a bare "rung 'unknown'" would send the operator hunting
    # through the plan for a problem that is in their PATH.
    echo "parked $NODE_ID reason=\"'fno do plan rung' did not answer; installed fno may predate it - run 'fno doctor update' or 'fno doctor --fix'\""
  else
    echo "parked $NODE_ID reason=\"plan rung '$_RUNG' is not dispatchable\""
  fi
  exit "$_EXIT_PARKED"
fi

# graph_node_id was already validated and guarded at Step 0; NODE_ID is not
# reassigned between there and here, so no second missing-id check is needed.

# Caller must hold node:<id> claim
set +o pipefail
_CLAIM_STATUS_OUT="$(FNO_CLAIMS_ROOT="$HOME" fno agents claim status "node:$NODE_ID" 2>/dev/null || true)"
_CLAIM_HOLDER_ACTUAL="$(printf '%s' "$_CLAIM_STATUS_OUT" | jq -r '.holder // ""' 2>/dev/null || true)"
set -o pipefail
if [ "$_CLAIM_HOLDER_ACTUAL" != "$CLAIM_HOLDER" ]; then
  echo "parked $NODE_ID reason=\"session does not hold node:$NODE_ID (holder='$_CLAIM_HOLDER_ACTUAL')\""
  exit "$_EXIT_PARKED"
fi

# Per-session sentinel: refuse double-handoff
SENTINEL="$FNO_DIR/.handoff-done-$SESSION_ID"
if [ -f "$SENTINEL" ]; then
  echo "parked $NODE_ID reason=\"handoff already completed for this session (idempotent refusal)\""
  exit "$_EXIT_PARKED"
fi

# Capability escalation has one configured destination rung. Historical
# boundary delegations do not consume it; one prior capability delegation does.
_PRIOR_COUNT=0
if [ -f "$EVENTS_FILE" ]; then
  set +o pipefail
  _PRIOR_COUNT="$(grep '"type":"delegated"' "$EVENTS_FILE" 2>/dev/null \
    | grep "\"node_id\":\"${NODE_ID}\"" 2>/dev/null \
    | grep '"handoff_kind":"capability_escalation"' 2>/dev/null \
    | wc -l | tr -d ' ' || echo 0)"
  set -o pipefail
fi
CHILD_GEN=2
if [ "$_PRIOR_COUNT" -gt 0 ]; then
  echo "parked $NODE_ID reason=\"chain-exhausted: capability escalation already spent\""
  exit "$_EXIT_PARKED"
fi

# Emit-capability preflight: emit `delegated` kind against a throwaway temp file
_TEMP_EVENTS="$(mktemp)"
_PREFLIGHT_OK=1
if ! fno doctor event emit --type "delegated" \
      --data "{\"node_id\":\"$NODE_ID\",\"from_session\":\"$SESSION_ID\",\"to_session\":\"preflight\",\"boundary\":\"$BOUNDARY\",\"generation\":$CHILD_GEN}" \
      --events "$_TEMP_EVENTS" >/dev/null 2>&1; then
  _PREFLIGHT_OK=0
fi
rm -f "$_TEMP_EVENTS"

if [ "$_PREFLIGHT_OK" -eq 0 ]; then
  echo "parked $NODE_ID reason=\"emit preflight failed for 'delegated' kind; stale installed fno? run: fno doctor update\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 2: Prove the selected destination can infer and use this worktree
# ---------------------------------------------------------------------------
_NODE_SLUG="$(fno backlog get "$NODE_ID" 2>/dev/null | jq -r '.slug // empty' 2>/dev/null || true)"
_NODE_SLUG="$(printf '%s' "$_NODE_SLUG" | tr '[:upper:]_' '[:lower:]-' \
  | sed -E 's/[^a-z0-9-]+/-/g; s/^-+//; s/-+$//' | cut -c1-48)"
[ -n "$_NODE_SLUG" ] || _NODE_SLUG="work"
CHILD_NAME="target-${NODE_ID}-${_NODE_SLUG}-g${CHILD_GEN}"
_CAPABILITY_NONCE="${HANDOFF_CAPABILITY_NONCE:-$(date +%s)-$$-${RANDOM:-0}}"
_CAPABILITY_CWD="${HANDOFF_CAPABILITY_EXPECTED_CWD:-$PWD}"
_CAPABILITY_ROOT="${HANDOFF_CAPABILITY_EXPECTED_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || true)}"
if [ -z "$_CAPABILITY_ROOT" ]; then
  echo "parked $NODE_ID reason=\"capability_probe: git root unreadable\""
  exit "$_EXIT_PARKED"
fi
_CAPABILITY_DIGEST="$(printf '%s\n%s\n%s' \
  "$_CAPABILITY_NONCE" "$_CAPABILITY_CWD" "$_CAPABILITY_ROOT" \
  | shasum -a 256 | awk '{print $1}')"
_CAPABILITY_EXPECTED="FNO_CAPABILITY_READY:${_CAPABILITY_DIGEST}"
_CAPABILITY_PROMPT="Read-only capability probe. Run pwd and git rev-parse --show-toplevel. Compute SHA-256 over these three newline-separated values with no trailing newline: nonce '${_CAPABILITY_NONCE}', the exact pwd output, and the exact git-root output. Reply with exactly FNO_CAPABILITY_READY:<digest> and nothing else."

_SPAWN_ACCOUNT_ARGS=()
[ -n "$DEST_ACCOUNT" ] && _SPAWN_ACCOUNT_ARGS=(--account "$DEST_ACCOUNT")
[ -n "$DEST_DISPATCH_ACCOUNT" ] && _SPAWN_ACCOUNT_ARGS=(--dispatch-account "$DEST_DISPATCH_ACCOUNT")
_SPAWN_EFFORT_ARGS=()
[ -n "$DEST_EFFORT" ] && _SPAWN_EFFORT_ARGS=(--effort "$DEST_EFFORT")

_ASK_RC=0
_ASK_OUT=""
_ASK_ERR_FILE="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/handoff-spawn-$$.err")"
_ASK_OUT="$(fno agents spawn --substrate pane \
  --harness "$DEST_HARNESS" --model "$DEST_MODEL" \
  ${_SPAWN_ACCOUNT_ARGS[@]+"${_SPAWN_ACCOUNT_ARGS[@]}"} \
  ${_SPAWN_EFFORT_ARGS[@]+"${_SPAWN_EFFORT_ARGS[@]}"} \
  --cwd "$PWD" --name "$CHILD_NAME" "$_CAPABILITY_PROMPT" \
  2>"$_ASK_ERR_FILE")" || _ASK_RC=$?
_ASK_ERR="$(cat "$_ASK_ERR_FILE" 2>/dev/null)"; rm -f "$_ASK_ERR_FILE"

set +o pipefail
_SPAWN_RECEIPT="$(printf '%s\n' "$_ASK_OUT" | jq -c \
  'select(type == "object" and (.name // "") != "")' 2>/dev/null | head -1 || true)"
set -o pipefail
_RECEIPT_NAME="$(printf '%s' "$_SPAWN_RECEIPT" | jq -r '.name // empty' 2>/dev/null || true)"
_RECEIPT_HARNESS="$(printf '%s' "$_SPAWN_RECEIPT" | jq -r '.harness // empty' 2>/dev/null || true)"
_RECEIPT_MODEL="$(printf '%s' "$_SPAWN_RECEIPT" | jq -r '.model // empty' 2>/dev/null || true)"
_RECEIPT_ACCOUNT="$(printf '%s' "$_SPAWN_RECEIPT" | jq -r '.account // .dispatch_account // empty' 2>/dev/null || true)"
_RECEIPT_BOUND="$(printf '%s' "$_SPAWN_RECEIPT" | jq -r '.bound // false' 2>/dev/null || true)"
_RECEIPT_READINESS="$(printf '%s' "$_SPAWN_RECEIPT" | jq -r '.readiness // empty' 2>/dev/null || true)"
CHILD_SID="$(printf '%s' "$_SPAWN_RECEIPT" | jq -r \
  '(.session_id | select(. != "")) // (.short_id | select(. != "")) // empty' \
  2>/dev/null || true)"
_EXPECTED_ACCOUNT="${DEST_ACCOUNT:-$DEST_DISPATCH_ACCOUNT}"

_CAPABILITY_FAILURE=""
if [ "$_ASK_RC" -ne 0 ] || [ -z "$_SPAWN_RECEIPT" ]; then
  _CAPABILITY_FAILURE="spawn rc=$_ASK_RC${_ASK_ERR:+: $(printf '%s' "$_ASK_ERR" | tr '\n' ' ' | cut -c1-160)}"
elif [ "$_RECEIPT_NAME" != "$CHILD_NAME" ]; then
  _CAPABILITY_FAILURE="receipt name $_RECEIPT_NAME != $CHILD_NAME"
elif [ "$_RECEIPT_HARNESS" != "$DEST_HARNESS" ]; then
  _CAPABILITY_FAILURE="receipt harness $_RECEIPT_HARNESS != $DEST_HARNESS"
elif [ "$_RECEIPT_MODEL" != "$DEST_MODEL" ]; then
  _CAPABILITY_FAILURE="receipt model $_RECEIPT_MODEL != $DEST_MODEL"
elif [ -n "$_EXPECTED_ACCOUNT" ] && [ "$_RECEIPT_ACCOUNT" != "$_EXPECTED_ACCOUNT" ]; then
  _CAPABILITY_FAILURE="receipt account $_RECEIPT_ACCOUNT != $_EXPECTED_ACCOUNT"
elif [ "$_RECEIPT_BOUND" != "true" ]; then
  _CAPABILITY_FAILURE="child is not bound"
elif [ "$_RECEIPT_READINESS" != "ready" ]; then
  _CAPABILITY_FAILURE="child readiness is ${_RECEIPT_READINESS:-unknown}, not ready"
elif [ -z "$CHILD_SID" ]; then
  _CAPABILITY_FAILURE="spawn receipt has no child session identity"
fi

_TRUTH_HANDLE="${CHILD_SID:-$CHILD_NAME}"
_CAPABILITY_ELAPSED=0
_CAPABILITY_TIMEOUT="${HANDOFF_CAPABILITY_TIMEOUT:-$VERIFY_TIMEOUT}"
_CAPABILITY_INTERVAL="${HANDOFF_CAPABILITY_INTERVAL:-1}"
while [ -z "$_CAPABILITY_FAILURE" ] && [ "$_CAPABILITY_ELAPSED" -lt "$_CAPABILITY_TIMEOUT" ]; do
  _TRUTH_OUT="$(fno agents truth "$_TRUTH_HANDLE" --json 2>/dev/null || true)"
  _TRUTH_MESSAGE="$(printf '%s' "$_TRUTH_OUT" | jq -r '.last_message // empty' 2>/dev/null || true)"
  _TRUTH_MODEL_KIND="$(printf '%s' "$_TRUTH_OUT" | jq -r '.observed_model.kind // empty' 2>/dev/null || true)"
  _TRUTH_MODEL="$(printf '%s' "$_TRUTH_OUT" | jq -r '.observed_model.model // empty' 2>/dev/null || true)"
  if [ "$_TRUTH_MESSAGE" = "$_CAPABILITY_EXPECTED" ] \
      && [ "$_TRUTH_MODEL_KIND" = "observed" ] \
      && [ "$_TRUTH_MODEL" = "$DEST_MODEL" ]; then
    break
  fi
  sleep "$_CAPABILITY_INTERVAL" 2>/dev/null || true
  _CAPABILITY_ELAPSED=$((_CAPABILITY_ELAPSED + _CAPABILITY_INTERVAL))
done
if [ -z "$_CAPABILITY_FAILURE" ] \
    && { [ "${_TRUTH_MESSAGE:-}" != "$_CAPABILITY_EXPECTED" ] \
      || [ "${_TRUTH_MODEL_KIND:-}" != "observed" ] \
      || [ "${_TRUTH_MODEL:-}" != "$DEST_MODEL" ]; }; then
  _CAPABILITY_FAILURE="truth mismatch: message/model did not prove destination within ${_CAPABILITY_TIMEOUT}s"
fi

if [ -n "$_CAPABILITY_FAILURE" ]; then
  fno agents stop "$CHILD_NAME" >/dev/null 2>&1 || true
  fno agents rm "$CHILD_NAME" >/dev/null 2>&1 || true
  _emit_event "handoff_failed" \
    "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"capability_probe\",\"detail\":\"$(printf '%s' "$_CAPABILITY_FAILURE" | tr '\n' ' ' | cut -c1-300)\"}"
  echo "parked $NODE_ID reason=\"capability_probe: $_CAPABILITY_FAILURE\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 3: Write handoff brief artifact
# Convention: .fno/artifacts/handoff/{boundary}-{session_id}.md
# ---------------------------------------------------------------------------
mkdir -p "$ARTIFACTS_DIR"
BRIEF_FILE="$ARTIFACTS_DIR/${BOUNDARY}-${SESSION_ID}.md"
_TS="$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)"

if [ ! -f "$BRIEF_FILE" ]; then
  cat > "$BRIEF_FILE" <<BRIEFEOF
---
phase: ${BOUNDARY}
session_id: ${SESSION_ID}
timestamp: ${_TS}
generation: ${CHILD_GEN}
from_session: ${SESSION_ID}
node_id: ${NODE_ID}
plan_path: ${PLAN_PATH}
---
# Handoff Brief

This session (generation $((CHILD_GEN-1))) completed the ${BOUNDARY} boundary and
delegated remaining pipeline work to generation ${CHILD_GEN}.

The successor session should re-enter via: /fno:target ${NODE_ID}
with the same worktree, branch, and .fno/ state as this session.

Successor name: ${CHILD_NAME}
BRIEFEOF
fi

# Durable resume receipt (x-c3a2): write the typed, immutable, versioned
# receipt alongside the brief. The brief is human-readable succession context;
# the receipt is the machine-readable EVIDENCE a successor revalidates against
# live claim/HEAD/worktree before any write. Best-effort: a receipt write
# failure (fno unavailable, identity already written) must NOT abort the
# handoff - the brief + `delegated` event remain the primary succession path.
# The receipt reuses the event-journal reducers, so it stores no second copy.
_RECEIPT_HEAD="$(git rev-parse HEAD 2>/dev/null || true)"
_RECEIPT_BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
_RECEIPT_REPO="$(basename "$(git rev-parse --show-toplevel 2>/dev/null)" 2>/dev/null || true)"
_TMP_RECEIPT_ERR="$(mktemp 2>/dev/null || echo "${TMPDIR:-/tmp}/fno-receipt-err.$$")"
if fno do resume receipt write \
      --node "$NODE_ID" \
      --session "$SESSION_ID" \
      --phase "$BOUNDARY" \
      --generation "$CHILD_GEN" \
      --repo "${_RECEIPT_REPO:-footnote}" \
      --worktree "$PWD" \
      --branch "${_RECEIPT_BRANCH:-}" \
      --head "${_RECEIPT_HEAD:-}" \
      --next-verb "/fno:target" \
      --next-target "$NODE_ID" \
      >/dev/null 2>"$_TMP_RECEIPT_ERR"; then
  :
else
  # already_exists is benign on a re-handoff for the same identity; anything
  # else is a warning so a broken receipt writer is greppable, not silent.
  if ! grep -q '"already_exists"' "$_TMP_RECEIPT_ERR" 2>/dev/null; then
    echo "handoff: resume receipt write failed (non-fatal; brief+delegated remain authoritative): $(cat "$_TMP_RECEIPT_ERR" 2>/dev/null)" >&2
  fi
fi
rm -f "$_TMP_RECEIPT_ERR" 2>/dev/null || true

# Builder crumb trail (x-4852): the shared journal carries multiple nodes, so
# select only crumbs correlated to this handoff's node. Summarize that tail for
# the delegation receipt and append the last crumbs to the successor's brief so
# it picks up from the trail instead of re-deriving. Best-effort: a missing,
# rotated, malformed, or legacy uncorrelated row is ignored, never fatal.
_CRUMB_SUMMARY="crumbs: none"
if [ -f "$EVENTS_FILE" ] || [ -f "$EVENTS_FILE.1" ]; then
  set +o pipefail
  # Per-line parse (`-R` + `fromjson?`): a malformed/non-object line mid-file is
  # skipped, not fatal - `jq -c 'select(...)'` aborts at the first bad line and
  # drops every crumb after it (the exact malformed-log case this must tolerate).
  _CRUMBS="$( { [ -f "$EVENTS_FILE.1" ] && cat "$EVENTS_FILE.1"; [ -f "$EVENTS_FILE" ] && cat "$EVENTS_FILE"; } 2>/dev/null \
    | jq -Rc --arg node "$NODE_ID" \
      'fromjson? | select(.type? == "builder_step" and .data.node_id? == $node)' \
      2>/dev/null || true)"
  _CRUMB_N="$(printf '%s' "$_CRUMBS" | grep -c . || true)"
  if [ "${_CRUMB_N:-0}" -gt 0 ]; then
    _CRUMB_LAST="$(printf '%s\n' "$_CRUMBS" | tail -1 | jq -r '.data.outcome // "?"' 2>/dev/null || echo "?")"
    _CRUMB_SUMMARY="crumbs: ${_CRUMB_N} attempts, last outcome=${_CRUMB_LAST}"
    {
      printf '\n## Builder crumb trail (last 5)\n\n'
      printf '%s\n' "$_CRUMBS" | tail -5 \
        | jq -r '"- tried \(.data.tried) - found \(.data.found // "?") - fix \(.data.fix // "?") -> \(.data.outcome)"' 2>/dev/null
    } >> "$BRIEF_FILE" 2>/dev/null || true
  fi
  set -o pipefail
fi

# ---------------------------------------------------------------------------
# Step 3: Reserve dispatch:<node> claim (bridge token)
# ---------------------------------------------------------------------------
DISPATCH_KEY="dispatch:$NODE_ID"
DISPATCH_HOLDER="handoff:$SESSION_ID"

_DISPATCH_RC=0
FNO_CLAIMS_ROOT="$HOME" fno agents claim acquire "$DISPATCH_KEY" \
  --holder "$DISPATCH_HOLDER" --ttl 3m \
  --reason "handoff bridge for $SESSION_ID" >/dev/null 2>&1 || _DISPATCH_RC=$?

if [ "$_DISPATCH_RC" -ne 0 ]; then
  # Someone else holds the dispatch reservation
  echo "parked $NODE_ID reason=\"dispatch reservation $DISPATCH_KEY held by another party (rc=$_DISPATCH_RC)\""
  exit "$_EXIT_PARKED"
fi

# From this point, unwind must release the dispatch reservation on failure.

# ---------------------------------------------------------------------------
# Step 4: Archive target-state.md to {plan_path}.artifacts/
# ---------------------------------------------------------------------------
PLAN_ARTIFACTS_DIR="${PLAN_PATH}.artifacts"
mkdir -p "$PLAN_ARTIFACTS_DIR"
ARCHIVED_STATE="$PLAN_ARTIFACTS_DIR/target-state-${SESSION_ID}.md"

_ARCHIVE_RC=0
mv "$STATE_FILE" "$ARCHIVED_STATE" 2>/dev/null || _ARCHIVE_RC=$?

if [ "$_ARCHIVE_RC" -ne 0 ]; then
  # Unwind: release dispatch reservation
  FNO_CLAIMS_ROOT="$HOME" fno agents claim release "$DISPATCH_KEY" \
    --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true
  echo "parked $NODE_ID reason=\"failed to archive manifest (rc=$_ARCHIVE_RC)\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 5: Release node claim
# ---------------------------------------------------------------------------
_RELEASE_RC=0
FNO_CLAIMS_ROOT="$HOME" fno agents claim release "node:$NODE_ID" \
  --holder "$CLAIM_HOLDER" >/dev/null 2>&1 || _RELEASE_RC=$?

if [ "$_RELEASE_RC" -ne 0 ]; then
  # Unwind: restore manifest, release dispatch reservation
  _RESTORE_RC=0
  mv "$ARCHIVED_STATE" "$STATE_FILE" 2>/dev/null || _RESTORE_RC=$?
  FNO_CLAIMS_ROOT="$HOME" fno agents claim release "$DISPATCH_KEY" \
    --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true

  _emit_event "handoff_failed" \
    "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"release_failed\",\"detail\":\"claim release exited $_RELEASE_RC\"}"

  if [ "$_RESTORE_RC" -ne 0 ]; then
    # Archive is gone AND restore failed: unrecoverable
    echo "handoff-restore-failed $NODE_ID reason=\"release_failed + restore_failed\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  echo "parked $NODE_ID reason=\"claim release failed (rc=$_RELEASE_RC)\""
  exit "$_EXIT_PARKED"
fi

# From this point, the parent's claim is released. Any failure that cannot
# restore the manifest MUST exit 12.

# ---------------------------------------------------------------------------
# Step 7: Submit the target command to the proven successor
# ---------------------------------------------------------------------------
# Build command: inject the refusal flag when auto_merge_approved != true.
# The flag is the attributable carrier in the command; the exported env var is
# the MECHANICAL carrier (x-9d11): the successor's init folds TARGET_NO_MERGE
# even if it never passes the flag through, so a post-compaction or thin-skinned
# child cannot inherit merge authority the parent refused.
SPAWN_FLAGS=""
if [ "$AUTO_MERGE_APPROVED" != "true" ]; then
  SPAWN_FLAGS="--no-merge"
  export TARGET_NO_MERGE=1
else
  # Clear-on-absent like every sibling setter (review round 7): a carrier
  # leaked into this session's env must not outlive a manifest that granted
  # merge authority.
  unset TARGET_NO_MERGE
fi
if [ -n "$TARGET_SIZE" ]; then
  SPAWN_FLAGS="$SPAWN_FLAGS $TARGET_SIZE"
fi
SPAWN_FLAGS="$(printf '%s' "$SPAWN_FLAGS" | sed 's/^[[:space:]]*//' | sed 's/[[:space:]]*$//')"

case "$DEST_HARNESS" in
  codex) _TARGET_VERB='$fno:target' ;;
  opencode) _TARGET_VERB='/fno:target' ;;
  *) _TARGET_VERB='/fno:target' ;;
esac
if [ -n "$SPAWN_FLAGS" ]; then
  CHILD_CMD="$_TARGET_VERB $SPAWN_FLAGS $NODE_ID"
else
  CHILD_CMD="$_TARGET_VERB $NODE_ID"
fi

_ASK_RC=0
_ASK_OUT=""
# The child already proved it can infer and use this worktree. Submit only after
# the parent claim is released, so the child's target init can acquire the node.
_ASK_ERR_FILE="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/handoff-spawn-$$.err")"
_ASK_OUT="$(fno agents mail send "$_TRUTH_HANDLE" --raw "$CHILD_CMD" 2>"$_ASK_ERR_FILE")" || _ASK_RC=$?
_ASK_ERR="$(cat "$_ASK_ERR_FILE" 2>/dev/null)"; rm -f "$_ASK_ERR_FILE"

# Only a nonzero raw-submit rc is a delivery failure. Task execution is proven
# separately after this step; a send receipt is never treated as consumption.
if [ "$_ASK_RC" -ne 0 ] || printf '%s\n' "$_ASK_OUT" | grep -q '"status":[[:space:]]*"refused"'; then
  fno agents stop "$CHILD_NAME" >/dev/null 2>&1 || true
  fno agents rm "$CHILD_NAME" >/dev/null 2>&1 || true
  # Target-seed failure: unwind in order
  #   (a) re-acquire node:<id> FIRST; capture the rc
  _REACQ_RC=0
  FNO_CLAIMS_ROOT="$HOME" fno agents claim acquire "node:$NODE_ID" \
    --holder "$CLAIM_HOLDER" --ttl "$CLAIM_TTL" >/dev/null 2>&1 || _REACQ_RC=$?

  if [ "$_REACQ_RC" -ne 0 ]; then
    # Re-acquire failed: another worker may now hold the claim.
    # Do NOT restore the manifest (leave it archived so this session closes
    # safely). Release the dispatch reservation and exit 12.
    FNO_CLAIMS_ROOT="$HOME" fno agents claim release "$DISPATCH_KEY" \
      --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"reacquire_failed\",\"detail\":\"target_seed + re-acquire node:$NODE_ID failed (rc=$_REACQ_RC); claim may be held by another worker\"}"
    echo "handoff-claim-lost $NODE_ID reason=\"re-acquire failed after target_seed; claim may be held by another worker - parent must NOT continue this node\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  #   (b) restore archived manifest
  _RESTORE_RC=0
  mv "$ARCHIVED_STATE" "$STATE_FILE" 2>/dev/null || _RESTORE_RC=$?

  #   (c) release dispatch reservation
  FNO_CLAIMS_ROOT="$HOME" fno agents claim release "$DISPATCH_KEY" \
    --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true

  _FAIL_DETAIL="target seed rc=$_ASK_RC${_ASK_ERR:+: $(printf '%s' "$_ASK_ERR" | tr '\n' ' ' | cut -c1-160)}"

  if [ "$_RESTORE_RC" -ne 0 ]; then
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"restore_failed\",\"detail\":\"target_seed + restore mv failed\"}"
    echo "handoff-restore-failed $NODE_ID reason=\"target_seed + restore_failed\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  _emit_event "handoff_failed" \
    "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"target_seed\",\"detail\":\"${_FAIL_DETAIL}\"}"

  echo "parked $NODE_ID reason=\"target_seed: $_FAIL_DETAIL\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 8: Prove the child executed target and owns the new manifest
# ---------------------------------------------------------------------------
_VERIFY_ELAPSED=0
_CHILD_TARGET_READY=0
_CHILD_EXPECTED_HOLDER="target-session:$CHILD_SID"
while [ "$_VERIFY_ELAPSED" -lt "$VERIFY_TIMEOUT" ]; do
  _CHILD_CLAIM="$(FNO_CLAIMS_ROOT="$HOME" fno agents claim status "node:$NODE_ID" 2>/dev/null || true)"
  _CHILD_HOLDER="$(printf '%s' "$_CHILD_CLAIM" | jq -r '.holder // empty' 2>/dev/null || true)"
  _CHILD_MANIFEST_SESSION=""
  _CHILD_MANIFEST_NODE=""
  if [ -f "$STATE_FILE" ]; then
    _CHILD_MANIFEST_SESSION="$(_parse_manifest_field "$STATE_FILE" "harness_session_id")"
    [ -n "$_CHILD_MANIFEST_SESSION" ] \
      || _CHILD_MANIFEST_SESSION="$(_parse_manifest_field "$STATE_FILE" "claude_session_id")"
    _CHILD_MANIFEST_NODE="$(_validate_node_id "$(_parse_body_field "$STATE_FILE" "graph_node_id")")"
  fi
  if [ "$_CHILD_HOLDER" = "$_CHILD_EXPECTED_HOLDER" ] \
      && [ "$_CHILD_MANIFEST_SESSION" = "$CHILD_SID" ] \
      && [ "$_CHILD_MANIFEST_NODE" = "$NODE_ID" ]; then
    _CHILD_TARGET_READY=1
    break
  fi
  sleep "$VERIFY_INTERVAL" 2>/dev/null || true
  _VERIFY_ELAPSED=$((_VERIFY_ELAPSED + VERIFY_INTERVAL))
done

if [ "$_CHILD_TARGET_READY" -eq 0 ]; then
  fno agents stop "$CHILD_NAME" >/dev/null 2>&1 || true
  FNO_CLAIMS_ROOT="$HOME" fno agents claim release "node:$NODE_ID" \
    --holder "$_CHILD_EXPECTED_HOLDER" >/dev/null 2>&1 || true
  fno agents rm "$CHILD_NAME" >/dev/null 2>&1 || true

  _REACQ_RC=0
  FNO_CLAIMS_ROOT="$HOME" fno agents claim acquire "node:$NODE_ID" \
    --holder "$CLAIM_HOLDER" --ttl "$CLAIM_TTL" >/dev/null 2>&1 || _REACQ_RC=$?
  if [ "$_REACQ_RC" -ne 0 ]; then
    FNO_CLAIMS_ROOT="$HOME" fno agents claim release "$DISPATCH_KEY" \
      --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"reacquire_failed\",\"detail\":\"target_execution + re-acquire node:$NODE_ID failed (rc=$_REACQ_RC)\"}"
    echo "handoff-claim-lost $NODE_ID reason=\"re-acquire failed after target_execution; parent must NOT continue this node\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  _RESTORE_RC=0
  rm -f "$STATE_FILE" 2>/dev/null || true
  mv "$ARCHIVED_STATE" "$STATE_FILE" 2>/dev/null || _RESTORE_RC=$?
  FNO_CLAIMS_ROOT="$HOME" fno agents claim release "$DISPATCH_KEY" \
    --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true
  if [ "$_RESTORE_RC" -ne 0 ]; then
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"restore_failed\",\"detail\":\"target_execution + restore mv failed\"}"
    echo "handoff-restore-failed $NODE_ID reason=\"target_execution + restore_failed\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  _emit_event "handoff_failed" \
    "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"target_execution\",\"detail\":\"child claim/manifest proof missing or mismatched after ${VERIFY_TIMEOUT}s\"}"
  echo "parked $NODE_ID reason=\"target_execution: child claim/manifest proof missing or mismatched\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 8: Commit the delegation
# ---------------------------------------------------------------------------

# 8a. Emit delegated only after capability and target-execution proof.
_CHILD_SESSION="$CHILD_SID"
_emit_event "delegated" \
  "{\"node_id\":\"$NODE_ID\",\"from_session\":\"$SESSION_ID\",\"to_session\":\"$CHILD_NAME\",\"child_session\":\"$_CHILD_SESSION\",\"boundary\":\"$BOUNDARY\",\"generation\":$CHILD_GEN,\"harness\":\"$DEST_HARNESS\",\"model\":\"$DEST_MODEL\",\"account\":\"${DEST_ACCOUNT:-$DEST_DISPATCH_ACCOUNT}\",\"handoff_kind\":\"capability_escalation\"}"

# 8b. Emit session_satisfied (trigger=delegated)
# Compute gate_state_hash from archived manifest (sha256 of the file, or "none")
_GATE_HASH="none"
if [ -f "$ARCHIVED_STATE" ]; then
  set +o pipefail
  _GATE_HASH="$(shasum -a 256 "$ARCHIVED_STATE" 2>/dev/null | awk '{print $1}' || true)"
  set -o pipefail
  [ -z "$_GATE_HASH" ] && _GATE_HASH="none"
fi

_emit_event "session_satisfied" \
  "{\"source\":\"delegated\",\"reason\":\"do-phase delegated to $CHILD_NAME\",\"session_id\":\"$SESSION_ID\",\"gate_state_hash\":\"$_GATE_HASH\"}"

# 8c. Delegating session's ledger session-record (step 6, ab-f8e5f214 / AC7-EDGE).
# The manifest was archived in Step 4, so the stop-hook shim's finalize cannot
# read it (and in fact the shim exits early on the now-missing manifest). Write
# the paper-trail row HERE via the `finalize` verb against the ARCHIVED manifest,
# with termination_reason=delegated. `delegated` is a non-ship reason, so finalize
# writes ONLY the ledger row (stamp/graduate/handoff stay the SUCCESSOR's job)
# and emits session_finalized for observability. Best-effort: failure never
# blocks the committed delegation. Resolve fno-agents the same way the shim does.
_ABI_AGENTS_BIN=""
if [ -n "${FNO_AGENTS_BIN:-}" ] && [ -x "${FNO_AGENTS_BIN}" ]; then
  _ABI_AGENTS_BIN="$FNO_AGENTS_BIN"
else
  _REPO_ROOT="$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
  if [ -x "${_REPO_ROOT}/crates/fno-agents/target/release/fno-agents" ]; then
    _ABI_AGENTS_BIN="${_REPO_ROOT}/crates/fno-agents/target/release/fno-agents"
  elif [ -x "${_REPO_ROOT}/crates/fno-agents/target/debug/fno-agents" ]; then
    _ABI_AGENTS_BIN="${_REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
  elif command -v fno-agents >/dev/null 2>&1; then
    _ABI_AGENTS_BIN="$(command -v fno-agents)"
  fi
fi
if [ -n "$_ABI_AGENTS_BIN" ]; then
  "$_ABI_AGENTS_BIN" finalize \
    --state "$ARCHIVED_STATE" \
    --cwd "$PWD" \
    --reason delegated \
    --events "$EVENTS_FILE" \
    >>"$FNO_DIR/finalize.stderr.log" 2>&1 \
    || echo "handoff: WARN: finalize (delegated ledger record) exited non-zero; paper-trail row may be missing (non-blocking)" >&2
else
  echo "handoff: WARN: fno-agents binary not found; skipping delegated ledger record (non-blocking)" >&2
fi

# 8d. Best-effort: append session_id to plan frontmatter session_ids (either form)
python3 - "$PLAN_PATH" "$SESSION_ID" 2>/dev/null <<'PYEOF'
import sys, re

plan_path = sys.argv[1]
sid = sys.argv[2]

try:
    with open(plan_path, 'r') as f:
        content = f.read()

    # Find frontmatter block (between first two ---)
    fm_match = re.match(r'^(---\n)(.*?)(---\n)', content, re.DOTALL)
    if not fm_match:
        sys.exit(0)

    fm = fm_match.group(2)
    rest = content[fm_match.end():]

    # Check if session_ids field exists
    sids_match = re.search(r'^session_ids:(.*)$', fm, re.MULTILINE)
    if sids_match:
        current = sids_match.group(1).strip()
        # Parse inline list: [a, b] or just append
        if current == '':
            # Block form: a bare `session_ids:` header with indented `- id`
            # children. Rewriting the header as an inline list would strand
            # those children underneath it, which is malformed YAML AND drops
            # every id already recorded. Append a sibling child instead; with
            # no children this still produces a valid one-item block list.
            fm_lines = fm.splitlines(True)
            idx = fm[:sids_match.start()].count('\n')
            end = idx + 1
            while end < len(fm_lines) and (
                fm_lines[end].startswith((' ', '\t')) or not fm_lines[end].strip()
            ):
                end += 1
            while end > idx + 1 and not fm_lines[end - 1].strip():
                end -= 1
            fm_lines.insert(end, '  - ' + sid + '\n')
            new_fm = ''.join(fm_lines)
            new_content = '---\n' + new_fm + '---\n' + rest
            with open(plan_path, 'w') as f:
                f.write(new_content)
            sys.exit(0)
        if current.startswith('[') and current.endswith(']'):
            inner = current[1:-1].strip()
            if inner:
                new_val = '[' + inner + ', ' + sid + ']'
            else:
                new_val = '[' + sid + ']'
        else:
            # scalar or unknown: wrap both
            new_val = '[' + current.strip() + ', ' + sid + ']'
        new_fm = re.sub(r'^session_ids:.*$', 'session_ids: ' + new_val, fm, flags=re.MULTILINE)
    else:
        # Append new field
        new_fm = fm + 'session_ids: [' + sid + ']\n'

    new_content = '---\n' + new_fm + '---\n' + rest
    with open(plan_path, 'w') as f:
        f.write(new_content)
except Exception as e:
    print('handoff: warn: failed to update session_ids in plan: ' + str(e), file=sys.stderr)
    sys.exit(0)
PYEOF
true  # python3 best-effort; rc ignored

# 8e. Touch per-session sentinel; clear any PreCompact arming marker (guard c).
touch "$SENTINEL"
rm -f "$FNO_DIR/.handoff-armed-$SESSION_ID"

# ---------------------------------------------------------------------------
# Step 8 complete: print delegated line (step 9 is the calling LLM's job)
# ---------------------------------------------------------------------------
# Crumb-trail annotation (x-4852): operator-visible, printed BEFORE the strict
# `delegated ...` decision line so the parser (which keys on that prefix) is
# unaffected. The successor inherits the same worktree events.jsonl + the brief.
echo "$_CRUMB_SUMMARY"
echo "delegated $NODE_ID child=$CHILD_NAME session=$_CHILD_SESSION generation=$CHILD_GEN"
exit 0
