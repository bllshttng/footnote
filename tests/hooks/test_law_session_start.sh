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

# A subject is a table of contents, and a table of contents cannot warn you.
# A ruling that says IN ITS OWN TEXT not to reopen it speaks its first
# sentence instead of being named: measured 2026-09-07, a session read
# `review-coverage` in this line as a topic it knew and spent ninety minutes
# arguing against the ruling behind it.
expect_contains "a settled ruling speaks its first sentence" \
    'echo "{\"decisions\":[{\"subject\":\"review-coverage\",\"decision\":\"Two reviews maximum. This is settled. Do not reopen it.\"}],\"total\":1}"' \
    "review-coverage: Two reviews maximum."

expect_contains "a settled ruling is labelled as settled" \
    'echo "{\"decisions\":[{\"subject\":\"s\",\"decision\":\"Origin never gates. Do not re-derive it.\"}],\"total\":1}"' \
    "Settled, do not re-derive:"

# The whole point: the settled block sits outside RENDER_CAP, so the ruling a
# confident agent is about to break cannot be the one truncated away.
expect_contains "a settled ruling survives the render cap" \
    'echo "{\"decisions\":[{\"subject\":\"filler-0\"},{\"subject\":\"filler-1\"},{\"subject\":\"filler-2\"},{\"subject\":\"filler-3\"},{\"subject\":\"filler-4\"},{\"subject\":\"filler-5\"},{\"subject\":\"filler-6\"},{\"subject\":\"filler-7\"},{\"subject\":\"filler-8\"},{\"subject\":\"filler-9\"},{\"subject\":\"late-law\",\"decision\":\"Two rounds complete the phase. Do not reopen it.\"}],\"total\":11}"' \
    "late-law: Two rounds complete the phase."

# Narrow on purpose: the preamble is byte-budgeted, and only a ruling whose
# own text carries the clause is worth the bytes.
out="$(run_with_stub 'echo "{\"decisions\":[{\"subject\":\"ordinary\",\"decision\":\"Blueprints run on opus.\"}],\"total\":1}"')"
if [[ "$out" == *"ordinary"* && "$out" != *"Settled, do not re-derive"* ]]; then
    echo "  PASS: an ordinary ruling stays a name"
    pass=$((pass + 1))
else
    echo "  FAIL: an ordinary ruling was rendered as settled"
    echo "    got: $out"
    fail=$((fail + 1))
fi

# The block is exempt from RENDER_CAP, so it carries its own ceiling. Without
# one, a store that grows settled rulings grows the preamble every session with
# nothing measuring it. Six settled rows, cap of three, and the remainder is
# COUNTED rather than dropped in silence.
expect_contains "an over-cap settled block names what it left out" \
    'echo "{\"decisions\":[{\"subject\":\"settled-0\",\"decision\":\"Ruling 0 stands. Do not reopen it.\"},{\"subject\":\"settled-1\",\"decision\":\"Ruling 1 stands. Do not reopen it.\"},{\"subject\":\"settled-2\",\"decision\":\"Ruling 2 stands. Do not reopen it.\"},{\"subject\":\"settled-3\",\"decision\":\"Ruling 3 stands. Do not reopen it.\"},{\"subject\":\"settled-4\",\"decision\":\"Ruling 4 stands. Do not reopen it.\"},{\"subject\":\"settled-5\",\"decision\":\"Ruling 5 stands. Do not reopen it.\"}],\"total\":6}"' \
    "and 3 more settled ruling(s)"

out="$(run_with_stub 'echo "{\"decisions\":[{\"subject\":\"settled-0\",\"decision\":\"Ruling 0 stands. Do not reopen it.\"},{\"subject\":\"settled-1\",\"decision\":\"Ruling 1 stands. Do not reopen it.\"},{\"subject\":\"settled-2\",\"decision\":\"Ruling 2 stands. Do not reopen it.\"},{\"subject\":\"settled-3\",\"decision\":\"Ruling 3 stands. Do not reopen it.\"},{\"subject\":\"settled-4\",\"decision\":\"Ruling 4 stands. Do not reopen it.\"},{\"subject\":\"settled-5\",\"decision\":\"Ruling 5 stands. Do not reopen it.\"}],\"total\":6}"')"
if [[ "$out" == *"settled-2"* && "$out" != *"settled-3"* ]]; then
    echo "  PASS: the settled block stops at its cap"
    pass=$((pass + 1))
else
    echo "  FAIL: the settled block ignored its cap"
    echo "    got: $out"
    fail=$((fail + 1))
fi

echo
echo "Results: $pass passed, $fail failed"
[[ $fail -eq 0 ]] || exit 1
