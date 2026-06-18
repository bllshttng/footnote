#!/usr/bin/env bash
# archive-worktree.sh - safely tear down an abilities worktree.
#
# Mirrors scripts/setup/setup-worktree.sh in reverse: kills processes
# rooted in the worktree path (with operator confirmation), runs strict
# pre-removal checks, calls `git worktree remove`, and prunes stale
# administrative state. The branch is preserved by default.
#
# Usage:
#   bash scripts/setup/archive-worktree.sh <name|path>
#   bash scripts/setup/archive-worktree.sh                # archives cwd
#
# Flags:
#   --force       Skip strict checks (dirty tree, unpushed commits, live
#                 target). Equivalent to `git worktree remove --force`.
#   --yes         Skip the process-kill confirmation prompt.
#   --delete-branch  After removing the worktree, delete its branch with
#                    `git branch -D` (force). Default: keep branch.
#
# Exit codes:
#   0  worktree removed
#   1  usage error / target resolution failed
#   2  strict check failed (use --force to override)
#   3  user declined process-kill prompt
#   4  git worktree remove failed

set -euo pipefail

FORCE=0
ASSUME_YES=0
DELETE_BRANCH=0
TARGET_ARG=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --force) FORCE=1; shift ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    --delete-branch) DELETE_BRANCH=1; shift ;;
    -h|--help)
      sed -n '2,/^set -euo/p' "$0" | sed 's/^# //; s/^#//'
      exit 0
      ;;
    --) shift; break ;;
    -*) echo "archive-worktree: unknown flag: $1" >&2; exit 1 ;;
    *) TARGET_ARG="$1"; shift ;;
  esac
done

