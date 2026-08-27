#!/usr/bin/env bash
# test-refusal-remedies.sh -- x-d19e: refusal text names non-destructive
# remedies, and a destructive-only remedy states the loss first.
#
# A refusal is self-teaching runtime text; an agent obeys it literally (a
# king passed --force to five rows because the error string named it; another
# withdrew four unclaimed mails because the nag named only withdraw). These
# checks pin the remedy text so a rewrite cannot reintroduce the shape.
#
# The Rust rm-gate messages are covered by behavior tests in
# crates/fno-agents/src/daemon.rs (daemon::tests::rm_*). This script covers
# the surfaces without a unit-testable seam:
#   1. worktree-create-hook.sh refusal + branch hint
#   2. preflight.sh unidentified-lock refusal
#   3. client_verbs.rs resume cwd messages (source-level: the paths eprintln
#      inside the command flow with env side effects)
#
# Every assert is a POSITIVE marker: a string that must be present, never
# only an absence (a missing marker has three explanations; a present one
# has one).
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FAILURES=0

require() { # require <haystack-file> <needle> <label>
    local file="$1" needle="$2" label="$3"
    if grep -qF -- "$needle" "$file"; then
        echo "ok   $label"
    else
        echo "FAIL $label"
        echo "     missing in $file: $needle"
        FAILURES=$((FAILURES + 1))
    fi
}

require_before() { # require_before <haystack-file> <first> <second> <label>
    local file="$1" first="$2" second="$3" label="$4"
    local i j
    # Byte offsets, not line numbers: the loss sentence and the command may
    # share one echo line, and the ordering within that line is the claim.
    i=$(grep -boF -- "$first" "$file" | head -1 | cut -d: -f1)
    j=$(grep -boF -- "$second" "$file" | head -1 | cut -d: -f1)
    if [[ -n "$i" && -n "$j" && "$i" -lt "$j" ]]; then
        echo "ok   $label"
    else
        echo "FAIL $label"
        echo "     '$first' must appear before '$second' in $file (got offsets: ${i:-none}/${j:-none})"
        FAILURES=$((FAILURES + 1))
    fi
}

HOOK="$REPO_ROOT/scripts/setup/worktree-create-hook.sh"
PRE="$REPO_ROOT/scripts/ci/preflight.sh"
CV="$REPO_ROOT/crates/fno-agents/src/client_verbs.rs"

# --- 1. worktree-create-hook.sh -------------------------------------------
# Reuse rung named.
require "$HOOK" "Reuse it instead" "wt-create: refusal offers reuse first"
# Guarded archive path named.
require "$HOOK" "archive-worktree.sh" "wt-create: refusal names guarded archive"
# Loss stated before the plain removal command.
require_before "$HOOK" "are LOST" "git worktree remove \$WORKTREE_PATH" \
    "wt-create: loss precedes removal command"
# No --force anywhere in the refusal block (force skips git's own guards).
if grep -qF -- "worktree remove --force" "$HOOK"; then
    echo "FAIL wt-create: refusal still names 'worktree remove --force'"
    FAILURES=$((FAILURES + 1))
else
    echo "ok   wt-create: no --force removal taught"
fi
# Branch hint offers rename, not -D.
require "$HOOK" "git branch -m" "wt-create: branch hint offers rename"
if grep -qE 'branch -D' "$HOOK"; then
    echo "FAIL wt-create: hint still teaches 'git branch -D'"
    FAILURES=$((FAILURES + 1))
else
    echo "ok   wt-create: no 'git branch -D' taught"
fi

# --- 2. preflight.sh -------------------------------------------------------
# The check is named before the rm -rf, and the loss is stated first.
require "$PRE" "pgrep -f preflight" "preflight: refusal names the liveness check"
require_before "$PRE" "LOSES the mutual" "rm -rf '\$LOCKDIR'" \
    "preflight: loss precedes rm -rf"
# The command remains discoverable (last resort stays a real rung).
require "$PRE" "rm -rf '\$LOCKDIR'" "preflight: rm -rf remains the last rung"

# --- 3. client_verbs.rs resume cwd messages --------------------------------
require "$CV" "the row is the resume handle" "resume: messages name the handle cost"
require "$CV" "fno agents adopt" "resume: messages name adopt as recovery"
require "$CV" "gone for good" "resume: rm framed as the gone-for-good case"

if [[ "$FAILURES" -gt 0 ]]; then
    echo "$FAILURES check(s) failed"
    exit 1
fi
echo "all refusal-remedy checks passed"
