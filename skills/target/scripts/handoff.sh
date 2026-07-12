#!/usr/bin/env bash
# handoff.sh - Target session succession protocol helper.
#
# Executes the ordered 8-step handoff sequence (sec 2.1 of the design doc)
# as a single atomic invocation. The calling LLM session invokes this helper;
# the helper performs all state mutations; the LLM performs only step 9 (close).
#
# Usage:
#   handoff.sh --boundary <blueprint-do|wave> [--flags "<modifiers>"]
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
BOUNDARY=""
CHILD_FLAGS=""

while [ $# -gt 0 ]; do
  case "$1" in
    --boundary)
      BOUNDARY="${2:-}"
      shift 2
      ;;
    --flags)
      CHILD_FLAGS="${2:-}"
      shift 2
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

if [ -z "$BOUNDARY" ]; then
  echo "usage: handoff.sh --boundary <blueprint-do|wave> [--flags \"<modifiers>\"]" >&2
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
if [ -f "$_CONFIG_SH" ]; then
  # Set LOCAL_SETTINGS relative to FNO_DIR so config works in sandbox
  LOCAL_SETTINGS="$FNO_DIR/config.toml"
  source "$_CONFIG_SH"
  GENERATION_CAP=$(get_config "target.handoff.generation_cap" "4")
  USED_PCT_TRIGGER=$(get_config "target.handoff.used_pct_trigger" "50")
  HANDOFF_ENABLED=$(get_config "target.handoff.enabled" "true")
else
  GENERATION_CAP="4"
  USED_PCT_TRIGGER="50"
  HANDOFF_ENABLED="true"
fi

# ---------------------------------------------------------------------------
# Helper: emit an event to events.jsonl
# Accepts: emit_event <type> <json-data>
# fno event emit is the PRIMARY writer (validates kind, takes the file lock).
# The direct printf append is the FALLBACK only when fno exits nonzero
# (stale binary, unknown kind, daemon unavailable).
# The preflight at step 1 guarantees fno can emit `delegated`, so in the
# normal path fno always succeeds and printf never runs, preventing double-writes
# that would corrupt the generation count and lineage chain.
# ---------------------------------------------------------------------------
_emit_event() {
  local etype="$1"
  local edata="$2"
  local ts
  ts="$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)"
  # fno is primary; printf is fallback only when fno fails.
  # Always pass --source target so the envelope is correct even when
  # target-state.md has already been archived (step 4 happens before step 8).
  if ! fno event emit --type "$etype" --data "$edata" --events "$EVENTS_FILE" \
       --source target >/dev/null 2>&1; then
    echo "handoff: WARN: fno event emit failed for type '$etype'; using printf fallback" >&2
    printf '{"ts":"%s","type":"%s","source":"target","data":%s}\n' \
      "$ts" "$etype" "$edata" >> "$EVENTS_FILE" 2>/dev/null \
      || echo "handoff: WARN: printf fallback append also failed for type '$etype'" >&2
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
# Current key is claude_session_id; fall back to the pre-rename claude_transcript_id
# for one release so an in-flight manifest written by an older binary still resolves.
TRANSCRIPT_ID="$(_parse_manifest_field "$STATE_FILE" "claude_session_id")"
[ -n "$TRANSCRIPT_ID" ] || TRANSCRIPT_ID="$(_parse_manifest_field "$STATE_FILE" "claude_transcript_id")"
NODE_ID="$(_validate_node_id "$(_parse_body_field "$STATE_FILE" "graph_node_id")")"
CLAIM_KEY="$(_parse_body_field "$STATE_FILE" "target_claim_key")"
CLAIM_HOLDER="$(_parse_body_field "$STATE_FILE" "target_claim_holder")"
_CLAIM_TTL_RAW="$(_parse_body_field "$STATE_FILE" "target_claim_ttl")"
CLAIM_TTL="${_CLAIM_TTL_RAW:-2h}"

# Owning harness of THIS (parent) session, from the ambient session markers in
# the same precedence order as cli/src/fno/harness_identity.py (x-efc7). The
# successor name is namespaced by it (tgt-<node>-<harness>-gN) so two dispatchers
# on different harnesses cannot collide on one registry name (x-3e70: codex's
# self-handoff died on `agent tgt-fc7-g2 already exists` after abi-loop dispatched
# a claude worker of the same name). Unknown/no-marker defaults to `claude`,
# which both preserves the legacy claude lineage and stays distinct from a codex
# lineage's name.
# Default-then-strip (whitespace-only markers count as unset, matching the
# Rust/Python resolvers' .trim()/.strip()); the `:-` default keeps the `//`
# expansion safe under `set -u` on an unset marker.
_cx_m="${CODEX_THREAD_ID:-}"; _cl_m="${CLAUDE_CODE_SESSION_ID:-}"
_cs_m="${CODEX_SESSION_ID:-}"; _gm_m="${GEMINI_SESSION_ID:-}"
if [ -n "${_cx_m//[[:space:]]/}" ]; then _HARNESS="codex"
elif [ -n "${_cl_m//[[:space:]]/}" ]; then _HARNESS="claude"
elif [ -n "${_cs_m//[[:space:]]/}" ]; then _HARNESS="codex"
elif [ -n "${_gm_m//[[:space:]]/}" ]; then _HARNESS="gemini"
else _HARNESS="claude"
fi

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

# plan frontmatter status: ready|in_progress|shipped (treat unknown as refuse)
set +o pipefail
PLAN_STATUS="$(grep -E "^status:[[:space:]]" "$PLAN_PATH" 2>/dev/null \
  | head -1 | sed 's/^status:[[:space:]]*//' | tr -d '"' | tr -d "'" | tr -d ' ' || true)"
set -o pipefail
case "$PLAN_STATUS" in
  ready|in_progress|shipped) ;;
  *)
    echo "parked $NODE_ID reason=\"plan status '$PLAN_STATUS' is not ready/in_progress/shipped\""
    exit "$_EXIT_PARKED"
    ;;
