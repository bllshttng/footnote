#!/bin/bash
# init-target-state.sh - write an IMMUTABLE inputs-only session manifest.
#
# IMMUTABILITY CONTRACT
# ---------------------
# This script writes target-state.md ONCE at session start. The file is a
# read-only manifest of session inputs; it must never be updated during the
# run except for a single allowance: first-fill of an empty plan_path field
# (done via `fno state set --field plan_path` after blueprint resolves it).
# No gate booleans, status, phase, iteration, or mutable tracking lists are
# written here. All control-plane decisions live in fno-agents loop-check.
# See: ab-d0337fbc (control-plane collapse wedge).
#
# TRIGGER GUARDS
# --------------
# Fires only when explicitly triggered:
#   - TARGET_START=1 env var (set by the target skill as its first action), OR
#   - `.fno/.target-starting` sentinel file (touched by the skill body)
#
# Without either signal this script is a no-op. This prevents ambient stub
# creation (stop hook pollution) in any project where the skill is referenced.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
STATE_FILE="$REPO_ROOT/.fno/target-state.md"
STATE_DIR="$(dirname "$STATE_FILE")"

# Guard: only run when the target skill explicitly asked for init.
if [[ "${TARGET_START:-}" != "1" ]] && [[ ! -f "$STATE_DIR/.target-starting" ]]; then
  exit 0
fi

# Consume both triggers so a single trigger fires at most once.
unset TARGET_START
rm -f "$STATE_DIR/.target-starting"

# ── Location pre-flight (shared verdict) ─────────────────────────────
# Refuse to write target-state.md into the canonical checkout on a
# protected branch (main, master, detached HEAD, or an undeterminable
# branch). Two terminals open at the same canonical path share .fno/, so a
# target session started there is felt by every sibling terminal.
#
# The canonical-vs-worktree + protected-branch classification is delegated
# to the shared helper (check-impl-location.sh) so /target, /do, /fix, and
# the SessionStart heads-up all consult ONE rule and never drift (design:
# Worktree Scope Hygiene, US2). This script keeps its own hard refusal; the
# helper only classifies. Verdict `canonical-protected` means "block here";
# an unborn (no-commit) repo and any linked worktree both classify as `ok`.
_LOC_HELPER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-impl-location.sh"
if [[ -f "$_LOC_HELPER" ]]; then
  _LOC_OUT="$(bash "$_LOC_HELPER" 2>/dev/null || true)"
  _TARGET_VERDICT="$(printf '%s\n' "$_LOC_OUT" | sed -n 's/^verdict=//p' | head -1)"
  _TARGET_INIT_BRANCH="$(printf '%s\n' "$_LOC_OUT" | sed -n 's/^branch=//p' | head -1)"
  if [[ "$_TARGET_VERDICT" == "canonical-protected" ]]; then
    if [[ "${TARGET_LOCATION_OK:-}" == "main-acknowledged" ]]; then
      echo "[init-target-state] WARNING: proceeding on canonical '${_TARGET_INIT_BRANCH:-<unknown>}' under TARGET_LOCATION_OK=main-acknowledged" >&2
    else
      case "$_TARGET_INIT_BRANCH" in
        "")
          _TARGET_DESC="an unknown branch (git rev-parse failed; check 'safe.directory' config or .git permissions)"
          ;;
        HEAD)
          _TARGET_DESC="detached HEAD (no branch checked out)"
          ;;
        *)
          _TARGET_DESC="branch '$_TARGET_INIT_BRANCH'"
          ;;
      esac
      cat >&2 <<EOF
[init-target-state] REFUSED: cwd is the canonical checkout on $_TARGET_DESC.

Writing .fno/target-state.md here pollutes every other terminal
rooted at this checkout — they all share .fno/, and the stop hook
will block exit in all of them until the target session completes.

Pick ONE:

  1) Worktree (recommended for /target M, L, or cross-project):
       git worktree add ~/conductor/workspaces/$(basename "$REPO_ROOT")/<slug> -b feature/<slug>
       cd ~/conductor/workspaces/$(basename "$REPO_ROOT")/<slug>
       bash scripts/setup/setup-worktree.sh  # if present
       # then re-run your target command

  2) Feature branch on this checkout (OK for /target S, single terminal):
       git checkout -b feature/<slug>
       # then re-run your target command

  3) Genuinely intend to target on $_TARGET_DESC (rare; hotfix-on-trunk):
       TARGET_LOCATION_OK=main-acknowledged <re-run your target command>

Refusing to write state file. See backlog ab-efcde945 for context.
EOF
      exit 1
    fi
  fi
  unset _TARGET_VERDICT _TARGET_INIT_BRANCH _TARGET_DESC _LOC_OUT
fi
unset _LOC_HELPER
unset TARGET_LOCATION_OK

# ── Input resolution ─────────────────────────────────────────────────
INITIAL_INPUT="${TARGET_INPUT:-}"
INITIAL_PLAN_PATH="${TARGET_PLAN_PATH:-}"
unset TARGET_INPUT TARGET_PLAN_PATH

LOCAL_SETTINGS="$REPO_ROOT/.fno/config.toml"
GLOBAL_SETTINGS="${HOME}/.fno/config.toml"

# ── Provider detection ───────────────────────────────────────────────
detect_provider() {
  if [[ -n "${CODEX_PLUGIN_ROOT:-}" ]]; then
    echo "codex"
  elif [[ -n "${GEMINI_PROJECT_DIR:-}" ]]; then
    echo "gemini"
  elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]]; then
    echo "claude"
  else
    echo "claude"
  fi
}

# ── Gemini agent-mode helpers (provider_mode field) ──────────────────
GEMINI_REQUIRED_AGENT_FILES=(archer.md reviewer.md roadmap-generator.md verifier.md)

is_truthy() {
  local value="${1:-}"
  [[ "$value" == "true" || "$value" == "yes" || "$value" == "1" ]]
}

config_flag_is_true() {
  local file="$1"
  local key="$2"
  [[ -f "$file" ]] || return 1
  local value
  # Flat config.toml: a top-level `${key} = value` line.
  value=$(sed -n "s/^${key}[[:space:]]*=[[:space:]]*//p" "$file" 2>/dev/null \
    | head -1 \
    | tr -d '"' | tr -d "'")
  [[ "$value" == "true" || "$value" == "yes" || "$value" == "1" ]]
}

gemini_agents_opted_in() {
  if [[ -n "${FNO_GEMINI_EXPERIMENTAL_AGENTS:-}" || -n "${GEMINI_EXPERIMENTAL_AGENTS:-}" ]]; then
    local env_value="${FNO_GEMINI_EXPERIMENTAL_AGENTS:-${GEMINI_EXPERIMENTAL_AGENTS:-}}"
    is_truthy "$env_value"
    return $?
  fi
  config_flag_is_true "$LOCAL_SETTINGS" "gemini_experimental_agents" \
    || config_flag_is_true "$GLOBAL_SETTINGS" "gemini_experimental_agents"
}

gemini_missing_agent_files() {
  local name
  for name in "${GEMINI_REQUIRED_AGENT_FILES[@]}"; do
    if [[ ! -f "$REPO_ROOT/.gemini/agents/${name}" ]]; then
      printf '%s\n' "$name"
    fi
  done
}

