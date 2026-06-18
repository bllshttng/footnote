#!/usr/bin/env bash
set -euo pipefail

# Test size routing: task attributes -> target size flag
# Tests the algorithm described in skills/megawalk/references/size-routing.md

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

PASS=0
FAIL=0

assert_size() {
  local test_name="$1"
  local expected="$2"
  local actual="$3"
  if [[ "$actual" == "$expected" ]]; then
    echo "  PASS: $test_name (got $actual)"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $test_name (expected $expected, got $actual)"
    FAIL=$((FAIL + 1))
  fi
}

# --- Size routing function (from size-routing.md) ---

size_up() {
  case "$1" in S) echo "M" ;; M) echo "L" ;; *) echo "L" ;; esac
}

size_down() {
  case "$1" in L) echo "M" ;; M) echo "S" ;; *) echo "S" ;; esac
}

route_size() {
  local points="${1:-0}" domain="${2:-code}" plan_path="${3:-}"
  local base="M"

  # 1. Check plan phase count
  if [[ -n "$plan_path" && -d "$plan_path" ]]; then
    local phase_count
    phase_count=$(ls "$plan_path"/[0-9][0-9]-*.md 2>/dev/null | grep -cv '00-INDEX.md' 2>/dev/null || echo 0)
    if (( phase_count >= 4 )); then base="L"
    elif (( phase_count >= 2 )); then base="M"
    elif (( phase_count >= 1 )); then base="S"
    fi
  else
    # 2. Use estimated points
    if (( points >= 9 )); then base="L"
    elif (( points >= 4 )); then base="M"
    elif (( points >= 1 )); then base="S"
    fi
  fi

  # 3. Apply domain modifier
  case "$domain" in
    infrastructure|security|migration) base=$(size_up "$base") ;;
    docs) base=$(size_down "$base") ;;
  esac

  echo "$base"
}

# --- Tests ---

echo "=== Size Routing Tests ==="
echo ""

# Test 1: Low points -> S
echo "Test 1: Low points (2) -> S"
result=$(route_size 2 "code" "")
assert_size "points=2, domain=code" "S" "$result"

# Test 2: Medium points -> M
echo "Test 2: Medium points (6) -> M"
result=$(route_size 6 "code" "")
assert_size "points=6, domain=code" "M" "$result"

# Test 3: High points -> L
echo "Test 3: High points (10) -> L"
result=$(route_size 10 "code" "")
assert_size "points=10, domain=code" "L" "$result"

# Test 4: No attributes -> default M
echo "Test 4: No attributes -> M"
result=$(route_size 0 "code" "")
assert_size "no points, no plan" "M" "$result"

# Test 5: Security domain modifier (+1)
echo "Test 5: Security domain (5 pts) -> L (M + security = L)"
result=$(route_size 5 "security" "")
assert_size "points=5, domain=security" "L" "$result"

# Test 6: Docs domain modifier (-1)
echo "Test 6: Docs domain (6 pts) -> S (M - docs = S)"
result=$(route_size 6 "docs" "")
assert_size "points=6, domain=docs" "S" "$result"

# Test 7: Phase count routing (1 phase -> S)
echo "Test 7: Plan with 1 phase -> S"
mkdir -p "$TMP_DIR/plan-1phase"
touch "$TMP_DIR/plan-1phase/00-INDEX.md"
touch "$TMP_DIR/plan-1phase/01-core.md"
result=$(route_size 0 "code" "$TMP_DIR/plan-1phase")
assert_size "1 phase file" "S" "$result"

# Test 8: Phase count routing (4 phases -> L)
echo "Test 8: Plan with 4 phases -> L"
mkdir -p "$TMP_DIR/plan-4phase"
touch "$TMP_DIR/plan-4phase/00-INDEX.md"
touch "$TMP_DIR/plan-4phase/01-setup.md"
touch "$TMP_DIR/plan-4phase/02-core.md"
touch "$TMP_DIR/plan-4phase/03-integration.md"
touch "$TMP_DIR/plan-4phase/04-docs.md"
result=$(route_size 0 "code" "$TMP_DIR/plan-4phase")
assert_size "4 phase files" "L" "$result"

# Test 9: Phase count wins over points when plan exists
echo "Test 9: Plan with 4 phases overrides low points (2 pts)"
result=$(route_size 2 "code" "$TMP_DIR/plan-4phase")
assert_size "points=2 but 4 phases" "L" "$result"

# Test 10: Security + low points = M (S bumped to M)
echo "Test 10: Security domain (2 pts) -> M (S + security = M)"
result=$(route_size 2 "security" "")
assert_size "points=2, domain=security" "M" "$result"

# Test 11: L stays L with security modifier
echo "Test 11: Security domain (10 pts) -> L (L + security = L, capped)"
result=$(route_size 10 "security" "")
assert_size "points=10, domain=security" "L" "$result"

# Test 12: S stays S with docs modifier
echo "Test 12: Docs domain (2 pts) -> S (S - docs = S, floor)"
result=$(route_size 2 "docs" "")
assert_size "points=2, domain=docs" "S" "$result"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
exit "$FAIL"