esac

# graph_node_id was already validated and guarded at Step 0; NODE_ID is not
# reassigned between there and here, so no second missing-id check is needed.

# Caller must hold node:<id> claim
set +o pipefail
_CLAIM_STATUS_OUT="$(FNO_CLAIMS_ROOT="$HOME" fno claim status "node:$NODE_ID" 2>/dev/null || true)"
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

# Generation cap check
# child_gen = 2 + (count of THIS harness's `delegated` events for this node_id).
# Scoping the count to (node, harness) keeps each lineage's generation monotonic
# and its cap independent - a codex lineage's handoffs never consume a claude
# lineage's budget (x-3e70). Pre-change events carry no harness field and so do
# not match, which is correct: their names lacked the harness infix too, so a
# reset count can never re-mint an existing name.
_PRIOR_COUNT=0
if [ -f "$EVENTS_FILE" ]; then
  set +o pipefail
  _PRIOR_COUNT="$(grep '"type":"delegated"' "$EVENTS_FILE" 2>/dev/null \
    | grep "\"node_id\":\"${NODE_ID}\"" 2>/dev/null \
    | grep "\"harness\":\"${_HARNESS}\"" 2>/dev/null \
    | wc -l | tr -d ' ' || echo 0)"
  set -o pipefail
fi
CHILD_GEN="$((2 + _PRIOR_COUNT))"
if [ "$CHILD_GEN" -gt "$GENERATION_CAP" ]; then
  echo "parked $NODE_ID reason=\"chain-exhausted: generation $CHILD_GEN exceeds cap $GENERATION_CAP\""
  exit "$_EXIT_PARKED"
fi

# Pressure boundary check (wave only)
if [ "$BOUNDARY" = "wave" ]; then
  # Resolve transcript path
  _TRANSCRIPT_PATH=""
  if [ -n "$TRANSCRIPT_ID" ]; then
    # Claude Code project-dir encoding: both / and . in the cwd path are
    # replaced by - to form the directory name under ~/.claude/projects/.
    # Pure parameter expansion (bash 3.2+): single-pass bracket-class replacement.
    _ENCODED_CWD="${PWD//[\/.]/-}"
    _TRANSCRIPT_PATH="$HOME/.claude/projects/$_ENCODED_CWD/$TRANSCRIPT_ID.jsonl"
  fi

  # Locate context-probe.sh (same script directory, or on PATH)
  _PROBE_SCRIPT="$_SCRIPT_DIR/context-probe.sh"
  if [ ! -f "$_PROBE_SCRIPT" ]; then
    _PROBE_SCRIPT="$(command -v context-probe.sh 2>/dev/null || true)"
  fi

  _PROBE_EXIT=3
  _PROBE_OUT=""
  if [ -n "$_PROBE_SCRIPT" ] && [ -f "$_PROBE_SCRIPT" ] && [ -n "$_TRANSCRIPT_PATH" ]; then
    _PROBE_OUT="$(bash "$_PROBE_SCRIPT" "$_TRANSCRIPT_PATH" 2>/dev/null)" || true
    _PROBE_EXIT=$?
  fi

  if [ "$_PROBE_EXIT" -ne 0 ]; then
    # Emit handoff_probe_unreadable and park
    _emit_event "handoff_probe_unreadable" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"probe_exit\":$_PROBE_EXIT,\"transcript_path\":\"${_TRANSCRIPT_PATH:-}\"}"
    echo "parked $NODE_ID reason=\"no-pressure: probe returned exit $_PROBE_EXIT (unreadable)\""
    exit "$_EXIT_PARKED"
  fi

  set +o pipefail
  _USED_PCT="$(printf '%s' "$_PROBE_OUT" | jq -r '.used_pct // 0' 2>/dev/null || echo 0)"
  set -o pipefail
  if [ "$_USED_PCT" -lt "$USED_PCT_TRIGGER" ]; then
    echo "parked $NODE_ID reason=\"no-pressure: used_pct=$_USED_PCT < trigger=$USED_PCT_TRIGGER\""
    exit "$_EXIT_PARKED"
  fi
