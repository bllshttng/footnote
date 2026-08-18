#!/usr/bin/env bash
# tests/ci/test_smoke_modes.sh
#
# Exercises `fno test smoke`'s mode machinery (keep-going, failure record,
# --retry-failed, --only, subset labelling) against a tiny hermetic registry
# via the SMOKE_REGISTRY_FILE / SMOKE_FAILURE_RECORD test seams, so the real
# ~57 structural steps (and their uv/cargo prerequisites) never run.
#
# Covers AC1-EDGE (keep-going harvests all failures), AC1-UI (summary + header),
# AC2-FR (subset labelled), AC3-ERR (corrupt/missing record -> full run).
#
# The runner is `fno-py test smoke` (the deployed console script, on PATH inside
# a smoke run via cli/.venv/bin; falls back to `uv run --project cli fno-py` for
# a standalone local run).

set -uo pipefail
# Under pipefail, `echo | grep -q` SIGPIPEs the echo when grep exits on its
# first match, so a hit can surface as exit 141 and fail the assertion (a
# load-dependent flake seen inside a real preflight run). Assertions therefore
# read the whole stream: plain grep with stdout sent to /dev/null.

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

REC="$TMP/failures.txt"
REG="$TMP/registry.py"
cat > "$REG" <<'EOF'
STEPS = [("alpha pass", ".", "true"), ("bravo fail", ".", "exit 1"),
         ("charlie pass", ".", "true"), ("delta fail", ".", "false")]
EOF

# Prefer the worktree's venv'd fno-py (current source); fall back to uv run so
# a fresh checkout with no synced venv still resolves. Never trust a global
# `fno-py` on PATH: a deployed build is stale and lacks the smoke subcommand.
VENVED="$REPO_ROOT/cli/.venv/bin/fno-py"
if [[ -x "$VENVED" ]]; then
    RUNNER=("$VENVED" test smoke)
else
    # Absolute --project: the changed-mode cases below run from a throwaway repo,
    # where a relative `cli` would not resolve.
    RUNNER=(uv run --project "$REPO_ROOT/cli" fno-py test smoke)
fi

FAILS=0
ok()   { echo "  ok: $1"; }
fail() { echo "  FAIL: $1"; FAILS=$((FAILS+1)); }

run() { SMOKE_REGISTRY_FILE="$REG" SMOKE_FAILURE_RECORD="$REC" "${RUNNER[@]}" "$@"; }

echo "== AC1-EDGE: keep-going harvests all failures + records them =="
out="$(run --keep-going 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "exit non-zero on failures" || fail "expected non-zero exit"
echo "$out" | grep "fail.*bravo fail" >/dev/null && ok "bravo in summary" || fail "bravo missing from summary"
echo "$out" | grep "fail.*delta fail" >/dev/null && ok "delta in summary" || fail "delta missing from summary"
echo "$out" | grep "pass.*alpha pass" >/dev/null && ok "alpha pass in summary" || fail "alpha missing"
grep -qx "bravo fail" "$REC" && ok "bravo recorded" || fail "bravo not recorded"
grep -qx "delta fail" "$REC" && ok "delta recorded" || fail "delta not recorded"
[[ $(wc -l < "$REC") -eq 2 ]] && ok "record has exactly 2 entries" || fail "record entry count wrong"

echo "== AC1-UI: header states mode + step count =="
echo "$out" | grep "mode=FULL steps=4/4 keep-going" >/dev/null && ok "header full/keep-going" || fail "header wrong: $(echo "$out" | grep mode=)"

echo "== AC2-FR / retry: --retry-failed runs exactly the recorded steps =="
out="$(run --retry-failed --keep-going 2>&1)"
echo "$out" | grep "RETRY SUBSET steps=2/4" >/dev/null && ok "retry subset header 2/4" || fail "retry header wrong: $(echo "$out" | grep mode=)"
echo "$out" | grep "settle-green push" >/dev/null && ok "subset warns to run full before push" || fail "no subset warning"
echo "$out" | grep "bravo fail" >/dev/null && ok "retry ran bravo" || fail "retry missed bravo"
echo "$out" | grep "delta fail" >/dev/null && ok "retry ran delta" || fail "retry missed delta"
echo "$out" | grep "alpha pass" >/dev/null && fail "retry wrongly ran alpha" || ok "retry skipped alpha (not recorded)"

