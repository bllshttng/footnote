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
#     SKIP it with a stderr warning
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
# no-op, and `fno do target start` treats any non-zero from this script as fatal
# ("refusing to initialize against unverified shared state", target_cli.py).
if [[ "$CANONICAL" -ef "$WORKTREE" ]]; then
  echo "setup-worktree: refusing to symlink canonical -> canonical (no-op): $CANONICAL" >&2
  exit 0
fi

# Refuse a WORKTREE that does not exist yet. The mkdir below creates the whole
# path, so a caller that hands over a location nobody made gets a fully linked
# directory conjured at it. On 2026-09-03 nine such directories sat under
# <repo>/worktrees/, which .claude/rules/worktrees.md forbids outright: a test
# mock printed that path and this script built each one, symlink farm and all.
# Worktrees live at <repo>/.claude/worktrees/<name> or ~/.fno/worktrees/<name>,
# and every real caller creates the tree before linking into it, so an absent
# directory means the caller is wrong about where it is.
#
# Non-zero here, unlike the canonical no-op above. That case is a successful
# nothing-to-do; this one is a caller bug, and `fno do target start` should
# stop on it rather than initialize against state linked into thin air.
if [[ ! -d "$WORKTREE" ]]; then
  echo "setup-worktree: refusing to conjure a worktree that does not exist: $WORKTREE" >&2
  echo "setup-worktree: create it first (git worktree add <path>), then run this from inside it" >&2
  exit 1
fi

mkdir -p "$WORKTREE/.claude"

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

# fno state links RETIRED. Project state moved into the repo's space under
# ~/.fno/spaces/<slug>/ (keyed on the canonical root), which every worktree
# resolves identically, so there is nothing to link: config.toml is found by
# the config loader's climb to canonical, the events journal is ONE space
# file, and carveouts/wake-signals/codemap resolve through the space too.
# The only checkout-local fno file is .fno/config.toml (committed project
# config), and a per-worktree override layer (config.local.toml) stays
# per-worktree by ABSENCE of a link, as before.

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

# Salvage-ref post-commit hook: makes every worktree's commits
# gc-proof and enumerable at commit time, the one moment guaranteed to
# occur before a worker is killed. Worktrees share one git-common-dir hooks
# directory, so this installs (or, if a post-commit hook already exists and
# is not ours, prepends to) ONE shared post-commit dispatcher - idempotent
# across every worktree's own setup run, and it resolves each committing
# worktree's OWN checked-out copy of hooks/worktree-salvage-ref.sh at
# execution time via `git rev-parse --show-toplevel`, so it stays correct
# no matter which worktree fires it.
_salvage_marker="worktree-salvage-ref.sh"
# The one source of truth for the dispatcher body, built once and reused by
# both the create and the prepend path below, so a future change (e.g. a
# guard) cannot land in one copy and drift from the other. Never exec/exit:
# a THIRD-PARTY hook this lands ahead of may itself end in a bare `exit` (a
# common idiom, and the exact shape a prior version of this body used), and
# anything positioned AFTER that `exit` is unreachable regardless of file
# order - only code that runs before it and falls through is guaranteed to
# fire. git ignores post-commit's own exit code, so falling through costs
# nothing.
_salvage_dispatcher_body='toplevel="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -n "$toplevel" ]; then
  _fno_salvage_script="$toplevel/hooks/'"$_salvage_marker"'"
  [ -x "$_fno_salvage_script" ] && "$_fno_salvage_script"
