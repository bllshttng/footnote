#!/usr/bin/env bash
# archive-worktree.sh - safely tear down an fno workspace worktree.
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
#   --force       Measure and disclose dirty paths, unpushed commits, and live
#                 target evidence, then override positive checks. Unreadable
#                 evidence still refuses removal.
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
#   5  salvage of local-only .fno do state failed (worktree kept)
#   6  app-owned Codex worktree (archive the associated chat instead)

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

# Codex Desktop owns worktrees it creates beneath CODEX_HOME/worktrees. Their
# chat lifecycle snapshots and removes them; deleting one through Git leaves
# the app with stale ownership metadata. Path placement is only trusted here
# because this is a deletion guard (false positives keep data), never as proof
# that an external allocator created a native worktree. --force cannot override
# a foreign lifecycle owner.
CODEX_WORKTREES_RAW="${CODEX_HOME:-$HOME/.codex}/worktrees"
if [[ -d "$CODEX_WORKTREES_RAW" ]]; then
  CODEX_WORKTREES_ROOT="$(cd "$CODEX_WORKTREES_RAW" && pwd)"
  case "$TARGET/" in
    "$CODEX_WORKTREES_ROOT/"*)
      echo "archive-worktree: refusing to remove app-owned Codex worktree: $TARGET" >&2
      echo "    In Codex Desktop, archive its associated chat; the app owns snapshot and cleanup." >&2
      exit 6
      ;;
  esac
fi

# Operate from canonical so removing TARGET doesn't yank our cwd out from
# under us. Bash keeps the script in memory, but any later `cd $TARGET`
# or relative-path resolution would fail.
cd "$CANONICAL"

BRANCH="$(git -C "$TARGET" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "(detached)")"
# A detached HEAD names itself "HEAD" (rev-parse succeeds; the fallback above
# only fires on a hard error), so normalize to one shape for every consumer
# below: strict check 2, --delete-branch, and the salvage namer.
if [[ "$BRANCH" == "HEAD" ]]; then
  BRANCH="(detached)"
fi

echo "=== Archiving worktree ===" >&2
echo "    Path:   $TARGET" >&2
echo "    Branch: $BRANCH" >&2

# ---- Strict pre-removal checks and force disclosure -----------------------
# Measure every strict condition in both modes. --force may override positive
# dirty, unpushed, and live-session evidence, but it never overrides an
# unreadable probe: an unknown state is not a safe state to discard.
_CHECK_REAPABLE_LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd)/worktree-reapable.sh"
if [[ -f "$_CHECK_REAPABLE_LIB" ]]; then
  # shellcheck source=/dev/null
  source "$_CHECK_REAPABLE_LIB"
fi

