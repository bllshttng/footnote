#!/usr/bin/env bash
# check-impl-location.sh - shared, read-only location verdict.
#
# One source of truth for "is it safe to implement here?", consumed by the
# implementation entry points (/target, /do, /fix), by init-target-state.sh
# (which keeps its own hard refusal), and by the universal SessionStart
# heads-up. Extracting it here is what keeps the canonical-main guard from
# drifting across the three skills (design: Worktree Scope Hygiene, US2).
#
# Contract:
#   - Emits a key=value block on stdout and ALWAYS exits 0. It is advisory;
#     the CONSUMER decides whether to block (init-target-state.sh exits 1,
#     /do and /fix refuse, SessionStart only warns).
#   - Degrades silently to the safe default (verdict=ok, nested_count=0)
#     outside a git repo or when any git command fails.
#
# Output keys (scalars are single-valued; nested_path repeats):
#   verdict=ok|canonical-protected   the blocking location gate
#   is_canonical=0|1                 1 when this checkout is the canonical one
#   branch=<name>                    abbrev-ref HEAD ("" when rev-parse failed)
#   is_unborn=0|1                    1 on a fresh repo with no commits (allowed)
#   nested_count=<N>                 worktrees registered under <root>/.claude/worktrees/
#   nested_path=<abs-path>           one line per offending nested worktree
#
# Detection mirrors hooks/helpers/init-target-state.sh exactly:
#   - root: `git rev-parse --show-toplevel` with a `pwd` fallback (a corrupt
#     HEAD makes show-toplevel fail; the fallback keeps the gate live so an
#     undeterminable canonical state is still refused, not waved through).
#   - canonical vs linked worktree: equal --git-dir / --git-common-dir means
#     canonical (also a submodule or a --separate-git-dir clone). They differ
#     ONLY for a linked worktree. Both-empty (corrupt HEAD) compares equal and
#     is treated as canonical, matching the original guard.
#   - protected branches on the canonical checkout: main, master, a detached
#     HEAD, or an unknown branch (rev-parse failed). An unborn HEAD (fresh
#     repo, no commits) is allowed.
#   - nested worktrees: any worktree in `git worktree list` whose path is under
#     <current-checkout-root>/.claude/worktrees/. Scoping to the CURRENT
#     checkout (not the canonical) keeps a clean conductor-worktree session
#     silent while still flagging the canonical checkout that `grep -r` would
#     actually descend into.

set -uo pipefail

_verdict="ok"
_is_canonical=0
_branch=""
_is_unborn=0
_nested_count=0
_nested_paths=()

_emit() {
  printf 'verdict=%s\nis_canonical=%s\nbranch=%s\nis_unborn=%s\nnested_count=%s\n' \
    "$_verdict" "$_is_canonical" "$_branch" "$_is_unborn" "$_nested_count"
  local _p
  for _p in "${_nested_paths[@]+"${_nested_paths[@]}"}"; do
    printf 'nested_path=%s\n' "$_p"
  done
}

# Resolve the checkout root. A corrupt HEAD makes --show-toplevel fail; fall
# back to pwd so the .git-exists guard below still fires (the gate must refuse
# an undeterminable canonical state, not silently allow it).
_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)

# Canonical / protected-branch classification only when this looks like a git
# checkout (a .git entry exists). A non-git directory has no .git and stays
# verdict=ok (mirrors init-target-state.sh's outer guard).
if [[ -e "$_root/.git" ]]; then
  _git_dir=$(git -C "$_root" rev-parse --git-dir 2>/dev/null || echo "")
  _git_common_dir=$(git -C "$_root" rev-parse --git-common-dir 2>/dev/null || echo "")
  if [[ -n "$_git_dir" ]]; then
    _git_dir=$(cd "$_root" && cd "$_git_dir" 2>/dev/null && pwd) || _git_dir=""
  fi
  if [[ -n "$_git_common_dir" ]]; then
    _git_common_dir=$(cd "$_root" && cd "$_git_common_dir" 2>/dev/null && pwd) || _git_common_dir=""
  fi
  # Equal (including both-empty, the corrupt-HEAD case) => canonical /
  # submodule / --separate-git-dir; differing => a linked worktree.
  if [[ "$_git_dir" == "$_git_common_dir" ]]; then
    _is_canonical=1
    _branch=$(git -C "$_root" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
    # Positive unborn signal: HEAD is a symbolic ref to a branch with no commit
    # yet. Distinguishes a genuinely fresh repo from a rev-parse failure caused
    # by a corrupt HEAD / dubious ownership (which must stay protected).
    _head_sym=$(git -C "$_root" symbolic-ref --quiet HEAD 2>/dev/null || true)
    if [[ -n "$_head_sym" ]]; then
      if ! git -C "$_root" rev-parse --verify --quiet "$_head_sym" >/dev/null 2>&1; then
        _is_unborn=1
      fi
    fi
    if [[ "$_is_unborn" != "1" ]]; then
      case "$_branch" in
        main|master|HEAD|"") _verdict="canonical-protected" ;;
      esac
    fi
  fi
fi

# Nested-worktree detection: any registered worktree under
# <root>/.claude/worktrees/. Resolve both <root> and each worktree path to
# PHYSICAL paths (pwd -P) before the prefix match: git's show-toplevel and
# worktree-list paths can mix logical/physical on symlinked roots (macOS
# /var -> /private/var, /tmp -> /private/tmp), which would defeat the match.
# The trailing slash keeps it a strict-descendant test. Capture the listing
# into a variable first (not a process-substitution loop source) so a git
# error surfaces as empty input rather than a masked pipeline status.
_root_phys=$(cd "$_root" 2>/dev/null && pwd -P)
if [[ -z "$_root_phys" ]]; then
  _root_phys="$_root"
fi
_nested_prefix="$_root_phys/.claude/worktrees/"
_wt_list=$(git -C "$_root" worktree list --porcelain 2>/dev/null || true)
while IFS= read -r _line; do
  case "$_line" in
    "worktree "*)
      _wt="${_line#worktree }"
      _wt_phys=$(cd "$_wt" 2>/dev/null && pwd -P)
      if [[ -z "$_wt_phys" ]]; then
        _wt_phys="$_wt"
      fi
      case "$_wt_phys/" in
        "$_nested_prefix"*) _nested_paths+=("$_wt") ;;
      esac
      ;;
  esac
done <<< "$_wt_list"
_nested_count=${#_nested_paths[@]}

# A per-project `never` worktree policy means working on the canonical checkout
# IS the policy (an Obsidian vault whose working tree is the product), so the
# protected-branch gate must not refuse it. Consult the SAME resolver ensure uses
# (`fno worktree policy`); a bare `never` on line 1 flips the verdict to ok. Fail
# CLOSED to the current verdict when fno is absent or errors (empty != never).
if [[ "$_verdict" == "canonical-protected" ]] && command -v fno >/dev/null 2>&1; then
  if [[ "$(fno worktree policy --repo "$_root" 2>/dev/null | head -1)" == "never" ]]; then
    _verdict="ok"
  fi
fi

_emit
exit 0
