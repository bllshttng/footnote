#!/usr/bin/env bash
# tests/ci/test_loc_ratchet.sh
#
# Test harness for scripts/ci/loc-ratchet.sh.
#
# The gate is a per-PR executable-LOC delta check: delta <= 0 passes; delta > 0
# passes only if the PR body carries a `loc-exception:` line with a non-empty
# rationale. (The trajectory log and a CUMULATIVE baseline metric were removed;
# this suite was rewritten to match that contract.)
#
# Scenarios:
#   T01 - zero delta: rc=0
#   T02 - negative delta: rc=0, prints negative delta
#   T03 - positive delta, NO loc-exception: rc nonzero (FAIL)   [direction 1]
#   T04 - positive delta, WITH loc-exception: rc=0 (PASS)        [direction 2]
#   T05 - positive delta, loc-exception with empty rationale: rc nonzero
#   T06 - growth in NON-manifest path: delta 0, rc=0
#   T07 - test-pattern exclusion: growth in tests/ and test_foo.sh = delta 0
#   T08 - extension filter: .md/.json inside manifest dir = delta 0
#   T09 - binary file in manifest dir: skipped without crashing
#   T10 - missing manifest: rc nonzero (fail-closed)
#   T11 - prefix-glob include entry matches sub/loop*
#
# Exit codes: 0 pass, 1 fail, 77 skip (missing deps)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
RATCHET_SCRIPT="${REPO_ROOT}/scripts/ci/loc-ratchet.sh"

log()  { printf '[loc-ratchet] %s\n' "$*"; }
fail() { printf '[loc-ratchet] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[loc-ratchet] PASS: %s\n' "$*"; }
skip() { printf '[loc-ratchet] SKIP: %s\n' "$*" >&2; exit 77; }

[[ -f "${RATCHET_SCRIPT}" ]] || fail "loc-ratchet.sh not found at ${RATCHET_SCRIPT}"
bash -n "${RATCHET_SCRIPT}" || fail "loc-ratchet.sh failed bash -n"