measure_strict_state() {
  FORCE_DIRTY_STATUS=""
  FORCE_DIRTY_STATUS_RC=0
  if FORCE_DIRTY_STATUS="$(git -C "$TARGET" status --short 2>/dev/null)"; then
    :
  else
    FORCE_DIRTY_STATUS_RC=$?
  fi

  FORCE_UNPUSHED_COUNT=0
  FORCE_UNPUSHED_EVIDENCE=""
  FORCE_UNPUSHED_REASON=""
  FORCE_DEFAULT_REF=""
  _FORCE_UNPUSHED_LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" 2>/dev/null && pwd)/worktree-unpushed.sh"
  if [[ -f "$_FORCE_UNPUSHED_LIB" ]]; then
    # shellcheck source=/dev/null
    source "$_FORCE_UNPUSHED_LIB"
  else
    wt_refresh_remote_refs() { git -C "${1:-.}" fetch origin main >/dev/null 2>&1; }
  fi
  FORCE_REMOTE_REFRESH_OK=0
  if wt_refresh_remote_refs "$TARGET" >/dev/null 2>&1; then
    FORCE_REMOTE_REFRESH_OK=1
  else
    FORCE_UNPUSHED_REASON="remote refs not verifiable (fetch --all --prune failed)"
  fi
  FORCE_UPSTREAM="$(git -C "$TARGET" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)"
  if [[ "$FORCE_REMOTE_REFRESH_OK" -eq 0 ]]; then
    :
  elif [[ "$BRANCH" == "(detached)" ]]; then
    if wt_refresh_remote_refs "$TARGET" >/dev/null 2>&1; then
      if ! FORCE_UNPUSHED_COUNT="$(git -C "$TARGET" rev-list --count HEAD --not --remotes 2>/dev/null)" || [[ ! "$FORCE_UNPUSHED_COUNT" =~ ^[0-9]+$ ]]; then
        FORCE_UNPUSHED_REASON="git could not count commits absent from remotes"
      elif [[ "$FORCE_UNPUSHED_COUNT" -gt 0 ]]; then
        if ! FORCE_UNPUSHED_EVIDENCE="$(git -C "$TARGET" log --oneline -n 10 HEAD --not --remotes 2>/dev/null)"; then
          FORCE_UNPUSHED_REASON="git could not list commits absent from remotes"
        fi
      fi
    fi
  elif [[ -n "$FORCE_UPSTREAM" ]]; then
    if ! FORCE_AHEAD="$(git -C "$TARGET" rev-list --count "$FORCE_UPSTREAM"..HEAD 2>/dev/null)" || [[ ! "$FORCE_AHEAD" =~ ^[0-9]+$ ]]; then
      FORCE_UNPUSHED_REASON="git could not compare HEAD with $FORCE_UPSTREAM"
    else
      FORCE_UNPUSHED_COUNT="$FORCE_AHEAD"
      if [[ "$FORCE_UNPUSHED_COUNT" -gt 0 ]] && ! FORCE_UNPUSHED_EVIDENCE="$(git -C "$TARGET" log --oneline "$FORCE_UPSTREAM"..HEAD 2>/dev/null)"; then
        FORCE_UNPUSHED_REASON="git could not list commits ahead of $FORCE_UPSTREAM"
      fi
    fi
  else
    FORCE_DEFAULT_REF="$(git -C "$TARGET" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null | sed 's|^refs/remotes/||' || true)"
    if [[ -z "$FORCE_DEFAULT_REF" ]]; then
      FORCE_FIRST_REMOTE="$(git -C "$TARGET" remote 2>/dev/null)"
      FORCE_FIRST_REMOTE="${FORCE_FIRST_REMOTE%%$'\n'*}"
      if [[ -n "$FORCE_FIRST_REMOTE" ]]; then
        FORCE_DEFAULT_REF="$(git -C "$TARGET" symbolic-ref --quiet "refs/remotes/$FORCE_FIRST_REMOTE/HEAD" 2>/dev/null | sed 's|^refs/remotes/||' || true)"
      fi
    fi
    if [[ -n "$FORCE_DEFAULT_REF" ]] && git -C "$TARGET" rev-parse --verify --quiet "$FORCE_DEFAULT_REF" >/dev/null; then
      if ! FORCE_AHEAD="$(git -C "$TARGET" rev-list --count "$FORCE_DEFAULT_REF"..HEAD 2>/dev/null)" || [[ ! "$FORCE_AHEAD" =~ ^[0-9]+$ ]]; then
        FORCE_UNPUSHED_REASON="git could not compare HEAD with $FORCE_DEFAULT_REF"
      else
        FORCE_UNPUSHED_COUNT="$FORCE_AHEAD"
        if [[ "$FORCE_UNPUSHED_COUNT" -gt 0 ]] && ! FORCE_UNPUSHED_EVIDENCE="$(git -C "$TARGET" log --oneline "$FORCE_DEFAULT_REF"..HEAD 2>/dev/null)"; then
          FORCE_UNPUSHED_REASON="git could not list commits ahead of $FORCE_DEFAULT_REF"
        fi
      fi
    else
      FORCE_UNPUSHED_REASON="no upstream and no resolvable remote HEAD"
    fi
  fi
  if [[ -n "$FORCE_UNPUSHED_REASON" ]]; then
    if [[ "$FORCE" -eq 1 || "$BRANCH" == "(detached)" || -n "$FORCE_UPSTREAM" ]]; then
      echo "archive-worktree: unpushed state not verifiable at $TARGET: $FORCE_UNPUSHED_REASON" >&2
      echo "    Refusing removal until remote state is verifiable." >&2
      exit 2
    fi
    echo "archive-worktree: WARN: $FORCE_UNPUSHED_REASON; skipping unpushed-commit check" >&2
    echo "    Set with: git remote set-head <remote> --auto" >&2
  fi

  FORCE_LIVE_EVIDENCE=""
  FORCE_LIVE_CLAIM_UNVERIFIABLE=0
  FORCE_TARGET_STATE="$TARGET/.fno/target-state.md"
  if [[ -f "$FORCE_TARGET_STATE" ]]; then
    if grep -qE '^status:[[:space:]]*IN_PROGRESS' "$FORCE_TARGET_STATE"; then
      FORCE_LIVE_EVIDENCE="status: IN_PROGRESS at $FORCE_TARGET_STATE"
    else
      FORCE_GUARD_LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/scripts/lib/target-guard.sh"
      if [[ -f "$FORCE_GUARD_LIB" ]] && source "$FORCE_GUARD_LIB" 2>/dev/null; then
        if target_claim_probe "$FORCE_TARGET_STATE"; then
          FORCE_LIVE_EVIDENCE="live node claim at $FORCE_TARGET_STATE"
        elif [[ "$?" -eq 2 ]]; then
          FORCE_LIVE_CLAIM_UNVERIFIABLE=1
        else
          FORCE_OWNER_PID="$(sed -nE '/^owner_pid:[[:space:]]*[0-9]+/{s/^owner_pid:[[:space:]]*//;p;q;}' "$FORCE_TARGET_STATE" 2>/dev/null || true)"
          if [[ -n "$FORCE_OWNER_PID" ]] && kill -0 "$FORCE_OWNER_PID" 2>/dev/null; then
            FORCE_LIVE_EVIDENCE="owner_pid $FORCE_OWNER_PID alive at $FORCE_TARGET_STATE"
          fi
        fi
      else
        FORCE_LIVE_CLAIM_UNVERIFIABLE=1
      fi
    fi
  fi
  if [[ "$FORCE_LIVE_CLAIM_UNVERIFIABLE" -eq 1 ]]; then
    echo "archive-worktree: live target claim not verifiable at $FORCE_TARGET_STATE" >&2
    echo "    Refusing removal until claim state is readable." >&2
    exit 2
  fi

  if [[ "$FORCE" -eq 1 ]]; then
    if [[ "$FORCE_DIRTY_STATUS_RC" -ne 0 ]]; then
      echo "archive-worktree: --force cannot override an unreadable working-tree status at $TARGET" >&2
      exit 2
    fi
    echo "FORCE: pre-removal disclosure; the following worktree state will be discarded if removal proceeds:" >&2
    if [[ -n "$FORCE_DIRTY_STATUS" ]]; then
      echo "    dirty paths (git status --short):" >&2
      printf '%s\n' "$FORCE_DIRTY_STATUS" | sed 's/^/      /' >&2
    else
      echo "    dirty paths: none" >&2
    fi
    if [[ "$FORCE_UNPUSHED_COUNT" -gt 0 ]]; then
      echo "    unpushed commits ($FORCE_UNPUSHED_COUNT):" >&2
      printf '%s\n' "$FORCE_UNPUSHED_EVIDENCE" | sed 's/^/      /' >&2
    else
      echo "    unpushed commits: none" >&2
    fi
    if [[ -n "$FORCE_LIVE_EVIDENCE" ]]; then
      echo "    live-session evidence: $FORCE_LIVE_EVIDENCE" >&2
    else
      echo "    live-session evidence: none" >&2
    fi
  fi
}

