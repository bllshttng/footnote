#!/usr/bin/env bash
# check-pr-node-closure-selftest.sh - self-test for check-pr-node-closure.sh.
#
# Scenarios: target (branch id claimed) passes, contained (a second claimed id
# also in the trailer) passes, missing (branch id absent from the trailer)
# fails, malformed (trailer present but never names the branch id) fails,
# non-node branch skips, and a prose-only mention (never the exact trailer
# line) fails.
# Exit: 0 pass, 1 fail.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="${SCRIPT_DIR}/check-pr-node-closure.sh"

log()  { printf '[pr-node-closure] %s\n' "$*"; }
fail() { printf '[pr-node-closure] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[pr-node-closure] PASS: %s\n' "$*"; }

[[ -f "$GATE" ]] || fail "gate not found at ${GATE}"
bash -n "$GATE" || fail "gate failed bash -n"

# run <body> <head_ref>; echoes the gate's exit code via $?.
run() {
  local body="$1"; local ref="$2"
  PR_BODY="$body" PR_HEAD_REF="$ref" bash "$GATE" >/dev/null 2>&1
}

# run_err <body> <head_ref> <errfile>; captures stderr for content assertions.
run_err() {
  local body="$1"; local ref="$2"; local errfile="$3"
  PR_BODY="$body" PR_HEAD_REF="$ref" bash "$GATE" >/dev/null 2>"$errfile"
}

# run_err fails: a two-line body must hear that only the LAST line counts.
ERR=$(mktemp)
if run_err $'Backlog-Closure: x-aaaa\nBacklog-Closure: x-bbbb' "feature/x-aaaa" "$ERR"; then
  fail "two-line body should fail"
fi
for want in "2 closure lines" "x-bbbb" "x-aaaa" "--extra"; do
  grep -q -e "$want" "$ERR" || fail "two-line refusal should name '$want'"
done
pass "two-line refusal names the count, the id read, the id wanted, and --extra"

# run_err fails: a body with no closure line still says how many lines it read.
if run_err "no closure line here at all." "feature/x-aaaa" "$ERR"; then
  fail "no-trailer body should fail"
fi
grep -q "0 closure lines" "$ERR" || fail "no-trailer refusal should say '0 closure lines'"
pass "no-trailer refusal names the zero count"

# The shipped workflow's remedy must name --extra, or a reader steered to it
# by the gate's annotation learns the singular form again.
grep -q -- '--extra' "${SCRIPT_DIR}/../../.github/workflows/pr-node-closure.yml" \
  || fail "pr-node-closure.yml remedy should name --extra"
pass "workflow remedy names --extra"

# target: the branch's own node id is exactly claimed.
run "Fixes the thing.

Backlog-Closure: x-aaaa" "feature/x-aaaa" \
  && pass "target: claimed id passes" || fail "target should pass"

# contained: the trailer also names a second (contained) id; the branch's own
# id is still present, so it still passes.
run "Backlog-Closure: x-aaaa x-1111" "feature/x-aaaa" \
  && pass "contained: extra claimed id still passes" || fail "contained should pass"

# missing: the branch names an id the trailer never claims.
if run "Backlog-Closure: x-0000" "feature/x-aaaa"; then
  fail "missing claim should fail"
else
  pass "missing claim fails"
fi

# malformed: a trailer line exists but never names the branch's id (a typo'd
# token is silently dropped by the parser, so it reads the same as absent).
if run "Backlog-Closure: x-bbbb" "feature/x-aaaa"; then
  fail "malformed claim should fail"
else
  pass "malformed claim fails"
fi

# non-node-branch: no id-shaped segment in the ref at all.
run "no trailer here" "main" \
  && pass "non-node branch skips" || fail "non-node branch should skip"

# prose-only: the id is mentioned in prose, never on the exact trailer line.
if run "This PR also touches x-aaaa in passing." "feature/x-aaaa"; then
  fail "prose-only mention should fail"
else
  pass "prose-only mention fails"
fi

# all-hex suffix: a real id's suffix ("cdef") is itself a valid node-id
# PREFIX shape, so a following segment must never re-glue with it into a
# second, bogus candidate (review fix: reproduced live pre-fix).
run "Backlog-Closure: x-cccc" "feature/x-cccc-1234" \
  && pass "all-hex suffix never invents a second candidate" \
  || fail "all-hex suffix should not invent a bogus second candidate"

