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
#   4. rm-naming refusals in dispatch.py and the Rust ask twins
#      (claude_ask/codex_ask/gemini_ask) + their teardown warnings
#   5. the stop/rm livelock exits in daemon.rs (no_op dead ends named)
#
# Every REMEDY claim is pinned by a positive marker: a string that must be
# present. The two absence checks (--force, branch -D) are backstops paired
# with a positive sibling on the same message; they guard against
# reintroduction, never stand alone as the evidence a remedy landed (an
# absence has three explanations; a present marker has one).
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
# The check is named before the rm -rf, and the loss is stated first. The
# needle pins the -fl + read-what-matched form: bare `pgrep -f` substring
# false-positives on an editor holding the path, which strands a stale lock.
require "$PRE" "pgrep -fl preflight.sh" "preflight: refusal names the liveness check"
require "$PRE" "read what matched" "preflight: check says to read the match, not trust it"
require_before "$PRE" "LOSES the mutual" "rm -rf '\$LOCKDIR'" \
    "preflight: loss precedes rm -rf"
# The command remains discoverable (last resort stays a real rung).
require "$PRE" "rm -rf '\$LOCKDIR'" "preflight: rm -rf remains the last rung"

# --- 3. resume cwd messages ---------------------------------------------------
# The Rust arms have no unit seam (they eprintln inside the command flow), so
# each property is pinned by a marker on ONE physical source line (a phrase
# split across a Rust string continuation is not pinned by grep). The Python
# twin is NOT grepped here: cli/tests/agents/test_resume_cli.py asserts the
# RENDERED ResumeResult.stderr, the stronger instrument - a source grep over
# the same messages would false-green on a fragment refactor.
require "$CV" "the row is the resume handle" "resume(rust): messages name the handle cost"
require "$CV" "fno agents adopt" "resume(rust): rm-then-adopt rebind pair named"
require "$CV" "gone for good" "resume(rust): rm framed as the gone-for-good case"
require "$CV" "recoverable first" "resume(rust): path-recovery check precedes the remedy"
require "$CV" "nothing resumable" "resume(rust): idless row told the truth, no id claim"

# --- 4. rm-naming refusals: the Rust ask twins ---------------------------------
# The Python rm-fallback refusals retired with the ask legs (the ask-adapter
# port): their remedy text now lives only in the Rust twins, and these checks
# are the live contract. The guard-on-one-of-N-paths trap this section once
# guarded (dispatch.py rewritten while the twins kept the rm-first text) ends
# when the Python emitter does.
DIS="$REPO_ROOT/cli/src/fno/agents/dispatch.py"
# The needle matches the single literal line that closes both refusals.
require "$DIS" "fno agents rm --help\`, not here." \
    "rm-fallback(py): override lives in --help, never in the refusal"
require "$DIS" "the exchange finished" "teardown(py): states the exchange finished first"

for pair in "claude:crates/fno-agents/src/claude_ask.rs" "codex:crates/fno-agents/src/codex_ask.rs" "gemini:crates/fno-agents/src/gemini_ask.rs"; do
    h=${pair%%:*}
    f="$REPO_ROOT/${pair#*:}"
    require "$f" "from the harness if it can still name" "followup($h): names the recovery remedy"
    require "$f" "drops the row and its route" "followup($h): names what rm drops"
done
require "$REPO_ROOT/crates/fno-agents/src/codex_ask.rs" "the exchange finished" "teardown(codex): states the exchange finished first"
require "$REPO_ROOT/crates/fno-agents/src/gemini_ask.rs" "the exchange finished" "teardown(gemini): states the exchange finished first"

# --- 5. stop/rm livelock exits -------------------------------------------------
DM="$REPO_ROOT/crates/fno-agents/src/daemon.rs"
require "$DM" "If stop answers no_op" "rm(live): names the stop no_op dead end"
require "$DM" "If stop refuses or no-ops" "rm(idless): names the stop dead end"
require "$DM" "stop-then-rm has no exit here" "stop_claude(idless): no false rm-clears claim"

if [[ "$FAILURES" -gt 0 ]]; then
    echo "$FAILURES check(s) failed"
    exit 1
fi
echo "all refusal-remedy checks passed"
