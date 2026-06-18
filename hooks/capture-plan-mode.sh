#!/usr/bin/env bash
# capture-plan-mode.sh - PostToolUse(ExitPlanMode) hook. Claude-Code-only.
#
# When a plan is approved in Claude Code's native Plan Mode, this hook captures
# it to .fno/.pending-plan.md so a subsequent bare `/target` can detect,
# enrich, and (on confirm) execute it. On any CLI without an ExitPlanMode tool
# the matcher never fires, so this is a no-op there (graceful degradation).
#
# Detection-path contract (source-confirmed, ab-588650c7):
#   Approve and keep-planning route to DIFFERENT hook events. A kept-planning /
#   rejected ExitPlanMode fires PermissionDenied (the can-use-tool path), NOT
#   PostToolUse; PostToolUse fires only after a successful tool call, i.e. after
#   the user approved. So a PostToolUse fire here ALREADY means approval - the
#   event type is the discriminator and the Output carries no approval field
#   (no `approved`/`decision`/`isError`; those were never real fields). We
#   capture on every fire and skip only on the one genuine pending signal: the
#   teammate path's awaitingLeaderApproval==true (submitted to a team lead, not
#   yet approved). The /target confirm step remains the human backstop.
#   Provenance: open-sourced Claude Code tree (ExitPlanModeV2Tool.ts:110-142,
#   304-312; toolExecution.ts:1001/1081 vs :1483). See
#   docs/architecture/target-plan-mode-integration.md and
#   skills/target/references/plan-mode-backfill.md.
#
# Failure policy: NEVER fatal. Every error path logs to
# .fno/hook-events.jsonl and exits 0 so the hook can never block the
# user's tool call.

set -uo pipefail

# jq parses the hook payload; degrade silently if absent (cannot log without it).
command -v jq >/dev/null 2>&1 || exit 0

HOOK_INPUT="$(cat 2>/dev/null || true)"
[[ -n "$HOOK_INPUT" ]] || exit 0

TOOL_NAME="$(printf '%s' "$HOOK_INPUT" | jq -r '.tool_name // empty' 2>/dev/null || echo "")"
# Defensive: the matcher should guarantee ExitPlanMode, but never write for
# any other tool even if mis-registered with an empty matcher.
[[ "$TOOL_NAME" == "ExitPlanMode" ]] || exit 0

CWD="$(printf '%s' "$HOOK_INPUT" | jq -r '.cwd // empty' 2>/dev/null || echo "")"
[[ -n "$CWD" ]] || CWD="$PWD"
# Resolve the repo root so the sidecar lands where /target detection looks:
# detect-pending-plan.sh and init resolve .fno from `git --show-toplevel`.
# A Plan Mode approval made from a repo SUBDIRECTORY must still write to
# <repo-root>/.fno, or the next bare /target sees result=none and the
# approved plan is stranded.
REPO_ROOT="$(git -C "$CWD" rev-parse --show-toplevel 2>/dev/null || echo "$CWD")"
STATE_DIR="$REPO_ROOT/.fno"

SESSION_ID="$(printf '%s' "$HOOK_INPUT" | jq -r '.session_id // empty' 2>/dev/null || echo "")"

# Non-fatal diagnostic logger (mirrors the .fno/hook-events.jsonl idiom).
log_event() {
  local ev="$1"
  local extra="${2:-{\}}"
  mkdir -p "$STATE_DIR" 2>/dev/null || true
  jq -nc \
    --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg event "$ev" \
    --arg sid "$SESSION_ID" \
    --argjson extra "$extra" \
    '{ts:$ts, event:$event, session_id:$sid} + $extra' \
    >> "$STATE_DIR/hook-events.jsonl" 2>/dev/null || true
}

# Skip gate (source-confirmed, ab-588650c7). The Output has NO approval field;
# a PostToolUse fire already means the plan was approved (keep-planning fires
# PermissionDenied, a different event). The one genuine "not approved yet"
# signal is the teammate path: awaitingLeaderApproval==true means the plan was
# submitted to a team lead and must NOT be captured as pending. tool_response
# may be a string/absent; jq scalar-access errors are swallowed and default to
# "0" (write the safe default).
AWAITING="$(printf '%s' "$HOOK_INPUT" | jq -r '
  if   (.tool_response | type) != "object"             then "0"
  elif (.tool_response.awaitingLeaderApproval == true) then "1"
  else "0" end' 2>/dev/null || echo "0")"