gemini_agent_mode() {
  local provider="$1"
  if [[ "$provider" != "gemini" ]]; then
    echo "standard"
    return 0
  fi
  if ! gemini_agents_opted_in; then
    echo "stable_fallback"
    return 0
  fi
  local missing
  missing="$(gemini_missing_agent_files)"
  if [[ -n "$missing" ]]; then
    echo "stable_fallback"
    return 0
  fi
  echo "experimental_agents"
}

gemini_upgrade_reason() {
  local provider="$1"
  if [[ "$provider" != "gemini" ]]; then
    echo ""
    return 0
  fi
  if ! gemini_agents_opted_in; then
    echo "Gemini experimental agents not opted in"
    return 0
  fi
  local missing
  missing="$(gemini_missing_agent_files)"
  if [[ -n "$missing" ]]; then
    missing="${missing//$'\n'/ }"
    missing="${missing% }"
    echo "Missing Gemini project agent files: $missing"
    return 0
  fi
  echo "Gemini experimental project agents available"
}

# ── has_ui inference (predictive, from plan) ─────────────────────────
# Delegates to canonical scripts/lib/infer-has-ui.sh so plan-time inference
# shares the same locked globs as executor routing (ab-15c470cf).
_derive_has_ui_from_plan() {
  local plan_path="${1:-}"
  [[ -z "$plan_path" ]] && { printf 'false'; return 0; }
  plan_path="${plan_path%%#*}"
  [[ -e "$plan_path" ]] || { printf 'false'; return 0; }
  local infer_has_ui="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}/scripts/lib/infer-has-ui.sh"
  [[ -f "$infer_has_ui" ]] || { printf 'false'; return 0; }
  local result
  result=$( { grep -rhoE '[A-Za-z0-9_][A-Za-z0-9_./-]*[/.][A-Za-z0-9_./-]+' -- "$plan_path" 2>/dev/null || true; } \
    | bash "$infer_has_ui" 2>/dev/null )
  [[ "$result" == "true" ]] && printf 'true' || printf 'false'
}

# ── Skip-flag resolution ─────────────────────────────────────────────
# Size-profile defaults + per-flag TARGET_NO_* env overrides.
_is_true() {
  case "${1:-}" in
    1|true|TRUE|True|yes|YES|Yes) return 0 ;;
    *) return 1 ;;
  esac
}
_bool() { _is_true "$1" && printf 'true' || printf 'false'; }

case "${TARGET_SIZE:-}" in
  S|s)
    no_external_default=true
    no_docs_default=true
    no_ship_default=false
    no_verify_default=true
    no_goals_default=true
    no_browser_default=true
    no_clean_default=true
    no_how_to_default=true
    no_memory_default=true
    no_deferrals_capture_default=true
    ;;
  L|l)
    no_external_default=false
    no_docs_default=false
    no_ship_default=false
    no_verify_default=false
    no_goals_default=false
    no_browser_default=false
    no_clean_default=false
    no_how_to_default=false
    no_memory_default=false
    no_deferrals_capture_default=false
    ;;
  *)
    # Legacy / unset / M: preserve original hardcoded defaults.
    no_external_default=false
    no_docs_default=false
    no_ship_default=false
    no_verify_default=true
    no_goals_default=false
    no_browser_default=false
    no_clean_default=true
    no_how_to_default=false
    no_memory_default=false
    no_deferrals_capture_default=false
    ;;
esac

# Apply per-flag overrides. Empty / unset means "use profile default".
no_external=$( [[ -n "${TARGET_NO_EXTERNAL:-}" ]] && _bool "$TARGET_NO_EXTERNAL" || printf '%s' "$no_external_default" )
no_docs=$(     [[ -n "${TARGET_NO_DOCS:-}"     ]] && _bool "$TARGET_NO_DOCS"     || printf '%s' "$no_docs_default" )
no_ship=$(     [[ -n "${TARGET_NO_SHIP:-}"     ]] && _bool "$TARGET_NO_SHIP"     || printf '%s' "$no_ship_default" )
# batch-lane Wave 2/3 (x-6cdf): a batched member commits to a shared batch branch
# and ships via the batch PR, not its own. loop-check reads this flag to
# terminate as DoneBatched (not a hang) on the member's promise. Set by the
# active-backlog daemon's batched dispatch (TARGET_BATCHED=1); default false.
batched=$(     [[ -n "${TARGET_BATCHED:-}"     ]] && _bool "$TARGET_BATCHED"     || printf '%s' "false" )
no_verify=$(   [[ -n "${TARGET_NO_VERIFY:-}"   ]] && _bool "$TARGET_NO_VERIFY"   || printf '%s' "$no_verify_default" )
no_goals=$(    [[ -n "${TARGET_NO_GOALS:-}"    ]] && _bool "$TARGET_NO_GOALS"    || printf '%s' "$no_goals_default" )
no_browser=$(  [[ -n "${TARGET_NO_BROWSER:-}"  ]] && _bool "$TARGET_NO_BROWSER"  || printf '%s' "$no_browser_default" )
no_clean=$(    [[ -n "${TARGET_NO_CLEAN:-}"    ]] && _bool "$TARGET_NO_CLEAN"    || printf '%s' "$no_clean_default" )
no_how_to=$(   [[ -n "${TARGET_NO_HOW_TO:-}"   ]] && _bool "$TARGET_NO_HOW_TO"   || printf '%s' "$no_how_to_default" )
no_memory=$(   [[ -n "${TARGET_NO_MEMORY:-}"   ]] && _bool "$TARGET_NO_MEMORY"   || printf '%s' "$no_memory_default" )
no_deferrals_capture=$( [[ -n "${TARGET_NO_DEFERRALS_CAPTURE:-}" ]] && _bool "$TARGET_NO_DEFERRALS_CAPTURE" || printf '%s' "${no_deferrals_capture_default:-false}" )
has_ui=$(      [[ -n "${TARGET_HAS_UI:-}"      ]] && _bool "$TARGET_HAS_UI"      || _derive_has_ui_from_plan "${INITIAL_PLAN_PATH:-}" )

# ── Attended / advisory inputs ────────────────────────────────────────
# attended: false when TARGET_UNATTENDED=1 OR config.unattended.enabled is true.
# advisory: true when TARGET_ADVISORY=1 OR plan frontmatter has `gates: advisory`.

# Source config.sh once here so it is available for both attended detection and
# budget/auto-merge resolution below. Safe to source multiple times (idempotent).
_CONFIG_SH="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}/scripts/lib/config.sh"
if [[ -f "$_CONFIG_SH" ]]; then
  # shellcheck source=/dev/null
  source "$_CONFIG_SH"
fi
# config.sh sources the `fno paths` shell-stub, which exports STATE_DIR as the
# GLOBAL abilities home ($HOME/.fno) - a name collision with this
# script's project-local STATE_DIR. Re-derive ours or every sentinel/manifest
# path below silently retargets the global dir (caught by
# tests/hooks/test_pending_plan_wipe.sh, ab-d0337fbc).
STATE_FILE="$REPO_ROOT/.fno/target-state.md"
STATE_DIR="$(dirname "$STATE_FILE")"

_attended="true"
if [[ "${TARGET_UNATTENDED:-}" == "1" ]]; then
  _attended="false"
