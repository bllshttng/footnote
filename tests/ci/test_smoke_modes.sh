#!/usr/bin/env bash
# tests/ci/test_smoke_modes.sh
#
# Exercises scripts/ci/smoke.sh's mode machinery (keep-going, failure record,
# --retry-failed, --only, subset labelling) against a tiny hermetic registry
# via the SMOKE_REGISTRY_FILE / SMOKE_FAILURE_RECORD test seams, so the real
# 45 steps (and their uv/cargo prerequisites) never run.
#
# Covers AC1-EDGE (keep-going harvests all failures), AC1-UI (summary + header),
# AC2-FR (subset labelled), AC3-ERR (corrupt/missing record -> full run).

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
SMOKE="$REPO_ROOT/scripts/ci/smoke.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

REC="$TMP/failures.txt"
REG="$TMP/registry.sh"
cat > "$REG" <<'EOF'
register_steps() {
    step "alpha pass" "." 'true'
    step "bravo fail" "." 'exit 1'
    step "charlie pass" "." 'true'
    step "delta fail" "." 'false'
}
EOF

FAILS=0
ok()   { echo "  ok: $1"; }
fail() { echo "  FAIL: $1"; FAILS=$((FAILS+1)); }

run() { SMOKE_REGISTRY_FILE="$REG" SMOKE_FAILURE_RECORD="$REC" bash "$SMOKE" "$@"; }

echo "== AC1-EDGE: keep-going harvests all failures + records them =="
out="$(run --keep-going 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "exit non-zero on failures" || fail "expected non-zero exit"
echo "$out" | grep -q "fail.*bravo fail" && ok "bravo in summary" || fail "bravo missing from summary"
echo "$out" | grep -q "fail.*delta fail" && ok "delta in summary" || fail "delta missing from summary"
echo "$out" | grep -q "pass.*alpha pass" && ok "alpha pass in summary" || fail "alpha missing"
grep -qx "bravo fail" "$REC" && ok "bravo recorded" || fail "bravo not recorded"
grep -qx "delta fail" "$REC" && ok "delta recorded" || fail "delta not recorded"
[[ $(wc -l < "$REC") -eq 2 ]] && ok "record has exactly 2 entries" || fail "record entry count wrong"

echo "== AC1-UI: header states mode + step count =="
echo "$out" | grep -q "mode=FULL steps=4/4 keep-going" && ok "header full/keep-going" || fail "header wrong: $(echo "$out" | grep mode=)"

echo "== AC2-FR / retry: --retry-failed runs exactly the recorded steps =="
out="$(run --retry-failed --keep-going 2>&1)"
echo "$out" | grep -q "RETRY SUBSET steps=2/4" && ok "retry subset header 2/4" || fail "retry header wrong: $(echo "$out" | grep mode=)"
echo "$out" | grep -q "settle-green push" && ok "subset warns to run full before push" || fail "no subset warning"
echo "$out" | grep -q "bravo fail" && ok "retry ran bravo" || fail "retry missed bravo"
echo "$out" | grep -q "delta fail" && ok "retry ran delta" || fail "retry missed delta"
echo "$out" | grep -q "alpha pass" && fail "retry wrongly ran alpha" || ok "retry skipped alpha (not recorded)"

echo "== AC3-ERR: corrupt failure record -> full fallback =="
printf 'this step does not exist\n\x00garbage\n' > "$REC"
out="$(run --retry-failed --keep-going 2>&1)"
echo "$out" | grep -q "falling back to FULL run" && ok "notes fallback" || fail "no fallback note"
echo "$out" | grep -q "steps=4/4" && ok "ran full 4/4" || fail "did not run full: $(echo "$out" | grep mode=)"

echo "== AC3-ERR: missing failure record -> full fallback =="
rm -f "$REC"
out="$(run --retry-failed 2>&1)"
echo "$out" | grep -q "falling back to FULL run" && ok "missing record -> fallback" || fail "no fallback on missing record"

echo "== --only glob selects by name; no-match hard-fails =="
out="$(run --only '*fail' --keep-going 2>&1)"
echo "$out" | grep -q "ONLY SUBSET steps=2/4" && ok "only subset 2/4" || fail "only header wrong: $(echo "$out" | grep mode=)"
run --only 'zzz-none' >/dev/null 2>&1 && fail "no-match should exit non-zero" || ok "no-match exits non-zero"

echo "== zero steps is never green =="
EMPTY="$TMP/empty.sh"; echo 'register_steps() { :; }' > "$EMPTY"
SMOKE_REGISTRY_FILE="$EMPTY" SMOKE_FAILURE_RECORD="$REC" bash "$SMOKE" --keep-going >/dev/null 2>&1 \
    && fail "empty registry should not be green" || ok "empty registry exits non-zero"

echo "== --list is verbatim-stable (all-pass registry) =="
out="$(run --list)"
[[ "$(echo "$out" | wc -l | tr -d ' ')" == "4" ]] && ok "--list prints 4 names" || fail "--list count wrong"

echo ""
if [[ $FAILS -eq 0 ]]; then echo "test_smoke_modes: ALL PASS"; exit 0
else echo "test_smoke_modes: $FAILS FAILED"; exit 1; fi
