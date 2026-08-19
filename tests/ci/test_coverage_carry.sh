#!/usr/bin/env bash
# tests/ci/test_coverage_carry.sh
#
# Test harness for scripts/ci/coverage-carry.sh: a PATH-shadowing `gh` stub
# dispatching on argv, and the real `git patch-id`.
#
# Cases (one per acceptance bullet in the plan):
#   T01 - matching identities on a publisher verdict -> carry, single marker
#         (also the POSITIVE CONTROL: the carry case must PASS, or a stub
#         that silently refuses everything would make this whole file green
#         without ever exercising the carry path)
#   T02 - coverage-override verdict -> no-carry, refused by name, no
#         identity computed
#   T03 - differing diffs -> no-carry code-changed
#   T04 - both diffs empty -> no-carry no-diff-identity (never a carry on
#         two absences)
#   T05 - status read fails three times -> no-carry status-read-failed,
#         distinct from an absent status
#
# Exit codes: 0 pass, 1 fail
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CARRY_SCRIPT="${REPO_ROOT}/scripts/ci/coverage-carry.sh"

FAILURES=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

[[ -x "$CARRY_SCRIPT" ]] || { echo "FAIL: coverage-carry.sh not executable at $CARRY_SCRIPT" >&2; exit 1; }
bash -n "$CARRY_SCRIPT" || { echo "FAIL: coverage-carry.sh failed bash -n" >&2; exit 1; }

REPO="owner/repo"
CTX="fno/review-coverage"
BEFORE="1111111111111111111111111111111111111111"
HEAD="2222222222222222222222222222222222222222"
BASE="main"

DIFF_A='diff --git a/foo.txt b/foo.txt
index e69de29..d95f3ad 100644
--- a/foo.txt
+++ b/foo.txt
@@ -0,0 +1 @@
+hello
'
DIFF_B='diff --git a/foo.txt b/foo.txt
index e69de29..d95f3ad 100644
--- a/foo.txt
+++ b/foo.txt
@@ -0,0 +1 @@
+goodbye
'

# make_stub_gh <stub_dir> <status_mode: ok|fail> <before_state> <before_desc> <before_diff> <head_diff>
# Logs every compare call to $stub_dir/compare.log so a test can assert an
# identity was never computed when the publisher-allowlist check should
# have short-circuited before reaching it.
make_stub_gh() {
  local stub_dir="$1" status_mode="$2" before_state="$3" before_desc="$4" before_diff="$5" head_diff="$6"
  mkdir -p "$stub_dir"
  printf '%s' "$before_diff" > "$stub_dir/before.diff"
  printf '%s' "$head_diff" > "$stub_dir/head.diff"
  cat > "$stub_dir/gh" <<STUB
#!/usr/bin/env bash
case "\$*" in
  *"commits/${BEFORE}/status"*".state"*)
    if [ "$status_mode" = "fail" ]; then
      echo "stub gh: simulated status read failure" >&2
      exit 1
    fi
    printf '%s' "$before_state"
    ;;
  *"commits/${BEFORE}/status"*".description"*)
    if [ "$status_mode" = "fail" ]; then
      echo "stub gh: simulated status read failure" >&2
      exit 1
    fi
    printf '%s' "$before_desc"
    ;;
  *"compare/${BASE}...${BEFORE}"*)
    echo "compare:${BEFORE}" >> "$stub_dir/compare.log"
    cat "$stub_dir/before.diff"
    ;;
  *"compare/${BASE}...${HEAD}"*)
    echo "compare:${HEAD}" >> "$stub_dir/compare.log"
    cat "$stub_dir/head.diff"
    ;;
  *)
    echo "stub gh: unmatched args: \$*" >&2
    exit 1
    ;;
esac
STUB
  chmod +x "$stub_dir/gh"
}

run_carry() { # <stub_dir>
  local stub_dir="$1"
  PATH="$stub_dir:$PATH" bash "$CARRY_SCRIPT" \
    --repo "$REPO" --context "$CTX" --before "$BEFORE" --head "$HEAD" --base "$BASE"
}

