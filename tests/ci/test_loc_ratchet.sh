#!/usr/bin/env bash
# tests/ci/test_loc_ratchet.sh
#
# Test harness for scripts/ci/loc-ratchet.sh (counting core, Task 1.1).
#
# Scenarios:
#   T01 - zero delta: passes rc=0
#   T02 - negative delta: passes rc=0, prints negative delta
#   T03 - positive delta in manifest path: exits nonzero with stub message
#   T04 - growth in NON-manifest path: delta 0, rc=0
#   T05 - test-pattern exclusion: growth in tests/ and test_foo.py = delta 0
#   T06 - extension filter: growth in .md/.json inside manifest dir = delta 0
#   T07 - binary file in manifest dir: skipped without crashing
#   T08 - missing manifest: rc nonzero with fail-closed message
#   T09 - missing trajectory: rc nonzero
#   T10 - cumulative = live - baseline, printed correctly
#   T11 - prefix-glob include entry matches loop_check.rs under sub/loop*
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
# Builds a minimal git repo at $TMP/repo with:
#   hooks/check.sh       - a manifest-matched file (8 lines)
#   scripts/lib/util.sh  - a manifest-matched file (5 lines)
#   sub/loop_entry.rs    - matches a prefix-glob include (3 lines)
#   docs/readme.md       - NOT matched (extension .md excluded)
#   src/other.py         - NOT matched (not under any manifest include path)
#   hooks/tests/t.sh     - NOT matched (test path exclusion)
#   hooks/test_foo.sh    - NOT matched (test filename pattern)
#   hooks/check_test.sh  - NOT matched (test suffix pattern)
# Sets SANDBOX_BASE_BRANCH to the initial branch name (e.g. "main").
SANDBOX_BASE_BRANCH=""
build_sandbox_repo() {
    local repo="$TMP/repo"
    mkdir -p "$repo"
    cd "$repo"
    git init -q
    git config user.email "test@example.com"
    git config user.name "Test User"

    # Create directory structure
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

    # Capture the actual initial branch name (git >= 2.28 may use 'main')
    SANDBOX_BASE_BRANCH=$(git rev-parse --abbrev-ref HEAD)

    # Create a feature branch from this base
    git checkout -q -b feature
    cd "$OLDPWD"
}

# ── Helper: write fixture manifest pointing at sandbox paths ──────────────────
write_fixture_manifest() {
    local manifest_path="$1"
    cat > "$manifest_path" <<'MANIFEST'
# loc-ratchet-manifest.yaml fixture (test-only; paths relative to repo root)
# Include-entry semantics:
#   trailing /   = directory prefix match
#   trailing *   = path-prefix glob
#   otherwise    = exact file
# Exclude-glob: strip **/ prefix; tests/** = path-segment rule;
#               test_*, *_test.* = basename patterns.
# Baseline re-anchor: add a new baseline: block with a dated note.
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

# ── Helper: write fixture trajectory ─────────────────────────────────────────
write_fixture_trajectory() {
    local traj_path="$1"
    local baseline_loc="${2:-13}"
    cat > "$traj_path" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
TRAJ
}

# ── Run ratchet in sandbox ───────────────────────────────────────────────────
# Args: sandbox_repo_path, manifest_path, trajectory_path, extra_args...
# Returns: rc, combined stdout+stderr in $RATCHET_OUT
run_ratchet() {
    local repo="$1"; shift
    local manifest="$1"; shift
    local traj="$1"; shift

    RATCHET_OUT=$(
        cd "$repo"
        LOC_RATCHET_MANIFEST="$manifest" \
        LOC_RATCHET_TRAJECTORY="$traj" \
            bash "${RATCHET_SCRIPT}" "$@" 2>&1
    ) && RATCHET_RC=0 || RATCHET_RC=$?
}

# ─────────────────────────────────────────────────────────────────────────────
# BUILD THE SANDBOX REPO
# ─────────────────────────────────────────────────────────────────────────────
REPO="$TMP/repo"
build_sandbox_repo

MANIFEST="$TMP/fixture-manifest.yaml"
TRAJ="$TMP/fixture-trajectory.yaml"
write_fixture_manifest "$MANIFEST"
# baseline_loc = 8 (hooks/check.sh) + 5 (scripts/lib/util.sh) + 3 (sub/loop_entry.rs) = 16
write_fixture_trajectory "$TRAJ" 16

# ─────────────────────────────────────────────────────────────────────────────
# T01: zero delta passes rc=0
# ─────────────────────────────────────────────────────────────────────────────
log "T01: zero delta -> rc=0"
# No changes on feature branch vs base (just after checkout)
run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -eq 0 ]] || fail "T01: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -q "delta" || fail "T01: expected 'delta' in output, got: $RATCHET_OUT"
pass "T01: zero delta passes rc=0"

# ─────────────────────────────────────────────────────────────────────────────
# T02: negative delta (deletion) passes rc=0
# ─────────────────────────────────────────────────────────────────────────────
log "T02: negative delta (deletion) -> rc=0"
cd "$REPO"
# Delete 3 lines from hooks/check.sh (8 -> 5 lines)
printf 'line1\nline2\nline3\nline4\nline5\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "delete 3 lines from hooks/check.sh"
cd "$OLDPWD"

run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -eq 0 ]] || fail "T02: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qE 'delta.*-3|-3.*delta' \
    || fail "T02: expected negative delta -3 in output, got: $RATCHET_OUT"
pass "T02: negative delta passes rc=0"

# Reset: restore hooks/check.sh to 8 lines for subsequent tests
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "restore hooks/check.sh"
cd "$OLDPWD"

# ─────────────────────────────────────────────────────────────────────────────
# T03: positive delta in manifest path, no exception -> exits nonzero
# (originally verified stub message; now verifies the real fail path with
#  no PR_BODY and no trajectory entry)
# ─────────────────────────────────────────────────────────────────────────────
log "T03: positive delta in manifest path, no exception -> rc nonzero"
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nnewline9\nnewline10\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "add 2 lines to hooks/check.sh"
cd "$OLDPWD"

run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T03: expected nonzero rc, got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "FAIL|exception|PR.body|no exception" \
    || fail "T03: expected FAIL message, got: $RATCHET_OUT"
pass "T03: positive delta without exception exits nonzero"

# Reset: restore hooks/check.sh to 8 lines
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
git add hooks/check.sh
git commit -q -m "restore hooks/check.sh"
cd "$OLDPWD"

# ─────────────────────────────────────────────────────────────────────────────
# T04: growth in NON-manifest path -> delta 0, rc=0
# ─────────────────────────────────────────────────────────────────────────────
log "T04: growth in non-manifest path -> delta 0, rc=0"
cd "$REPO"
printf 'x = 1\nx = 2\nx = 3\n' > src/other.py
git add src/other.py
git commit -q -m "grow src/other.py (not in manifest)"
cd "$OLDPWD"

run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -eq 0 ]] || fail "T04: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T04: non-manifest growth is delta 0, rc=0"

# ─────────────────────────────────────────────────────────────────────────────
# T05: test-pattern exclusion -> delta 0
# ─────────────────────────────────────────────────────────────────────────────
log "T05: test-pattern exclusion (tests/ dir + test_*.sh) -> delta 0"
cd "$REPO"
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/tests/t.sh
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/test_foo.sh
printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check_test.sh
git add hooks/tests/t.sh hooks/test_foo.sh hooks/check_test.sh
git commit -q -m "grow test-patterned files in hooks/"
cd "$OLDPWD"

