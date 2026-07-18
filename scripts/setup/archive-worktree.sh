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
#   5  salvage of local-only .fno state failed (worktree kept)

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
    # `|| true`: symbolic-ref exits non-zero when origin/HEAD is unset, and
    # under set -euo pipefail pipefail would propagate that and abort the whole
    # archive before the empty-DEFAULT_REF fallback below could handle it.
    DEFAULT_REF="$(git -C "$TARGET" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null | sed 's|^refs/remotes/||' || true)"
    if [[ -z "$DEFAULT_REF" ]]; then
      # Pipe-free first-line (no `| head` that could SIGPIPE under pipefail).
      FIRST_REMOTE="$(git -C "$TARGET" remote 2>/dev/null)"
      FIRST_REMOTE="${FIRST_REMOTE%%$'\n'*}"
      if [[ -n "$FIRST_REMOTE" ]]; then
        DEFAULT_REF="$(git -C "$TARGET" symbolic-ref --quiet "refs/remotes/$FIRST_REMOTE/HEAD" 2>/dev/null | sed 's|^refs/remotes/||' || true)"
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
  #    in-progress autonomous work. Legacy manifests carried status:
  #    IN_PROGRESS; the modern immutable manifest has no status field, so a
  #    live owner_pid is the liveness signal (else this check was a silent
  #    no-op for every current session).
  TARGET_STATE="$TARGET/.fno/target-state.md"
  if [[ -f "$TARGET_STATE" ]]; then
    if grep -qE '^status:[[:space:]]*IN_PROGRESS' "$TARGET_STATE"; then
      echo "archive-worktree: target session IN_PROGRESS at $TARGET_STATE" >&2
      echo "    Cancel it first (touch $TARGET/.fno/.target-cancelled) or use --force." >&2
      exit 2
    fi
    # Pipeline-free + fail-open: a manifest without owner_pid must degrade to
    # empty (not-live), never abort the script under set -euo pipefail.
    OWNER_PID="$(sed -nE '/^owner_pid:[[:space:]]*[0-9]+/{s/^owner_pid:[[:space:]]*//;p;q;}' "$TARGET_STATE" 2>/dev/null || true)"
    if [[ -n "$OWNER_PID" ]] && kill -0 "$OWNER_PID" 2>/dev/null; then
      echo "archive-worktree: live target session (owner_pid $OWNER_PID alive) at $TARGET_STATE" >&2
      echo "    Cancel it first (touch $TARGET/.fno/.target-cancelled) or use --force." >&2
      exit 2
    fi
  fi
fi

# ---- Process cleanup -----------------------------------------------------
# Collect PIDs rooted in TARGET (cwd under it) *and* PIDs whose cmdline
# references TARGET. Both surfaces matter: lsof catches editors and shells
# with cwd inside the worktree; pgrep -f catches background processes that
# may have changed directory after launch.
# `lsof -a -d cwd +D` (NOT bare `+D`) keys on the cwd fd: uv hardlinks the same
# venv `.so` inodes into every worktree, so a daemon mmapping one shows up under
# every worktree's path with bare `+D`. Anchoring on cwd drops those phantoms.
PIDS=""
if command -v lsof >/dev/null 2>&1; then
  PIDS="$(lsof -a -d cwd +D "$TARGET" 2>/dev/null | awk 'NR>1 {print $2}' | sort -u || true)"
fi
# `pgrep -f` matches its pattern as an extended regex against the full
# cmdline. Pass TARGET unescaped and any `.`/`+`/`[` in the path matches
# unrelated processes, which we'd then SIGTERM after a single y/N prompt
# (codex P1 / gemini medium). Escape regex metacharacters so the match is
# effectively literal.
TARGET_RE="$(printf '%s' "$TARGET" | sed -e 's/[][\\.^$*+?(){}|/]/\\&/g')"
PIDS_F="$(pgrep -f -- "$TARGET_RE" 2>/dev/null || true)"
# Exclude our OWN process group, not just $$. When this script is invoked with
# TARGET as argv[1] (e.g. by the merged sweep), pgrep -f matches the script's
# own forks - the command-substitution subshells and pgrep itself all carry
# TARGET in their cmdline with a PID != $$ but sharing our PGID. A genuine
# squatter always runs in a SEPARATE session/PGID, so filtering our PGID drops
# every self-match while keeping real ones. Without this the script false-
# matched itself, prompted on /dev/tty, and (headless) declined with exit 3.
MY_PGID="$(ps -o pgid= -p "$$" 2>/dev/null | tr -d ' ' || true)"
ALL_PIDS=""
while IFS= read -r pid; do
  [[ -z "$pid" || "$pid" == "$$" ]] && continue
  # Only LIVE pids can be squatters. pgrep -f matches this script's own
  # transient forks (the command-substitution subshells carry TARGET as argv),
  # which have already exited by the time we get here - ps returns nothing for
  # them, so a PGID check alone can't drop them. Skip anything already gone.
  kill -0 "$pid" 2>/dev/null || continue
  # Belt-and-suspenders for a still-live self-fork: our own process group.
  if [[ -n "$MY_PGID" ]]; then
    pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ' || true)"
    [[ "$pgid" == "$MY_PGID" ]] && continue
  fi
  # Concurrent-sweep race: another archive-worktree.sh / worktree-lifecycle.sh
  # run carries TARGET in its argv but lives in a DIFFERENT PGID, so the check
  # above misses it. It is our own tooling, never a squatter - never SIGTERM it.
  case "$(ps -o command= -p "$pid" 2>/dev/null)" in
    *archive-worktree.sh*|*worktree-lifecycle.sh*) continue ;;
  esac
  ALL_PIDS+="$pid"$'\n'