elif declare -F get_config >/dev/null 2>&1; then
  if [[ "$(get_config "unattended.enabled" "false")" == "true" ]]; then
    _attended="false"
  fi
fi

_advisory="false"
if [[ "${TARGET_ADVISORY:-}" == "1" ]]; then
  _advisory="true"
elif [[ -n "${INITIAL_PLAN_PATH:-}" ]]; then
  _plan_gates=""
  _plan_file="${INITIAL_PLAN_PATH%%#*}"
  if [[ -f "$_plan_file" ]]; then
    _plan_gates=$(sed -n 's/^gates:[[:space:]]*//p' "$_plan_file" 2>/dev/null | head -1 | tr -d '"' | tr -d "'" | xargs)
  fi
  if [[ "${_plan_gates:-}" == "advisory" ]]; then
    _advisory="true"
  fi
fi

# ── Budget cap resolution ─────────────────────────────────────────────
# Resolved from settings config.budget.<attended|unattended>.{wall_clock_cap_minutes,cost_cap_usd}.
# Falls back to flat budget_cap: for cost (legacy). Lines are OMITTED when unconfigured.
_budget_mode="attended"
[[ "$_attended" == "false" ]] && _budget_mode="unattended"

_budget_wall_clock=""
_budget_cost=""
if declare -F get_config >/dev/null 2>&1; then
  _budget_wall_clock="$(get_config "budget.${_budget_mode}.wall_clock_cap_minutes" "")"
  _budget_cost="$(get_config "budget.${_budget_mode}.cost_cap_usd" "")"
  # Legacy flat key fallback for cost
  if [[ -z "$_budget_cost" ]]; then
    _budget_cost="$(get_config "budget_cap" "")"
  fi
fi

# ── Auto-merge inputs (read-only at init; no mutable tracking lists) ──
AUTO_MERGE_ENABLED="false"
AUTO_MERGE_APPROVED="false"
if declare -F get_auto_merge_enabled >/dev/null 2>&1; then
  AUTO_MERGE_ENABLED="$(get_auto_merge_enabled 2>/dev/null)" || {
    echo "[init-target-state] warn: auto-merge config lookup failed; defaulting to disabled" >&2
    AUTO_MERGE_ENABLED="false"
  }
fi
if [[ "${TARGET_NO_MERGE:-}" == "1" ]]; then
  AUTO_MERGE_APPROVED="false"
elif [[ "${TARGET_AUTO_MERGE:-}" == "1" ]]; then
  AUTO_MERGE_APPROVED="true"
elif declare -F is_auto_merge_allowed_for >/dev/null 2>&1 && is_auto_merge_allowed_for "target" 2>/dev/null; then
  AUTO_MERGE_APPROVED="true"
fi

# Auto-merge implies external review on.
if _is_true "$AUTO_MERGE_APPROVED" && _is_true "$no_external"; then
  printf '[init-target-state] auto-merge approved: forcing no_external=false (was true via %s)\n' \
    "$( [[ -n "${TARGET_NO_EXTERNAL:-}" ]] && printf 'TARGET_NO_EXTERNAL' || printf 'size profile %s' "${TARGET_SIZE:-M}" )" >&2
  no_external=false
fi

# ── Helpers ──────────────────────────────────────────────────────────
ensure_dir() {
  local path="$1"
  mkdir -p "$path" 2>/dev/null && return 0
  echo "target: ERROR: cannot create directory: $path" >&2
  echo "target:   check filesystem permissions, read-only mount, or ENOSPC" >&2
  exit 1
}

# ── State validity check (for stale-state detection) ─────────────────
is_valid_state_file() {
  [[ -f "$STATE_FILE" ]] || return 1
  # A valid manifest has a YAML frontmatter block (--- ... ---).
  awk '
    NR == 1 { if ($0 != "---") { exit 1 } in_frontmatter = 1; next }
    in_frontmatter && $0 == "---" { found_close = 1; in_frontmatter = 0; next }
    END { exit !(found_close) }
  ' "$STATE_FILE"
}

# ── Directory scaffolding ────────────────────────────────────────────
ensure_dir "$STATE_DIR"
ensure_dir "$STATE_DIR/artifacts"

# ── Per-worktree init lock ───────────────────────────────────────────
# Prevents two concurrent init invocations in the same worktree from racing
# on the state file. Atomic via temp-file + hardlink (link(2) is atomic and
# fails with EEXIST). Lock contents: PID + lstart so PID-reuse is detected.
INIT_LOCK_FILE="$STATE_DIR/.init.lock"

_init_process_provenance() {
  ps -p "$1" -o lstart= 2>/dev/null \
    | tr -s '[:space:]' ' ' \
    | sed -e 's/^ //' -e 's/ $//' \
    || true
}

_init_process_alive() {
  ps -p "$1" >/dev/null 2>&1
}

_init_process_holds_lock() {
  local pid="$1"
  local stored_prov="$2"
  _init_process_alive "$pid" || return 1
  local current_prov
  current_prov=$(_init_process_provenance "$pid")
  [[ "$current_prov" == "$stored_prov" ]]
}

_init_acquire_lock() {
  local prov tmp
  prov=$(_init_process_provenance $$)
  tmp="${INIT_LOCK_FILE}.tmp.$$"
  if ! printf '%s\n%s\n' "$$" "$prov" > "$tmp" 2>/dev/null; then
    rm -f "$tmp" 2>/dev/null
    return 1
  fi
  if ln "$tmp" "$INIT_LOCK_FILE" 2>/dev/null; then
    rm -f "$tmp" 2>/dev/null
    return 0
  fi
  rm -f "$tmp" 2>/dev/null
  return 1
}

_init_release_lock() {
  rm -f "$INIT_LOCK_FILE"
}

if ! _init_acquire_lock; then
  _LOCK_PID=$(sed -n '1p' "$INIT_LOCK_FILE" 2>/dev/null || true)
  _LOCK_PROV=$(sed -n '2p' "$INIT_LOCK_FILE" 2>/dev/null || true)
  if [[ -n "$_LOCK_PID" ]] && _init_process_holds_lock "$_LOCK_PID" "$_LOCK_PROV"; then
    echo "target: ERROR: another init-target-state is running here (PID $_LOCK_PID)" >&2
    echo "target: If this is wrong, remove $INIT_LOCK_FILE and retry." >&2
    exit 75
  fi
  rm -f "$INIT_LOCK_FILE"
  if ! _init_acquire_lock; then
    echo "target: ERROR: cannot acquire init lock at $INIT_LOCK_FILE (lost reclaim race)" >&2
    exit 75
  fi
  echo "target: reclaimed stale init lock (prior PID '$_LOCK_PID' dead, reused, or unreadable)" >&2
fi
trap _init_release_lock EXIT

