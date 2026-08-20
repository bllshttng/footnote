#!/usr/bin/env bash
# worktree-salvage-ref.sh - post-commit hook: make a commit gc-proof and
# enumerable before anything can kill the worker that made it.
#
# The founding case: a worker killed mid-run never reaches a stop gate, so
# commit time is the only moment guaranteed to occur before a kill. The
# local `update-ref` is offline, atomic, and about a millisecond - it always
# succeeds, so durability never depends on the network that may be exactly
# what just killed the worker. Remote mirroring is repository opt-in via
# `git config --local fno.salvageRemoteMirror true`; when enabled it runs
# detached and best effort, and its failure is harmless because the local ref
# already holds.
#
# Deliberately outside refs/heads/: at commit time nothing has classified
# this work yet, so a commit-time hook must create no branch and no PR. A
# recovery push into refs/heads/ happens only later, only after the
# three-way classifier positively says STRANDED (worktree_stranded.py).
#
# ponytail: no pruning. A ref is ~40 bytes and 64 worktrees cost under 3 KB.
# When object retention is measured as a real cost, prune with
# `git for-each-ref refs/fno/salvage` plus `update-ref -d`.

toplevel="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
ref="refs/fno/salvage/$(basename "$toplevel")"

git update-ref "$ref" HEAD || exit 0

# Never upload work-in-progress without an explicit repository-local opt-in.
# When enabled, the mirror stays fully detached so a dead network cannot delay
# the commit it is meant to protect.
if [[ "$(git config --local --bool --get fno.salvageRemoteMirror 2>/dev/null)" == "true" ]]; then
  ( git push --quiet origin "+HEAD:$ref" >/dev/null 2>&1 & disown ) 2>/dev/null
fi

exit 0
