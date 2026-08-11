#!/usr/bin/env bash
# setup-worktree.sh - link gitignored shared files from the canonical project
# into a worktree. Idempotent and never destructive.
#
# Usage:
#   bash scripts/setup/setup-worktree.sh                           # auto-detect canonical
#   CANONICAL=/path/to/canonical bash scripts/setup/setup-worktree.sh
#
# Conductor calls this via conductor.json's scripts.setup hook with
# CONDUCTOR_ROOT_PATH set to the canonical project. Manual `git worktree
# add` or the fno git-worktrees skill should call this directly.
#
# Safety contract (load-bearing):
#   - Uses `ln -sf` to create or refresh symlinks; never `rm -rf` a target
#   - If a target already exists as a real (non-symlink) file or directory,
#     SKIP it with a stderr warning, except events.jsonl's lock-protected migration
#   - Never deletes an existing symlink either; ln -sf replaces atomically
#   - Each link is independent so a failure on one does not block the rest

set -euo pipefail

# Defensive PATH - some worktrees inherit a stripped PATH from per-directory
# env hooks (direnv, etc.). Prepend the standard system paths so coreutils
# (mkdir, ln, rm, ls) always resolve.
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"

# Resolve canonical project root (where the shared files live). Priority:
#   1. CANONICAL env var (manual override)
#   2. CONDUCTOR_ROOT_PATH (set by Conductor when invoking via scripts.setup)
#   3. git-common-dir resolution (works from any worktree of the same repo)
#   4. $HOME/code/me/fno (last-ditch fallback for non-git contexts)
CANONICAL="${CANONICAL:-${CONDUCTOR_ROOT_PATH:-}}"
if [[ -z "$CANONICAL" ]]; then
  # In a worktree, git-common-dir points at the main repo's .git directory.
  # Going one level up gets the canonical worktree (the main checkout).
  COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null || true)
  if [[ -n "$COMMON_DIR" && -d "$COMMON_DIR" ]]; then
    CANONICAL=$(cd "$COMMON_DIR/.." && pwd)
  else
    CANONICAL="$HOME/code/me/fno"
  fi
fi

if [[ ! -d "$CANONICAL" ]]; then
  echo "setup-worktree: canonical project not found at $CANONICAL" >&2
  exit 1
fi

# Resolve worktree root (where we are linking INTO). Default to cwd.
WORKTREE="${WORKTREE:-$(pwd)}"

# Refuse the whole script when the two roots are one directory, BEFORE the
# mkdir and every link helper. Not a check inside link_artifact/link_file/
# link_dir: when the roots coincide the script is a no-op at best and
# destructive at worst, so the answer is to not run it at all. link_artifact
# `rm -f`s the real file before `ln -sf "$source" "$target"`, which with equal
# paths leaves a symlink pointing at itself - that is how this repo's
# .fno/codemap.md was lost on 2026-07-26.
#
# `-ef` compares device+inode, so a symlinked, relative, or /tmp-vs-/private/tmp
# invocation cannot slip past it the way a string equality test would.
#
# Exit 0, not non-zero: "already canonical, nothing to link" is a successful
# no-op, and `fno target start` treats any non-zero from this script as fatal
# ("refusing to initialize against unverified shared state", target_cli.py).
if [[ "$CANONICAL" -ef "$WORKTREE" ]]; then
  echo "setup-worktree: refusing to symlink canonical -> canonical (no-op): $CANONICAL" >&2
  exit 0
fi

mkdir -p "$WORKTREE/.fno" "$WORKTREE/.claude"

# Link a single file. Skips if target is already a non-symlink real file.
# Reserved for files where local divergence might be user data we cannot lose
# (settings, ledgers, task lists).
link_file() {
  local rel="$1"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"

  if [[ ! -e "$source" ]]; then
    echo "setup-worktree: source missing, skipping: $rel" >&2
    return 0
  fi

  if [[ -e "$target" && ! -L "$target" ]]; then
    echo "setup-worktree: refusing to overwrite real file: $target" >&2
    return 0
  fi

  ln -sf "$source" "$target"
}

# Link a regenerable artifact. Replaces existing real files in the worktree
# because the canonical copy is authoritative and the artifact is rebuilt
# on demand (e.g. codemap.md). NEVER call this on user data.
link_artifact() {
  local rel="$1"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"

  if [[ ! -e "$source" ]]; then
    echo "setup-worktree: source missing, skipping: $rel" >&2
    return 0
  fi

  # If the target is a real file (not a symlink), replace it. Real dirs are
  # NOT replaced by this helper - that's link_dir's job and it has its own
  # safety check.
  if [[ -e "$target" && ! -L "$target" && ! -d "$target" ]]; then
    rm -f "$target"
  fi

  ln -sf "$source" "$target"
}