# ── Contested-liveness activity probe (x-ba4b) ───────────────────────
# The steal guard for the stale-session archive below. A free/stale node claim
# is NOT sufficient to archive a prior manifest and reclaim: the observed bug
# (x-e780) was a live session working under a dead supervisor pid whose claim
# read non-live, then got archived+stolen. This probe asks the second question:
# does the worktree show FRESH activity? Newest mtime among git-tracked-modified
# files + the .fno/scratchpad tree (a live /target writes there continuously).
# Within the window => a live session is here => refuse (contested), never steal.
# Sets _ACTIVITY_EVIDENCE for the BLOCKED message; returns 0 iff fresh.
# Window default 15m; override via TARGET_CLAIM_ACTIVITY_WINDOW or, when wired,
# config.claims.activity_window.
# NOTE: mtime is the ONLY signal on purpose - "a process cwd'd in the worktree"
# is unusable here because init's OWN parent (the target session) is legitimately
# cwd'd in the worktree, so it would false-positive on every run and strand
# genuinely-dead nodes. mtime measures actual work, not mere presence.
# Validate the env override is a bare integer (seconds); a non-numeric value
# like "15m" or "abc" must NOT reach the `(( now - newest < window ))` arithmetic
# (it would abort under set -u). Fall through to config, then the 900s default.
_ACTIVITY_WINDOW="${TARGET_CLAIM_ACTIVITY_WINDOW:-}"
if ! [[ "$_ACTIVITY_WINDOW" =~ ^[0-9]+$ ]]; then
  _cfg_win="$(fno config get config.claims.activity_window 2>/dev/null || true)"
  if [[ "$_cfg_win" =~ ^[0-9]+$ ]]; then _ACTIVITY_WINDOW="$_cfg_win"; else _ACTIVITY_WINDOW=900; fi
  unset _cfg_win
fi
_ACTIVITY_EVIDENCE=""

_stat_mtime() {  # portable epoch-seconds mtime; 0 if absent/unreadable
  stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || echo 0
}

_worktree_has_fresh_activity() {
  local root="$1" window="$2" now newest=0 mt f
  now="$(date -u +%s)"
  # git-tracked files modified vs HEAD (staged + unstaged).
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    [[ -e "$root/$f" ]] || continue
    mt="$(_stat_mtime "$root/$f")"
    if (( mt > newest )); then newest="$mt"; _ACTIVITY_EVIDENCE="modified $f"; fi
  done < <(git -C "$root" diff --name-only HEAD 2>/dev/null || true)
  # scratchpad tree - a live /target session writes here continuously.
  if [[ -d "$root/.fno/scratchpad" ]]; then
    while IFS= read -r f; do
      [[ -z "$f" ]] && continue
      mt="$(_stat_mtime "$f")"
      if (( mt > newest )); then newest="$mt"; _ACTIVITY_EVIDENCE="scratchpad ${f##*/}"; fi
    done < <(find "$root/.fno/scratchpad" -type f 2>/dev/null || true)
  fi
  # AC1-EDGE: borderline mtimes err toward "fresh" (skip is cheap, steal is not)
  # via strict `<` against the window, and clock skew that makes a file appear in
  # the future (mt > now) yields a negative age that also trips the guard.
  if (( newest > 0 )) && (( now - newest < window )); then
    _ACTIVITY_EVIDENCE="$_ACTIVITY_EVIDENCE ($(( now - newest ))s ago, window ${window}s)"
    return 0
  fi
  return 1
}

# ── Stale-session archive ────────────────────────────────────────────
# Archive a prior session's manifest so the fresh-init branch runs cleanly.
# Two orphan signals, checked in order:
#   1. An explicit terminal status (COMPLETE/BLOCKED/ABORTED) - legacy
#      manifests that still carried a mutable status field.
#   2. A node claim that is no longer live. The immutable manifest dropped its
#      status field, so without this a completed session's manifest survives in
#      the shared .fno and every new session (even in a fresh worktree, via the
#      .fno symlink) trips on it and hand-clears it. The reap keys on the CLAIM,
#      NOT owner_pid: owner_pid is the transient `fno target init` wrapper pid
#      (dead ~1s after init returns, per cli/src/fno/claims/session_pid.py), so
#      it cannot tell a completed session from a live one. The node claim is
#      acquired with the DURABLE session pid (nearest claude ancestor) + TTL, so
#      its liveness is the real signal. A LIVE claim is preserved: a concurrent
#      or resuming sibling still owns this slot and the claim-acquire layer
#      below handles the collision - we must never clobber its live manifest
#      (codex P1 on PR #61). A manifest with no claim key (free-text/plan run)
#      is preserved conservatively. Degrade-safe: if `fno claim status` errors
#      or is unparseable, _CLAIM_STATE is empty and we do NOT reap.
# This block only runs under TARGET_START (new-session intent).
if [[ -f "$STATE_FILE" ]]; then
  # sed with `q` quits after the first match: avoids a `head -1` pipeline that
  # could SIGPIPE the upstream sed under `set -o pipefail` (gemini medium).
  _STALE_STATUS=$(sed -n '/^status:[[:space:]]*/{s/^status:[[:space:]]*//p;q;}' "$STATE_FILE" | xargs 2>/dev/null || true)
  _STALE_CLAIM_KEY=$(sed -n '/^target_claim_key:[[:space:]]*/{s/^target_claim_key:[[:space:]]*//p;q;}' "$STATE_FILE" | tr -d '"' | xargs 2>/dev/null || true)
  _STALE_SESSION_ID=$(sed -n '/^session_id:[[:space:]]*/{s/^session_id:[[:space:]]*//p;q;}' "$STATE_FILE" | tr -d '"' | xargs 2>/dev/null || true)
  _STALE_REASON=""
  case "${_STALE_STATUS:-}" in
    COMPLETE|BLOCKED|ABORTED) _STALE_REASON="status $_STALE_STATUS" ;;
  esac
  # Claimless free-text and plan runs have no lock whose release can delimit a
  # completed run. A successful terminal-complete finalization is the durable
  # boundary: archive only when the project event log records this exact
  # manifest session with a completed reason. Budget/stuck finalizations remain
  # resumable, and malformed or unreadable logs preserve the manifest.
  if [[ -z "$_STALE_REASON" && -z "$_STALE_CLAIM_KEY" \
        && -n "$_STALE_SESSION_ID" && -f "$STATE_DIR/events.jsonl" \
        && -x "$(command -v python3 2>/dev/null || true)" ]]; then
    if python3 - "$STATE_DIR/events.jsonl" "$_STALE_SESSION_ID" <<'PYEOF'
import json
import sys

path, session_id = sys.argv[1:]
try:
    lines = open(path, encoding="utf-8", errors="replace")
except OSError:
    raise SystemExit(1)
with lines:
    for line in lines:
        try:
            event = json.loads(line)
        except (json.JSONDecodeError, TypeError):
            continue
        data = event.get("data") if isinstance(event, dict) else None
        if (
            event.get("type") == "session_finalized"
            and isinstance(data, dict)
            and data.get("session_id") == session_id
            and data.get("termination_reason")
            in {
                "DonePRGreen",
                "DoneAdvisory",
                "DoneBatched",
                "DoneAwaitingMerge",
                "DonePlanned",
                "NoWork",
            }
        ):
            raise SystemExit(0)
