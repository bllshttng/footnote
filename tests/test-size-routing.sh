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
  local points="${1:-0}" domain="${2:-code}"
  local base="M"

  # 1. Use estimated points
  if (( points >= 9 )); then base="L"
  elif (( points >= 4 )); then base="M"
  elif (( points >= 1 )); then base="S"
  fi

  # 2. Apply domain modifier
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
result=$(route_size 2 "code")
assert_size "points=2, domain=code" "S" "$result"

# Test 2: Medium points -> M
echo "Test 2: Medium points (6) -> M"
result=$(route_size 6 "code")
assert_size "points=6, domain=code" "M" "$result"

# Test 3: High points -> L
echo "Test 3: High points (10) -> L"
result=$(route_size 10 "code")
assert_size "points=10, domain=code" "L" "$result"

# Test 4: No attributes -> default M
echo "Test 4: No attributes -> M"
result=$(route_size 0 "code")
assert_size "no points" "M" "$result"

# Test 5: Security domain modifier (+1)
echo "Test 5: Security domain (5 pts) -> L (M + security = L)"
result=$(route_size 5 "security")
assert_size "points=5, domain=security" "L" "$result"

# Test 6: Docs domain modifier (-1)
echo "Test 6: Docs domain (6 pts) -> S (M - docs = S)"
result=$(route_size 6 "docs")
assert_size "points=6, domain=docs" "S" "$result"

# Test 10: Security + low points = M (S bumped to M)
echo "Test 10: Security domain (2 pts) -> M (S + security = M)"
result=$(route_size 2 "security")
assert_size "points=2, domain=security" "M" "$result"

# Test 11: L stays L with security modifier
echo "Test 11: Security domain (10 pts) -> L (L + security = L, capped)"
result=$(route_size 10 "security")
assert_size "points=10, domain=security" "L" "$result"

# Test 12: S stays S with docs modifier
echo "Test 12: Docs domain (2 pts) -> S (S - docs = S, floor)"
result=$(route_size 2 "docs")
assert_size "points=2, domain=docs" "S" "$result"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
exit "$FAIL"