if [[ "$AWAITING" == "1" ]]; then
  log_event "plan_mode_capture_skipped" '{"reason":"awaiting_leader_approval"}'
  exit 0
fi

# The plan body. The V2 ExitPlanMode tool saves the plan to disk and the inline
# `plan` field is frequently null with `filePath` populated (Output schema:
# ExitPlanModeV2Tool.ts:110-142), so read the file first. Capture order:
#   1. tool_response.filePath  - the saved plan file, the tool's source of truth
#   2. tool_input.planFilePath - same path, from the normalized input
#   3. tool_input.plan / tool_response.plan - the inline body, fallback
PLAN_FILE="$(printf '%s' "$HOOK_INPUT" | jq -r '(.tool_response.filePath?) // (.tool_input.planFilePath?) // empty' 2>/dev/null || echo "")"
PLAN=""
if [[ -n "$PLAN_FILE" ]]; then
  # Resolve a relative path against the tool's cwd ($CWD), not the hook's
  # process cwd, so the file resolves the same way the tool wrote it. `-f`
  # restricts to a regular file (skip a directory/device); `cat --` stops
  # option parsing for a path that begins with '-'.
  [[ "$PLAN_FILE" == /* ]] || PLAN_FILE="$CWD/$PLAN_FILE"
  if [[ -f "$PLAN_FILE" && -r "$PLAN_FILE" ]]; then
    PLAN="$(cat -- "$PLAN_FILE" 2>/dev/null || echo "")"
  fi
fi
# Fall back to the inline body when no readable file path yielded content.
if [[ -z "${PLAN//[[:space:]]/}" ]]; then
  PLAN="$(printf '%s' "$HOOK_INPUT" | jq -r '(.tool_input.plan?) // (.tool_response.plan?) // empty' 2>/dev/null || echo "")"
fi

# Boundary: empty / whitespace-only plan (no file body, no inline body) -> no sidecar.
if [[ -z "${PLAN//[[:space:]]/}" ]]; then
  log_event "plan_mode_capture_skipped" '{"reason":"empty_plan"}'
  exit 0
fi

# Derive a kebab slug from the first markdown heading or first non-empty line.
SLUG="$(printf '%s' "$PLAN" \
  | grep -m1 -E '[^[:space:]]' 2>/dev/null \
  | sed -E 's/^#{1,6}[[:space:]]+//; s/[^A-Za-z0-9]+/-/g; s/^-+//; s/-+$//' \
  | tr '[:upper:]' '[:lower:]' \
  | cut -c1-60)"
[[ -n "$SLUG" ]] || SLUG="plan-mode"

CAPTURED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
SIDECAR="$STATE_DIR/.pending-plan.md"
TMP="$SIDECAR.tmp.$$"

mkdir -p "$STATE_DIR" 2>/dev/null || {
  log_event "plan_mode_capture_failed" '{"reason":"mkdir_failed"}'
  exit 0
}

# Last-writer-wins: a fresh approval overwrites any prior pending sidecar.
# Written atomically (tmp + mv) so a partial write never leaves a torn sidecar.
if {
  printf -- '---\n'
  printf 'captured_at: %s\n' "$CAPTURED_AT"
  printf 'session_id: %s\n' "${SESSION_ID:-unknown}"
  printf 'slug: %s\n' "$SLUG"
  printf 'source: claude-plan-mode\n'
  printf 'status: pending\n'
  printf -- '---\n\n'
  printf '%s\n' "$PLAN"
} > "$TMP" 2>/dev/null && mv -f "$TMP" "$SIDECAR" 2>/dev/null; then
  log_event "plan_mode_captured" "$(jq -nc --arg slug "$SLUG" '{slug:$slug,status:"pending"}' 2>/dev/null || echo '{}')"
else
  rm -f "$TMP" 2>/dev/null || true
  log_event "plan_mode_capture_failed" '{"reason":"write_failed"}'
fi

exit 0