raise SystemExit(1)
PYEOF
    then
      _STALE_REASON="finalized session $_STALE_SESSION_ID"
    fi
  fi
  if [[ -z "$_STALE_REASON" && -n "$_STALE_CLAIM_KEY" ]]; then
    # `fno claim status --json` is single-line; parse `state`. The reap decision
    # is now two-factor (x-ba4b), because a not-live claim ALONE is not proof the
    # slot is abandoned - the bug (x-e780) was a live session under a dead pid:
    #   live | suspect -> preserve. The claim is protected (suspect = TTL-
    #     unexpired dead pid, a respawned worker); never archive+steal.
    #   "" (error/unparseable) -> preserve (degrade-safe; do NOT reap on a bad
    #     probe, exactly as before).
    #   free | stale | corrupted -> a reap CANDIDATE, but only when the worktree
    #     shows NO fresh activity. Fresh activity => a live session is working
    #     here; refuse as `contested` (the claim-wait BLOCKED contract: the
    #     caller relays REASON/UNBLOCKS_AFTER and stops) rather than steal.
    _CLAIM_STATE=$(fno claim status "$_STALE_CLAIM_KEY" --json 2>/dev/null \
      | sed -n 's/.*"state"[[:space:]]*:[[:space:]]*"\([a-z]*\)".*/\1/p' || true)
    case "${_CLAIM_STATE:-}" in
      free|stale|corrupted)
        # Reap CANDIDATE, gated on activity below.
        if _worktree_has_fresh_activity "$REPO_ROOT" "$_ACTIVITY_WINDOW"; then
          # Contested: a live session owns this worktree despite a
          # $_CLAIM_STATE claim. Emit the BLOCKED contract and STOP - never
          # archive a live session's manifest, never reclaim its node.
          _NODE_ID="${_STALE_CLAIM_KEY#node:}"
          echo "RESULT: BLOCKED" >&1
          echo "TASK: ${TARGET_INPUT:-unknown}" >&1
          echo "REASON: contested - $_STALE_CLAIM_KEY reads '$_CLAIM_STATE' but the worktree shows fresh activity (${_ACTIVITY_EVIDENCE}); refusing to archive a live session's manifest and steal its node" >&1
          echo "UNBLOCKS_AFTER: the live session releases $_STALE_CLAIM_KEY or its worktree activity ages past ${_ACTIVITY_WINDOW}s" >&1
          exit 0
        fi
        _STALE_REASON="dead claim $_STALE_CLAIM_KEY ($_CLAIM_STATE); no fresh worktree activity" ;;
      *)
        # "" (probe error) | live | suspect | any UNKNOWN/future state -> preserve
        # (degrade-safe: only a KNOWN not-live state is ever a reap candidate).
        : ;;
    esac
  fi
  if [[ -n "$_STALE_REASON" ]]; then
    _ARCHIVE_PATH="$STATE_DIR/target-state.terminal.$(date -u +%Y%m%dT%H%M%SZ).md"
    mv "$STATE_FILE" "$_ARCHIVE_PATH"
    echo "target: prior session ($_STALE_REASON); archived to $(basename "$_ARCHIVE_PATH"); writing fresh state" >&2
  fi
  unset _STALE_STATUS _STALE_CLAIM_KEY _STALE_SESSION_ID _STALE_REASON _CLAIM_STATE _ARCHIVE_PATH
fi

# ── Scratchpad scaffolding ────────────────────────────────────────────
SCRATCHPAD_DIR="$STATE_DIR/scratchpad"
if [[ ! -f "$STATE_FILE" ]] || ! is_valid_state_file; then
  rm -rf "$SCRATCHPAD_DIR"
  ensure_dir "$SCRATCHPAD_DIR/execution"
  ensure_dir "$SCRATCHPAD_DIR/research"
fi

# ── Sentinel cleanup ─────────────────────────────────────────────────
rm -f "$STATE_DIR/.registered"
rm -f "$STATE_DIR/.target-cancelled-final"
rm -f "$STATE_DIR/.orphan-block-count"

if [[ ! -f "$STATE_FILE" ]] || ! is_valid_state_file; then
  rm -f "$STATE_DIR"/.phase-stall-* 2>/dev/null || true
fi

# ── Corrupt state cleanup ────────────────────────────────────────────
if [[ -f "$STATE_FILE" ]] && ! is_valid_state_file; then
  _CORRUPT_PATH="$STATE_DIR/target-state.corrupt.$(date -u +%Y%m%dT%H%M%SZ).md"
  mv "$STATE_FILE" "$_CORRUPT_PATH"
  echo "[init-target-state] WARNING: corrupted state archived to $_CORRUPT_PATH" >&2
fi

# ── Fresh-session guard (completed-sentinel) ──────────────────────────
# If a recent session completed (sentinel < 5 min old), don't reinitialize.
if [[ ! -f "$STATE_FILE" ]]; then
  COMPLETED_SENTINEL="$STATE_DIR/.target-completed"
  if [[ -f "$COMPLETED_SENTINEL" ]]; then
    SENTINEL_EPOCH=$(stat -c "%Y" "$COMPLETED_SENTINEL" 2>/dev/null || stat -f "%m" "$COMPLETED_SENTINEL" 2>/dev/null || echo "0")
    NOW_EPOCH=$(date -u "+%s")
    SENTINEL_AGE=$(( NOW_EPOCH - SENTINEL_EPOCH ))
    if [[ $SENTINEL_AGE -lt 300 ]]; then
      echo "Recent completion sentinel (age: ${SENTINEL_AGE}s) - not reinitializing" >&2
      exit 0
    else
      rm -f "$COMPLETED_SENTINEL"
    fi
  fi
fi

