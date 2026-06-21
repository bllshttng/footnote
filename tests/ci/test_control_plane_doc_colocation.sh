#!/usr/bin/env bash
# tests/ci/test_control_plane_doc_colocation.sh
#
# Test harness for scripts/ci/control-plane-doc-colocation.sh (advisory nudge).
#
# Scenarios (the check is ADVISORY, so every case must exit 0):
#   T01 - control-plane changed, no docs/architecture/ touched -> ADVISORY + ::warning, rc=0
#   T02 - control-plane changed AND docs/architecture/ touched  -> PASS, no advisory, rc=0
#   T03 - only non-control-plane files changed                  -> PASS (no control plane), rc=0
#   T04 - only test files under a control-plane dir changed     -> PASS (test exclusion), rc=0
#   T05 - missing manifest                                      -> soft no-op notice, rc=0
#   T06 - prefix-glob include entry (sub/loop*) triggers advisory, rc=0
#
# Exit codes: 0 pass, 1 fail, 77 skip (missing deps)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ADVISORY_SCRIPT="${REPO_ROOT}/scripts/ci/control-plane-doc-colocation.sh"

fail() { printf '[doc-coloc] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[doc-coloc] PASS: %s\n' "$*"; }

[[ -f "${ADVISORY_SCRIPT}" ]] || fail "script not found at ${ADVISORY_SCRIPT}"
bash -n "${ADVISORY_SCRIPT}" || fail "script failed bash -n"

TMP=$(mktemp -d -t doc-coloc-test-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

FIXTURE_MANIFEST="$TMP/manifest.yaml"
write_fixture_manifest() {
    cat > "$FIXTURE_MANIFEST" <<'MANIFEST'
include:
  - hooks/
  - scripts/lib/
  - sub/loop*
extensions:
  - sh
  - rs
exclude:
  - "**/tests/**"
MANIFEST
}

BASE_BRANCH=""
build_repo() {
    local repo="$TMP/repo"
    rm -rf "$repo"; mkdir -p "$repo"; cd "$repo"
    git init -q
    git config user.email "test@example.com"
    git config user.name "Test User"
    mkdir -p hooks scripts/lib sub docs/architecture src hooks/tests
    printf 'a\n' > hooks/check.sh
    printf 'b\n' > scripts/lib/util.sh
    printf 'c\n' > sub/loop_entry.rs
    printf 'doc\n' > docs/architecture/seed.md
    printf 'x=1\n' > src/app.py
    git add -A
    git commit -q -m "base"
    BASE_BRANCH=$(git rev-parse --abbrev-ref HEAD)
    git checkout -q -b feature
}

# Run the script against the sandbox; echoes combined stdout+stderr, sets RC.
RC=0
run_advisory() {
    local manifest="$1"
    cd "$TMP/repo"
    set +e
    OUT=$(LOC_RATCHET_MANIFEST="$manifest" bash "$ADVISORY_SCRIPT" --base "$BASE_BRANCH" 2>&1)
    RC=$?
    set -e
}

write_fixture_manifest

# ── T01: control-plane change, no architecture doc ───────────────────────────
build_repo
printf 'a\nchanged\n' > "$TMP/repo/hooks/check.sh"
git -C "$TMP/repo" commit -q -am "touch hook only"
run_advisory "$FIXTURE_MANIFEST"
[[ "$RC" -eq 0 ]] || fail "T01 expected rc=0 (advisory), got $RC"
grep -q "ADVISORY" <<< "$OUT" || fail "T01 expected ADVISORY in output: $OUT"
grep -q "::warning" <<< "$OUT" || fail "T01 expected ::warning annotation: $OUT"
pass "T01 control-plane without docs -> advisory, rc=0"

# ── T02: control-plane change WITH architecture doc ──────────────────────────
build_repo
printf 'a\nchanged\n' > "$TMP/repo/hooks/check.sh"
printf 'doc\nupdated\n' > "$TMP/repo/docs/architecture/seed.md"
git -C "$TMP/repo" commit -q -am "touch hook and doc"
run_advisory "$FIXTURE_MANIFEST"
[[ "$RC" -eq 0 ]] || fail "T02 expected rc=0, got $RC"
grep -q "PASS:" <<< "$OUT" || fail "T02 expected PASS: $OUT"
grep -q "ADVISORY" <<< "$OUT" && fail "T02 must NOT advise when docs touched: $OUT"
pass "T02 control-plane with docs -> pass, no advisory"

# ── T03: only non-control-plane files ────────────────────────────────────────
build_repo
printf 'x=2\n' > "$TMP/repo/src/app.py"
git -C "$TMP/repo" commit -q -am "app only"
run_advisory "$FIXTURE_MANIFEST"
[[ "$RC" -eq 0 ]] || fail "T03 expected rc=0, got $RC"
grep -q "no control-plane paths changed" <<< "$OUT" || fail "T03 expected no-control-plane PASS: $OUT"
pass "T03 non-control-plane only -> pass"

# ── T04: only test files under a control-plane dir ───────────────────────────
build_repo
printf 'tt\n' > "$TMP/repo/hooks/tests/t.sh"
git -C "$TMP/repo" add -A
git -C "$TMP/repo" commit -q -m "test files only"
run_advisory "$FIXTURE_MANIFEST"
[[ "$RC" -eq 0 ]] || fail "T04 expected rc=0, got $RC"
grep -q "no control-plane paths changed" <<< "$OUT" || fail "T04 test-only should not trigger: $OUT"
pass "T04 test-only under control-plane -> pass (excluded)"

# ── T05: missing manifest -> soft no-op ──────────────────────────────────────
build_repo
printf 'a\nchanged\n' > "$TMP/repo/hooks/check.sh"
git -C "$TMP/repo" commit -q -am "touch hook"
run_advisory "$TMP/does-not-exist.yaml"
[[ "$RC" -eq 0 ]] || fail "T05 missing manifest must be soft no-op rc=0, got $RC"
grep -q "manifest not found" <<< "$OUT" || fail "T05 expected manifest-not-found notice: $OUT"
pass "T05 missing manifest -> soft no-op, rc=0"

# ── T06: prefix-glob include (sub/loop*) ─────────────────────────────────────
build_repo
printf 'c\nchanged\n' > "$TMP/repo/sub/loop_entry.rs"
git -C "$TMP/repo" commit -q -am "touch glob-matched file"
run_advisory "$FIXTURE_MANIFEST"
[[ "$RC" -eq 0 ]] || fail "T06 expected rc=0, got $RC"
grep -q "ADVISORY" <<< "$OUT" || fail "T06 glob include should trigger advisory: $OUT"
pass "T06 prefix-glob include -> advisory"

printf '[doc-coloc] ALL PASS\n'
