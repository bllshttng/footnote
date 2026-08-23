#!/usr/bin/env bash
# scripts/ci/check-plan-rung-authority.sh
#
# Path-uniqueness guard for plan readiness (x-3571).
#
# "Is this plan ready?" used to be answered in seven places across three
# languages over four vocabularies, with two of the answers using opposite
# failure policies and no file referencing another. `fno.graph.ladder.plan_rung`
# is now the sole classifier; Bash reaches it through `fno do plan rung` and Rust
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
# finalize.rs shells out to `fno do plan validate`/`stamp`. The registered Rust
# plan readers consume activation-specific keys, never `status:`.
#
# So there is nothing on the far side to pin, and a parity harness would freeze
# a contract with one participant. What can actually regress is someone ADDING
# a Rust plan-status reader, so that is what this guards.
#
# Pure text extraction: no build, no Rust binary, no Python import.
set -uo pipefail

# Every ratchet below compares a `sort`ed inventory against a literal frozen in
# this file, so the collation order IS the contract. Left to the ambient locale
# it is not: a UTF-8 locale folds punctuation and orders `client_verbs.rs` before
# `client.rs`, while CI's C locale does the reverse, so the same tree failed on a
# developer machine and passed on CI with a diff no one could read (the two lists
# hold identical entries in swapped positions). Pin it once, here.
export LC_ALL=C

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
# ALLOWLIST BY FILE, not by pattern. An earlier version required `^status:` and
# a `$PLAN_PATH`-shaped variable on the SAME physical line, which any of these
# walk straight past:
#
#     grep '^status:' \
#         "$PLAN_PATH"                 # line continuation
#     plan="$1"; grep '^status:' "$plan"   # local variable name
#
# A guard a two-line reformat defeats is the decorative guard this check exists
# to remove. So: flag EVERY shell `^status:` extraction, then subtract the files
# known to read a different artifact. Adding a new reader now forces a choice -
# route through `fno do plan rung`, or add the file here with a reason - instead of
# passing silently.
#
# `sed -i` is excluded: an in-place substitution WRITES a status (test fixtures
# stamping a plan at a chosen rung), it does not classify one.
#
# Scoped to PRODUCTION shell - `tests/` is excluded on purpose. A test that
# greps `^status: in_review` is asserting on output a stamper produced, not
# classifying a plan for dispatch; folding those in would mean allowlisting a
# dozen files today and one more per future test, which is how a guard becomes
# noise and then gets switched off. The invariant worth enforcing is narrower
# and sharper: no production shell script decides readiness for itself.
#
# Each allowlisted file reads a DIFFERENT status axis - `.fno/target-state.md`
# (IN_PROGRESS / COMPLETE / BLOCKED) or the Plan-Mode sidecar (pending /
# consumed) - so none is a second readiness implementation.
ALLOWED_STATUS_READERS="
hooks/target-stopfailure.sh
hooks/target-postcompact-reinject.sh
hooks/target-subagent-guard.sh
hooks/worktree-remove.sh
hooks/helpers/init-target-state.sh
scripts/setup/archive-worktree.sh
scripts/lib/archive-artifacts.sh
scripts/lib/handoff-generator.sh
scripts/lib/worktree-lifecycle.sh
skills/target/scripts/detect-pending-plan.sh
scripts/ci/check-plan-rung-authority.sh
"
offenders=""
while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$ALLOWED_STATUS_READERS" in
        *"$f"*) continue ;;
    esac
    offenders="${offenders}${f}
"
done <<EOF
$(
    git ls-files -z -- 'hooks/*.sh' 'scripts/*.sh' 'skills/*.sh' 2>/dev/null \
        | xargs -0 grep -lE '\^status:' 2>/dev/null \
        | while IFS= read -r cand; do
              # Keep the file only if at least one `^status:` line is a READ.
              if grep -E '\^status:' "$cand" 2>/dev/null | grep -qv 'sed -i'; then
                  echo "$cand"
              fi
          done
)
EOF
offenders="$(printf '%s' "$offenders" | grep -v '^$' || true)"
if [ -n "$offenders" ]; then
    violation "a shell script reads \`status:\` itself; call \`fno do plan rung\` instead" \
        "$offenders" \
        "If this file reads a DIFFERENT status axis (the target-state manifest or" \
        "the Plan-Mode sidecar), add it to ALLOWED_STATUS_READERS with its reason."
else
    note "OK: no unlisted shell script extracts \`status:\`"
fi