# Link a directory by symlinking the dir itself (not its contents).
# Same skip-if-real-dir-exists rule as link_file.
link_dir() {
  local rel="$1"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"

  if [[ ! -d "$source" ]]; then
    echo "setup-worktree: source dir missing, skipping: $rel" >&2
    return 0
  fi

  if [[ -e "$target" && ! -L "$target" ]]; then
    if [[ -d "$target" ]] && [[ -n "$(ls -A "$target" 2>/dev/null || true)" ]]; then
      echo "setup-worktree: refusing to overwrite non-empty real dir: $target" >&2
      return 0
    fi
    # Empty real dir - safe to remove and replace with symlink. Uses rmdir
    # which only works on empty dirs (will fail loudly otherwise).
    rmdir "$target" 2>/dev/null || {
      echo "setup-worktree: could not remove existing $target, skipping" >&2
      return 0
    }
  fi

  # -n / --no-dereference: when target already exists as a symlink-to-dir,
  # treat it as the link name (replace it in place) rather than following
  # it and creating a new link INSIDE it. Without -n, a repeat run lands a
  # recursive symlink `target/<basename(target)>` inside the canonical
  # destination, polluting shared state. Both BSD (macOS) and GNU `ln`
  # accept -n. Codex flagged this on PR #320 round 3.
  ln -sfn "$source" "$target"
}

# Acquire the same owner-token mkdir mutex used by the Python and Rust event
# writers, without stale-stealing a lock whose holder setup cannot identify.
acquire_events_dir() {
  local lock_dir="$1"
  local token="$2"
  local attempts=0
  while ! mkdir "$lock_dir" 2>/dev/null; do
    if (( attempts >= 300 )); then
      return 1
    fi
    sleep 0.1
    attempts=$((attempts + 1))
  done
  if ! printf '%s' "$token" > "$lock_dir/owner" 2>/dev/null; then
    rmdir "$lock_dir" 2>/dev/null || true
    return 1
  fi
}

release_events_dir() {
  local lock_dir="$1"
  local token="$2"
  [[ -d "$lock_dir" ]] || return 0
  [[ -r "$lock_dir/owner" ]] || return 0
  [[ "$(< "$lock_dir/owner")" == "$token" ]] || return 0
  rm -f "$lock_dir/owner"
  rmdir "$lock_dir" 2>/dev/null || true
}

EVENTS_MIGRATION_TOKEN=""
EVENTS_MIGRATION_DIRS=()