t01() {
  local stub_dir out rc marker_count
  stub_dir="$(mktemp -d -t coverage-carry-test-XXXXXX)"
  make_stub_gh "$stub_dir" ok success "covered: 2 reviewed at ${BEFORE:0:8} [carried from deadbeef]" "$DIFF_A" "$DIFF_A"
  out="$(run_carry "$stub_dir")"; rc=$?
  if [[ "$rc" -ne 0 ]]; then fail "T01: expected rc=0, got $rc: $out"; rm -rf "$stub_dir"; return; fi
  if [[ "$out" != carry\ * ]]; then fail "T01: expected a carry line, got: $out"; rm -rf "$stub_dir"; return; fi
  [[ "$out" == *"$BEFORE"* ]] || fail "T01: carry line does not name the before-sha: $out"
  marker_count="$(grep -o '\[carried from' <<<"$out" | wc -l | tr -d ' ')"
  [[ "$marker_count" -eq 1 ]] || fail "T01: expected exactly one [carried from marker, got $marker_count: $out"
  pass "T01 identical identities on a publisher verdict -> carry with one marker (positive control)"
  rm -rf "$stub_dir"
}

t02() {
  local stub_dir out rc
  stub_dir="$(mktemp -d -t coverage-carry-test-XXXXXX)"
  make_stub_gh "$stub_dir" ok success "coverage-override label applied by jn" "$DIFF_A" "$DIFF_A"
  out="$(run_carry "$stub_dir")"; rc=$?
  if [[ "$rc" -ne 0 ]]; then fail "T02: expected rc=0, got $rc: $out"; rm -rf "$stub_dir"; return; fi
  [[ "$out" == "no-carry not-a-publisher-verdict:coverage-override" ]] || fail "T02: expected refused-by-name, got: $out"
  [[ ! -f "$stub_dir/compare.log" ]] || fail "T02: identity was computed despite a non-publisher verdict: $(cat "$stub_dir/compare.log")"
  pass "T02 coverage-override verdict refused by name, no identity computed"
  rm -rf "$stub_dir"
}

t03() {
  local stub_dir out rc
  stub_dir="$(mktemp -d -t coverage-carry-test-XXXXXX)"
  make_stub_gh "$stub_dir" ok success "covered: 2 reviewed at ${BEFORE:0:8}" "$DIFF_A" "$DIFF_B"
  out="$(run_carry "$stub_dir")"; rc=$?
  if [[ "$rc" -ne 0 ]]; then fail "T03: expected rc=0, got $rc: $out"; rm -rf "$stub_dir"; return; fi
  [[ "$out" == "no-carry code-changed" ]] || fail "T03: expected no-carry code-changed, got: $out"
  pass "T03 differing diffs -> no-carry code-changed"
  rm -rf "$stub_dir"
}

t04() {
  local stub_dir out rc
  stub_dir="$(mktemp -d -t coverage-carry-test-XXXXXX)"
  make_stub_gh "$stub_dir" ok success "covered: 2 reviewed at ${BEFORE:0:8}" "" ""
  out="$(run_carry "$stub_dir")"; rc=$?
  if [[ "$rc" -ne 0 ]]; then fail "T04: expected rc=0, got $rc: $out"; rm -rf "$stub_dir"; return; fi
  [[ "$out" == "no-carry no-diff-identity" ]] || fail "T04: expected no-carry no-diff-identity, got: $out"
  pass "T04 two empty diffs never match -> no-carry no-diff-identity"
  rm -rf "$stub_dir"
}

t05() {
  local stub_dir out rc
  stub_dir="$(mktemp -d -t coverage-carry-test-XXXXXX)"
  make_stub_gh "$stub_dir" fail success "covered: 2 reviewed at ${BEFORE:0:8}" "$DIFF_A" "$DIFF_A"
  out="$(run_carry "$stub_dir" 2>/dev/null)"; rc=$?
  if [[ "$rc" -ne 0 ]]; then fail "T05: expected rc=0, got $rc: $out"; rm -rf "$stub_dir"; return; fi
  [[ "$out" == "no-carry status-read-failed" ]] || fail "T05: expected no-carry status-read-failed, got: $out"
  pass "T05 an unreadable status is status-read-failed, distinct from an absent status"
  rm -rf "$stub_dir"
}

t01
t02
t03
t04
t05

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
  echo "ALL TESTS PASSED (test_coverage_carry.sh)"
else
  echo "FAILED: $FAILURES test(s) failed" >&2
  exit 1
fi