TMP=$(mktemp -d -t loc-ratchet-test-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

# ── Helper: build a sandbox git repo ────────────────────────────────────────
SANDBOX_BASE_BRANCH=""
build_sandbox_repo() {
    local repo="$TMP/repo"
    mkdir -p "$repo"
    cd "$repo"
    git init -q
    git config user.email "test@example.com"
    git config user.name "Test User"

    mkdir -p hooks/tests scripts/lib sub docs src hooks

    # Manifest-matched files (baseline content)
    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
    printf 'line1\nline2\nline3\nline4\nline5\n' > scripts/lib/util.sh
    printf 'line1\nline2\nline3\n' > sub/loop_entry.rs

    # Non-matched files
    printf '# doc\n' > docs/readme.md
    printf 'x = 1\n' > src/other.py
    printf 'test line\n' > hooks/tests/t.sh
    printf 'test line\n' > hooks/test_foo.sh
    printf 'test line\n' > hooks/check_test.sh

    git add -A
    git commit -q -m "base commit"

    SANDBOX_BASE_BRANCH=$(git rev-parse --abbrev-ref HEAD)
    git checkout -q -b feature
    cd "$OLDPWD"
}

# ── Helper: write fixture manifest ──────────────────────────────────────────
write_fixture_manifest() {
    local manifest_path="$1"
    cat > "$manifest_path" <<'MANIFEST'
# loc-ratchet-manifest.yaml fixture (test-only; paths relative to repo root)
include:
  - hooks/
  - scripts/lib/
  - sub/loop*
extensions:
  - sh
  - py
  - yaml
  - yml
  - rs
exclude:
  - "**/tests/**"
  - "**/test_*"
  - "**/*_test.*"
MANIFEST
}

# ── Run ratchet in sandbox ───────────────────────────────────────────────────
# Args: sandbox_repo_path, manifest_path, pr_body, extra_args...
# pr_body is exported as PR_BODY (the exception source). Returns rc in
# $RATCHET_RC and combined output in $RATCHET_OUT.
run_ratchet() {
    local repo="$1"; shift
    local manifest="$1"; shift
    local pr_body="$1"; shift

    RATCHET_OUT=$(
        cd "$repo"
        PR_BODY="$pr_body" \
        LOC_RATCHET_MANIFEST="$manifest" \
            bash "${RATCHET_SCRIPT}" "$@" 2>&1
    ) && RATCHET_RC=0 || RATCHET_RC=$?
}

# ─────────────────────────────────────────────────────────────────────────────
# BUILD THE SANDBOX REPO
# ─────────────────────────────────────────────────────────────────────────────
REPO="$TMP/repo"
build_sandbox_repo

MANIFEST="$TMP/fixture-manifest.yaml"
write_fixture_manifest "$MANIFEST"

# Convenience: empty PR body (no exception).
NO_BODY=""

# ─────────────────────────────────────────────────────────────────────────────
# T01: zero delta passes rc=0
# ─────────────────────────────────────────────────────────────────────────────
log "T01: zero delta -> rc=0"
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T01: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -q "delta" || fail "T01: expected 'delta' in output, got: $RATCHET_OUT"
pass "T01: zero delta passes rc=0"

# ─────────────────────────────────────────────────────────────────────────────
# T02: negative delta (deletion) passes rc=0
# ─────────────────────────────────────────────────────────────────────────────
log "T02: negative delta (deletion) -> rc=0"
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "delete 3 lines from hooks/check.sh"
cd "$OLDPWD"
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T02: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qE 'delta.*-3|-3.*delta' \
    || fail "T02: expected negative delta -3 in output, got: $RATCHET_OUT"
pass "T02: negative delta passes rc=0"

# Reset hooks/check.sh to 8 lines.
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "restore hooks/check.sh"
cd "$OLDPWD"

# ─────────────────────────────────────────────────────────────────────────────
# T03: positive delta, NO loc-exception -> rc nonzero (FAIL)   [direction 1]
# ─────────────────────────────────────────────────────────────────────────────
log "T03: positive delta, no loc-exception -> rc nonzero"
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nnewline9\nnewline10\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "add 2 lines to hooks/check.sh"
cd "$OLDPWD"
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -ne 0 ]] || fail "T03: expected nonzero rc, got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "FAIL|exception|loc-exception" \
    || fail "T03: expected FAIL/exception message, got: $RATCHET_OUT"
pass "T03: positive delta WITHOUT loc-exception exits nonzero"

# ─────────────────────────────────────────────────────────────────────────────
# T04: positive delta, WITH loc-exception -> rc=0 (PASS)        [direction 2]
# Same diff as T03; only the PR body differs. This is the both-direction proof
# that the gate did not become always-pass: identical delta, opposite verdict.
# ─────────────────────────────────────────────────────────────────────────────
log "T04: positive delta (same diff as T03), WITH loc-exception -> rc=0"
run_ratchet "$REPO" "$MANIFEST" "loc-exception: adding two probe lines for the usage-limit gate" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T04: expected rc=0 with loc-exception, got $RATCHET_RC. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "PASS.*exception" \
    || fail "T04: expected PASS (exception declared), got: $RATCHET_OUT"
pass "T04: positive delta WITH loc-exception passes rc=0"

# ─────────────────────────────────────────────────────────────────────────────
# T05: positive delta, loc-exception with EMPTY rationale -> rc nonzero
# The line must have a non-empty rationale after the colon.
# ─────────────────────────────────────────────────────────────────────────────
log "T05: positive delta, loc-exception with empty rationale -> rc nonzero"
run_ratchet "$REPO" "$MANIFEST" "loc-exception:    " --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -ne 0 ]] || fail "T05: expected nonzero rc for empty rationale, got 0. Output: $RATCHET_OUT"
pass "T05: loc-exception with empty rationale does not pass"

# Reset hooks/check.sh to 8 lines.
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "restore hooks/check.sh"
cd "$OLDPWD"