cleanup_events_migration() {
  local index
  for ((index=${#EVENTS_MIGRATION_DIRS[@]} - 1; index >= 0; index--)); do
    release_events_dir "${EVENTS_MIGRATION_DIRS[index]}" "$EVENTS_MIGRATION_TOKEN"
  done
  EVENTS_MIGRATION_DIRS=()
  EVENTS_MIGRATION_TOKEN=""
}

trap 'cleanup_events_migration' EXIT
trap 'cleanup_events_migration; exit 130' INT
trap 'cleanup_events_migration; exit 143' TERM

ensure_trailing_newline() {
  local path="$1"
  [[ -s "$path" ]] || return 0
  if [[ "$(tail -c 1 "$path" | wc -l | tr -d ' ')" == "0" ]]; then
    printf '\n' >> "$path"
  fi
}

wait_for_shell_event_writers() {
  local events_path="$1"
  local active_dir="${events_path}.shell-writers.d"
  local attempts=0
  local entries=()
  while [[ -d "$active_dir" ]]; do
    shopt -s nullglob
    entries=("$active_dir"/*)
    shopt -u nullglob
    local entry name pid
    for entry in "${entries[@]}"; do
      name="$(basename "$entry")"
      pid="${name%%.*}"
      if [[ "$pid" =~ ^[0-9]+$ ]] && ! kill -0 "$pid" 2>/dev/null; then
        rmdir "$entry" 2>/dev/null || true
      fi
    done
    shopt -s nullglob
    entries=("$active_dir"/*)
    shopt -u nullglob
    if (( ${#entries[@]} == 0 )); then
      rmdir "$active_dir" 2>/dev/null || true
      return 0
    fi
    if (( attempts >= 300 )); then
      return 1
    fi
    sleep 0.1
    attempts=$((attempts + 1))
  done
}

# Migrate a worktree-local journal before linking it to the canonical journal.
# The GC markers pause the bounded shell appenders, and the ordinary mutexes
# pause Python, Rust claims, and Journal writers. Locks are acquired in sorted
# path order so two concurrent setup runs cannot deadlock each other.
link_events_journal() {
  local rel=".fno/events.jsonl"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"
  local token="$(hostname):$$:$(date -u +%s):$RANDOM"
  EVENTS_MIGRATION_TOKEN="$token"
  EVENTS_MIGRATION_DIRS=()

  mkdir -p "$(dirname "$source")" "$(dirname "$target")"
  : >> "$source" || {
    echo "setup-worktree: cannot create canonical events journal: $source" >&2
    return 1
  }

  if [[ -L "$target" ]]; then
    ln -sfn "$source" "$target"
    return 0
  fi
  if [[ ! -e "$target" ]]; then
    ln -s "$source" "$target"
    return 0
  fi
  if [[ ! -f "$target" ]]; then
    echo "setup-worktree: refusing to replace non-file events journal: $target" >&2
    return 0
  fi

  local source_gc="${source}.gc.d"
  local target_gc="${target}.gc.d"
  local source_lock="${source}.lock.d"
  local target_lock="${target}.lock.d"
  local first_gc="$source_gc" second_gc="$target_gc"
  local first_lock="$source_lock" second_lock="$target_lock"
  if [[ "$second_gc" < "$first_gc" ]]; then
    first_gc="$target_gc"
    second_gc="$source_gc"
  fi
  if [[ "$second_lock" < "$first_lock" ]]; then
    first_lock="$target_lock"
    second_lock="$source_lock"
  fi

  if ! acquire_events_dir "$first_gc" "$token"; then
    echo "setup-worktree: events migration timed out on $first_gc" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$first_gc")
  if ! acquire_events_dir "$second_gc" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on $second_gc" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$second_gc")
  if ! wait_for_shell_event_writers "$source"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on ${source}.shell-writers.d" >&2
    return 1
  fi
  if ! wait_for_shell_event_writers "$target"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on ${target}.shell-writers.d" >&2
    return 1
  fi
  # Pre-rendezvous shells do not register, so retain one bounded rollout grace.
  sleep 0.1
  if ! acquire_events_dir "$first_lock" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on $first_lock" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$first_lock")
  if ! acquire_events_dir "$second_lock" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on $second_lock" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$second_lock")

  local rc=0
  local backup="${target}.pre-share.$(date -u +%Y%m%dT%H%M%SZ).$$"
  ensure_trailing_newline "$source" || rc=$?
  if (( rc == 0 )); then
    mv "$target" "$backup" || rc=$?
  fi
  if (( rc == 0 )); then
    ln -s "$source" "$target" || rc=$?
  fi
  if (( rc == 0 )) && [[ -s "$backup" ]]; then
    cat "$backup" >> "$source" || rc=$?
    if (( rc == 0 )); then
      ensure_trailing_newline "$source" || rc=$?
    fi
  fi

  if (( rc != 0 )); then
    if [[ -L "$target" ]]; then
      rm -f "$target" 2>/dev/null || true
    fi
    if [[ ! -e "$target" && -e "$backup" ]]; then
      mv "$backup" "$target" 2>/dev/null || true
    fi
    echo "setup-worktree: events migration failed; local journal retained: $target" >&2
  else
    echo "setup-worktree: migrated events journal; backup retained at $backup" >&2
  fi

  cleanup_events_migration
  return "$rc"
}

# Shared content (Obsidian vault link)
link_dir "internal"

# Shared fno state (project-level, propagates across worktrees)
link_file ".fno/config.toml"
# One journal per repository makes exact-HEAD gate evidence visible across
# isolated reviewer worktrees. Real worktree journals take the migration path
# above instead of link_file's ordinary skip-if-real-file behavior.
if ! link_events_journal; then
  echo "setup-worktree: events journal left worktree-local after migration failure" >&2
fi
# config.local.toml is deliberately NOT linked: it is the one config file kept
# per-worktree, layering the collision-prone keys (post_merge.parking_lot_path,
# project.id) on top of the shared config.toml (x-cbce). Do not add a
# link_file for it here - a link would re-share exactly the keys it exists to
# diverge. Absent by default (= shared behavior); seed one only when a worktree
# needs its own value.
# ledger.json / ledger.md are deliberately NOT linked: paths.ledger_json() is
# pinned GLOBAL (~/.fno/ledger.json), so a project-local copy is a stray fork,
# not a share. The former dual-write was the split-brain that corrupted
# node-level joins; linking it here re-created the stray in canonical AND every
# worktree, and a setup run whose WORKTREE was canonical linked the file to
# itself (an ELOOP that every .exists() probe reads as simply "absent").
# Do not add a link_file for either - tests/test-register-task.sh cB-AC5-FR
# asserts neither the worktree nor the canonical repo grows a stray ledger.
# carveouts.jsonl: a worktree-local carveout (deferred decision / out-of-scope
# bug) must be visible to the canonical retro-triage harvest at merge, so link
# it to canonical alongside the other shared ledgers. Skip-if-missing until the
# first carveout lands.
link_file ".fno/carveouts.jsonl"
# codemap is a regenerated artifact; last-writer-wins is the desired
# behavior so all worktrees see the latest map.
link_artifact ".fno/codemap.md"

# Wake signals (per-project, NOT per-session). Holds filesystem signals
# dropped by the inbox drain that the project's agents read on wake.
# Skip-if-missing so a fresh canonical doesn't error.
#
# Note: the cross-project inbox itself does NOT live under .fno/.
# Each project's inbox is at internal/agents/{project}/inbox.md (reached
# through the canonical internal/ symlink, which is linked separately
# above). Do not add a `.fno/inbox` link here.
link_dir ".fno/wake-signals"

# Consolidated gate-attestation artifacts ONLY. Per-phase artifacts
# (.fno/artifacts/<phase>-<session_id>.md) stay worktree-local on
# purpose: archive-artifacts.sh's session-aware stale sweep iterates
# `$artifacts_dir/*-*.md` at session end and moves any artifact whose
# frontmatter session_id != current_sid into ${plan_dir}/artifacts-archive/.
# If we symlinked the whole artifacts dir to canonical, worktree A's
# completion sweep would move worktree B's ACTIVE per-phase artifacts AND
# every prior consolidated file out from under them - breaking B's gate
# verification and defeating the "artifacts by PR" persistence goal. The
# consolidator (scripts/lib/consolidate-artifacts.sh) writes its retrospective
# files into the `consolidated/` subdir specifically, and the archive sweep's
# glob does not recurse into subdirectories, so symlinking only that subdir
# gives us cross-worktree persistence without crossing the sweep's reach.
# Codex flagged the original whole-dir link as P1 on PR #320 (round 2).
mkdir -p "$WORKTREE/.fno/artifacts"
# Canonical-side consolidated dir: best-effort. When it already exists as a
# symlink (pre-existing canonical state), `mkdir -p` trips ELOOP ("Too many
# levels of symbolic links"). That is benign - the link target is already
# there - but under `set -e` it would abort the WHOLE setup, leaving every
# link below (.claude/skills, .agents, ...) uncreated. Guard it so the rest
# of the linking always runs.
mkdir -p "$CANONICAL/.fno/artifacts/consolidated" 2>/dev/null || true
link_dir ".fno/artifacts/consolidated"

# Shared Claude Code state (autoMemoryDirectory pin, permission allowlist,
# locally-installed agents/commands/skills)
link_file ".claude/settings.local.json"
link_dir ".claude/agents"
link_dir ".claude/commands"
link_dir ".claude/skills"
# Scheduled tasks: the /schedule skill writes cron-like state here. Project
# level so worktrees see the same schedule and the lock prevents two
# worktrees racing on the same write. Skip-if-missing until the first
# schedule entry lands.
link_file ".claude/scheduled_tasks.json"
link_file ".claude/scheduled_tasks.lock"

# Other gitignored .claude/ state that should follow the canonical:
#   - skill-scoping-state.json: which skills are enabled per scope
#   - audit-progress.txt: long-running audit checkpoint
#   - plans/: free-form planning dir used by some skills
# All skip-if-missing so a fresh canonical doesn't error.
link_file ".claude/.skill-scoping-state.json"
link_file ".claude/audit-progress.txt"
link_dir ".claude/plans"

# Local notes (anything matching .claude/*.local.md is gitignored and
# treated as project-scoped scratchpad). Iterate the canonical so new
# files appear automatically without editing this script.
if [[ -d "$CANONICAL/.claude" ]]; then
  shopt -s nullglob
  for src in "$CANONICAL"/.claude/*.local.md; do
    link_file ".claude/$(basename "$src")"
  done
  shopt -u nullglob
fi

# Per-CLI config roots. All four are gitignored at the top level so they
# are safe to symlink wholesale when present. Skip-if-missing so the link
# step is a no-op for CLIs the canonical hasn't onboarded yet.
#   .agents         - provider/agent config (Codex, openclaw, fno)
#   .codex          - Codex CLI project state
#   .codex-plugin   - Codex plugin manifests
#   .gemini         - Gemini CLI project state (settings.json, agents/)
link_dir ".agents"
link_dir ".codex"
link_dir ".codex-plugin"
link_dir ".gemini"

echo "setup-worktree: linked shared state from $CANONICAL into $WORKTREE"
