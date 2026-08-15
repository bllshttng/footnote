#!/usr/bin/env bash
# Positive control for check-no-hardcoded-paths.sh.
#
# The gate scanned hooks/context-nudge.sh on every CI run and stayed green while
# that file wrote four latch files per session into the state-dir top level. The
# pattern was `\$HOME/\.fno/`: the brace form escaped it, and the required
# trailing slash excused a bare directory assignment. A gate nobody has watched
# fail is not known to work, so this asserts a positive hit per spelling rather
# than the gate's silence.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GATE="$REPO_ROOT/scripts/ci/check-no-hardcoded-paths.sh"
TMP_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMP_ROOT"' EXIT

FIXTURE_ROOT="$TMP_ROOT/repo"
git clone --quiet --no-hardlinks "$REPO_ROOT" "$FIXTURE_ROOT"
# Exercise the candidate gate even before it is committed.
cp "$GATE" "$FIXTURE_ROOT/scripts/ci/check-no-hardcoded-paths.sh"
# The gate resolves its scan root from CWD (`git rev-parse --show-toplevel`),
# not from its own location, so every invocation must run from inside the
# fixture. Called from the real checkout it would scan the real checkout and
# report a clean tree no matter what the fixture contains.
run_gate() { ( cd "$FIXTURE_ROOT" && bash scripts/ci/check-no-hardcoded-paths.sh "$@" ); }

if ! run_gate >/dev/null 2>&1; then
    echo "FAIL: clean fixture must pass the hardcoded-path gate" >&2
    exit 1
fi

# Each case is one spelling of the same literal. All four must be rejected.
assert_rejected() {
    local label="$1" scope="$2" body="$3"
    local probe="$FIXTURE_ROOT/$scope/_selftest_probe.sh"
    local output="" actual_exit=0

    printf '#!/usr/bin/env bash\n%s\n' "$body" > "$probe"
    output="$(run_gate 2>&1)" || actual_exit=$?
    rm -f "$probe"

    if [[ "$actual_exit" -eq 0 ]]; then
        echo "FAIL: $label passed the gate" >&2
        exit 1
    fi
    if ! grep -q '_selftest_probe.sh' <<<"$output"; then
        echo "FAIL: $label was rejected but the report did not name the file" >&2
        exit 1
    fi
    echo "  ok: $label rejected"
}

assert_rejected "hooks/ brace form, no trailing slash" hooks 'D="${HOME}/.fno"'
assert_rejected "hooks/ bare form with trailing slash" hooks 'D="$HOME/.fno/graph.json"'
assert_rejected "skills/ brace form with trailing slash" skills 'D="${HOME}/.fno/bus"'
assert_rejected "skills/ bare form, no trailing slash" skills 'D="$HOME/.fno"'

# The sanctioned fallback form must still pass, or every migrated caller breaks.
assert_accepted() {
    local label="$1" body="$2"
    local probe="$FIXTURE_ROOT/hooks/_selftest_probe.sh"
    local actual_exit=0

    printf '#!/usr/bin/env bash\n%s\n' "$body" > "$probe"
    run_gate >/dev/null 2>&1 || actual_exit=$?
    rm -f "$probe"

    if [[ "$actual_exit" -ne 0 ]]; then
        echo "FAIL: $label must pass the gate" >&2
        exit 1
    fi
    echo "  ok: $label accepted"
}

assert_accepted "\${VAR:-\$HOME/.fno} fallback" 'D="${STATE_DIR:-$HOME/.fno}/latches"'
assert_accepted "\${VAR:-\${HOME}/.fno} fallback" 'D="${STATE_DIR:-${HOME}/.fno}/latches"'
assert_accepted "comment mentioning \${HOME}/.fno" '# writes to ${HOME}/.fno by default'

echo "PASS: check-no-hardcoded-paths rejects every spelling of the bare state dir"