measure_strict_state

if [[ "$FORCE" -eq 1 ]]; then
  INITIAL_DIRTY_STATUS="$FORCE_DIRTY_STATUS"
  INITIAL_UNPUSHED_COUNT="$FORCE_UNPUSHED_COUNT"
  INITIAL_UNPUSHED_EVIDENCE="$FORCE_UNPUSHED_EVIDENCE"
  INITIAL_UNPUSHED_REASON="$FORCE_UNPUSHED_REASON"
  INITIAL_UPSTREAM="$FORCE_UPSTREAM"
  INITIAL_DEFAULT_REF="${FORCE_DEFAULT_REF:-}"
  INITIAL_LIVE_EVIDENCE="$FORCE_LIVE_EVIDENCE"
fi

if [[ "$FORCE" -eq 0 ]]; then
  if [[ "$FORCE_DIRTY_STATUS_RC" -ne 0 ]]; then
    echo "archive-worktree: working-tree state not verifiable at $TARGET" >&2
    echo "    Refusing removal; commit/stash first or repair the git checkout." >&2
    exit 2
  fi
  _REAPABLE_LIB="$_CHECK_REAPABLE_LIB"
  if [[ -f "$_REAPABLE_LIB" ]]; then
    if ! wt_reapable "$TARGET"; then
      echo "archive-worktree: $WT_REAPABLE_LINE at $TARGET" >&2
      printf '%s\n' "$FORCE_DIRTY_STATUS" >&2
      echo "    --force to override, or commit/stash first." >&2
      exit 2
    fi
    # Cleared, but git will still object to the missing tracked files. Record
    # that so the removal can tell git we already checked (see REMOVE_FLAGS).
    case "$WT_REAPABLE_LINE" in
      *recoverable_deletions=0*) : ;;
      *recoverable_deletions=*) _WT_RECOVERABLE_ONLY=1
        echo "archive-worktree: $WT_REAPABLE_LINE" >&2 ;;
    esac
  elif [[ -n "$FORCE_DIRTY_STATUS" ]]; then
    echo "archive-worktree: dirty working tree at $TARGET" >&2
    printf '%s\n' "$FORCE_DIRTY_STATUS" >&2
    echo "    --force to override, or commit/stash first." >&2
    exit 2
  fi

  if [[ "$FORCE_UNPUSHED_COUNT" -gt 0 ]]; then
    if [[ "$BRANCH" == "(detached)" ]]; then
      echo "archive-worktree: $FORCE_UNPUSHED_COUNT commit(s) on detached HEAD not on any remote at $TARGET" >&2
    elif [[ -n "$FORCE_UPSTREAM" ]]; then
      echo "archive-worktree: $FORCE_UNPUSHED_COUNT unpushed commit(s) on $BRANCH vs $FORCE_UPSTREAM" >&2
    else
      echo "archive-worktree: $FORCE_UNPUSHED_COUNT commit(s) on $BRANCH ahead of $FORCE_DEFAULT_REF, no upstream set" >&2
    fi
    printf '%s\n' "$FORCE_UNPUSHED_EVIDENCE" >&2
    echo "    --force to override, or push first." >&2
    exit 2
  fi
  if [[ -n "$FORCE_LIVE_EVIDENCE" ]]; then
    echo "archive-worktree: live target session ($FORCE_LIVE_EVIDENCE)" >&2
    echo "    Cancel it first (touch $TARGET/.fno/.target-cancelled) or use --force." >&2
    exit 2
  fi