# Resolve target worktree path. Three input shapes:
#   1. absolute path                 -> use as-is
#   2. name (e.g. "athens")          -> match suffix in `git worktree list`
#   3. empty                         -> use $(pwd)
resolve_target() {
  local arg="$1"
  if [[ -z "$arg" ]]; then
    pwd
    return 0
  fi
  if [[ "$arg" = /* ]]; then
    printf '%s' "$arg"
    return 0
  fi
  # Name form: scan `git worktree list` for paths ending in /<arg>. Use a
  # bash literal suffix match (case glob) rather than awk regex - user
  # input would otherwise be interpreted as ERE and `name.foo` could
  # match `nameXfoo`. Collect ALL matches and refuse on ambiguity: two
  # worktrees sharing a basename (e.g. one under ~/conductor and one
  # under ~/.warp) must be disambiguated by absolute path, never silently
  # picked by list order (would mis-target the destructive remove).
  local matches=()
  local path
  while IFS= read -r path; do
    case "$path" in
      */"$arg") matches+=("$path") ;;
    esac
  done < <(git worktree list --porcelain 2>/dev/null \
    | awk '/^worktree / {sub(/^worktree /, ""); print}')
  if [[ ${#matches[@]} -eq 0 ]]; then
    echo "archive-worktree: no worktree matching '$arg' in git worktree list" >&2
    exit 1
  fi
  if [[ ${#matches[@]} -gt 1 ]]; then
    echo "archive-worktree: name '$arg' is ambiguous; ${#matches[@]} worktrees share that basename:" >&2
    local p
    for p in "${matches[@]}"; do
      echo "    $p" >&2
    done
    echo "    Re-run with an absolute path to disambiguate." >&2
    exit 1
  fi
  printf '%s' "${matches[0]}"
}

TARGET="$(resolve_target "$TARGET_ARG")"
TARGET="$(cd "$TARGET" 2>/dev/null && pwd)" || {
  echo "archive-worktree: target does not exist: $TARGET_ARG" >&2
  exit 1
}

# Refuse to archive the canonical (main) checkout. The main worktree is the
# one whose path equals `git worktree list` first entry; removing it would
# corrupt the repo.
# NB: awk acts on line 1 (the main worktree's `worktree <path>` line, always
# first in --porcelain output) and reads the rest without printing, rather than
# `exit`ing on the first match. With `set -euo pipefail`, an early `exit` closes
# the pipe while `git worktree list` is still writing (it dumps every worktree),
# so git takes SIGPIPE and the pipeline fails with 141. Once the worktree count
# grew large this turned the canonical-check into a silent abort on every run.
# Draining to EOF avoids the early close; output is unchanged.
CANONICAL="$(git -C "$TARGET" worktree list --porcelain 2>/dev/null \
  | awk 'NR==1 {sub(/^worktree /, ""); print}')"
if [[ "$TARGET" == "$CANONICAL" ]]; then
  echo "archive-worktree: refusing to archive canonical checkout: $TARGET" >&2
  exit 1
fi

# Operate from canonical so removing TARGET doesn't yank our cwd out from
# under us. Bash keeps the script in memory, but any later `cd $TARGET`
# or relative-path resolution would fail.
cd "$CANONICAL"

BRANCH="$(git -C "$TARGET" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "(detached)")"

echo "=== Archiving worktree ===" >&2
echo "    Path:   $TARGET" >&2
echo "    Branch: $BRANCH" >&2

# ---- Strict pre-removal checks (skip with --force) -----------------------
if [[ "$FORCE" -eq 0 ]]; then
  # 1. Working tree clean
  if [[ -n "$(git -C "$TARGET" status --porcelain 2>/dev/null)" ]]; then
    echo "archive-worktree: dirty working tree at $TARGET" >&2
    git -C "$TARGET" status --short >&2
    echo "    --force to override, or commit/stash first." >&2
    exit 2
  fi

  # 2. No unpushed commits. If branch has an upstream, compare to it; else
  #    compare to origin/main as a best-effort check for "anything not on
  #    the remote".
  UPSTREAM="$(git -C "$TARGET" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)"
  if [[ -n "$UPSTREAM" ]]; then
    AHEAD="$(git -C "$TARGET" rev-list --count "$UPSTREAM"..HEAD 2>/dev/null || echo 0)"
    if [[ "$AHEAD" -gt 0 ]]; then
      echo "archive-worktree: $AHEAD unpushed commit(s) on $BRANCH vs $UPSTREAM" >&2
      git -C "$TARGET" log --oneline "$UPSTREAM"..HEAD >&2
      echo "    --force to override, or push first." >&2
      exit 2
    fi
  else
    # No upstream; compare against the remote's default branch. Resolve in
    # this order so non-`main` defaults and non-`origin` remotes still get
    # the safety check:
    #   1. `git symbolic-ref refs/remotes/origin/HEAD`  ->  origin/<default>
    #   2. First remote's symbolic-ref HEAD             ->  <remote>/<default>
    #   3. Skip the check (warn) when nothing resolves
    DEFAULT_REF="$(git -C "$TARGET" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null | sed 's|^refs/remotes/||')"
    if [[ -z "$DEFAULT_REF" ]]; then
      # Pipe-free first-line (no `| head` that could SIGPIPE under pipefail).
      FIRST_REMOTE="$(git -C "$TARGET" remote 2>/dev/null)"
      FIRST_REMOTE="${FIRST_REMOTE%%$'\n'*}"
      if [[ -n "$FIRST_REMOTE" ]]; then
        DEFAULT_REF="$(git -C "$TARGET" symbolic-ref --quiet "refs/remotes/$FIRST_REMOTE/HEAD" 2>/dev/null | sed 's|^refs/remotes/||')"
      fi
    fi
    if [[ -n "$DEFAULT_REF" ]] && git -C "$TARGET" rev-parse --verify --quiet "$DEFAULT_REF" >/dev/null; then
      AHEAD="$(git -C "$TARGET" rev-list --count "$DEFAULT_REF"..HEAD 2>/dev/null || echo 0)"
      if [[ "$AHEAD" -gt 0 ]]; then
        echo "archive-worktree: $AHEAD commit(s) on $BRANCH ahead of $DEFAULT_REF, no upstream set" >&2
        git -C "$TARGET" log --oneline "$DEFAULT_REF"..HEAD >&2
        echo "    --force to override, or push first." >&2
        exit 2
      fi
    else
      echo "archive-worktree: WARN: no upstream and no resolvable remote HEAD; skipping unpushed-commit check" >&2
      echo "    Set with: git remote set-head <remote> --auto" >&2
    fi
  fi

  # 3. Live target session. target-state.md is the source of truth for
  #    in-progress autonomous work; refuse if status: IN_PROGRESS.
  TARGET_STATE="$TARGET/.fno/target-state.md"
  if [[ -f "$TARGET_STATE" ]] && grep -qE '^status:\s*IN_PROGRESS' "$TARGET_STATE"; then
    echo "archive-worktree: target session IN_PROGRESS at $TARGET_STATE" >&2
    echo "    Cancel it first (touch $TARGET/.fno/.target-cancelled) or use --force." >&2
    exit 2
  fi
fi

# ---- Process cleanup -----------------------------------------------------
# Collect PIDs that have files open under TARGET *and* PIDs whose cmdline
# references TARGET. Both surfaces matter: lsof catches editors and shells
# with cwd inside the worktree; pgrep -f catches background processes that
# may have changed directory after launch.
PIDS=""
if command -v lsof >/dev/null 2>&1; then
  PIDS="$(lsof +D "$TARGET" 2>/dev/null | awk 'NR>1 {print $2}' | sort -u || true)"
fi
# `pgrep -f` matches its pattern as an extended regex against the full
# cmdline. Pass TARGET unescaped and any `.`/`+`/`[` in the path matches
# unrelated processes, which we'd then SIGTERM after a single y/N prompt
# (codex P1 / gemini medium). Escape regex metacharacters so the match is
# effectively literal.
TARGET_RE="$(printf '%s' "$TARGET" | sed -e 's/[][\\.^$*+?(){}|/]/\\&/g')"
PIDS_F="$(pgrep -f -- "$TARGET_RE" 2>/dev/null || true)"
# Drop our own PID; we're running from inside this script.
ALL_PIDS="$(printf '%s\n%s\n' "$PIDS" "$PIDS_F" | grep -v "^$$\$" | grep -v '^$' | sort -u || true)"

if [[ -n "$ALL_PIDS" ]]; then
  echo "    Processes rooted in $TARGET:" >&2
  while IFS= read -r pid; do
    [[ -z "$pid" ]] && continue
    CMD="$(ps -p "$pid" -o command= 2>/dev/null || echo '(gone)')"
    echo "      $pid  $CMD" >&2
  done <<< "$ALL_PIDS"

  if [[ "$ASSUME_YES" -ne 1 ]]; then
    printf '    Send SIGTERM to these processes? [y/N] ' >&2
    read -r REPLY </dev/tty || REPLY="n"
    case "$REPLY" in
      y|Y|yes|YES) ;;
      *) echo "archive-worktree: declined; not archiving." >&2; exit 3 ;;
    esac
  fi

  # SIGTERM first, then SIGKILL on holdouts after 5 seconds. SIGTERM gives
  # editors/shells a chance to flush state; SIGKILL guarantees the path is
  # free before `git worktree remove`.
  while IFS= read -r pid; do
    [[ -z "$pid" ]] && continue
    kill -TERM "$pid" 2>/dev/null || true
  done <<< "$ALL_PIDS"
  sleep 5
  HOLDOUTS=""
  while IFS= read -r pid; do
    [[ -z "$pid" ]] && continue
    if kill -0 "$pid" 2>/dev/null; then
      HOLDOUTS+="$pid"$'\n'
    fi
  done <<< "$ALL_PIDS"
  if [[ -n "$HOLDOUTS" ]]; then
    echo "    SIGKILL holdouts: $(echo "$HOLDOUTS" | tr '\n' ' ')" >&2
    while IFS= read -r pid; do
      [[ -z "$pid" ]] && continue
      kill -KILL "$pid" 2>/dev/null || true
    done <<< "$HOLDOUTS"
  fi
fi

# ---- Remove the worktree -------------------------------------------------
REMOVE_FLAGS=""
[[ "$FORCE" -eq 1 ]] && REMOVE_FLAGS="--force"
if ! git worktree remove $REMOVE_FLAGS "$TARGET"; then
  echo "archive-worktree: git worktree remove failed" >&2
  exit 4
fi
git worktree prune

# ---- Branch handling -----------------------------------------------------
if [[ "$DELETE_BRANCH" -eq 1 && "$BRANCH" != "(detached)" ]]; then
  if git branch -D "$BRANCH" 2>/dev/null; then
    echo "    Deleted branch $BRANCH" >&2
  else
    echo "    Branch delete failed (already gone?): $BRANCH" >&2
  fi
else
  if [[ "$BRANCH" != "(detached)" ]]; then
    echo "    Branch $BRANCH preserved (use --delete-branch to remove)" >&2
  fi
fi

echo "archive-worktree: archived $TARGET" >&2
