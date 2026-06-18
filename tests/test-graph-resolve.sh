#!/usr/bin/env bash
# test-graph-resolve.sh -- coverage for scripts/lib/graph-resolve.sh
# (plan: internal/fno/plans/2026-05-05-fuzzy-resolver-prefix-match.md,
#  node: ab-7651a2c6)
#
# Exercises the shell resolver after it has been rewired to call into
# fno.graph.fuzzy.resolve_id. Each case isolates a behavior:
#   - non-ab passthrough
#   - ab-id full-length resolution (regression)
#   - ab-id prefix unique resolution
#   - ab-id prefix ambiguous (soft fail with stderr)
#   - ab-id prefix no-match (soft fail with stderr)
#   - RESOLVE_FUZZY=1 title fuzzy match (opt-in)
#   - missing graph.json (soft fail)
#
# Tests run by sourcing graph-resolve.sh in a subshell so each case starts
# clean, and pointing GRAPH_JSON at tests/fixtures/graph-fuzzy.json. The
# `abilities` package must be importable for the resolver to call resolve_id;
# we use `uv run` from the cli/ dir to provide that.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RESOLVER="$REPO_ROOT/scripts/lib/graph-resolve.sh"
FIXTURE="$SCRIPT_DIR/fixtures/graph-fuzzy.json"
CLI_DIR="$REPO_ROOT/cli"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

# Skip cleanly when uv isn't available - the harness needs uv to make the
# abilities package importable inside the python heredoc embedded in
# graph-resolve.sh. The legacy fallback path (rc=5) is exercised by a
# dedicated test below that intentionally bypasses uv.
if ! command -v uv >/dev/null 2>&1; then
    echo "SKIP: uv not installed; graph-resolve shell tests require it"
    exit 0
fi

# Run resolve_arg in a subshell so env / sourced state never leaks across
# tests. The arg and any extra env vars are passed via the environment
# (never spliced into the bash -c string) so single quotes, spaces, $, !,
# and other shell metacharacters in inputs cannot misroute the test. The
# harness mirrors graph-resolve.sh's own design constraint: env-passed
# values, never -c interpolation.
run_resolve() {
    local arg="$1"
    local extra_env="${2:-}"
    (
        cd "$CLI_DIR" || exit 99
        export GRAPH_JSON="$FIXTURE"
        export RUN_RESOLVE_ARG="$arg"
        # Parse extra_env as one or more KEY=VALUE pairs, exporting each
        # explicitly. Avoids the unquoted-expansion / split-and-glob
        # hazard of `export $extra_env` and supports multi-key inputs
        # like 'RESOLVE_FUZZY=1 RESOLVE_STRICT=1'.
        if [[ -n "$extra_env" ]]; then
            local pair
            for pair in $extra_env; do
                # shellcheck disable=SC2163
                export "$pair"
            done
        fi
        # Use uv run to ensure the abilities package is on PYTHONPATH.
        # The script body is a literal single-quoted string; the arg is
        # read from RUN_RESOLVE_ARG via the environment.
        uv run --quiet bash -c 'source "$0" && resolve_arg "$RUN_RESOLVE_ARG"' "$RESOLVER" 2>"$STDERR_CAPTURE"
    )
}

STDERR_CAPTURE=$(mktemp -t graph-resolve-stderr.XXXXXX)
trap 'rm -f "$STDERR_CAPTURE"' EXIT

# 1. Non-ab passthrough: arbitrary feature description echoes unchanged.
echo "test 1: non-ab passthrough"
result=$(run_resolve "build a feature")
if [[ "$result" == "build a feature" ]]; then
    pass "non-ab passthrough echoes unchanged"
else
    fail "non-ab passthrough" "expected 'build a feature', got '$result'"
fi

# 2. Full-length ab-id (regression): existing entry resolves to plan_path.
echo "test 2: ab-9728b70b full-length resolves to plan_path"
result=$(run_resolve "ab-9728b70b")
expected="internal/fno/plans/2026-05-05-provider-rotation-failover.md"
if [[ "$result" == "$expected" ]]; then
    pass "full-length id resolves"
else
    fail "full-length id" "expected '$expected', got '$result'"
fi