if [[ ! -f "$STATE_FILE" ]]; then
  TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

  # Wipe stale cancel sentinel from prior session.
  STALE_SENTINEL="$STATE_DIR/.target-cancelled"
  if [[ -f "$STALE_SENTINEL" ]]; then
    command -p rm -f "$STALE_SENTINEL" 2>/dev/null \
      && echo "target: init: removed stale .target-cancelled from prior session" >&2
  fi

  # Wipe stale Plan Mode sidecar (mirrors .target-cancelled pattern).
  PENDING_PLAN="$STATE_DIR/.pending-plan.md"
  if [[ -f "$PENDING_PLAN" ]]; then
    _PP_TTL="${PENDING_PLAN_TTL_SECONDS:-14400}"
    _PP_CLAUDE_SID="${TARGET_TRANSCRIPT_ID:-${CLAUDE_CODE_SESSION_ID:-}}"
    _PP_SID="$(grep -m1 '^session_id:' "$PENDING_PLAN" 2>/dev/null \
      | sed -e 's/^session_id:[[:space:]]*//' -e 's/\r$//')"
    _PP_MTIME=$(stat -c "%Y" "$PENDING_PLAN" 2>/dev/null || stat -f "%m" "$PENDING_PLAN" 2>/dev/null || echo 0)
    _PP_AGE=$(( $(date -u +%s) - _PP_MTIME ))
    _PP_WIPE=""
    if [[ "$_PP_AGE" -gt "$_PP_TTL" ]]; then
      _PP_WIPE="stale_ttl"
    elif [[ -n "$_PP_SID" && -n "$_PP_CLAUDE_SID" \
            && "$_PP_CLAUDE_SID" != "null" && "$_PP_SID" != "$_PP_CLAUDE_SID" ]]; then
      _PP_WIPE="session_mismatch"
    fi
    if [[ -n "$_PP_WIPE" ]]; then
      command -p rm -f "$PENDING_PLAN" 2>/dev/null \
        || /bin/rm -f "$PENDING_PLAN" 2>/dev/null \
        || rm -f "$PENDING_PLAN" 2>/dev/null || true
      if [[ ! -f "$PENDING_PLAN" ]]; then
        echo "target: init: removed stale .pending-plan.md ($_PP_WIPE)" >&2
      else
        echo "target: init: WARNING: could not remove stale .pending-plan.md ($_PP_WIPE)" >&2
      fi
    fi
  fi

  # ── Provider + cross-project ──────────────────────────────────────
  CROSS_PROJECT="${TARGET_CROSS_PROJECT:-false}"
  PROVIDER="$(detect_provider)"
  PROVIDER_MODE="$(gemini_agent_mode "$PROVIDER")"
  PROVIDER_UPGRADE_REASON="$(gemini_upgrade_reason "$PROVIDER")"

  # ── Session identifiers ───────────────────────────────────────────
  local_owner_pid="${PPID:-$$}"
  local_owner_cwd="$REPO_ROOT"

  # Empty when neither env var is set; the manifest renderer writes `null`
  # for empties and the stop-hook shim treats null/empty as guard-disabled
  # (codex P2 on #447: a literal "null" must never match-fail every real
  # transcript and silently disable the stop hook).
  claude_transcript_id="${TARGET_TRANSCRIPT_ID:-${CLAUDE_CODE_SESSION_ID:-null}}"

  # Codex exposes the durable conversation identity directly. Record it beside
  # the Claude transcript id; unset/whitespace-only values render as YAML null.
  _codex_thread_raw="${CODEX_THREAD_ID:-}"
  _codex_thread_compact="${_codex_thread_raw//[[:space:]]/}"
  if [[ -n "$_codex_thread_compact" ]]; then
    codex_thread_id="$_codex_thread_raw"
  else
    codex_thread_id="null"
  fi

  # session_id: {UTC-timestamp}-{infix}{PPID}-{6 hex chars of /dev/urandom}
  # ab-7303e5d7: TARGET_SESSION_ID is the absolute override (megawalk walkers
  # pre-assign it). Otherwise mint one id per target run. CODEX_THREAD_ID is a
  # durable conversation/claim-owner identity, but reusing it as session_id
  # would collide with prior loop termination and finalize events when the same
  # Codex conversation runs a second target.
  #
  # Provenance infix lives glued to the pid INSIDE segment 2 (never a 4th
  # dash-segment - 3 segments is load-bearing for split('-')[0] consumers). Driver
  # precedence: a driver-assigned TARGET_SESSION_ID already carries its tag (mw/mt)
  # and is used verbatim; the self-mint path below glues the 2-char PROVIDER code so
  # a direct/bg claude session reads {ts}-cl{pid}-{6hex}. Unknown/empty provider ->
  # no infix (preserves the legacy {ts}-{pid}-{6hex} shape; never a hard error).
  if [[ -n "$_codex_thread_compact" ]]; then
    _prov_infix="cx"
  else
    case "$PROVIDER" in
      claude)   _prov_infix="cl" ;;
      codex)    _prov_infix="cx" ;;
      gemini)   _prov_infix="gm" ;;
      agy)      _prov_infix="ag" ;;
      hermes)   _prov_infix="hm" ;;
      opencode) _prov_infix="oc" ;;
      *)        _prov_infix="" ;;
    esac
  fi
  if [[ -n "${TARGET_SESSION_ID:-}" ]]; then
    local_session_id="$TARGET_SESSION_ID"
  else
    local_sid_entropy="$(od -An -N3 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n' || echo "")"
    if [[ -n "$local_sid_entropy" ]]; then
      local_session_id="$(date -u +%Y%m%dT%H%M%SZ)-${_prov_infix}${local_owner_pid}-${local_sid_entropy}"
    else
      local_session_id="$(date -u +%Y%m%dT%H%M%SZ)-${_prov_infix}${local_owner_pid}-${RANDOM:-0}${RANDOM:-0}"
    fi
  fi
  if [[ -n "${TARGET_SESSION_ID:-}" ]]; then
    claim_owner_id="$TARGET_SESSION_ID"
  elif [[ -n "$_codex_thread_compact" ]]; then
    claim_owner_id="$_codex_thread_raw"
  else
    claim_owner_id="$local_session_id"
  fi

  # ── Mission context ───────────────────────────────────────────────
  _fmt_mission_str() {
    local v="$1"
    if [[ -z "$v" ]]; then
      echo "null"
    else
      echo "\"${v//\"/\\\"}\""
    fi
  }
  MISSION_ID_YAML="$(_fmt_mission_str "${TARGET_MISSION_ID:-}")"
  MISSION_WAVE_YAML="${TARGET_MISSION_WAVE:-null}"
  MISSION_SLUG_YAML="$(_fmt_mission_str "${TARGET_MISSION_SLUG:-}")"
  MISSION_FROM_MSG_ID_YAML="$(_fmt_mission_str "${TARGET_MISSION_FROM_MSG_ID:-}")"

  # ── owner_started_at ─────────────────────────────────────────────
  local_owner_started_at="$TIMESTAMP"

  # ── Escape helper values ──────────────────────────────────────────
  escaped_input="${INITIAL_INPUT//\"/\\\"}"
  escaped_plan_path="${INITIAL_PLAN_PATH//\"/\\\"}"
  escaped_reason="${PROVIDER_UPGRADE_REASON//\"/\\\"}"

  # ── Write the manifest (atomic via temp + mv) ─────────────────────
  local_temp="$(mktemp "${STATE_FILE}.tmp.XXXXXX")"

  # Budget lines: omit when unconfigured (loop-check treats absence as uncapped).
  _budget_lines=""
  if [[ -n "$_budget_wall_clock" ]]; then
    _budget_lines="${_budget_lines}budget_wall_clock_cap_minutes: ${_budget_wall_clock}"$'\n'
  fi
  if [[ -n "$_budget_cost" ]]; then
    _budget_lines="${_budget_lines}budget_cost_cap_usd: ${_budget_cost}"$'\n'
  fi

  cat > "$local_temp" << EOF
---
# fno_id = target-minted run id (canonical). session_id mirrors it for one
# release and is NOT the harness session (that is claude_session_id/codex_thread_id).
fno_id: $local_session_id
session_id: $local_session_id
created_at: $TIMESTAMP
input: "${escaped_input}"
plan_path: "${escaped_plan_path}"
cross_project: $CROSS_PROJECT
provider: $PROVIDER
provider_mode: ${PROVIDER_MODE:-standard}
provider_upgrade_reason: "${escaped_reason:-}"
owner_pid: $local_owner_pid
owner_started_at: $local_owner_started_at
owner_cwd: "$local_owner_cwd"
claude_session_id: $claude_transcript_id
codex_thread_id: $codex_thread_id
scratchpad_path: $REPO_ROOT/.fno/scratchpad
target_size: ${TARGET_SIZE:-}
# Dispatch pins - a model/provider chosen at \`fno target start\`/\`init\`, carried
# to this session's dispatched workers. Empty = unpinned (spawn-time defaults).
dispatch_model: ${TARGET_DISPATCH_MODEL:-}
dispatch_provider: ${TARGET_DISPATCH_PROVIDER:-}
# Skip flags - sourced from the size profile (TARGET_SIZE) plus per-flag
# TARGET_NO_* env overrides. The LLM must NEVER flip these on its own judgment.
no_external: $no_external
no_docs: $no_docs
no_ship: $no_ship
batched: $batched
no_verify: $no_verify
no_goals: $no_goals
no_browser: $no_browser
no_clean: $no_clean
no_how_to: $no_how_to
no_memory: $no_memory
no_deferrals_capture: $no_deferrals_capture
has_ui: $has_ui
# Attended / advisory inputs (consumed by fno-agents loop-check)
attended: $_attended
advisory: $_advisory
${_budget_lines}# Auto-merge inputs
auto_merge_enabled: $AUTO_MERGE_ENABLED
auto_merge_approved: $AUTO_MERGE_APPROVED
# Mission context
mission_id: $MISSION_ID_YAML
mission_wave: $MISSION_WAVE_YAML
mission_slug: $MISSION_SLUG_YAML
mission_from_msg_id: $MISSION_FROM_MSG_ID_YAML
---
# Target Session State