run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -eq 0 ]] || fail "T05: expected rc=0 (test paths excluded), got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T05: test-pattern files excluded from delta"

# ─────────────────────────────────────────────────────────────────────────────
# T06: extension filter -> .md/.json inside manifest dir = delta 0
# ─────────────────────────────────────────────────────────────────────────────
log "T06: extension filter -> .md/.json in manifest dir -> delta 0"
cd "$REPO"
printf 'doc line 1\ndoc line 2\ndoc line 3\n' > hooks/CHANGELOG.md
printf '{"key": "value"}\n' > scripts/lib/config.json
git add hooks/CHANGELOG.md scripts/lib/config.json
git commit -q -m "add .md and .json inside manifest dirs (not in extension whitelist)"
cd "$OLDPWD"

run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -eq 0 ]] || fail "T06: expected rc=0 (.md/.json not in extensions), got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T06: extension filter excludes .md/.json"

# ─────────────────────────────────────────────────────────────────────────────
# T07: binary file in manifest dir -> skipped without crashing
# ─────────────────────────────────────────────────────────────────────────────
log "T07: binary file in manifest dir -> skipped without crashing"
cd "$REPO"
# Create a binary file (NUL bytes) with a .sh extension
printf 'binary\x00content\x00here' > hooks/binary_tool.sh
git add hooks/binary_tool.sh
git commit -q -m "add binary file to hooks/"
cd "$OLDPWD"

run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

# Binary rows from numstat show "-" in added/deleted columns; should be skipped.
# rc must be 0 (binary is the only net change and it is skipped, so delta=0).
[[ "$RATCHET_RC" -eq 0 ]] \
    || fail "T07: expected rc=0 (binary skipped, delta=0), got $RATCHET_RC. Output: $RATCHET_OUT"
# The binary filename must NOT appear in the per-file breakdown (it was skipped).
echo "$RATCHET_OUT" | grep -qi "binary_tool" \
    && fail "T07: binary_tool.sh should not appear in per-file breakdown (should be skipped), got: $RATCHET_OUT" || true
pass "T07: binary file skipped without crashing (rc=0, not in breakdown)"

# Cleanup binary file
cd "$REPO"
git rm -q hooks/binary_tool.sh
git commit -q -m "remove binary file"
cd "$OLDPWD"

# ─────────────────────────────────────────────────────────────────────────────
# T08: missing manifest -> rc nonzero with fail-closed message
# ─────────────────────────────────────────────────────────────────────────────
log "T08: missing manifest -> rc nonzero, fail-closed message"
run_ratchet "$REPO" "/nonexistent/manifest.yaml" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T08: expected nonzero rc on missing manifest, got 0"
echo "$RATCHET_OUT" | grep -qiE "manifest|not found|missing" \
    || fail "T08: expected fail-closed message naming manifest, got: $RATCHET_OUT"
pass "T08: missing manifest fails closed"

# ─────────────────────────────────────────────────────────────────────────────
# T09: missing trajectory -> rc nonzero
# ─────────────────────────────────────────────────────────────────────────────
log "T09: missing trajectory -> rc nonzero"
run_ratchet "$REPO" "$MANIFEST" "/nonexistent/trajectory.yaml" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T09: expected nonzero rc on missing trajectory, got 0"
echo "$RATCHET_OUT" | grep -qiE "trajectory|not found|missing" \
    || fail "T09: expected fail-closed message naming trajectory, got: $RATCHET_OUT"
pass "T09: missing trajectory fails closed"

# ─────────────────────────────────────────────────────────────────────────────
# T10: cumulative = live - baseline, printed correctly
# ─────────────────────────────────────────────────────────────────────────────
log "T10: cumulative = live - baseline, printed correctly"
# Baseline is 16; at base HEAD, live count is exactly 16.
# After the commits above, there are no net changes to matched files vs base.
# Actually, we've accumulated some commits. Let's compute:
# hooks/check.sh: 8 lines (restored)
# scripts/lib/util.sh: 5 lines (unchanged)
# sub/loop_entry.rs: 3 lines (unchanged)
# Total matched live = 16 = baseline, so cumulative should be 0 or close.
# Use a trajectory with baseline=13 to test cumulative = live(16) - 13 = +3
write_fixture_trajectory "$TMP/traj-t10.yaml" 13

run_ratchet "$REPO" "$MANIFEST" "$TMP/traj-t10.yaml" --base "$SANDBOX_BASE_BRANCH"

[[ "$RATCHET_RC" -eq 0 ]] || fail "T10: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qE 'cumulative.*\+?3|3.*cumulative' \
    || fail "T10: expected cumulative=+3 in output (live 16 - baseline 13), got: $RATCHET_OUT"
pass "T10: cumulative printed correctly"

# ─────────────────────────────────────────────────────────────────────────────
# T11: prefix-glob include entry matches loop_check.rs under sub/loop*
# ─────────────────────────────────────────────────────────────────────────────
log "T11: prefix-glob include (sub/loop*) matches sub/loop_check.rs"
cd "$REPO"
printf 'fn main() {\n    // loop check\n    println!("ok");\n}\n' > sub/loop_check.rs
git add sub/loop_check.rs
git commit -q -m "add sub/loop_check.rs matching sub/loop* glob"
cd "$OLDPWD"

run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"

# sub/loop_check.rs has 4 lines added; this should be counted (positive delta)
[[ "$RATCHET_RC" -ne 0 ]] || fail "T11: expected nonzero rc (loop_check.rs adds 4 lines to manifest scope), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qi "loop_check.rs\|loop_check" \
    || fail "T11: expected loop_check.rs mentioned in output, got: $RATCHET_OUT"
pass "T11: prefix-glob sub/loop* matches sub/loop_check.rs"

# =============================================================================
# T12-T21: Exception protocol + decision table (Task 1.2)
#
# The exception protocol requires new-entry detection via `git show HEAD:path`
# and `git show MB:path`. The trajectory file must therefore be committed into
# the sandbox repo so git can access it.
#
# Approach: put the trajectory at $REPO/scripts/ci/loc-ratchet-trajectory.yaml
# and commit it. The baseline (no entries) is committed on the base branch, and
# each test commits an updated version (with or without entries) on the feature
# branch.
#
# The SANDBOX_BASE_BRANCH contains the empty-entries trajectory.
# Each test (re)commits the trajectory on HEAD (feature branch) and passes
# LOC_RATCHET_TRAJECTORY=$REPO/scripts/ci/loc-ratchet-trajectory.yaml.
# =============================================================================

# Path to trajectory inside sandbox repo (so git show can find it)
REPO_TRAJ="$REPO/scripts/ci/loc-ratchet-trajectory.yaml"
REPO_TRAJ_REL="scripts/ci/loc-ratchet-trajectory.yaml"

# Helper: commit a trajectory file into the sandbox repo on the current branch
commit_trajectory() {
    local content_file="$1"
    local msg="${2:-update trajectory}"
    mkdir -p "$REPO/scripts/ci"
    cp "$content_file" "$REPO_TRAJ"
    (cd "$REPO" && git add "$REPO_TRAJ_REL" && git commit -q -m "$msg")
}

