#!/usr/bin/env bash
# Orchestrate the full fno CLI pipeline end-to-end.
# Supports --dry-run to skip real PR creation and graph mutation.
#
# Usage:
#   bash dogfood-driver.sh [--dry-run] [--log-file /path/to/log]
#
# Exit codes:
#   0  - pipeline completed successfully
#   1  - pipeline failed
#   3  - no ready nodes found
set -euo pipefail

# ---- Argument parsing ----
DRY_RUN=false
LOG_FILE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=true; shift ;;
    --log-file) LOG_FILE="$2"; shift 2 ;;
    *) echo "ERROR: unknown argument $1" >&2; exit 1 ;;
  esac
done

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)
CLI_DIR="$REPO_ROOT/cli"

# Default log file
if [[ -z "$LOG_FILE" ]]; then
  LOG_FILE="$SCRIPT_DIR/dogfood-driver-$(date -u +%Y%m%dT%H%M%SZ).log"
fi

# Invocation counter
INVOCATIONS=0

# ---- Logging helpers ----
log() {
  local msg="[$(date -u +%Y-%m-%dT%H:%M:%SZ)] $*"
  echo "$msg" >&2
  echo "$msg" >> "$LOG_FILE"
}

log_invoke() {
  local cmd="$*"
  INVOCATIONS=$((INVOCATIONS + 1))
  echo "INVOKE[$INVOCATIONS]: $cmd" >> "$LOG_FILE"
  log "CLI call #$INVOCATIONS: $cmd"
}

# ---- CLI wrapper ----
cli() {
  log_invoke "fno $*"
  cd "$CLI_DIR"
  uv run fno-py "$@"
}

log "=== dogfood-driver started (dry_run=$DRY_RUN) ==="
log "Log file: $LOG_FILE"

# ---- Step 1: Probe ----
log "--- Step 1: probe ---"
probe_out=$(cli probe --json 2>/dev/null || echo '{"ok":false}')
log "probe result: $probe_out"
probe_ok=$(echo "$probe_out" | python3 -c "import sys,json; d=json.load(sys.stdin); print(str(d.get('ok',False)).lower())")
if [[ "$probe_ok" != "true" ]]; then
  log "WARNING: probe reported not-ok; continuing anyway (dry-run or offline)"
fi

# ---- Step 2: Pick target ----
log "--- Step 2: pick-target ---"
target_file="$SCRIPT_DIR/.target.json"
if [[ ! -f "$target_file" ]]; then
  bash "$SCRIPT_DIR/pick-target.sh"
fi

if [[ ! -f "$target_file" ]]; then
  log "ERROR: no .target.json after pick-target" >&2
  exit 3
fi

node_id=$(python3 -c "import json; d=json.load(open('$target_file')); print(d['id'])")
plan_path=$(python3 -c "import json; d=json.load(open('$target_file')); print(d.get('plan_path') or '')")
title=$(python3 -c "import json; d=json.load(open('$target_file')); print(d.get('title','?'))")
log "Target: $node_id - $title"
echo "TARGET: $node_id" >> "$LOG_FILE"

# ---- Step 3: Init session ----
log "--- Step 3: init-session ---"
if [[ "$DRY_RUN" == "true" ]]; then
  init_out=$(bash "$SCRIPT_DIR/init-session.sh" --dry-run)
else
  init_out=$(bash "$SCRIPT_DIR/init-session.sh")
fi
slug=$(echo "$init_out" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['slug'])")
log "Session slug: $slug"
echo "SESSION: $slug" >> "$LOG_FILE"

# ---- Step 4: Worker blueprint (LLM dispatch - always reports skill_dispatch_required in CLI v0) ----
log "--- Step 4: worker blueprint ---"
if [[ "$DRY_RUN" == "true" ]]; then
  log "DRY-RUN: skipping real worker blueprint (would call: fno worker blueprint --plan $plan_path)"
  blueprint_out='{"action":"llm_blueprint","plan_path":"'"$plan_path"'","current_phase":"blueprint","dry_run":true}'
  log "blueprint (simulated): $blueprint_out"
  echo "BLUEPRINT: $blueprint_out" >> "$LOG_FILE"