# 3. Prefix unique (NEW): ab-9728 resolves to the only entry with that prefix.
echo "test 3: ab-9728 prefix uniquely resolves"
result=$(run_resolve "ab-9728")
if [[ "$result" == "$expected" ]]; then
    pass "prefix unique resolves to plan_path"
else
    fail "prefix unique" "expected '$expected', got '$result'"
fi

# 4. Prefix ambiguous (NEW): ab-abcd has two matches; soft-fail echoes input
# and stderr should warn about candidates.
echo "test 4: ab-abcd prefix ambiguous soft-fails"
result=$(run_resolve "ab-abcd")
if [[ "$result" == "ab-abcd" ]]; then
    pass "ambiguous soft-fail echoes arg"
else
    fail "ambiguous soft-fail" "expected 'ab-abcd', got '$result'"
fi
if grep -q "ambiguous" "$STDERR_CAPTURE" 2>/dev/null; then
    pass "ambiguous stderr names the disambiguation"
else
    fail "ambiguous stderr" "expected 'ambiguous' in stderr, got: $(cat "$STDERR_CAPTURE")"
fi

# 5. Prefix no-match (NEW): ab-ffff is hex-shaped but no entry shares
# that prefix in the fixture; the resolver should soft-fail with stderr.
echo "test 5: ab-ffff hex-prefix no-match soft-fails"
: > "$STDERR_CAPTURE"  # reset
result=$(run_resolve "ab-ffff")
if [[ "$result" == "ab-ffff" ]]; then
    pass "prefix no-match echoes arg unchanged"
else
    fail "prefix no-match" "expected 'ab-ffff', got '$result'"
fi
if grep -q "no match" "$STDERR_CAPTURE" 2>/dev/null; then
    pass "prefix no-match stderr explains the miss"
else
    fail "prefix no-match stderr" "expected 'no match' in stderr, got: $(cat "$STDERR_CAPTURE")"
fi

# 5b. Non-hex ab-shaped input (ab-zzzz) is not even an ab-prefix query -
# the shell-level regex filter rejects it before python sees it. The
# resolver should pass it through silently (it could legitimately be a
# non-id argument that happens to start with 'ab-').
echo "test 5b: non-hex ab-shaped input passes through silently"
: > "$STDERR_CAPTURE"  # reset
result=$(run_resolve "ab-zzzz")
if [[ "$result" == "ab-zzzz" ]]; then
    pass "non-hex ab- echoes unchanged"
else
    fail "non-hex ab-" "expected 'ab-zzzz', got '$result'"
fi

# 6. Title fuzzy match opt-in via RESOLVE_FUZZY=1.
echo "test 6: RESOLVE_FUZZY=1 title fuzzy match resolves"
: > "$STDERR_CAPTURE"  # reset
# 'failover follow-up' is unique to ab-cccc0001's title in the fixture.
result=$(run_resolve "failover follow-up" "RESOLVE_FUZZY=1")
expected_followup="internal/fno/plans/2026-05-05-failover-followup.md"
if [[ "$result" == "$expected_followup" ]]; then
    pass "RESOLVE_FUZZY=1 routes title queries"
else
    fail "RESOLVE_FUZZY=1 title match" "expected '$expected_followup', got '$result' (stderr: $(cat "$STDERR_CAPTURE"))"
fi

# 7. RESOLVE_FUZZY default-off: title query echoes unchanged.
echo "test 7: title query without RESOLVE_FUZZY echoes unchanged"
: > "$STDERR_CAPTURE"  # reset
result=$(run_resolve "failover follow-up")
if [[ "$result" == "failover follow-up" ]]; then
    pass "default RESOLVE_FUZZY=0 leaves non-ab queries alone"
else
    fail "title default-off" "expected 'failover follow-up', got '$result'"
fi

# 7b. Shell-metacharacter / quoting safety: a query with single quotes,
# spaces, and a $ should round-trip unchanged through the env-passing
# harness. Catches the quoting-bug class flagged in the code review.
echo "test 7b: shell metacharacter inputs round-trip safely"
: > "$STDERR_CAPTURE"  # reset
result=$(run_resolve "build a feature with 'quotes' and \$dollars")
if [[ "$result" == "build a feature with 'quotes' and \$dollars" ]]; then
    pass "metacharacter input echoes unchanged"