# Helper: write trajectory content (baseline only, empty entries) to a temp file
make_traj_baseline() {
    local out="$1"
    local baseline_loc="${2:-16}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
TRAJ
}

# Helper: write trajectory content with one entry to a temp file
make_traj_one_entry() {
    local out="$1"
    local baseline_loc="${2:-16}"
    local entry_delta="${3:-2}"
    local entry_reason="${4:-some reason}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
  - date: 2026-06-04
    pr: null
    branch: test-branch
    delta: ${entry_delta}
    reason: "${entry_reason}"
TRAJ
}

# Helper: write trajectory with two entries to a temp file
make_traj_two_entries() {
    local out="$1"
    local baseline_loc="${2:-16}"
    local delta1="${3:-2}"
    local delta2="${4:-2}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
  - date: 2026-06-04
    pr: null
    branch: test-branch-1
    delta: ${delta1}
    reason: "first entry"
  - date: 2026-06-04
    pr: null
    branch: test-branch-2
    delta: ${delta2}
    reason: "second entry"
TRAJ
}

# Helper: write trajectory with one entry with empty reason to a temp file
make_traj_empty_reason() {
    local out="$1"
    local baseline_loc="${2:-16}"
    local entry_delta="${3:-2}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
  - date: 2026-06-04
    pr: null
    branch: test-branch
    delta: ${entry_delta}
    reason: ""
TRAJ
}

# Helper: run ratchet in the sandbox repo using the in-repo trajectory
run_ratchet_exc() {
    local body="${1:-}"
    RATCHET_OUT=$(
        cd "$REPO"
        LOC_RATCHET_MANIFEST="$MANIFEST" \
        LOC_RATCHET_TRAJECTORY="$REPO_TRAJ" \
        PR_BODY="$body" \
            bash "${RATCHET_SCRIPT}" --base "$SANDBOX_BASE_BRANCH" 2>&1
    ) && RATCHET_RC=0 || RATCHET_RC=$?
}

# Helper: run ratchet without setting PR_BODY (unset, not empty)
run_ratchet_exc_no_body() {
    RATCHET_OUT=$(
        cd "$REPO"
        LOC_RATCHET_MANIFEST="$MANIFEST" \
        LOC_RATCHET_TRAJECTORY="$REPO_TRAJ" \
            bash "${RATCHET_SCRIPT}" --base "$SANDBOX_BASE_BRANCH" 2>&1
    ) && RATCHET_RC=0 || RATCHET_RC=$?
}

# ── Setup: reset sandbox to a known state for exception protocol tests ────────
# Goal: feature branch has +2 lines vs base (hooks/check.sh 8->10 lines).
# After T11, the repo has sub/loop_check.rs (+4) and hooks/check.sh at 8 lines.
# Reset: remove loop_check.rs, set hooks/check.sh to 10 lines (+2 vs 8-line base).
log "Setting up exception-protocol test state"
(cd "$REPO"
    # Remove loop_check.rs if present (from T11)
    if [[ -f sub/loop_check.rs ]]; then
        git rm -q sub/loop_check.rs
        git commit -q -m "remove loop_check.rs for exception test reset"
    fi
    # Set hooks/check.sh to 10 lines (+2 vs base 8 lines)
    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nnewline9\nnewline10\n' > hooks/check.sh
    git add hooks/check.sh
    git commit -q -m "set hooks/check.sh to 10 lines (+2) for exception tests"
)

# Commit the baseline trajectory (empty entries) onto the SANDBOX_BASE_BRANCH.
# We need to temporarily switch to the base branch, add the file, and come back.
# But SANDBOX_BASE_BRANCH is behind feature; we can add the file on feature
# too (for MB detection, what matters is what was there at the merge-base).
# Since the sandbox was set up with feature branching off base immediately,
# MB = last commit of base branch. We need the trajectory committed BEFORE
# that branch point. The simplest approach: amend history is unsafe; instead
# commit the baseline trajectory on the base branch first (as a new commit
# there), then rebase feature on top of it.
#
# Simpler: use a fresh sub-repo for each exception test with the trajectory
# already on the base branch at branch time.
#
# Actually, simplest correct approach: create a dedicated "exception sandbox"
# where the baseline trajectory is committed as part of the base branch setup.

log "Building exception-protocol sandbox repo"
EXCL_REPO="$TMP/exc-repo"
mkdir -p "$EXCL_REPO"
(cd "$EXCL_REPO"
    git init -q
    git config user.email "test@example.com"
    git config user.name "Test User"

    mkdir -p hooks scripts/lib scripts/ci

    # Same baseline files as main sandbox
    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
    printf 'line1\nline2\nline3\nline4\nline5\n' > scripts/lib/util.sh

    # Baseline trajectory (empty entries) committed on base branch
    cat > scripts/ci/loc-ratchet-trajectory.yaml <<'TRAJ'
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: 13
  note: "fixture baseline for testing"
entries:
TRAJ

    git add -A
    git commit -q -m "base commit with baseline trajectory"
    EXC_BASE=$(git rev-parse --abbrev-ref HEAD)

    # Feature branch: add +5 lines to hooks/check.sh (8->13 lines, delta=+5)
    git checkout -q -b exc-feature
    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nL9\nL10\nL11\nL12\nL13\n' > hooks/check.sh
    git add hooks/check.sh
    git commit -q -m "add 5 lines to hooks/check.sh (delta=+5)"
)
# EXC_BASE was captured inside the subshell above; re-read it from git.
EXC_BASE=$(cd "$EXCL_REPO" && git branch | grep -v 'exc-feature' | tr -d '* ')

EXC_TRAJ="$EXCL_REPO/scripts/ci/loc-ratchet-trajectory.yaml"

# run_exc: run ratchet in exc-repo with given PR_BODY and trajectory content
run_exc() {
    local traj_content_file="$1"
    local body="${2:-}"
    # Update trajectory file and commit it
    cp "$traj_content_file" "$EXC_TRAJ"
    (cd "$EXCL_REPO" && git add scripts/ci/loc-ratchet-trajectory.yaml \
        && git commit -q --allow-empty -m "update trajectory for test")
    # Run ratchet
    RATCHET_OUT=$(
        cd "$EXCL_REPO"
        LOC_RATCHET_MANIFEST="$MANIFEST" \
        LOC_RATCHET_TRAJECTORY="$EXC_TRAJ" \
        PR_BODY="$body" \
            bash "${RATCHET_SCRIPT}" --base "$EXC_BASE" 2>&1
    ) && RATCHET_RC=0 || RATCHET_RC=$?
}

# run_exc_no_body: same but without setting PR_BODY
run_exc_no_body() {
    local traj_content_file="$1"
    cp "$traj_content_file" "$EXC_TRAJ"
    (cd "$EXCL_REPO" && git add scripts/ci/loc-ratchet-trajectory.yaml \
        && git commit -q --allow-empty -m "update trajectory for test (no body)")
    RATCHET_OUT=$(
        cd "$EXCL_REPO"
        LOC_RATCHET_MANIFEST="$MANIFEST" \
        LOC_RATCHET_TRAJECTORY="$EXC_TRAJ" \
            bash "${RATCHET_SCRIPT}" --base "$EXC_BASE" 2>&1
    ) && RATCHET_RC=0 || RATCHET_RC=$?
}