echo "== AC3-ERR: corrupt failure record -> full fallback =="
printf 'this step does not exist\n\x00garbage\n' > "$REC"
out="$(run --retry-failed --keep-going 2>&1)"
echo "$out" | grep "falling back to FULL run" >/dev/null && ok "notes fallback" || fail "no fallback note"
echo "$out" | grep "steps=4/4" >/dev/null && ok "ran full 4/4" || fail "did not run full: $(echo "$out" | grep mode=)"

echo "== AC3-ERR: missing failure record -> full fallback =="
rm -f "$REC"
out="$(run --retry-failed 2>&1)"
echo "$out" | grep "falling back to FULL run" >/dev/null && ok "missing record -> fallback" || fail "no fallback on missing record"

echo "== --only glob selects by name; no-match hard-fails =="
out="$(run --only '*pass' --keep-going 2>&1)"
echo "$out" | grep "ONLY SUBSET steps=2/4" >/dev/null && ok "only subset 2/4" || fail "only header wrong: $(echo "$out" | grep mode=)"
run --only 'zzz-none' >/dev/null 2>&1 && fail "no-match should exit non-zero" || ok "no-match exits non-zero"

echo "== zero steps is never green =="
EMPTY="$TMP/empty.py"; echo 'STEPS = []' > "$EMPTY"
SMOKE_REGISTRY_FILE="$EMPTY" SMOKE_FAILURE_RECORD="$REC" "${RUNNER[@]}" --keep-going >/dev/null 2>&1 \
    && fail "empty registry should not be green" || ok "empty registry exits non-zero"

echo "== --list is verbatim-stable (all-pass registry) =="
PASS_REG="$TMP/pass.py"
cat > "$PASS_REG" <<'EOF'
STEPS = [("alpha pass", ".", "true"), ("bravo pass", ".", "true"),
         ("charlie pass", ".", "true"), ("delta pass", ".", "true")]
EOF
out="$(SMOKE_REGISTRY_FILE="$PASS_REG" "${RUNNER[@]}" --list)"
[[ "$(echo "$out" | wc -l | tr -d ' ')" == "4" ]] && ok "--list prints 4 names" || fail "--list count wrong"

echo "== --changed: subset modes are mutually exclusive =="
for combo in "--changed --only *pass" "--changed --retry-failed"; do
    # shellcheck disable=SC2086
    out="$(run $combo 2>&1)"; rc=$?
    [[ $rc -eq 2 ]] && ok "refuses '$combo' (exit 2)" || fail "'$combo' should exit 2, got $rc"
    echo "$out" | grep "separate subset modes" >/dev/null && ok "names the conflict" || fail "no conflict message for '$combo'"
done
out="$(run --base HEAD 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && ok "--base without --changed refused" || fail "--base without --changed should exit 2"

# A throwaway repo with its own history: changed mode is exercised end to end
# without running the real suite, and every artifact it writes lands there.
NEW="$TMP/changedrepo"
mkdir -p "$NEW/tests" "$NEW/docs"
git -C "$NEW" init -q
git -C "$NEW" config user.email t@t.t; git -C "$NEW" config user.name t
printf '.fno/\n' > "$NEW/.gitignore"   # the receipt is runner output, not fixture content
echo base > "$NEW/docs/base.md"
git -C "$NEW" add -A; git -C "$NEW" commit -qm base
BASE_SHA="$(git -C "$NEW" rev-parse HEAD)"
printf '#!/usr/bin/env bash\necho boom\nexit 3\n' > "$NEW/tests/test_boom.sh"
chmod +x "$NEW/tests/test_boom.sh"
echo note > "$NEW/docs/unknown.md"
git -C "$NEW" add -A; git -C "$NEW" commit -qm head

FULL_REC="$TMP/full-record.txt"; rm -f "$FULL_REC"
changed() { (cd "$NEW" && SMOKE_FAILURE_RECORD="$FULL_REC" "${RUNNER[@]}" --changed "$@"); }