fi

# Emit-capability preflight: emit `delegated` kind against a throwaway temp file
_TEMP_EVENTS="$(mktemp)"
_PREFLIGHT_OK=1
if ! fno event emit --type "delegated" \
      --data "{\"node_id\":\"$NODE_ID\",\"from_session\":\"$SESSION_ID\",\"to_session\":\"preflight\",\"boundary\":\"$BOUNDARY\",\"generation\":$CHILD_GEN}" \
      --events "$_TEMP_EVENTS" >/dev/null 2>&1; then
  _PREFLIGHT_OK=0
fi
rm -f "$_TEMP_EVENTS"

if [ "$_PREFLIGHT_OK" -eq 0 ]; then
  echo "parked $NODE_ID reason=\"emit preflight failed for 'delegated' kind; stale installed fno? run: fno update\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 2: Write handoff brief artifact
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

Successor name: tgt-${NODE_ID:3:8}-${_HARNESS}-g${CHILD_GEN}
BRIEFEOF
fi

# ---------------------------------------------------------------------------
# Step 3: Reserve dispatch:<node> claim (bridge token)
# ---------------------------------------------------------------------------
DISPATCH_KEY="dispatch:$NODE_ID"
DISPATCH_HOLDER="handoff:$SESSION_ID"

_DISPATCH_RC=0
FNO_CLAIMS_ROOT="$HOME" fno claim acquire "$DISPATCH_KEY" \
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
  FNO_CLAIMS_ROOT="$HOME" fno claim release "$DISPATCH_KEY" \
    --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true
  echo "parked $NODE_ID reason=\"failed to archive manifest (rc=$_ARCHIVE_RC)\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 5: Release node claim
# ---------------------------------------------------------------------------
_RELEASE_RC=0
FNO_CLAIMS_ROOT="$HOME" fno claim release "node:$NODE_ID" \
  --holder "$CLAIM_HOLDER" >/dev/null 2>&1 || _RELEASE_RC=$?

if [ "$_RELEASE_RC" -ne 0 ]; then
  # Unwind: restore manifest, release dispatch reservation
  _RESTORE_RC=0
  mv "$ARCHIVED_STATE" "$STATE_FILE" 2>/dev/null || _RESTORE_RC=$?
  FNO_CLAIMS_ROOT="$HOME" fno claim release "$DISPATCH_KEY" \
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
# Step 6: Spawn successor
# ---------------------------------------------------------------------------
# Child name: tgt-<node-8hex-suffix>-<harness>-g<child_gen>. The harness infix
# namespaces the name by the parent lineage so two dispatchers on different
# harnesses cannot collide on one registry name (x-3e70).
_NODE_8HEX="${NODE_ID:3:8}"
CHILD_NAME="tgt-${_NODE_8HEX}-${_HARNESS}-g${CHILD_GEN}"

# Build command: inject no-merge when auto_merge_approved != true
SPAWN_FLAGS=""
if [ "$AUTO_MERGE_APPROVED" != "true" ]; then
  SPAWN_FLAGS="no-merge"
fi
if [ -n "$TARGET_SIZE" ]; then
  SPAWN_FLAGS="$SPAWN_FLAGS $TARGET_SIZE"
fi
# User-supplied flags override/extend
if [ -n "$CHILD_FLAGS" ]; then
  SPAWN_FLAGS="$SPAWN_FLAGS $CHILD_FLAGS"
fi
SPAWN_FLAGS="$(printf '%s' "$SPAWN_FLAGS" | sed 's/^[[:space:]]*//' | sed 's/[[:space:]]*$//')"

if [ -n "$SPAWN_FLAGS" ]; then
  CHILD_CMD="/fno:target $SPAWN_FLAGS $NODE_ID"
else
  CHILD_CMD="/fno:target $NODE_ID"