TRAJ_TMP="$TMP/traj-exc.yaml"

# ─────────────────────────────────────────────────────────────────────────────
# T12: both factors present + exact delta + non-empty reason -> rc 0 with warning
# AC2-HP: declared exception passes
# ─────────────────────────────────────────────────────────────────────────────
log "T12: both factors (body + ledger, delta=5, non-empty reason) -> rc=0 with warning"
make_traj_one_entry "$TRAJ_TMP" 13 5 "wedge needs the external read"
run_exc "$TRAJ_TMP" "loc-exception: wedge needs the external read"

[[ "$RATCHET_RC" -eq 0 ]] || fail "T12: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
# Exact wording from the pass-with-exception branch:
echo "$RATCHET_OUT" | grep -q "PASS (exception declared)" \
    || fail "T12: expected 'PASS (exception declared)' in output, got: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qE "\+5|delta.*5|5.*delta" \
    || fail "T12: expected delta=5 mentioned in output, got: $RATCHET_OUT"
pass "T12: both factors -> rc=0 with warning annotation"

# ─────────────────────────────────────────────────────────────────────────────
# T13: body line present but NO new trajectory entry -> rc nonzero naming ledger
# AC2-ERR: half-declared exception (body only)
# ─────────────────────────────────────────────────────────────────────────────
log "T13: body line present, no trajectory entry -> rc nonzero naming ledger factor"
make_traj_baseline "$TRAJ_TMP" 13
run_exc "$TRAJ_TMP" "loc-exception: wedge needs the external read"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T13: expected nonzero rc, got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "trajectory|ledger|entry|entries" \
    || fail "T13: expected message naming ledger/trajectory factor, got: $RATCHET_OUT"
pass "T13: body-only exception fails naming ledger factor"

# ─────────────────────────────────────────────────────────────────────────────
# T14: trajectory entry present but NO body line -> rc nonzero naming body factor
# AC2-ERR: half-declared exception (ledger only)
# ─────────────────────────────────────────────────────────────────────────────
log "T14: trajectory entry present, no body line -> rc nonzero naming body factor"
make_traj_one_entry "$TRAJ_TMP" 13 5 "wedge needs the external read"
run_exc "$TRAJ_TMP" ""

[[ "$RATCHET_RC" -ne 0 ]] || fail "T14: expected nonzero rc, got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "PR.body|body|loc-exception|no exception" \
    || fail "T14: expected message naming body factor, got: $RATCHET_OUT"
pass "T14: ledger-only exception fails naming body factor"

# ─────────────────────────────────────────────────────────────────────────────
# T15: delta mismatch (declared 7, computed 5) -> rc nonzero printing both numbers
# AC2-FR: delta drift during review
# ─────────────────────────────────────────────────────────────────────────────
log "T15: delta mismatch (declared 7, computed 5) -> rc nonzero printing both"
make_traj_one_entry "$TRAJ_TMP" 13 7 "wrong delta declared"
run_exc "$TRAJ_TMP" "loc-exception: wrong delta declared"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T15: expected nonzero rc, got 0. Output: $RATCHET_OUT"
# Anchor to the exact mismatch message wording from the script:
echo "$RATCHET_OUT" | grep -q "declares delta=7" \
    || fail "T15: expected 'declares delta=7' in output, got: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -q "computed delta is 5" \
    || fail "T15: expected 'computed delta is 5' in output, got: $RATCHET_OUT"
pass "T15: delta mismatch fails printing declared and computed values"

# ─────────────────────────────────────────────────────────────────────────────
# T16: two new trajectory entries -> rc nonzero (one borrow per PR rule)
# AC2-EDGE: exactly-one-new-entry rule
# ─────────────────────────────────────────────────────────────────────────────
log "T16: two new trajectory entries -> rc nonzero (one per PR rule)"
make_traj_two_entries "$TRAJ_TMP" 13 5 5
run_exc "$TRAJ_TMP" "loc-exception: first attempt"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T16: expected nonzero rc (two entries), got 0. Output: $RATCHET_OUT"
# Anchor to actual message wording:
echo "$RATCHET_OUT" | grep -q "found 2 new entries" \
    || fail "T16: expected 'found 2 new entries' in output, got: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -q "exactly one" \
    || fail "T16: expected 'exactly one' in output, got: $RATCHET_OUT"
pass "T16: two new entries fails with one-per-PR message"

# ─────────────────────────────────────────────────────────────────────────────
# T17: new trajectory entry with empty reason -> rc nonzero
# AC2-EDGE: empty reason fails
# ─────────────────────────────────────────────────────────────────────────────
log "T17: new trajectory entry with empty reason -> rc nonzero"
make_traj_empty_reason "$TRAJ_TMP" 13 5
run_exc "$TRAJ_TMP" "loc-exception: some reason in body"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T17: expected nonzero rc (empty reason), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "reason|empty|rationale" \
    || fail "T17: expected message about missing/empty reason, got: $RATCHET_OUT"
pass "T17: empty reason in trajectory entry fails"

# ─────────────────────────────────────────────────────────────────────────────
# T18: PR_BODY unset -> rc nonzero with "no exception declared"
# AC: null/empty PR_BODY = "no exception declared"
# ─────────────────────────────────────────────────────────────────────────────
log "T18: PR_BODY unset -> rc nonzero with 'no exception declared'"
make_traj_one_entry "$TRAJ_TMP" 13 5 "some reason"
run_exc_no_body "$TRAJ_TMP"

[[ "$RATCHET_RC" -ne 0 ]] || fail "T18: expected nonzero rc (PR_BODY unset), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "no exception|PR.body|body|loc-exception" \
    || fail "T18: expected 'no exception declared' style message, got: $RATCHET_OUT"
pass "T18: unset PR_BODY fails with no-exception-declared message"

# ─────────────────────────────────────────────────────────────────────────────
# T19: loc-exception: token mid-line or empty rationale does NOT satisfy regex
# ─────────────────────────────────────────────────────────────────────────────
log "T19: loc-exception: mid-line or empty rationale does not satisfy regex"
make_traj_one_entry "$TRAJ_TMP" 13 5 "some reason"