else
    fail "metacharacter input" "expected literal echo, got '$result'"
fi

# 8. Missing graph.json: soft-fail with warning.
echo "test 8: missing graph.json soft-fails"
: > "$STDERR_CAPTURE"  # reset
result=$(
    cd "$CLI_DIR" || exit 99
    export GRAPH_JSON="/tmp/nonexistent-graph-$$-$RANDOM.json"
    uv run --quiet bash -c "source '$RESOLVER' && resolve_arg 'ab-9728b70b'" 2>"$STDERR_CAPTURE"
)
if [[ "$result" == "ab-9728b70b" ]]; then
    pass "missing graph.json echoes arg unchanged"
else
    fail "missing graph.json" "expected 'ab-9728b70b', got '$result'"
fi

# 9. Legacy fallback: when the abilities package is not importable, the
# resolver should fall back to exact-match-only and tell the user that
# partial prefixes cannot resolve in this environment. Simulate by
# pointing PYTHONPATH at an empty directory that shadows the package.
# We invoke python3 directly (not uv run) and override PYTHONPATH so the
# import of fno.graph.fuzzy fails.
echo "test 9: legacy fallback (rc=5) on package import failure"
EMPTY_DIR=$(mktemp -d -t graph-resolve-empty.XXXXXX)
trap 'rm -rf "$STDERR_CAPTURE" "$EMPTY_DIR"' EXIT

# 9a. Full ab-id in legacy mode resolves via the legacy path.
: > "$STDERR_CAPTURE"
result=$(
    cd "$REPO_ROOT" || exit 99
    export GRAPH_JSON="$FIXTURE"
    export PYTHONPATH="$EMPTY_DIR"
    bash -c "source '$RESOLVER' && resolve_arg 'ab-9728b70b'" 2>"$STDERR_CAPTURE"
)
expected="internal/fno/plans/2026-05-05-provider-rotation-failover.md"
if [[ "$result" == "$expected" ]]; then
    pass "legacy fallback resolves full ab-id"
else
    fail "legacy fallback full id" "expected '$expected', got '$result' (stderr: $(cat "$STDERR_CAPTURE"))"
fi
if grep -q "falling back to legacy" "$STDERR_CAPTURE" 2>/dev/null; then
    pass "legacy fallback prints explicit notice"
else
    fail "legacy fallback notice" "expected 'falling back to legacy' in stderr, got: $(cat "$STDERR_CAPTURE")"
fi

# 9b. Partial ab-id in legacy mode echoes input + warns the user.
: > "$STDERR_CAPTURE"
result=$(
    cd "$REPO_ROOT" || exit 99
    export GRAPH_JSON="$FIXTURE"
    export PYTHONPATH="$EMPTY_DIR"
    bash -c "source '$RESOLVER' && resolve_arg 'ab-9728'" 2>"$STDERR_CAPTURE"
)
if [[ "$result" == "ab-9728" ]]; then
    pass "legacy fallback echoes partial prefix unchanged"
else
    fail "legacy fallback partial echo" "expected 'ab-9728', got '$result'"
fi
if grep -q "partial-prefix" "$STDERR_CAPTURE" 2>/dev/null; then
    pass "legacy fallback warns about partial-prefix limitation"
else
    fail "legacy fallback partial warning" "expected 'partial-prefix' in stderr, got: $(cat "$STDERR_CAPTURE")"
fi

# 10. Non-hex ab- inputs are filtered at the shell-regex level (hex-only
# gate) and pass through unchanged. Production graph IDs are all hex; the
# python API (`fno backlog get ab-tr000001`) can still resolve non-hex
# legacy IDs via exact equality. This test pins the shell's filter
# behavior so a future "loosen the regex" change must update this test.
echo "test 10: non-hex ab- input passes through shell filter"
: > "$STDERR_CAPTURE"
result=$(run_resolve "ab-tr000001")
if [[ "$result" == "ab-tr000001" ]]; then
    pass "non-hex ab- input echoes unchanged (filtered at shell regex)"
else
    fail "non-hex ab- shell filter" "expected 'ab-tr000001', got '$result'"
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]] || exit 1