fi

_ASK_RC=0
_ASK_OUT=""
# Group 1 (ab-8b3e4fe0): creation moved off `ask` - `spawn --provider claude`
# builds the same `claude --bg --name` launch (subscription lane) and prints a
# compact JSON receipt {"name", "short_id", "provider", "status"}. stderr goes
# to a temp file, NOT 2>&1: a stderr warning must never pollute the JSON
# receipt parse below (house rule; gemini review PR #457).
_ASK_ERR_FILE="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/handoff-spawn-$$.err")"
_ASK_OUT="$(fno agents spawn --provider claude --cwd "$PWD" "$CHILD_NAME" "$CHILD_CMD" 2>"$_ASK_ERR_FILE")" || _ASK_RC=$?
_ASK_ERR="$(cat "$_ASK_ERR_FILE" 2>/dev/null)"; rm -f "$_ASK_ERR_FILE"

# Parse short_id from the clean-stdout JSON receipt line (grep first as
# defense in depth). grep/jq exit nonzero on no match; protect against
# pipefail propagation.
set +o pipefail
CHILD_SID="$(printf '%s\n' "$_ASK_OUT" | grep -F '"short_id"' | head -1 | jq -r '.short_id // empty' 2>/dev/null || true)"
set -o pipefail

# Only a nonzero launch rc is a spawn failure. A clean rc with an empty
# CHILD_SID (unparseable/truncated receipt) is NOT a failure: the child may be
# live. It falls through to Step 7's name-keyed registry poll, the authoritative
# liveness oracle (Locked Decision 1) - the receipt short_id is audit data, not
# launch proof. Conflating the two used to park the parent while a receiptless
# child kept running, splitting the branch across two workers (x-1adb).
if [ "$_ASK_RC" -ne 0 ]; then
  # Spawn failure: unwind in order
  #   (a) re-acquire node:<id> FIRST; capture the rc
  _REACQ_RC=0
  FNO_CLAIMS_ROOT="$HOME" fno claim acquire "node:$NODE_ID" \
    --holder "$CLAIM_HOLDER" --ttl "$CLAIM_TTL" >/dev/null 2>&1 || _REACQ_RC=$?

  if [ "$_REACQ_RC" -ne 0 ]; then
    # Re-acquire failed: another worker may now hold the claim.
    # Do NOT restore the manifest (leave it archived so this session closes
    # safely). Release the dispatch reservation and exit 12.
    FNO_CLAIMS_ROOT="$HOME" fno claim release "$DISPATCH_KEY" \
      --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"reacquire_failed\",\"detail\":\"spawn_failed + re-acquire node:$NODE_ID failed (rc=$_REACQ_RC); claim may be held by another worker\"}"
    echo "handoff-claim-lost $NODE_ID reason=\"re-acquire failed after spawn_failed; claim may be held by another worker - parent must NOT continue this node\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  #   (b) restore archived manifest
  _RESTORE_RC=0
  mv "$ARCHIVED_STATE" "$STATE_FILE" 2>/dev/null || _RESTORE_RC=$?

  #   (c) release dispatch reservation
  FNO_CLAIMS_ROOT="$HOME" fno claim release "$DISPATCH_KEY" \
    --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true

  _FAIL_DETAIL="spawn rc=$_ASK_RC${_ASK_ERR:+: $(printf '%s' "$_ASK_ERR" | tr '\n' ' ' | cut -c1-160)}"

  if [ "$_RESTORE_RC" -ne 0 ]; then
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"restore_failed\",\"detail\":\"spawn_failed + restore mv failed\"}"
    echo "handoff-restore-failed $NODE_ID reason=\"spawn_failed + restore_failed\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  _emit_event "handoff_failed" \
    "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"spawn_failed\",\"detail\":\"${_FAIL_DETAIL}\"}"

  echo "parked $NODE_ID reason=\"spawn failed: $_FAIL_DETAIL\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 7: Verify child registered in registry (poll up to VERIFY_TIMEOUT)