# Sub-case (a): loc-exception: with only whitespace after colon
run_exc "$TRAJ_TMP" "loc-exception:   "
[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T19a: expected nonzero rc (whitespace-only rationale), got 0. Output: $RATCHET_OUT"

# Sub-case (b): loc-exception: buried mid-line (not at start of line)
run_exc "$TRAJ_TMP" "note: loc-exception: some reason"
[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T19b: expected nonzero rc (mid-line loc-exception), got 0. Output: $RATCHET_OUT"

pass "T19: mid-line or empty-rationale loc-exception: does not satisfy regex"

# ─────────────────────────────────────────────────────────────────────────────
# T20: red-to-green recovery: failing run -> add correct ledger entry -> rc 0
# AC1-FR/AC2-FR: recovery path
# ─────────────────────────────────────────────────────────────────────────────
log "T20: red-to-green recovery (fail then add correct entry + body -> rc 0)"
# First run: fail (no exception - empty entries)
make_traj_baseline "$TRAJ_TMP" 13
run_exc "$TRAJ_TMP" ""
[[ "$RATCHET_RC" -ne 0 ]] || fail "T20: expected first run to fail, got 0. Output: $RATCHET_OUT"

# Second run: add correct trajectory entry (delta=5, non-empty reason) + body
make_traj_one_entry "$TRAJ_TMP" 13 5 "legitimate growth after review"
run_exc "$TRAJ_TMP" "loc-exception: legitimate growth after review"
[[ "$RATCHET_RC" -eq 0 ]] \
    || fail "T20: expected second run to pass, got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T20: red-to-green recovery works"

# ─────────────────────────────────────────────────────────────────────────────
# T21: cumulative-positive warning appears when baseline makes cumulative > 0
# AC3-UI: warning when cumulative positive on any run
# ─────────────────────────────────────────────────────────────────────────────
log "T21: cumulative-positive warning when baseline < live count"
# exc-repo has hooks/check.sh=13, scripts/lib/util.sh=5 -> live=18, baseline=13 -> cumulative=+5
# Use baseline=13 (already set in make_traj helpers above)
# Run with no exception (will fail due to delta=+5) but cumulative warning should appear
make_traj_baseline "$TRAJ_TMP" 8
run_exc_no_body "$TRAJ_TMP"
# rc will be nonzero (delta=+5, no exception) - that's expected
echo "$RATCHET_OUT" | grep -qiE "initiative.*debt|still in debt" \
    || fail "T21: expected 'still in debt' warning in output, got: $RATCHET_OUT"
pass "T21: cumulative-positive 'still in debt' warning appears"

# ─────────────────────────────────────────────────────────────────────────────
# T22: per-file breakdown row is well-formed (tab-separated fields)
# AC1-DEFECT6a: literal \t in row string collapses fields; real tab required.
# The script builds MATCHED_FILES rows then reads them with IFS=$'\t'.
# When the bug is present (literal \t), read sees one field and printf gets
# only the filename - so the +added/-deleted/delta columns are BLANK in output.
# We detect this via the GITHUB_STEP_SUMMARY markdown table path: each row must
# be a proper "| file | +N | -N | N |" line, not "| filepath\t... | | | |".
# ─────────────────────────────────────────────────────────────────────────────
log "T22: per-file breakdown row has real tab separators (not literal \\t)"
T22_SUMMARY="$TMP/t22-step-summary.md"
: > "$T22_SUMMARY"
make_traj_baseline "$TRAJ_TMP" 13
# Run in exc-repo (has +5 delta) with GITHUB_STEP_SUMMARY set
RATCHET_OUT=$(
    cd "$EXCL_REPO"
    LOC_RATCHET_MANIFEST="$MANIFEST" \
    LOC_RATCHET_TRAJECTORY="$EXC_TRAJ" \
    GITHUB_STEP_SUMMARY="$T22_SUMMARY" \
        bash "${RATCHET_SCRIPT}" --base "$EXC_BASE" 2>&1
) && RATCHET_RC=0 || RATCHET_RC=$?

# The markdown table row for hooks/check.sh must have | +5 | as a column.
# With the bug: IFS=$'\t' read gives filepath="hooks/check.sh\t+5\t-0\t5"
# and added/deleted/file_delta="". The markdown row becomes "| +|  |  |".
# With the fix: all four fields parse correctly -> "| +5 | -0 | 5 |".
T22_ROW=$(grep -i 'check\.sh' "$T22_SUMMARY" || true)
[[ -n "$T22_ROW" ]] \
    || fail "T22: hooks/check.sh row not found in GITHUB_STEP_SUMMARY. Summary: $(cat "$T22_SUMMARY"). Output: $RATCHET_OUT"
# With the bug (literal \t): added="" -> row has "| +" not "| +5"
# With the fix (real tab): added="+5" -> row has "| +5"
echo "$T22_ROW" | grep -qE '\| \+5' \
    || fail "T22: '| +5' not found as separate column in check.sh row - added column is blank (literal \\t bug). Row: $T22_ROW"
echo "$T22_ROW" | grep -qE '\| -0' \
    || fail "T22: '| -0' not found as separate column in check.sh row - deleted column is blank (literal \\t bug). Row: $T22_ROW"
pass "T22: per-file breakdown rows are tab-separated (markdown table has | +5 and | -0 as separate columns)"

# ─────────────────────────────────────────────────────────────────────────────
# T23: trajectory without entries: key + zero-delta diff -> rc nonzero (parse fail)
# AC1-DEFECT2a: missing entries: key must be a parse failure on every run
# ─────────────────────────────────────────────────────────────────────────────
log "T23: trajectory missing entries: key + zero-delta diff -> rc nonzero (parse fail)"
# Create a trajectory with NO entries: key at all
TRAJ_NO_ENTRIES="$TMP/traj-no-entries.yaml"
cat > "$TRAJ_NO_ENTRIES" <<'TRAJ'
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: 16
  note: "fixture baseline - no entries key at all"
TRAJ
# T01 state: feature branch has no net changes to matched files vs base (delta=0)
# Use the main sandbox repo (still at zero-delta from the resets above)
run_ratchet "$REPO" "$MANIFEST" "$TRAJ_NO_ENTRIES" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T23: expected nonzero rc (missing entries: key), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "entries|parse|trajectory" \
    || fail "T23: expected parse-failure message mentioning entries/trajectory, got: $RATCHET_OUT"
pass "T23: trajectory without entries: key fails closed (rc nonzero with parse message)"

# ─────────────────────────────────────────────────────────────────────────────
# T24: trajectory WITH entries: key and empty list + zero delta -> rc 0
# AC1-DEFECT2b: valid empty-list entries: is not a parse error
# ─────────────────────────────────────────────────────────────────────────────
log "T24: trajectory with entries: key and empty list + zero delta -> rc 0"
# Build a dedicated zero-delta sandbox so we don't depend on main sandbox state.
T24_REPO="$TMP/t24-repo"
mkdir -p "$T24_REPO"
(cd "$T24_REPO"
    git init -q
    git config user.email "test@example.com"
    git config user.name "Test User"
    mkdir -p hooks scripts/lib
    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
    printf 'line1\nline2\nline3\nline4\nline5\n' > scripts/lib/util.sh
    git add -A
    git commit -q -m "base commit"
    T24_BASE=$(git rev-parse --abbrev-ref HEAD)
    git checkout -q -b t24-feature
    echo "T24_BASE=$T24_BASE" > "$TMP/t24-base.txt"
)
T24_BASE=$(cat "$TMP/t24-base.txt" | awk -F= '{print $2}')
# Trajectory with entries: key, empty list (no items under it) -- this is write_fixture_trajectory
TRAJ_EMPTY_ENTRIES="$TMP/traj-empty-entries.yaml"
write_fixture_trajectory "$TRAJ_EMPTY_ENTRIES" 13
run_ratchet "$T24_REPO" "$MANIFEST" "$TRAJ_EMPTY_ENTRIES" --base "$T24_BASE"
[[ "$RATCHET_RC" -eq 0 ]] \
    || fail "T24: expected rc=0 (entries: present, delta=0), got $RATCHET_RC. Output: $RATCHET_OUT"
pass "T24: empty entries list with entries: key present passes (rc=0)"

# ─────────────────────────────────────────────────────────────────────────────
# T25: bogus base ref -> rc nonzero, message mentions merge-base
# AC1-ERR: unreachable base ref fails closed
# ─────────────────────────────────────────────────────────────────────────────
log "T25: bogus base ref -> rc nonzero, message mentions merge-base"
run_ratchet "$REPO" "$MANIFEST" "$TRAJ" --base "does-not-exist-xyz"
[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T25: expected nonzero rc on bogus base ref, got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "merge.base|merge_base|cannot compute|unreachable" \
    || fail "T25: expected merge-base error message, got: $RATCHET_OUT"
pass "T25: bogus base ref fails closed with merge-base message"

# ─────────────────────────────────────────────────────────────────────────────
# T26: malformed manifest (no include: key) -> rc nonzero, fail-closed message
# AC1-ERR: include-less manifest must fail closed
# ─────────────────────────────────────────────────────────────────────────────
log "T26: malformed manifest (no include: key) -> rc nonzero with fail-closed message"
MANIFEST_NO_INCLUDE="$TMP/manifest-no-include.yaml"
cat > "$MANIFEST_NO_INCLUDE" <<'MANIFEST'
# manifest with no include: key at all
extensions:
  - sh
  - py
exclude:
  - "**/tests/**"
MANIFEST
run_ratchet "$REPO" "$MANIFEST_NO_INCLUDE" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T26: expected nonzero rc on manifest with no include: key, got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "include|manifest|missing|empty" \
    || fail "T26: expected fail-closed message mentioning include/manifest, got: $RATCHET_OUT"
pass "T26: include-less manifest fails closed with descriptive message"

# ─────────────────────────────────────────────────────────────────────────────
# T27: env-path (BASE_REF=main + refs/remotes/origin/main in sandbox) -> same delta as --base
# AC: CI's actual env-injection path (BASE_REF env + origin/ prefixing) is exercised
# ─────────────────────────────────────────────────────────────────────────────
log "T27: BASE_REF env + refs/remotes/origin/<base> -> same delta as --base form"
# Use exc-repo (has +5 delta, EXC_BASE known) with refs/remotes/origin/<base> wired.
# Seed origin ref so BASE_REF=<base-branch-name> resolves to origin/<base>.
(cd "$EXCL_REPO" && git update-ref "refs/remotes/origin/${EXC_BASE}" "$(git rev-parse "${EXC_BASE}")")

T27_OUT=$(
    cd "$EXCL_REPO"
    LOC_RATCHET_MANIFEST="$MANIFEST" \
    LOC_RATCHET_TRAJECTORY="$EXC_TRAJ" \
    BASE_REF="$EXC_BASE" \
        bash "${RATCHET_SCRIPT}" 2>&1
) && T27_RC=0 || T27_RC=$?

# Run the --base form for comparison (same exc-repo state, no body, no exception)
T27_BASE_OUT=$(
    cd "$EXCL_REPO"
    LOC_RATCHET_MANIFEST="$MANIFEST" \
    LOC_RATCHET_TRAJECTORY="$EXC_TRAJ" \
        bash "${RATCHET_SCRIPT}" --base "$EXC_BASE" 2>&1
) && T27_BASE_RC=0 || T27_BASE_RC=$?

# Both should fail (delta=+5, no exception), and both should show the same delta.
[[ "$T27_RC" -ne 0 ]] \
    || fail "T27: expected nonzero rc from BASE_REF path (delta=+5, no exception), got 0. Output: $T27_OUT"
[[ "$T27_BASE_RC" -ne 0 ]] \
    || fail "T27: expected nonzero rc from --base path (delta=+5, no exception), got 0. Output: $T27_BASE_OUT"
# Both outputs should mention the same +5 delta.
echo "$T27_OUT" | grep -qE '\+5|delta.*5' \
    || fail "T27: BASE_REF path did not show +5 delta. Output: $T27_OUT"
echo "$T27_BASE_OUT" | grep -qE '\+5|delta.*5' \
    || fail "T27: --base path did not show +5 delta. Output: $T27_BASE_OUT"
pass "T27: BASE_REF env path computes same delta as --base form"

# ─────────────────────────────────────────────────────────────────────────────
# T28: pass-with-exception + GITHUB_STEP_SUMMARY -> summary contains reason, +delta, projection
# AC2-UI: step-summary exception block is populated
# ─────────────────────────────────────────────────────────────────────────────
log "T28: pass-with-exception with GITHUB_STEP_SUMMARY -> summary has reason/delta/projection"
T28_SUMMARY="$TMP/t28-step-summary.md"
: > "$T28_SUMMARY"
make_traj_one_entry "$TRAJ_TMP" 13 5 "wedge needs the external read"
run_exc "$TRAJ_TMP" "loc-exception: wedge needs the external read"
# The trajectory is now committed; re-run with GITHUB_STEP_SUMMARY set.
RATCHET_OUT=$(
    cd "$EXCL_REPO"
    LOC_RATCHET_MANIFEST="$MANIFEST" \
    LOC_RATCHET_TRAJECTORY="$EXC_TRAJ" \
    PR_BODY="loc-exception: wedge needs the external read" \
    GITHUB_STEP_SUMMARY="$T28_SUMMARY" \
        bash "${RATCHET_SCRIPT}" --base "$EXC_BASE" 2>&1
) && RATCHET_RC=0 || RATCHET_RC=$?

[[ "$RATCHET_RC" -eq 0 ]] \
    || fail "T28: expected rc=0 (exception declared), got $RATCHET_RC. Output: $RATCHET_OUT"
# Step summary must contain: the declared reason, "+5" delta, and projection.
grep -q "wedge needs the external read" "$T28_SUMMARY" \
    || fail "T28: declared reason not found in step summary. Summary: $(cat "$T28_SUMMARY")"
grep -qE '\+5|\*\*Delta\*\*.*5' "$T28_SUMMARY" \
    || fail "T28: +delta not found in step summary. Summary: $(cat "$T28_SUMMARY")"
grep -qiE "post.merge|projection|cumulative" "$T28_SUMMARY" \
    || fail "T28: post-merge cumulative projection not found in step summary. Summary: $(cat "$T28_SUMMARY")"
pass "T28: step summary contains reason, +delta, and post-merge projection"

# ─────────────────────────────────────────────────────────────────────────────
# T29: manifest with empty extensions: section -> rc nonzero with fail-closed message
# AC-EDGE: empty extensions list must fail closed (no extensions = nothing matches = pass would be wrong)
# ─────────────────────────────────────────────────────────────────────────────
log "T29: manifest with empty extensions: section -> rc nonzero with fail-closed message"
MANIFEST_EMPTY_EXT="$TMP/manifest-empty-ext.yaml"
cat > "$MANIFEST_EMPTY_EXT" <<'MANIFEST'
include:
  - hooks/
  - scripts/lib/
extensions:
exclude:
  - "**/tests/**"
MANIFEST

run_ratchet "$REPO" "$MANIFEST_EMPTY_EXT" "$TRAJ" --base "$SANDBOX_BASE_BRANCH"
[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T29: expected nonzero rc (empty extensions: section), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "extensions|extension|empty|missing" \
    || fail "T29: expected fail-closed message mentioning extensions, got: $RATCHET_OUT"
pass "T29: empty extensions: section fails closed with descriptive message"

# =============================================================================
# T30-T33: Append-only enforcement, identity fields, corrected projection (PR #439 codex review)
# =============================================================================

# Helper: write trajectory with one entry that EDITS the seeded entry's delta/reason
# (simulates a PR rewriting an existing MB entry to match its own computed delta).
# The entry has the same date/branch as the seeded MB entry but different delta/reason.
make_traj_edited_entry() {
    local out="$1"
    local baseline_loc="${2:-13}"
    local new_delta="${3:-5}"
    local new_reason="${4:-edited reason}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
  - date: 2026-05-01
    pr: 437
    branch: seeded-branch
    delta: ${new_delta}
    reason: "${new_reason}"
TRAJ
}

# Helper: write a trajectory where MB already has one entry and HEAD has that
# same entry PLUS one legitimately new one (pr: backfill scenario uses different approach).
# For T31 (pr-backfill): MB entry has pr: null; HEAD same entry has pr: 439, plus a new entry.
make_traj_pr_backfill() {
    local out="$1"
    local baseline_loc="${2:-13}"
    local new_delta="${3:-5}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
  - date: 2026-05-01
    pr: 439
    branch: seeded-branch
    delta: 3
    reason: "prior seeded entry"
  - date: 2026-06-04
    pr: null
    branch: exc-feature
    delta: ${new_delta}
    reason: "this PR's new entry"
TRAJ
}

# Helper: write trajectory with new entry missing branch field
make_traj_missing_branch() {
    local out="$1"
    local baseline_loc="${2:-13}"
    local new_delta="${3:-5}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
  - date: 2026-06-04
    pr: null
    delta: ${new_delta}
    reason: "missing branch field"
TRAJ
}

# Helper: write trajectory with new entry with empty date
make_traj_empty_date() {
    local out="$1"
    local baseline_loc="${2:-13}"
    local new_delta="${3:-5}"
    cat > "$out" <<TRAJ
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: ${baseline_loc}
  note: "fixture baseline for testing"
entries:
  - date:
    pr: null
    branch: exc-feature
    delta: ${new_delta}
    reason: "empty date field"
TRAJ
}

# ── Build exc-repo-edit: a repo where MB has a seeded prior entry ─────────────
# This simulates a repo that already has one trajectory entry from a prior PR.
# The P1 bug: a PR edits that seeded entry's delta/reason (to match its own
# computed delta) and the old code sees the edited entry as "new" (passes).
# With the fix: the append-only check detects the seeded entry was removed -> FAIL.
log "Building exc-repo-edit sandbox (append-only P1 reproduction)"
EXC_EDIT_REPO="$TMP/exc-edit-repo"
mkdir -p "$EXC_EDIT_REPO"
(cd "$EXC_EDIT_REPO"
    git init -q
    git config user.email "test@example.com"
    git config user.name "Test User"
    mkdir -p hooks scripts/lib scripts/ci

    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
    printf 'line1\nline2\nline3\nline4\nline5\n' > scripts/lib/util.sh

    # MB trajectory: already has one seeded entry (delta=3, a prior PR borrow)
    cat > scripts/ci/loc-ratchet-trajectory.yaml <<'TRAJ'
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: 13
  note: "fixture baseline for testing"
entries:
  - date: 2026-05-01
    pr: 437
    branch: seeded-branch
    delta: 3
    reason: "prior seeded entry"
TRAJ

    git add -A
    git commit -q -m "base commit with seeded trajectory entry"
    EXC_EDIT_BASE=$(git rev-parse --abbrev-ref HEAD)
    echo "$EXC_EDIT_BASE" > /tmp/exc-edit-base.txt

    # Feature branch: add +5 lines to hooks/check.sh (delta=+5)
    git checkout -q -b exc-edit-feature
    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nL9\nL10\nL11\nL12\nL13\n' > hooks/check.sh
    git add hooks/check.sh
    git commit -q -m "add 5 lines to hooks/check.sh (delta=+5)"
)
EXC_EDIT_BASE=$(cat /tmp/exc-edit-base.txt)
EXC_EDIT_TRAJ="$EXC_EDIT_REPO/scripts/ci/loc-ratchet-trajectory.yaml"

# run_exc_edit: run ratchet in exc-edit-repo with the given trajectory content file
run_exc_edit() {
    local traj_content_file="$1"
    local body="${2:-}"
    cp "$traj_content_file" "$EXC_EDIT_TRAJ"
    (cd "$EXC_EDIT_REPO" && git add scripts/ci/loc-ratchet-trajectory.yaml \
        && git commit -q --allow-empty -m "update trajectory for edit test")
    RATCHET_OUT=$(
        cd "$EXC_EDIT_REPO"
        LOC_RATCHET_MANIFEST="$MANIFEST" \
        LOC_RATCHET_TRAJECTORY="$EXC_EDIT_TRAJ" \
        PR_BODY="$body" \
            bash "${RATCHET_SCRIPT}" --base "$EXC_EDIT_BASE" 2>&1
    ) && RATCHET_RC=0 || RATCHET_RC=$?
}

# ─────────────────────────────────────────────────────────────────────────────
# T30: PR edits existing seeded entry (changes delta+reason) -> FAIL (append-only)
# P1 reproduction: old code would see the edited entry as "new" and PASS this.
# New code must detect the seeded entry was removed and FAIL.
# ─────────────────────────────────────────────────────────────────────────────
log "T30: editing existing trajectory entry is detected as removal -> rc nonzero (append-only)"
# HEAD trajectory: seeded entry is EDITED (delta 3->5, reason changed) with no new appended entry.
# A correctly-filling PR should have APPENDED a new entry; this PR REPLACED the existing one.
make_traj_edited_entry "$TRAJ_TMP" 13 5 "edited to match computed delta"
run_exc_edit "$TRAJ_TMP" "loc-exception: editing prior entry to match computed delta"

[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T30: expected nonzero rc (append-only violation: seeded entry was edited), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "append.only|removed|modified|existing entr" \
    || fail "T30: expected append-only violation message, got: $RATCHET_OUT"
pass "T30: editing an existing trajectory entry is rejected (append-only)"

# ─────────────────────────────────────────────────────────────────────────────
# T31: pr-backfill allowance: setting pr: null->439 on existing entry plus a new
#      appended entry for this PR's positive delta -> PASS
# Identity excludes pr: field, so changing pr: null->439 on an existing entry
# does NOT count as a removal. The new entry is the second (legitimately new) one.
# ─────────────────────────────────────────────────────────────────────────────
log "T31: pr-backfill (pr: null->439 on existing entry) + new appended entry -> rc=0"
# MB trajectory: seeded entry has pr: null (exc-edit-repo's base has pr: 437 actually)
# We need a separate repo for T31 where the MB entry has pr: null.
T31_REPO="$TMP/t31-repo"
mkdir -p "$T31_REPO"
(cd "$T31_REPO"
    git init -q
    git config user.email "test@example.com"
    git config user.name "Test User"
    mkdir -p hooks scripts/lib scripts/ci

    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n' > hooks/check.sh
    printf 'line1\nline2\nline3\nline4\nline5\n' > scripts/lib/util.sh

    # MB trajectory: one seeded entry with pr: null
    cat > scripts/ci/loc-ratchet-trajectory.yaml <<'TRAJ'
baseline:
  commit: abc1234567890000000000000000000000000000
  executable_loc: 13
  note: "fixture baseline for testing"
entries:
  - date: 2026-05-01
    pr: null
    branch: seeded-branch
    delta: 3
    reason: "prior seeded entry"
TRAJ

    git add -A
    git commit -q -m "base commit (seeded entry with pr: null)"
    T31_BASE=$(git rev-parse --abbrev-ref HEAD)
    echo "$T31_BASE" > /tmp/t31-base.txt

    git checkout -q -b t31-feature
    # Add +5 lines to hooks/check.sh
    printf 'line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nL9\nL10\nL11\nL12\nL13\n' > hooks/check.sh
    git add hooks/check.sh
    git commit -q -m "add 5 lines to hooks/check.sh (delta=+5)"
)
T31_BASE=$(cat /tmp/t31-base.txt)
T31_TRAJ="$T31_REPO/scripts/ci/loc-ratchet-trajectory.yaml"

# HEAD trajectory: seeded entry gets pr: null->439 backfill, PLUS a new entry for this PR's delta=5
make_traj_pr_backfill "$TRAJ_TMP" 13 5
cp "$TRAJ_TMP" "$T31_TRAJ"
(cd "$T31_REPO" && git add scripts/ci/loc-ratchet-trajectory.yaml \
    && git commit -q --allow-empty -m "backfill pr: + new entry")

RATCHET_OUT=$(
    cd "$T31_REPO"
    LOC_RATCHET_MANIFEST="$MANIFEST" \
    LOC_RATCHET_TRAJECTORY="$T31_TRAJ" \
    PR_BODY="loc-exception: this PR new entry" \
        bash "${RATCHET_SCRIPT}" --base "$T31_BASE" 2>&1
) && RATCHET_RC=0 || RATCHET_RC=$?

[[ "$RATCHET_RC" -eq 0 ]] \
    || fail "T31: expected rc=0 (pr: backfill is exempt from append-only check), got $RATCHET_RC. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -q "PASS (exception declared)" \
    || fail "T31: expected 'PASS (exception declared)', got: $RATCHET_OUT"
pass "T31: pr: backfill on existing entry is exempt from append-only check"

# ─────────────────────────────────────────────────────────────────────────────
# T32: new entry missing branch: field -> FAIL naming the missing field
# AC: identity-field presence check on the newly appended entry
# ─────────────────────────────────────────────────────────────────────────────
log "T32: new entry missing branch: field -> rc nonzero naming 'branch'"
make_traj_missing_branch "$TRAJ_TMP" 13 5
run_exc "$TRAJ_TMP" "loc-exception: forgot branch field"

[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T32: expected nonzero rc (missing branch: field), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "branch|missing|required" \
    || fail "T32: expected message naming 'branch' field, got: $RATCHET_OUT"
pass "T32: missing branch: field in new entry fails with field-naming message"

# Sub-case: empty date field
log "T32b: new entry with empty date: field -> rc nonzero naming 'date'"
make_traj_empty_date "$TRAJ_TMP" 13 5
run_exc "$TRAJ_TMP" "loc-exception: forgot date field"

[[ "$RATCHET_RC" -ne 0 ]] \
    || fail "T32b: expected nonzero rc (empty date: field), got 0. Output: $RATCHET_OUT"
echo "$RATCHET_OUT" | grep -qiE "date|missing|required" \
    || fail "T32b: expected message naming 'date' field, got: $RATCHET_OUT"
pass "T32b: empty date: field in new entry fails with field-naming message"

# ─────────────────────────────────────────────────────────────────────────────
# T33: corrected projection - post-merge cumulative IS the live cumulative
# (not cumulative+delta which double-counts this PR's own lines)
# exc-repo: live LOC = 18 (hooks/check.sh=13 + scripts/lib/util.sh=5), baseline=13 -> cumulative=+5
# delta=+5 (the feature branch added 5 lines to hooks/check.sh)
# OLD (wrong): CUMULATIVE_AFTER = CUMULATIVE + DELTA = 5 + 5 = 10
# NEW (correct): post-merge cumulative = CUMULATIVE = 5 (already includes this PR's lines)
# ─────────────────────────────────────────────────────────────────────────────
log "T33: post-merge cumulative equals live cumulative (not cumulative+delta double-count)"
T33_SUMMARY="$TMP/t33-step-summary.md"
: > "$T33_SUMMARY"
make_traj_one_entry "$TRAJ_TMP" 13 5 "wedge projection test"
run_exc "$TRAJ_TMP" "loc-exception: wedge projection test"
# Re-run with GITHUB_STEP_SUMMARY to capture the projected cumulative
RATCHET_OUT=$(
    cd "$EXCL_REPO"
    LOC_RATCHET_MANIFEST="$MANIFEST" \
    LOC_RATCHET_TRAJECTORY="$EXC_TRAJ" \
    PR_BODY="loc-exception: wedge projection test" \
    GITHUB_STEP_SUMMARY="$T33_SUMMARY" \
        bash "${RATCHET_SCRIPT}" --base "$EXC_BASE" 2>&1
) && RATCHET_RC=0 || RATCHET_RC=$?

[[ "$RATCHET_RC" -eq 0 ]] \
    || fail "T33: expected rc=0, got $RATCHET_RC. Output: $RATCHET_OUT"
# The live cumulative at HEAD in exc-repo: hooks/check.sh=13 + util.sh=5 = 18, baseline=13 -> cumulative=+5
# Post-merge cumulative must be 5 (not 10). Grep for "5" in the projection line.
# Specifically assert "10" does NOT appear as the projection (the double-count value).
echo "$RATCHET_OUT" | grep -iE "post.merge|cumulative after" \
    | grep -qE '\b10\b' \
    && fail "T33: post-merge cumulative shows 10 (double-count bug). Output: $RATCHET_OUT" || true
# Assert the summary also doesn't show 10 as the projection
grep -iE "post.merge|projection|cumulative" "$T33_SUMMARY" \
    | grep -qE '\b10\b' \
    && fail "T33: step summary shows 10 as projection (double-count bug). Summary: $(cat "$T33_SUMMARY")" || true
# Assert cumulative=5 IS mentioned in the output
echo "$RATCHET_OUT" | grep -qiE "cumulative.*\b5\b|\b5\b.*cumulative" \
    || fail "T33: expected cumulative=5 in output. Output: $RATCHET_OUT"
pass "T33: post-merge cumulative equals live cumulative (no double-count)"

# ─────────────────────────────────────────────────────────────────────────────
# Shellcheck (optional - skip if not on PATH)
# ─────────────────────────────────────────────────────────────────────────────
if command -v shellcheck &>/dev/null; then
    log "shellcheck: loc-ratchet.sh"
    shellcheck -S warning "${RATCHET_SCRIPT}" \
        || fail "shellcheck found issues in loc-ratchet.sh"
    pass "shellcheck: loc-ratchet.sh clean"

    log "shellcheck: test_loc_ratchet.sh"
    shellcheck -S warning "${SCRIPT_DIR}/test_loc_ratchet.sh" \
        || fail "shellcheck found issues in test_loc_ratchet.sh"
    pass "shellcheck: test_loc_ratchet.sh clean"
else
    log "shellcheck not on PATH, skipping (not a failure)"
fi

log "ALL SCENARIOS PASSED"
exit 0