fi

# ---- Process cleanup -----------------------------------------------------
# Collect PIDs rooted in TARGET (cwd under it) *and* PIDs whose cmdline
# references TARGET. Both surfaces matter: lsof catches editors and shells
# with cwd inside the worktree; pgrep -f catches background processes that
# may have changed directory after launch.
# Enumerate cwd descriptors without a path operand, then match TARGET with a
# path-segment boundary. This preserves cwd-only semantics without recursively
# walking every file below the worktree.
PIDS=""
CWD_SNAPSHOT_OK=0
if command -v lsof >/dev/null 2>&1; then
  CWD_ROWS=""
  if CWD_ROWS="$(lsof -a -d cwd -Fpn 2>/dev/null)"; then
    CWD_SNAPSHOT_OK=1
    TARGET_PHYSICAL="$(cd "$TARGET" 2>/dev/null && pwd -P)" || TARGET_PHYSICAL="$TARGET"
    PIDS="$(printf '%s\n' "$CWD_ROWS" | awk -v root="$TARGET_PHYSICAL" -v logical="$TARGET" '
      /^p[0-9]+$/ { pid = substr($0, 2); next }
      /^n/ && pid != "" {
        cwd = substr($0, 2)
        if (cwd == root || index(cwd, root "/") == 1 ||
            cwd == logical || index(cwd, logical "/") == 1) print pid
      }
    ' | sort -u)"
  fi
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

if [[ "$CWD_SNAPSHOT_OK" -ne 1 ]]; then
  echo "archive-worktree: cwd process snapshot unavailable; refusing to treat $TARGET as idle" >&2
  exit 2
fi

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