echo "== AC1: a failing selected harness exits with the child's own code =="
out="$(changed --base "$BASE_SHA" --head HEAD 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "propagates the child exit 3 (not a flattened 1)" || fail "expected rc 3, got $rc"
echo "$out" | grep "CHANGED SUBSET" >/dev/null && ok "labelled CHANGED SUBSET" || fail "no CHANGED SUBSET label"
echo "$out" | grep "mode=FULL" >/dev/null && fail "changed run claimed mode=FULL" || ok "never claims mode=FULL"
echo "$out" | grep "shell-harness-self.*test_boom.sh" >/dev/null && ok "receipt names the selecting rule" || fail "no rule in receipt"

echo "== AC4: an unmapped path stays visible and claims no coverage =="
echo "$out" | grep "unmapped docs/unknown.md" >/dev/null && ok "unmapped path listed" || fail "unmapped path hidden"

echo "== AC6/AC9: a changed run never writes the FULL failure record =="
[[ ! -f "$FULL_REC" ]] && ok "full failure record untouched" || fail "changed run wrote the full record"
[[ -f "$NEW/.fno/changed-last-receipt.json" ]] && ok "receipt written in the changed namespace" \
    || fail "no changed-mode receipt"
grep -q '"verdict": "red"' "$NEW/.fno/changed-last-receipt.json" && ok "receipt records the verdict" \
    || fail "receipt verdict missing"
grep -q '"first_signal_seconds"' "$NEW/.fno/changed-last-receipt.json" && ok "receipt records first-signal timing (AC10)" \
    || fail "receipt missing AC10 metrics"

echo "== AC5: an unresolvable base is UNEVALUATED, never a partial green =="
out="$(changed --base 0000000000000000000000000000000000000000 --head HEAD 2>&1)"; rc=$?
[[ $rc -eq 21 ]] && ok "exits 21 (unevaluated)" || fail "expected rc 21, got $rc"
echo "$out" | grep "UNEVALUATED" >/dev/null && ok "says UNEVALUATED with the git cause" || fail "no UNEVALUATED line"
echo "$out" | grep -i "verdict=green" >/dev/null && fail "unevaluated run printed a green verdict" || ok "no green verdict"

echo "== AC4: nothing selected is exit 20, not green =="
git -C "$NEW" rm -q tests/test_boom.sh; git -C "$NEW" commit -qm "drop harness"
NOSEL_BASE="$(git -C "$NEW" rev-parse HEAD)"
echo more > "$NEW/docs/only-docs.md"; git -C "$NEW" add -A; git -C "$NEW" commit -qm docsonly
out="$(changed --base "$NOSEL_BASE" --head HEAD 2>&1)"; rc=$?
[[ $rc -eq 20 ]] && ok "exits 20 (nothing selected)" || fail "expected rc 20, got $rc"
echo "$out" | grep "selected NOTHING" >/dev/null && ok "says the selector found nothing" || fail "silent zero-selection"

echo "== a step exit colliding with a sentinel reports a failure, not a non-verdict =="
# In-band signalling: if a child's own 20/21 were propagated, preflight would
# read "nothing selected"/"unevaluated" and fall through to the full gate
# instead of stopping - a real red downgraded to a non-verdict.
for code in 20 21; do
    mkdir -p "$NEW/tests"   # git drops the dir when the last tracked file leaves
    printf '#!/usr/bin/env bash\nexit %s\n' "$code" > "$NEW/tests/test_collide.sh"
    chmod +x "$NEW/tests/test_collide.sh"
    git -C "$NEW" add -A; git -C "$NEW" commit -qm "collide $code"
    COLLIDE_BASE="$(git -C "$NEW" rev-parse HEAD~1)"
    out="$(changed --base "$COLLIDE_BASE" --head HEAD 2>&1)"; rc=$?
    [[ $rc -eq 1 ]] && ok "child exit $code reported as 1" || fail "child exit $code leaked as rc $rc"
    echo "$out" | grep "collides with a changed-mode sentinel" >/dev/null && ok "names the collision ($code)" \
        || fail "collision not explained ($code)"
    git -C "$NEW" rm -q tests/test_collide.sh; git -C "$NEW" commit -qm "drop collide $code"
done


echo ""
if [[ $FAILS -eq 0 ]]; then echo "test_smoke_modes: ALL PASS"; exit 0
else echo "test_smoke_modes: $FAILS FAILED"; exit 1; fi