Immutable session manifest. Initialized at $TIMESTAMP.
plan_path may be first-filled if empty; all other fields are read-only.
EOF

  mv "$local_temp" "$STATE_FILE"

  # ── Graph + node claim (gate-provenance phase 02b; ab-fcf9cec5) ───
  _GRAPH_FILE="${HOME}/.fno/graph.json"
  _NODE_ID=""
  if [[ "$INITIAL_INPUT" =~ ^ab-[0-9a-f]{8}$ ]] || { [[ "$INITIAL_INPUT" =~ ^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$ ]] && grep -q "\"${INITIAL_INPUT}\"" "$_GRAPH_FILE" 2>/dev/null; }; then
    _NODE_ID="$INITIAL_INPUT"
  elif [[ -f "$_GRAPH_FILE" && -n "$INITIAL_PLAN_PATH" ]]; then
    _NODE_ID=$(python3 - "$_GRAPH_FILE" "$INITIAL_PLAN_PATH" "$REPO_ROOT" <<'PYEOF' 2>/dev/null || true
import json, os, sys
graph_path, raw_target, repo_root = sys.argv[1], sys.argv[2], sys.argv[3]
if not os.path.isabs(raw_target):
    raw_target = os.path.join(repo_root, raw_target)
try:
    target = os.path.realpath(raw_target)
except OSError:
    sys.exit(0)
try:
    data = json.load(open(graph_path))
except Exception:
    sys.exit(0)
entries = data.get("entries", []) if isinstance(data, dict) else data
for entry in entries:
    plan_path = entry.get("plan_path")
    if not plan_path:
        continue
    abs_plan = plan_path if os.path.isabs(plan_path) else os.path.join(repo_root, plan_path)
    try:
        if os.path.realpath(abs_plan) == target:
            print(entry.get("id", ""))
            break
    except OSError:
        pass
PYEOF
)
  fi

  if [[ -n "$_NODE_ID" && -n "$claim_owner_id" ]]; then
    # Does THIS session own the node? Set by `fno claim` below, the sole liveness
    # authority (x-4af4). The TTL claim runs FIRST and the graph lock is stamped
    # only on its success, so a legitimate STALE steal never leaves a dead prior
    # owner on the node (the stale-locked-by-leak this reorder fixes).
    _NODE_OWNED=0
    _ROADMAP_TASKS="${REPO_ROOT}/scripts/roadmap-tasks.py"

    # fno claim acquire (global TTL lock; authoritative mutex)
    if command -v fno >/dev/null 2>&1; then
      _CLAIM_KEY="node:${_NODE_ID}"
      _CLAIM_HOLDER="target-session:${claim_owner_id}"
      _CLAIM_TTL="${TARGET_CLAIM_TTL:-2h}"
      # Durable session pid for the hybrid liveness pid-arm (ab-cc5553f2): the
      # nearest `claude` ancestor outlives the transient init subprocess, so an
      # alive-but-suspended session keeps its node claim past the TTL. Empty =>
      # omit --pid and degrade to TTL-only liveness, byte-for-byte as before.
      _SESSION_PID="$(fno claim session-pid --from-pid "$$" 2>/dev/null || true)"
      _PID_FLAGS=""; [[ "$_SESSION_PID" =~ ^[0-9]+$ ]] && _PID_FLAGS="--pid $_SESSION_PID"
      # Unquoted on purpose: empty => zero args (bash 3.2 set -u safe, unlike an
      # empty "${array[@]}"); the regex guarantees $_SESSION_PID is digits only.
      if FNO_CLAIMS_ROOT="$HOME" fno claim acquire "$_CLAIM_KEY" \
            --holder "$_CLAIM_HOLDER" --ttl "$_CLAIM_TTL" $_PID_FLAGS \
            --reason "target dispatch" >/dev/null 2>"$STATE_DIR/.claim-err"; then
        echo "target_claim_key: \"$_CLAIM_KEY\"" >> "$STATE_FILE"
        echo "target_claim_holder: \"$_CLAIM_HOLDER\"" >> "$STATE_FILE"
        echo "target_claim_ttl: \"$_CLAIM_TTL\"" >> "$STATE_FILE"
        rm -f "$STATE_DIR/.claim-err"
        _NODE_OWNED=1
      else
        _acq_rc=$?
        # The modern claim is the authority: if it failed, this session does not
        # own the node even if the legacy layer happened to win (the handoff
        # retry below re-sets this on a successful re-acquire).
        _NODE_OWNED=0
        echo "target: WARNING: fno claim acquire failed (rc=$_acq_rc) for $_CLAIM_KEY" >&2
        [[ -s "$STATE_DIR/.claim-err" ]] && cat "$STATE_DIR/.claim-err" >&2
        if [[ "$_acq_rc" -eq 1 ]]; then
          # Check if this is a sanctioned handoff successor before cancelling.
          # A delegated event in events.jsonl names child_session as a prefix
          # of the current session's CLAUDE_CODE_SESSION_ID; if matched, retry
          # the claim acquire for up to TARGET_CLAIM_WAIT_TIMEOUT seconds
          # instead of immediately touching the cancel sentinel (AC2-FR).
          _EVENTS_FILE="$STATE_DIR/events.jsonl"
          _THIS_SID="${claude_transcript_id:-}"
          # Strip dashes and lowercase for prefix comparison (short bg hex vs UUID)
          _SID_NODASH="$(printf '%s' "$_THIS_SID" | tr -d '-' | tr '[:upper:]' '[:lower:]')"
          _IS_HANDOFF_SUCCESSOR=0
          if [[ -n "$_SID_NODASH" && -f "$_EVENTS_FILE" ]]; then
            # Look for a delegated event for this node whose child_session is a
            # prefix of our session id (minimum 6 hex chars).
            #
            # Verified relationship (2026-06-05): the claude create path (now spawn --provider claude; pre-Group-1: ask)
            # prints a short-id via the "backgrounded · [0-9a-f]{8} ·" banner
            # (crates/fno-agents/src/claude_ask.rs:287). That 8-hex token is the
            # FIRST 8 HEX CHARS of the child's CLAUDE_CODE_SESSION_ID (= the full
            # UUID stored in claude_transcript_id). Empirically confirmed:
            # bg job f47aa2eb <-> transcript f47aa2eb-9913-480f-9af8-a40eadbb2940.
            # The prefix match is therefore sound for sanctioned handoff successors.
            _delegated_evs=""
            _delegated_evs="$(grep '"type":"delegated"' "$_EVENTS_FILE" 2>/dev/null \
                               | grep "\"node_id\":\"${_NODE_ID}\"" 2>/dev/null || true)"
            while IFS= read -r _ev_line; do
              _ev_child="$(printf '%s' "$_ev_line" \
                | grep -o '"child_session":"[^"]*"' 2>/dev/null \
                | sed 's/"child_session":"//;s/"//' | tr -d '-' \
                | tr '[:upper:]' '[:lower:]' || true)"
              if [[ -n "$_ev_child" && ${#_ev_child} -ge 6 ]]; then
                # prefix match: our sid starts with the child_session hex
                _pfx="${_SID_NODASH:0:${#_ev_child}}"
                if [[ "$_pfx" == "$_ev_child" ]]; then
                  _IS_HANDOFF_SUCCESSOR=1
                  break
                fi
              fi
            done <<< "$_delegated_evs"
          fi

          if [[ "$_IS_HANDOFF_SUCCESSOR" -eq 1 ]]; then
            # Bounded claim-wait: retry until the parent releases or timeout.
            _WAIT_TIMEOUT="${TARGET_CLAIM_WAIT_TIMEOUT:-60}"
            _WAIT_INTERVAL="${TARGET_CLAIM_WAIT_INTERVAL:-5}"
            _waited=0
            _claim_acquired=0
            echo "target: handoff successor detected; waiting up to ${_WAIT_TIMEOUT}s for claim release of $_CLAIM_KEY" >&2
            while [[ "$_waited" -lt "$_WAIT_TIMEOUT" ]]; do
              sleep "$_WAIT_INTERVAL" 2>/dev/null || true
              # Advance by the real interval (exact accounting); a zero
              # interval still advances by 1 so the bounded wait terminates
              # (tests use INTERVAL=0 to skip wall-clock sleeps).
              _waited=$((_waited + (_WAIT_INTERVAL > 0 ? _WAIT_INTERVAL : 1)))
              if FNO_CLAIMS_ROOT="$HOME" fno claim acquire "$_CLAIM_KEY" \
                    --holder "$_CLAIM_HOLDER" --ttl "$_CLAIM_TTL" $_PID_FLAGS \
                    --reason "target dispatch" >/dev/null 2>"$STATE_DIR/.claim-err"; then
                _claim_acquired=1
                break
              fi
            done
            if [[ "$_claim_acquired" -eq 1 ]]; then
              echo "target_claim_key: \"$_CLAIM_KEY\"" >> "$STATE_FILE"
              echo "target_claim_holder: \"$_CLAIM_HOLDER\"" >> "$STATE_FILE"
              echo "target_claim_ttl: \"$_CLAIM_TTL\"" >> "$STATE_FILE"
              rm -f "$STATE_DIR/.claim-err"
              _NODE_OWNED=1
            else
              echo "target_claim_blocked_reason: handoff_claim_wait_timeout" >> "$STATE_FILE"
              echo "RESULT: BLOCKED" >&1
              echo "TASK: ${TARGET_INPUT:-unknown}" >&1
              echo "REASON: node claim never freed within ${_WAIT_TIMEOUT}s after delegated event named this session" >&1
              echo "UNBLOCKS_AFTER: parent generation releases node:${_NODE_ID} or stale-claim recovery reclaims it" >&1
            fi
          else
            touch "$STATE_DIR/.target-cancelled"
            echo "target_claim_blocked_reason: claim_held_by_other" >> "$STATE_FILE"
            echo "graph_node_claim_refused: held_by_other" >> "$STATE_FILE"
          fi
        else
          echo "target_claim_blocked_reason: acquire_error_rc_${_acq_rc}" >> "$STATE_FILE"
        fi
      fi
    fi

    # Graph lock stamp on claim success: unconditional (overwriting a stale prior
    # owner is the point), retried once. Non-fatal - the TTL claim is
    # authoritative and the graph field is display/routing metadata, so a
    # lock-contended graph.json must not abort init (AC9-FR).
    if [[ "$_NODE_OWNED" -eq 1 && -f "$_GRAPH_FILE" ]]; then
      _STAMP_LOG="$STATE_DIR/.init-claim.log"
      # Harness stamp (US6): the holder's provider + harness-session UUID, so an
      # operator/peek can jump from a node straight to `claude -r <uuid>`. The
      # UUID prefers codex thread, then the claude transcript, then gemini.
      _HARNESS_SESSION=""
      if [[ -n "${_codex_thread_compact:-}" ]]; then
        _HARNESS_SESSION="${_codex_thread_raw:-}"
      elif [[ -n "${claude_transcript_id:-}" && "${claude_transcript_id:-}" != "null" ]]; then
        _HARNESS_SESSION="$claude_transcript_id"
      elif [[ -n "${GEMINI_SESSION_ID:-}" ]]; then
        _HARNESS_SESSION="$GEMINI_SESSION_ID"
      fi
      # Unquoted expansion below (same pattern as $_PID_FLAGS): provider + UUID
      # are single tokens, so word-splitting yields exactly the intended args. An
      # empty provider omits the flag rather than passing a blank value.
      _HARNESS_FLAGS=""
      [[ -n "${PROVIDER:-}" ]] && _HARNESS_FLAGS="--locked-by-harness $PROVIDER"
      [[ -n "$_HARNESS_SESSION" ]] && _HARNESS_FLAGS="$_HARNESS_FLAGS --locked-by-harness-session $_HARNESS_SESSION"
      if python3 "$_ROADMAP_TASKS" update "$_NODE_ID" --locked-by "$claim_owner_id" $_HARNESS_FLAGS 2>"$_STAMP_LOG" >/dev/null \
         || python3 "$_ROADMAP_TASKS" update "$_NODE_ID" --locked-by "$claim_owner_id" $_HARNESS_FLAGS 2>"$_STAMP_LOG" >/dev/null; then
        rm -f "$_STAMP_LOG"
        echo "target: graph node $_NODE_ID lock stamped for $claim_owner_id" >&2
      else
        echo "target: WARNING: graph locked_by stamp failed (non-fatal; TTL claim authoritative; see $_STAMP_LOG)" >&2
      fi
    fi
    # graph_node_id written exactly once: the node id when a claim layer won and
    # the node actually exists in the graph, else null (a missing graph.json or an
    # ab-id not present in the graph stays null - the modern claim is just a lock
    # and does not prove the backlog row exists).
    if [[ "$_NODE_OWNED" -eq 1 && -f "$_GRAPH_FILE" ]] \
         && grep -q "\"${_NODE_ID}\"" "$_GRAPH_FILE" 2>/dev/null; then
      echo "graph_node_id: $_NODE_ID" >> "$STATE_FILE"
    else
      echo "graph_node_id: null" >> "$STATE_FILE"
    fi
  else
    echo "graph_node_id: null" >> "$STATE_FILE"
  fi

  echo "target: session manifest written: $STATE_FILE"

else
  # State file already exists. Leave it alone - the immutable manifest must
  # not be overwritten on resume. Terminal states (COMPLETE/BLOCKED) stay
  # terminal until the user starts a new target session.
  echo "target: session manifest exists; leaving unchanged"
fi