fi'
_common_hooks_dir="$(git -C "$WORKTREE" rev-parse --git-common-dir 2>/dev/null)" || _common_hooks_dir=""
if [[ -n "$_common_hooks_dir" ]]; then
  [[ "$_common_hooks_dir" = /* ]] || _common_hooks_dir="$WORKTREE/$_common_hooks_dir"
  _common_hooks_dir="$_common_hooks_dir/hooks"
  mkdir -p "$_common_hooks_dir" 2>/dev/null || true
  _post_commit="$_common_hooks_dir/post-commit"
  if [[ ! -e "$_post_commit" ]]; then
    {
      echo "#!/usr/bin/env bash"
      echo "# Installed by scripts/setup/setup-worktree.sh. Dispatches to the"
      echo "# COMMITTING worktree's own checked-out salvage-ref hook, never a fixed"
      echo "# worktree's copy - see hooks/worktree-salvage-ref.sh for the real logic."
      printf '%s\n' "$_salvage_dispatcher_body"
    } > "$_post_commit"
    chmod +x "$_post_commit"
    echo "setup-worktree: installed shared post-commit salvage-ref hook at $_post_commit"
  elif ! grep -q "$_salvage_marker" "$_post_commit" 2>/dev/null; then
    # PREPEND, never append: a code-review finding caught that appending
    # after an existing hook's own `exit` (a common idiom - the create
    # path's PRIOR body was exactly this shape) makes the appended block
    # dead code, unreachable regardless of its position in the file.
    # Prepending ahead of the shebang-preserved original guarantees this
    # runs first; the fall-through body above never exits early, so the
    # original hook's own logic still runs right after it either way.
    _tmp_post_commit="$(mktemp "${_post_commit}.XXXXXX" 2>/dev/null)" || _tmp_post_commit=""
    if [[ -n "$_tmp_post_commit" ]]; then
      _first_line="$(head -n 1 "$_post_commit" 2>/dev/null)"
      if [[ "$_first_line" == "#!"* ]]; then
        {
          printf '%s\n' "$_first_line"
          echo "# Prepended by scripts/setup/setup-worktree.sh."
          printf '%s\n' "$_salvage_dispatcher_body"
          echo ""
          tail -n +2 "$_post_commit"
        } > "$_tmp_post_commit"
      else
        {
          echo "# Prepended by scripts/setup/setup-worktree.sh."
          printf '%s\n' "$_salvage_dispatcher_body"
          echo ""
          cat "$_post_commit"
        } > "$_tmp_post_commit"
      fi
      mv -f "$_tmp_post_commit" "$_post_commit"
      chmod +x "$_post_commit"
      echo "setup-worktree: prepended salvage-ref call to existing post-commit hook at $_post_commit"
    else
      echo "setup-worktree: could not prepend salvage-ref call (mktemp failed)" >&2
    fi
  fi
else
  echo "setup-worktree: could not resolve git-common-dir; salvage-ref hook not installed" >&2
fi

# Salvage remote mirror: ON by default for every fno worktree (x-28ff). The
# founding case loses work only while commits live on one disk; pushing HEAD
# to refs/fno/salvage/<worktree> at commit time closes that window. With
# extensions.worktreeConfig on this lands in THIS worktree's own config;
# `git config --local --unset fno.salvageRemoteMirror` opts back out (e.g. an
# air-gapped clone).
if git -C "$WORKTREE" config --local fno.salvageRemoteMirror true 2>/dev/null; then
  echo "setup-worktree: salvage mirror: on (refs/fno/salvage/$(basename "$WORKTREE") on origin)"
else
  echo "setup-worktree: salvage mirror not set (git config failed); commits stay local-only" >&2
fi

if (( events_journal_shared == 0 )); then
  echo "setup-worktree: linked independent state but events journal is not shared" >&2
  exit 1
fi

echo "setup-worktree: linked shared state from $CANONICAL into $WORKTREE"

if command -v fno >/dev/null 2>&1; then
  # Two-step deploy window: the installed fno can predate the fold, so try the
  # canonical spelling first and fall back to the retired root one.
  if ! fno agents workspace worktree cleanup --cargo-targets --apply \
      && ! fno workspace worktree cleanup --cargo-targets --apply; then
    echo "setup-worktree: cargo target cleanup failed; worktree remains usable" >&2
  fi
else
  echo "setup-worktree: cargo target cleanup skipped; fno is unavailable" >&2
fi
