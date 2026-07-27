#!/usr/bin/env bash
# scripts/ci/check-plan-rung-authority.sh
#
# Path-uniqueness guard for plan readiness (x-3571).
#
# "Is this plan ready?" used to be answered in seven places across three
# languages over four vocabularies, with two of the answers using opposite
# failure policies and no file referencing another. `fno.graph.ladder.plan_rung`
# is now the sole classifier; Bash reaches it through `fno plan rung` and Rust
# reaches it by shelling out or by reading Python-derived JSON.
#
# This check exists because that consolidation is only worth anything while it
# holds. Re-deriving the rung anywhere else is cheap to do by accident and
# invisible in review - the whole defect class started as one reasonable-looking
# `grep '^status:'`.
#
# THE RUST SIDE HAS NO PARSER, AND THAT IS THE INVARIANT.
# The design that produced this check assumed `loopcheck.rs` parsed plan
# frontmatter status and specified a fixture-corpus parity harness against it.
# Reading the source says otherwise: loopcheck.rs and loop_target.rs parse
# `.fno/target-state.md` (a DIFFERENT vocabulary - COMPLETE|BLOCKED|ABORTED),
# finalize.rs shells out to `fno plan validate`/`stamp`, and loop_megawalk.rs
# takes plan_path from `fno backlog next` JSON whose status Python already
# derived. Only kill_criteria.rs opens a plan document at all, and it extracts
# the `kill_criteria:` block, never `status:`.
#
# So there is nothing on the far side to pin, and a parity harness would freeze
# a contract with one participant. What can actually regress is someone ADDING
# a Rust plan-status reader, so that is what this guards.
#
# Pure text extraction: no build, no Rust binary, no Python import.
set -uo pipefail

cd "$(dirname "$0")/../.." || exit 2
fail=0

note() { echo "  $*"; }
violation() {
    echo "VIOLATION: $1"
    shift
    for line in "$@"; do echo "    $line"; done
    echo ""
    fail=1
}

# ---------------------------------------------------------------------------
# 1. Bash must not re-derive a plan's rung.
#
# Scoped to PLAN-named paths on purpose. Most `^status:` reads in this repo are
# against `.fno/target-state.md`, whose vocabulary (IN_PROGRESS / COMPLETE) has
# nothing to do with plan rungs; banning the pattern outright would be almost
# entirely false positives and would get switched off within a month.
# ---------------------------------------------------------------------------
echo "--- Bash: no plan-status re-parsing ---"
# `sed -i` is excluded: an in-place substitution WRITES a status (test fixtures
# stamping a plan at a chosen rung), it does not classify one. The defect is
# reading a rung, not setting one.
# The file set comes from `git ls-files`, never from a directory walk. ripgrep
# layers .gitignore, ~/.ignore and .rgignore over any traversal, and one of
# those legitimately carries `target/` for build output - which also swallows
# `skills/target/`, the single most important directory this check covers. A
# guard that silently searches nothing is the decorative guard this check
# exists to prevent, and this one would have shipped green.
bash_hits="$(
    git ls-files -z -- '*.sh' 2>/dev/null \
        | xargs -0 grep -nE '\^status:.*\$\{?(PLAN_PATH|PLAN_DIR|PLAN_FILE)' 2>/dev/null \
        | grep -v 'sed -i' || true
)"
if [ -n "$bash_hits" ]; then
    violation "a shell script classifies plan status itself; call \`fno plan rung\` instead" \
        "$bash_hits"
else
    note "OK: no shell script extracts \`status:\` from a plan path"
fi

# ---------------------------------------------------------------------------
# 2. Rust must not grow a plan-status reader.
#
# Freeze the set of Rust sources that open a plan document. kill_criteria.rs is
# the sole legitimate member: it extracts `kill_criteria:`, not `status:`.
# ---------------------------------------------------------------------------
echo "--- Rust: no plan-status reader ---"
EXPECTED_RUST_PLAN_READERS="crates/fno-agents/src/kill_criteria.rs"
actual=$(
    git ls-files -z -- 'crates/**/*.rs' 2>/dev/null \
        | xargs -0 grep -lE 'read_to_string\(&?plan' 2>/dev/null \
        | grep -v '/tests/' | sort || true
)
if [ "$actual" != "$EXPECTED_RUST_PLAN_READERS" ]; then
    violation "the set of Rust sources reading a plan document changed" \
        "expected: $EXPECTED_RUST_PLAN_READERS" \
        "actual:   ${actual:-(none)}" \
        "A new Rust plan reader must NOT classify \`status:\` itself - shell out to" \
        "\`fno plan rung\` (exit 0 = dispatchable). If the new reader genuinely needs" \
        "no status, add it to EXPECTED_RUST_PLAN_READERS with a one-line reason."
else
    note "OK: kill_criteria.rs is still the only Rust plan reader"
fi

# Narrowed to a FRONTMATTER key extraction (`"status" =>` in a match arm, or a
# `status:` line prefix test). A bare `status` token is meaningless here -
# kill_criteria.rs legitimately shells `git status --porcelain`.
fm_status="$(
    grep -nE '"status"[[:space:]]*=>|starts_with\("status:|"\^?status:' \
        crates/fno-agents/src/kill_criteria.rs 2>/dev/null || true
)"
if [ -n "$fm_status" ]; then
    violation "kill_criteria.rs now extracts frontmatter \`status\`; it must read \`kill_criteria:\` only" \
        "$fm_status"
else
    note "OK: kill_criteria.rs extracts kill_criteria only, never frontmatter status"
fi

# ---------------------------------------------------------------------------
# 3. Python keeps one rung table.
# ---------------------------------------------------------------------------
echo "--- Python: one rung table ---"
tables=$(
    git ls-files -z -- 'cli/src/**/*.py' 2>/dev/null \
        | xargs -0 grep -l '_STATUS_TO_RUNG' 2>/dev/null | sort || true
)
if [ "$tables" != "cli/src/fno/graph/ladder.py" ]; then
    violation "the rung table must live only in cli/src/fno/graph/ladder.py" \
        "found in: ${tables:-(nowhere - was plan_rung deleted?)}"
else
    note "OK: the rung table lives only in ladder.py"
fi

# The two policies must stay opposite. A future "simplification" that makes
# is_selectable and is_dispatchable share one predicate would either quarantine
# the backlog on a vault unmount or dispatch against unreadable plans.
if ! grep -q 'def is_selectable' cli/src/fno/graph/ladder.py 2>/dev/null \
    || ! grep -q 'def is_dispatchable' cli/src/fno/graph/ladder.py 2>/dev/null; then
    violation "is_selectable and is_dispatchable must both exist in ladder.py" \
        "They fail OPEN and CLOSED respectively; collapsing them is never a cleanup."
else
    note "OK: both named policies are present"
fi

echo ""
if [ "$fail" -ne 0 ]; then
    echo "FAIL: plan-rung authority check"
    exit 1
fi
echo "PASS: plan-rung authority check"