else
  log "NOTE: worker blueprint dispatches to LLM; in v0 this returns skill_dispatch_required"
  blueprint_out=$(cli worker blueprint --plan "$REPO_ROOT/$plan_path" 2>/dev/null || echo '{"action":"skill_dispatch_required"}')
  log "blueprint result: $blueprint_out"
  echo "BLUEPRINT: $blueprint_out" >> "$LOG_FILE"
  blueprint_action=$(echo "$blueprint_out" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('action','unknown'))" 2>/dev/null || echo "unknown")
  if [[ "$blueprint_action" == "skill_dispatch_required" || "$blueprint_action" == "llm_blueprint" ]]; then
    log "NOTE: blueprint requires LLM (expected for CLI v0) - recording as known gap"
    echo "GAP: worker blueprint requires LLM dispatch (skill_dispatch_required)" >> "$LOG_FILE"
  fi
fi

# ---- Step 5: Worker ship ----
log "--- Step 5: worker ship ---"
if [[ "$DRY_RUN" == "true" ]]; then
  log "DRY-RUN: skipping real PR creation (mocking gh pr create via --dry-run)"
  fake_pr=9999
  ship_out="{\"action\":\"pr_created\",\"pr_number\":$fake_pr,\"pr_url\":\"https://github.com/owner/repo/pull/$fake_pr\",\"dry_run\":true,\"auto_merge_armed\":false}"
  log "ship (simulated): $ship_out"
  echo "SHIP: $ship_out" >> "$LOG_FILE"
  echo "SKILL_INVOCATIONS: 0 (dry-run)" >> "$LOG_FILE"
else
  log "NOTE: calling real worker ship"
  ship_out=$(cli worker ship --plan "$REPO_ROOT/$plan_path" 2>/dev/null || echo '{"action":"error"}')
  log "ship result: $ship_out"
  echo "SHIP: $ship_out" >> "$LOG_FILE"
fi

# ---- Step 6: Worker review (skill dispatch expected in v0) ----
log "--- Step 6: worker review (reconcile) ---"
if [[ "$DRY_RUN" == "true" ]]; then
  log "DRY-RUN: skipping real review/reconcile"
  review_out='{"action":"approved","external_review_passed":true,"dry_run":true}'
  reconcile_out="{\"action\":\"pr_merged\",\"pr_number\":9999,\"pr_url\":\"https://github.com/owner/repo/pull/9999\",\"dry_run\":true}"
  log "review (simulated): $review_out"
  log "reconcile (simulated): $reconcile_out"
  echo "REVIEW: $review_out" >> "$LOG_FILE"
  echo "RECONCILE: $reconcile_out" >> "$LOG_FILE"
else
  log "NOTE: worker review dispatches to skill; in v0 this may return skill_dispatch_required"
  review_out=$(cli worker review --plan "$REPO_ROOT/$plan_path" 2>/dev/null || echo '{"action":"skill_dispatch_required"}')
  log "review result: $review_out"
  echo "REVIEW: $review_out" >> "$LOG_FILE"
fi

# ---- Step 7: Graph update to completed ----
log "--- Step 7: graph update completed ---"
if [[ "$DRY_RUN" == "true" ]]; then
  log "DRY-RUN: skipping graph node completion update"
  echo "GRAPH_UPDATE: dry_run=true node=$node_id status=completed" >> "$LOG_FILE"
else
  cli graph update --id "$node_id" --status completed >> "$LOG_FILE" 2>&1 || true
fi

# ---- Summary ----
log "=== Pipeline complete ==="
log "Total CLI invocations: $INVOCATIONS"
log "Log file: $LOG_FILE"
echo "INVOCATIONS_TOTAL: $INVOCATIONS" >> "$LOG_FILE"

summary="{\"dry_run\":$([[ "$DRY_RUN" == "true" ]] && echo "true" || echo "false"),\"target\":\"$node_id\",\"title\":\"$title\",\"session\":\"$slug\",\"invocations\":$INVOCATIONS,\"log_file\":\"$LOG_FILE\",\"status\":\"complete\"}"
echo "$summary"
echo "SUMMARY: $summary" >> "$LOG_FILE"