# ---------------------------------------------------------------------------
# 2. Rust must not grow a plan-status reader.
#
# Freeze the known plan-document readers.
# Both consume activation-specific markers, never `status:`.
# ---------------------------------------------------------------------------
echo "--- Rust: no plan-status reader ---"
EXPECTED_RUST_PLAN_READERS="crates/fno-agents/src/delivery_completion.rs
crates/fno-agents/src/kill_criteria.rs"
actual=$(
    git ls-files -z -- 'crates/**/*.rs' 2>/dev/null \
        | xargs -0 grep -lE '((std::)?fs::read(_to_string)?\([^)]*plan|read_to_string\([^)]*plan|read_plan|load_plan|parse_plan)' 2>/dev/null \
        | grep -v '/tests/' | LC_ALL=C sort || true
)
if [ "$actual" != "$EXPECTED_RUST_PLAN_READERS" ]; then
    violation "the set of Rust sources reading a plan document changed" \
        "expected: $EXPECTED_RUST_PLAN_READERS" \
        "actual:   ${actual:-(none)}" \
        "A new Rust plan reader must NOT classify \`status:\` itself - shell out to" \
        "\`fno do plan rung\` (exit 0 = dispatchable). If the new reader genuinely needs" \
        "no status, add it to EXPECTED_RUST_PLAN_READERS with a one-line reason."
else
    note "OK: the registered Rust plan-reader set is unchanged"
fi

# Every registered reader rejects any `"status"` string literal: serde YAML
# accepts direct lookup, indexing, parsed-key comparisons, and Value::from, so
# enumerating access syntax would be a decorative guard. The sole subtraction
# is kill_criteria.rs's exact existing `git status --porcelain` argv line.
# Scan every registered reader so growing the allowlist cannot silently weaken
# the guard.
fm_status=""
while IFS= read -r reader; do
    [ -n "$reader" ] || continue
    matches="$(
        grep -nF '"status"' "$reader" 2>/dev/null || true
    )"
    if [ "$reader" = "crates/fno-agents/src/kill_criteria.rs" ]; then
        matches="$(
            printf '%s\n' "$matches" \
                | grep -vE '^[0-9]+:[[:space:]]*\.args\(\["status",[[:space:]]*"--porcelain"\]\)[;]?[[:space:]]*$' \
                || true
        )"
    fi
    if [ -n "$matches" ]; then
        fm_status="${fm_status}${reader}:
${matches}
"
    fi
done <<EOF
$EXPECTED_RUST_PLAN_READERS
EOF
if [ -n "$fm_status" ]; then
    violation "a registered Rust plan reader extracts frontmatter \`status\`" \
        "$fm_status"
else
    note "OK: registered Rust plan readers never extract frontmatter status"
fi

# ---------------------------------------------------------------------------
# 3. Python keeps one rung table.
# ---------------------------------------------------------------------------
echo "--- Python: one rung table ---"
tables=$(
    git ls-files -z -- 'cli/src/**/*.py' 2>/dev/null \
        | xargs -0 grep -l '_STATUS_TO_RUNG' 2>/dev/null | LC_ALL=C sort || true
)
if [ "$tables" != "cli/src/fno/graph/ladder.py" ]; then
    violation "the rung table must live only in cli/src/fno/graph/ladder.py" \
        "found in: ${tables:-(nowhere - was plan_rung deleted?)}"
else
    note "OK: the rung table lives only in ladder.py"
fi

# Dispatch policy stays explicit for both plan-bearing and plan-less nodes.
# Autonomous selection reads plan_rung directly through selection_guards; a
# second is_selectable predicate would be decorative because no selector calls it.
if grep -q 'def is_selectable' cli/src/fno/graph/ladder.py 2>/dev/null; then
    violation "is_selectable must not return as a decorative policy" \
        "Autonomous selectors read plan_rung through selection_guards."
elif ! grep -q 'def is_dispatchable' cli/src/fno/graph/ladder.py 2>/dev/null \
    || ! grep -q 'def is_cold_dispatchable' cli/src/fno/graph/ladder.py 2>/dev/null; then
    violation "both real dispatch policies must exist in ladder.py" \
        "Expected is_dispatchable and is_cold_dispatchable."
else
    note "OK: real dispatch policies are present and is_selectable is absent"
fi

echo ""
if [ "$fail" -ne 0 ]; then
    echo "FAIL: plan-rung authority check"
    exit 1
fi
echo "PASS: plan-rung authority check"