# ─────────────────────────────────────────────────────────────────────────────
# T06: growth in NON-manifest path -> delta 0, rc=0
# ─────────────────────────────────────────────────────────────────────────────
log "T06: growth in non-manifest path -> delta 0, rc=0"
cd "$REPO"
printf 'x = 1\nx = 2\nx = 3\n' > src/other.py
git add src/other.py
git commit -q -m "grow src/other.py (not in manifest)"
cd "$OLDPWD"
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T06: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T06: non-manifest growth is delta 0, rc=0"

# ─────────────────────────────────────────────────────────────────────────────
# T07: test-pattern exclusion -> delta 0
# ─────────────────────────────────────────────────────────────────────────────
log "T07: test-pattern exclusion (tests/ dir + test_*.sh) -> delta 0"
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/tests/t.sh
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/test_foo.sh
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check_test.sh
git add hooks/tests/t.sh hooks/test_foo.sh hooks/check_test.sh
git commit -q -m "grow test-patterned files in hooks/"
cd "$OLDPWD"
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T07: expected rc=0 (test paths excluded), got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T07: test-pattern files excluded from delta"

# ─────────────────────────────────────────────────────────────────────────────
# T08: extension filter -> .md/.json inside manifest dir = delta 0
# ─────────────────────────────────────────────────────────────────────────────
log "T08: extension filter -> .md/.json in manifest dir -> delta 0"
cd "$REPO"
printf 'doc line 1\ndoc line 2\ndoc line 3\n' > hooks/CHANGELOG.md
printf '{"key": "value"}\n' > scripts/lib/config.json
git add hooks/CHANGELOG.md scripts/lib/config.json
git commit -q -m "add .md and .json inside manifest dirs (not in extension whitelist)"
cd "$OLDPWD"
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T08: expected rc=0 (extension filter), got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T08: non-whitelisted extensions excluded from delta"

# ─────────────────────────────────────────────────────────────────────────────
# T09: binary file in manifest dir -> skipped without crashing
# ─────────────────────────────────────────────────────────────────────────────
log "T09: binary file in manifest dir -> skipped, no crash"
cd "$REPO"
printf '\x00\x01\x02\x03binary\x00data\n' > hooks/blob.sh
git add hooks/blob.sh
git commit -q -m "add binary file in hooks/"
cd "$OLDPWD"
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T09: expected rc=0 (binary skipped), got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T09: binary file skipped without crashing"

# ─────────────────────────────────────────────────────────────────────────────
# T10: missing manifest -> rc nonzero (fail-closed)
# ─────────────────────────────────────────────────────────────────────────────
log "T10: missing manifest -> rc nonzero"
run_ratchet "$REPO" "/nonexistent/manifest.yaml" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -ne 0 ]] || fail "T10: expected nonzero rc for missing manifest, got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "manifest" || fail "T10: expected manifest error, got: $RATCHET_OUT"
pass "T10: missing manifest fails closed"

# ─────────────────────────────────────────────────────────────────────────────
# T11: prefix-glob include entry matches sub/loop*
# ─────────────────────────────────────────────────────────────────────────────
log "T11: prefix-glob include (sub/loop*) is matched"
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\n' > sub/loop_entry.rs
git add sub/loop_entry.rs
git commit -q -m "grow sub/loop_entry.rs (matched by sub/loop* glob)"
cd "$OLDPWD"
# +2 lines in a matched path with no exception -> must FAIL (proves it was counted).
run_ratchet "$REPO" "$MANIFEST" "$NO_BODY" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -ne 0 ]] || fail "T11: expected nonzero rc (glob path counted, no exception), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -q "sub/loop_entry.rs" \
    || fail "T11: expected sub/loop_entry.rs in breakdown, got: $RATCHET_OUT"
# And the same diff WITH an exception passes.
run_ratchet "$REPO" "$MANIFEST" "loc-exception: glob-path growth is covered by the exception" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -eq 0 ]] || fail "T11: expected rc=0 with exception on glob path, got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T11: prefix-glob include entry matched in both directions"

log "ALL TESTS PASSED"