# ---------------------------------------------------------------------------
_VERIFY_ELAPSED=0
_CHILD_LIVE=0
while [ "$_VERIFY_ELAPSED" -lt "$VERIFY_TIMEOUT" ]; do
  set +o pipefail
  _LIST_ROW="$(fno agents list 2>/dev/null \
    | jq -c --arg n "$CHILD_NAME" '.agents[]? | select(.name==$n)' 2>/dev/null \
    | head -1 || true)"
  _LIST_STATUS="$(printf '%s' "$_LIST_ROW" | jq -r '.status // empty' 2>/dev/null || true)"
  set -o pipefail
  if [ "$_LIST_STATUS" = "live" ]; then
    _CHILD_LIVE=1
    # Backfill child identity from the live registry row when the spawn receipt
    # yielded no short_id, so a receiptless-but-live child commits as a real
    # delegation instead of parking (x-1adb). to_session stays CHILD_NAME; a row
    # that also lacks short_id/session_id leaves CHILD_SID empty -> "unknown".
    if [ -z "$CHILD_SID" ]; then
      set +o pipefail
      CHILD_SID="$(printf '%s' "$_LIST_ROW" | jq -r '.short_id // .session_id // empty' 2>/dev/null || true)"
      set -o pipefail
    fi
    break
  fi
  sleep "$VERIFY_INTERVAL" 2>/dev/null || true
  _VERIFY_ELAPSED=$((_VERIFY_ELAPSED + VERIFY_INTERVAL))
done

if [ "$_CHILD_LIVE" -eq 0 ]; then
  # Verify failed: unwind in order
  #   (a) re-acquire node:<id> FIRST; capture the rc
  _REACQ_RC=0
  FNO_CLAIMS_ROOT="$HOME" fno claim acquire "node:$NODE_ID" \
    --holder "$CLAIM_HOLDER" --ttl "$CLAIM_TTL" >/dev/null 2>&1 || _REACQ_RC=$?

  if [ "$_REACQ_RC" -ne 0 ]; then
    # Re-acquire failed: another worker may now hold the claim.
    # Do NOT restore the manifest (leave it archived so this session closes
    # safely). Release the dispatch reservation and exit 12.
    FNO_CLAIMS_ROOT="$HOME" fno claim release "$DISPATCH_KEY" \
      --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"reacquire_failed\",\"detail\":\"verify_timeout + re-acquire node:$NODE_ID failed (rc=$_REACQ_RC); claim may be held by another worker\"}"
    echo "handoff-claim-lost $NODE_ID reason=\"re-acquire failed after verify_timeout; claim may be held by another worker - parent must NOT continue this node\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  #   (b) restore archived manifest
  _RESTORE_RC=0
  mv "$ARCHIVED_STATE" "$STATE_FILE" 2>/dev/null || _RESTORE_RC=$?

  #   (c) release dispatch reservation
  FNO_CLAIMS_ROOT="$HOME" fno claim release "$DISPATCH_KEY" \
    --holder "$DISPATCH_HOLDER" >/dev/null 2>&1 || true

  if [ "$_RESTORE_RC" -ne 0 ]; then
    # Restore failed: emit restore_failed (supersedes verify_timeout in the event log)
    _emit_event "handoff_failed" \
      "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"restore_failed\",\"detail\":\"verify_timeout + restore mv failed\"}"
    echo "handoff-restore-failed $NODE_ID reason=\"verify_timeout + restore_failed\""
    exit "$_EXIT_RESTORE_FAILED"
  fi

  _emit_event "handoff_failed" \
    "{\"node_id\":\"$NODE_ID\",\"session_id\":\"$SESSION_ID\",\"reason\":\"verify_timeout\",\"detail\":\"child $CHILD_NAME not live within ${VERIFY_TIMEOUT}s\"}"

  echo "parked $NODE_ID reason=\"verify timeout: child $CHILD_NAME not live within ${VERIFY_TIMEOUT}s\""
  exit "$_EXIT_PARKED"
fi

# ---------------------------------------------------------------------------
# Step 8: Commit the delegation
# ---------------------------------------------------------------------------

# 8a. Emit delegated event. child_session degrades to "unknown" only when
# neither the receipt nor the registry row carried a short_id/session_id
# (AC4-EDGE); correctness rides on to_session=CHILD_NAME, not this field.
_CHILD_SESSION="${CHILD_SID:-unknown}"
_emit_event "delegated" \
  "{\"node_id\":\"$NODE_ID\",\"from_session\":\"$SESSION_ID\",\"to_session\":\"$CHILD_NAME\",\"child_session\":\"$_CHILD_SESSION\",\"boundary\":\"$BOUNDARY\",\"generation\":$CHILD_GEN,\"harness\":\"$_HARNESS\"}"

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

# 8d. Best-effort: append session_id to plan frontmatter session_ids inline-list
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
        if current.startswith('[') and current.endswith(']'):
            inner = current[1:-1].strip()
            if inner:
                new_val = '[' + inner + ', ' + sid + ']'
            else:
                new_val = '[' + sid + ']'
        elif current == '' or current == '[]':
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
echo "delegated $NODE_ID child=$CHILD_NAME session=$_CHILD_SESSION generation=$CHILD_GEN"
exit 0