done < <(printf '%s\n%s\n' "$PIDS" "$PIDS_F" | grep -v '^$' | sort -u)
ALL_PIDS="$(printf '%s' "$ALL_PIDS" | grep -v '^$' | sort -u || true)"

if [[ -n "$ALL_PIDS" ]]; then
  echo "    Processes rooted in $TARGET:" >&2
  while IFS= read -r pid; do
    [[ -z "$pid" ]] && continue
    CMD="$(ps -p "$pid" -o command= 2>/dev/null || echo '(gone)')"
    echo "      $pid  $CMD" >&2
  done <<< "$ALL_PIDS"

  if [[ "$ASSUME_YES" -ne 1 ]]; then
    # No controlling tty (a non-interactive sweep): decline cleanly with one
    # line instead of letting `read </dev/tty` spew "/dev/tty: Device not
    # configured" and decline anyway. Same rc=3; SIGTERMing live processes
    # stays opt-in via --yes / --kill-orphans, never a headless default.
    # `-r /dev/tty` only tests the perm bits, so actually open it - on macOS a
    # session with no controlling terminal fails the open, not the test.
    # Probe the open in a SUBSHELL first: `exec` is a POSIX special built-in and
    # a redirection failure on it can exit a non-interactive shell outright
    # (a rule distinct from set -e, not reliably suppressed by the `if`), which
    # would bypass this clean exit 3 on Linux bash. The subshell's exit can't
    # kill us; its stderr goes to /dev/null. Only once it proves openable do we
    # apply fd 3 to THIS shell (guaranteed to succeed, so no exit risk).
    if ! ( exec 3</dev/tty ) 2>/dev/null; then
      echo "archive-worktree: processes present and no tty for confirmation; re-run with --yes or interactively" >&2
      exit 3
    fi
    exec 3</dev/tty
    printf '    Send SIGTERM to these processes? [y/N] ' >&2
    read -r REPLY <&3 || REPLY="n"
    exec 3<&-
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

# ---- Salvage local-only .fno state before removal (data-loss guard) ------
# A worktree's .fno mixes symlinks (canonical state) with REAL local-only
# files (artifacts/, scratchpad/, events.jsonl, target-state.md, *.log) that
# `git worktree remove` would delete silently. Copy every real (non-symlink)
# entry into the canonical .fno keyed by date+node BEFORE removal: directories
# to <canon>/.fno/<name>/<date>-<node>/ so tools find salvaged runs in place,
# loose files together under <canon>/.fno/salvage/<date>-<node>/. A copy
# failure KEEPS the worktree (exit 5) - losing state to save disk is never the
# trade. Skipped when .fno is a whole-dir symlink (all canonical already) or
# absent. Every caller (this script, the merged sweep, the ritual prune,
# manual use) inherits the guard.
_salvage_node() {
  local st="$TARGET/.fno/target-state.md" n=""
  if [[ -f "$st" ]]; then
    n="$(sed -nE '/^graph_node_id:[[:space:]]*/{s/^graph_node_id:[[:space:]]*//;p;q;}' "$st" 2>/dev/null | tr -d '"'"'"' ' || true)"
  fi
  [[ -z "$n" ]] && n="${BRANCH##*/}"
  [[ -z "$n" || "$n" == "(detached)" ]] && n="$(basename "$TARGET")"
  printf '%s' "$n"
}

salvage_fno() {
  local src="$TARGET/.fno"
  [[ -d "$src" && ! -L "$src" ]] || return 0
  local canon_fno="$CANONICAL/.fno"
  local node date entry base dest
  node="$(_salvage_node)"
  date="$(date +%Y%m%d)"
  for entry in "$src"/* "$src"/.[!.]*; do
    [[ -e "$entry" ]] || continue   # unmatched glob
    [[ -L "$entry" ]] && continue    # canonical symlink -> already shared
    base="$(basename "$entry")"
    case "$base" in
      *.lock|*.stamp|*-stamp) continue ;;
    esac
    if [[ -d "$entry" ]]; then
      dest="$canon_fno/$base/${date}-${node}"
      mkdir -p "$dest" 2>/dev/null \
        && cp -R "$entry"/. "$dest"/ 2>/dev/null \
        || { echo "archive-worktree: salvage failed: $entry -> $dest" >&2; return 5; }
    else
      dest="$canon_fno/salvage/${date}-${node}"
      mkdir -p "$dest" 2>/dev/null \
        && cp "$entry" "$dest/$base" 2>/dev/null \
        || { echo "archive-worktree: salvage failed: $entry -> $dest/$base" >&2; return 5; }
    fi
  done
  return 0
}

if ! salvage_fno; then
  echo "archive-worktree: keeping worktree $TARGET (salvage failed, nothing removed)" >&2
  exit 5
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
