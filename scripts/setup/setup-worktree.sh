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
# add` or the abilities git-worktrees skill should call this directly.
#
# Safety contract (load-bearing):
#   - Uses `ln -sf` to create or refresh symlinks; never `rm -rf` a target
#   - If a target already exists as a real (non-symlink) file or directory,
#     SKIP it with a stderr warning - we never overwrite real local state
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
#   4. $HOME/code/me/abilities (last-ditch fallback for non-git contexts)
CANONICAL="${CANONICAL:-${CONDUCTOR_ROOT_PATH:-}}"
if [[ -z "$CANONICAL" ]]; then
  # In a worktree, git-common-dir points at the main repo's .git directory.
  # Going one level up gets the canonical worktree (the main checkout).
  COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null || true)
  if [[ -n "$COMMON_DIR" && -d "$COMMON_DIR" ]]; then
    CANONICAL=$(cd "$COMMON_DIR/.." && pwd)
  else
    CANONICAL="$HOME/code/me/abilities"
  fi
fi

if [[ ! -d "$CANONICAL" ]]; then
  echo "setup-worktree: canonical project not found at $CANONICAL" >&2
  exit 1
fi

# Resolve worktree root (where we are linking INTO). Default to cwd.
WORKTREE="${WORKTREE:-$(pwd)}"

if [[ "$CANONICAL" == "$WORKTREE" ]]; then
  echo "setup-worktree: refusing to symlink canonical -> canonical (no-op)" >&2
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

# Shared content (Obsidian vault link)
link_dir "internal"

# Shared abilities state (project-level, propagates across worktrees)
link_file ".fno/settings.yaml"
# settings.local.yaml is deliberately NOT linked: it is the one config file kept
# per-worktree, layering the collision-prone keys (config.post_merge.parking_lot_path,
# config.project.id) on top of the shared settings.yaml (x-cbce). Do not add a
# link_file for it here - a link would re-share exactly the keys it exists to
# diverge. Absent by default (= shared behavior); seed one only when a worktree
# needs its own value.
link_file ".fno/ledger.json"
# ledger.md is the kanban rendering paired with ledger.json. Skip-if-missing
# until the renderer has run at least once on the canonical.
link_file ".fno/ledger.md"
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
#   .agents         - provider/agent config (Codex, openclaw, abilities)
#   .codex          - Codex CLI project state
#   .codex-plugin   - Codex plugin manifests
#   .gemini         - Gemini CLI project state (settings.json, agents/)
link_dir ".agents"
link_dir ".codex"
link_dir ".codex-plugin"
link_dir ".gemini"

echo "setup-worktree: linked shared state from $CANONICAL into $WORKTREE"
