#!/usr/bin/env bash
# hooks/law-session-start.sh, driven end to end against a stubbed `fno`.
#
# The hook's whole job is to make live operator law visible at SessionStart,
# so its failure mode is rendering nothing. Nothing is also what a healthy
# store with no law renders, which is why every case here asserts a POSITIVE
# marker in stdout and the two silent cases assert silence is the ONLY
# correct answer for that input.
#
# Two of these cases are regressions caught in review rather than invented:
# a payload that does not parse used to exit 0 in silence, and a stdout
# preamble carrying its own JSON object used to be read as the answer.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
HOOK="hooks/law-session-start.sh"
[[ -f "$HOOK" ]] || { echo "FAIL: $HOOK not found from $(pwd)"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STUB="$TMP/bin"
mkdir -p "$STUB"

pass=0
fail=0

# Build a stub `fno` whose body is $1, run the real hook with it on PATH.
run_with_stub() {
    { printf '#!/usr/bin/env bash\n'; printf '%s\n' "$1"; } > "$STUB/fno"
    chmod +x "$STUB/fno"
    PATH="$STUB:$PATH" bash "$HOOK" 2>/dev/null
}

expect_contains() {
    local name="$1" body="$2" needle="$3" out
    out="$(run_with_stub "$body")"
    if [[ "$out" == *"$needle"* ]]; then
        echo "  PASS: $name"
        pass=$((pass + 1))
    else
        echo "  FAIL: $name"
        echo "    wanted substring: $needle"
        echo "    got: ${out:-<empty>}"
        fail=$((fail + 1))
    fi
}

expect_silent() {
    local name="$1" body="$2" out
    out="$(run_with_stub "$body")"
    if [[ -z "$out" ]]; then
        echo "  PASS: $name"
        pass=$((pass + 1))
    else
        echo "  FAIL: $name (expected no output)"
        echo "    got: $out"
        fail=$((fail + 1))
    fi
}

echo "=== law-session-start hook ==="

# The happy path, and the reason the hook names subjects rather than a count:
# the specimen agent did not know a python-to-rust-conversion ruling existed.
expect_contains "live law is named by subject" \
    'echo "{\"decisions\":[{\"subject\":\"python-to-rust-conversion\"}],\"total\":1}"' \
    "python-to-rust-conversion"

expect_contains "live law carries the read verb" \
    'echo "{\"decisions\":[{\"subject\":\"a-ruling\"}],\"total\":1}"' \
    'fno backlog decisions <subject>'

# No law at all is the correct steady state on a fresh install.
expect_silent "an empty store renders nothing" \
    'echo "{\"decisions\":[],\"total\":0}"'

# An fno too old to know the verb returns Typer's exit 2 on EVERY session.
expect_silent "an unknown subcommand (exit 2) does not nag forever" \
    'exit 2'

# REGRESSION: any parse error used to exit 0 in silence, which is
# indistinguishable from a store holding no law.
expect_contains "unparseable output is reported, never silent" \
    'echo "not json at all"' \
    "could not be read"

# REGRESSION: the scan took the first PARSEABLE object, so a preamble
# carrying its own JSON swallowed the real payload and rendered nothing.
expect_contains "a JSON preamble does not swallow the payload" \
    'echo "dedup: {\"a\": 1}"; echo "{\"decisions\":[{\"subject\":\"real-ruling\"}],\"total\":1}"' \
    "real-ruling"

# The narrower half of the same trap: an unparseable brace in the preamble.
expect_contains "a broken brace in the preamble is skipped" \
    'echo "note: { broken"; echo "{\"decisions\":[{\"subject\":\"survives\"}],\"total\":1}"' \
    "survives"

# A read that fails is a report, not an absence. The bound exists because a
# stale deployed fno took 32s on this verb.
expect_contains "a failed read names the failure" \
    'exit 7' \
    "could not be read"

# Damage with nothing live is a damage report. It used to say "0 live
# ruling(s) the operator already made" and then tell the reader to go read one.
expect_contains "a damaged store with no live law reports the damage" \
    'echo "{\"decisions\":[],\"total\":0,\"damaged\":3}"' \
    "could not be parsed"

# An absence assertion, so it rides alongside the positive one above: the
# same input must both name the damage and NOT tell the reader to go read a
# ruling the store could not confirm exists.
out="$(run_with_stub 'echo "{\"decisions\":[],\"total\":0,\"damaged\":3}"')"
if [[ "$out" != *"Read one before deciding"* ]]; then
    echo "  PASS: a damaged-store report omits the read-one instruction"
    pass=$((pass + 1))
else
    echo "  FAIL: a damaged-store report still tells the reader to read one"
    fail=$((fail + 1))
fi

# Live law AND damage: the list renders, with the incompleteness stated.
expect_contains "damage beside live law is reported as incompleteness" \
    'echo "{\"decisions\":[{\"subject\":\"one\"}],\"total\":1,\"damaged\":2}"' \
    "this list is incomplete"

# A row that is not a dict must not take the whole list down with it.
expect_contains "a malformed row does not lose its healthy siblings" \
    'echo "{\"decisions\":[\"oops\",{\"subject\":\"healthy\"}],\"total\":2}"' \
    "healthy"

echo
echo "Results: $pass passed, $fail failed"
[[ $fail -eq 0 ]] || exit 1
