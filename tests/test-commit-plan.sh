#!/usr/bin/env bash
# test-commit-plan.sh - one commit per blueprint write, the plan path only.
# Hermetic mktemp git repos; no real vault is touched.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$REPO_ROOT/skills/blueprint/scripts/commit-plan.sh"

pass=0; fail=0
ok()  { echo "PASS: $1"; pass=$((pass+1)); }
bad() { echo "FAIL: $1"; fail=$((fail+1)); }
check_contains() { printf '%s' "$3" | grep -qF "$2" && ok "$1" || bad "$1 (needle='$2' in: $3)"; }
check_eq() { [ "$2" = "$3" ] && ok "$1" || bad "$1 (want '$2', got '$3')"; }

make_repo() {
  local r; r="$(mktemp -d)"
  git -C "$r" init -q
  git -C "$r" config user.email t@example.com
  git -C "$r" config user.name t
  git -C "$r" commit -q --allow-empty -m root
  echo "$r"
}

# --- T1: two writes -> two commits, each naming the node ---
R1="$(make_repo)"
P1="$R1/plan-x-0001.md"
echo "v1" > "$P1"
OUT="$(bash "$SCRIPT" "$P1" x-0001 "initial blueprint")"
check_contains "T1: first write committed" "committed " "$OUT"
echo "v2" > "$P1"
OUT="$(bash "$SCRIPT" "$P1" x-0001 "review finding 2")"
check_contains "T1: second write committed" "committed " "$OUT"
check_eq "T1: git log holds 2 commits" "2" "$(git -C "$R1" log --format=%H -- "$P1" | wc -l | tr -d ' ')"
check_eq "T1: each message names the node twice (subject + trailer)" "4" \
  "$(git -C "$R1" log --format=%B -- "$P1" | grep -c x-0001)"
check_contains "T1: cause in subject" "blueprint(x-0001): review finding 2" \
  "$(git -C "$R1" log -1 --format=%s -- "$P1")"

# --- T2: other staged work stays staged and out of the commit ---
R2="$(make_repo)"
echo "other" > "$R2/other.md"
git -C "$R2" add other.md
echo "plan" > "$R2/plan.md"
OUT="$(bash "$SCRIPT" "$R2/plan.md" x-1 "initial blueprint")"
check_contains "T2: plan committed" "committed " "$OUT"
check_eq "T2: commit touches only the plan" "plan.md" \
  "$(git -C "$R2" show --name-only --format= HEAD)"
check_eq "T2: other file still staged" "other.md" \
  "$(git -C "$R2" diff --cached --name-only)"

# --- T3: outside any git repo -> unversioned, exit 0 ---
D3="$(mktemp -d)"
echo "plan" > "$D3/plan.md"
OUT="$(GIT_CEILING_DIRECTORIES="$D3" bash "$SCRIPT" "$D3/plan.md" x-1 cause)"; RC=$?
check_contains "T3: unversioned line" "unversioned " "$OUT"
check_eq "T3: exit 0" "0" "$RC"

# --- T4: no change since the last commit -> unchanged, no new commit ---
BEFORE="$(git -C "$R1" rev-parse HEAD)"
OUT="$(bash "$SCRIPT" "$P1" x-0001 "noop")"
check_contains "T4: unchanged line" "unchanged " "$OUT"
check_eq "T4: HEAD did not move" "$BEFORE" "$(git -C "$R1" rev-parse HEAD)"

# --- T5: bad args -> skipped, exit 2 ---
OUT="$(bash "$SCRIPT" "$P1" x-0001)"; RC=$?
check_contains "T5: skipped line" "skipped reason=missing-args" "$OUT"
check_eq "T5: exit 2" "2" "$RC"

rm -rf "$R1" "$R2" "$D3"
echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