# slash-spanning: a path component and the next one must never re-glue into a
# candidate. "feat/cafe" names no node, so demanding "feat-cafe" would red a
# PR over a line nothing could generate.
run "no trailer here" "feat/cafe" \
  && pass "slash-spanning segments never glue into a candidate" \
  || fail "a candidate must never span a '/'"

# no-space-after-colon: the runtime parser (fno.pr.closure) accepts zero
# spaces after "Backlog-Closure:" - the gate must too (round-7 review fix:
# reproduced live pre-fix, where this well-formed trailer read as missing).
run "Backlog-Closure:x-aaaa" "feature/x-aaaa" \
  && pass "no space after colon still passes" \
  || fail "no space after colon should still pass"

# colonless new spelling: `Fixes <id>` with no colon is the writer's form now.
run "Fixes x-aaaa" "feature/x-aaaa" \
  && pass "colonless Fixes line passes" \
  || fail "colonless Fixes line should pass"

# the minted shape: <kind>/<node>-<mini-slug> binds like feature/<node> did.
run "Fixes x-cccc" "bugfix/x-cccc-wrong-close" \
  && pass "minted-shape branch passes" \
  || fail "minted-shape branch should pass"

# compact legacy ids are still valid closure claims.
if OUTPUT=$(PR_BODY="Fixes xd863 x664b" PR_HEAD_REF="feature/xd863" bash "$GATE"); then
  [[ "$OUTPUT" == *"all present in the exact trailer"* ]] \
    || fail "compact legacy branch was skipped instead of checked"
  pass "compact legacy id passes"
else
  fail "compact legacy id should pass"
fi

# lowercase keyword with colon: `fixes: <id>` reads the same.
run "fixes: x-aaaa" "feature/x-aaaa" \
  && pass "lowercase colonless-spelled fixes line passes" \
  || fail "lowercase fixes line should pass"

# no-space-after-comma: the runtime parser treats a comma as equivalent to a
# space (round-8 review fix: a second id right after a comma, with no space,
# used to read as missing even though it binds fine at merge time).
run "Backlog-Closure:x-cccc,x-aaaa" "feature/x-aaaa" \
  && pass "no space after comma still passes" \
  || fail "no space after comma should still pass"

# stray-internal-colon: a second id glued to the first with a bare ":" (no
# comma, no space) is ONE malformed token to the runtime parser
# (parse_closure_trailer tokenizes only on whitespace/",", so
# "x-aaaa:x-1111" never splits and is_wellformed_node_id rejects the whole
# token - zero ids bound). The gate must fail this, not pass it via the
# label's own colon being mistaken for a separator (round-10 review fix:
# reproduced live pre-fix, where this passed the gate and bound nothing).
if run "Backlog-Closure:x-aaaa:x-1111" "feature/x-1111"; then
  fail "stray internal colon should fail (parser binds zero ids from it)"
else
  pass "stray internal colon between ids fails"
fi

# zero-claim refusal: the remedy must name the Retarget line and the recipe.
if run_err $'Fixes x-bbbb' "feature/x-aaaa" "$ERR"; then
  fail "wrong-node body should fail"
fi
for want in "Retarget" "create.md"; do
  grep -q -e "$want" "$ERR" || fail "wrong-node refusal should name '$want'"
done
pass "wrong-node refusal names the Retarget line and the create.md recipe"

# corpus gate rows: every fixture case carrying gate replays through the real
# gate, so the shared corpus pins the bash leg the same way it pins the Rust
# parser and the Python forwarder tests.
CORPUS="${SCRIPT_DIR}/../../tests/fixtures/pr-closure-cases.json"
if printf '' | base64 -d >/dev/null 2>&1; then B64D="base64 -d"; else B64D="base64 -D"; fi
corpus_rows=0
while IFS=$'\t' read -r head_ref gate body_b64; do
  body="$(printf '%s' "$body_b64" | $B64D)"
  corpus_rows=$((corpus_rows + 1))
  if [[ "$gate" == "pass" ]]; then
    run "$body" "$head_ref" || fail "corpus row should pass: $head_ref / $body"
  elif run "$body" "$head_ref"; then
    fail "corpus row should fail: $head_ref / $body"
  fi
done < <(jq -r '.cases[] | select(.gate) | [.head_ref, .gate, (.body|@base64)] | @tsv' "$CORPUS")
[[ $corpus_rows -gt 0 ]] || fail "no corpus gate rows replayed (fixture missing or jq failed)"
pass "corpus gate rows all match ($corpus_rows rows)"

log "all scenarios passed"