if [[ "$FORCE" -eq 1 ]]; then
  measure_strict_state
  FORCE_STATE_CHANGED=0
  [[ "$FORCE_DIRTY_STATUS" == "$INITIAL_DIRTY_STATUS" ]] || FORCE_STATE_CHANGED=1
  [[ "$FORCE_UNPUSHED_COUNT" == "$INITIAL_UNPUSHED_COUNT" ]] || FORCE_STATE_CHANGED=1
  [[ "$FORCE_UNPUSHED_EVIDENCE" == "$INITIAL_UNPUSHED_EVIDENCE" ]] || FORCE_STATE_CHANGED=1
  [[ "$FORCE_UNPUSHED_REASON" == "$INITIAL_UNPUSHED_REASON" ]] || FORCE_STATE_CHANGED=1
  [[ "$FORCE_UPSTREAM" == "$INITIAL_UPSTREAM" ]] || FORCE_STATE_CHANGED=1
  [[ "${FORCE_DEFAULT_REF:-}" == "$INITIAL_DEFAULT_REF" ]] || FORCE_STATE_CHANGED=1
  [[ "$FORCE_LIVE_EVIDENCE" == "$INITIAL_LIVE_EVIDENCE" ]] || FORCE_STATE_CHANGED=1
  if [[ "$FORCE_STATE_CHANGED" -eq 1 ]]; then
    echo "archive-worktree: forced state changed after disclosure; keeping $TARGET" >&2
    echo "    Re-run after the worktree is idle and its state is stable." >&2
    exit 2
  fi
fi

# ---- Salvage local-only .fno do state before removal (data-loss guard) ------
# A worktree's .fno mixes symlinks (canonical state) with REAL local-only
# files (artifacts/, scratchpad/, target-state.md, *.log) that
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
# Two DIFFERENT forces share one word, and conflating them is why the strict
# check alone was not enough. Our `--force` overrides disclosed positive checks
# (dirty, unpushed, live session). `git worktree remove --force` skips GIT's own check, which
# counts a tracked file missing from disk as "modified" and refuses with exit
# 4. So a worktree we affirmatively cleared as recoverable-only still failed to
# remove, and the whole predicate change was inert on exactly the 17 worktrees
# it targets. Pass git's force only after OUR check said yes on its own terms:
# the unpushed-commit and live-session guards above have already run.
# RE-READ AT REMOVAL TIME. The verdict above was taken before the process sweep
# SIGTERM/SIGKILLed anything rooted here and before salvage ran; an editor or
# agent killed mid-write can leave a modified tracked file behind. Git's refusal
# is the last line of defence, so only wave it aside on a verdict that is still
# true right now (the same rule the liveness re-check follows).
if [[ "${_WT_RECOVERABLE_ONLY:-0}" -eq 1 ]]; then
  if wt_reapable "$TARGET"; then
    REMOVE_FLAGS="--force"
  else
    echo "archive-worktree: $WT_REAPABLE_LINE at removal time; keeping $TARGET" >&2
    exit 2
  fi
fi
if ! git worktree remove $REMOVE_FLAGS "$TARGET"; then
  echo "archive-worktree: git worktree remove failed" >&2
  exit 4
fi
git worktree prune

# ---- Branch handling -----------------------------------------------------
BRANCH_DELETE_RC=0
BRANCH_DELETED=0
if [[ "$DELETE_BRANCH" -eq 1 && "$BRANCH" != "(detached)" ]]; then
  if git branch -D "$BRANCH" 2>/dev/null; then
    BRANCH_DELETED=1
    echo "    Deleted branch $BRANCH" >&2
  else
    BRANCH_DELETE_RC=1
    echo "    Branch delete failed (already gone?): $BRANCH" >&2
  fi
else
  if [[ "$BRANCH" != "(detached)" ]]; then
    echo "    Branch $BRANCH preserved (use --delete-branch to remove)" >&2
  fi
fi

if [[ "$FORCE" -eq 1 ]]; then
  if [[ "$BRANCH_DELETED" -eq 1 ]]; then
    echo "archive-worktree: archived $TARGET (--force discarded the disclosed worktree state; branch $BRANCH deleted)" >&2
  elif [[ "$BRANCH" != "(detached)" ]]; then
    echo "archive-worktree: archived $TARGET (--force discarded the disclosed worktree state; branch $BRANCH preserved)" >&2
  else
    echo "archive-worktree: archived $TARGET (--force discarded the disclosed worktree state; detached branch data has no named branch)" >&2
  fi
else
  if [[ "$BRANCH_DELETED" -eq 1 ]]; then
    echo "archive-worktree: archived $TARGET (worktree directory discarded; branch $BRANCH deleted)" >&2
  elif [[ "$BRANCH" != "(detached)" ]]; then
    echo "archive-worktree: archived $TARGET (worktree directory discarded; branch $BRANCH preserved)" >&2
  else
    echo "archive-worktree: archived $TARGET (worktree directory discarded; detached branch data has no named branch)" >&2
  fi
fi
exit "$BRANCH_DELETE_RC"
