#!/usr/bin/env bash
# test-setup-worktree-canonical-guard.sh - the same-root refusal in
# scripts/setup/setup-worktree.sh - guards against the canonical-to-canonical
# self-destruction where link_artifact `rm -f`s a real file and replaces it
# with a symlink pointing at itself.
#
# The guard is load-bearing against real data loss, not untidiness:
# link_artifact `rm -f`s the real file before `ln -sf "$source" "$target"`,
# so with CANONICAL == WORKTREE it deletes the file and replaces it with a
# symlink pointing at itself. That is how this repo's .fno/codemap.md was
# lost on 2026-07-26. The script had no coverage until this file.
#
# Tests:
#  1. Same root: exits 0, names the root, and .fno/ is untouched (still a real
#     file with its original bytes, not a symlink).
#  2. Same root reached through a symlinked path: still refuses. `-ef` compares
#     device+inode, which a string equality test on the two roots would miss.
#  3. Distinct roots: links as it does today, so the guard changes nothing for
#     the real invocation.
#
# Bash 3.2 compatible; hermetic mktemp sandboxes.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$REPO_ROOT/scripts/setup/setup-worktree.sh"

pass=0
fail=0

check_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (expected='$expected' actual='$actual')"
    fail=$((fail+1))
  fi
}

check_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -qF "$needle"; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (needle='$needle' not found in: $haystack)"
    fail=$((fail+1))
  fi
}

# A canonical checkout carrying the two artifact shapes the script links:
# a regenerable one (codemap.md, which link_artifact rm -f's) and a user-data
# one (config.toml, which link_file skips).
make_canonical() {
  local root="$1"
  mkdir -p "$root/.fno" "$root/.claude"
  printf 'CODEMAP-ORIGINAL\n' > "$root/.fno/codemap.md"
  printf 'CONFIG-ORIGINAL\n' > "$root/.fno/config.toml"
}

# ---------------------------------------------------------------------------
# T1: same root - refuse, exit 0, name the root, touch nothing
# ---------------------------------------------------------------------------
SBX1="$(mktemp -d)"
make_canonical "$SBX1"
OUT1="$(CANONICAL="$SBX1" WORKTREE="$SBX1" bash "$SETUP" 2>&1)"
RC1=$?

check_eq "T1: exits 0 (a no-op is a success, and target start treats non-zero as fatal)" "0" "$RC1"
check_contains "T1: refusal names the root" "$SBX1" "$OUT1"
check_contains "T1: refusal says what it declined" "canonical -> canonical" "$OUT1"
check_eq "T1: codemap.md survives with its original bytes" \
  "CODEMAP-ORIGINAL" "$(cat "$SBX1/.fno/codemap.md" 2>/dev/null)"
if [ -L "$SBX1/.fno/codemap.md" ]; then
  echo "FAIL: T1: codemap.md became a symlink (the 2026-07-26 self-link bug)"
  fail=$((fail+1))
else
  echo "PASS: T1: codemap.md is still a real file, not a self-referential symlink"
  pass=$((pass+1))
fi
check_eq "T1: config.toml survives" \
  "CONFIG-ORIGINAL" "$(cat "$SBX1/.fno/config.toml" 2>/dev/null)"

# ---------------------------------------------------------------------------
# T2: same root via a symlinked path - inode comparison, not string equality
# ---------------------------------------------------------------------------
SBX2="$(mktemp -d)"
make_canonical "$SBX2/real"
ln -s "$SBX2/real" "$SBX2/alias"
OUT2="$(CANONICAL="$SBX2/real" WORKTREE="$SBX2/alias" bash "$SETUP" 2>&1)"
RC2=$?

check_eq "T2: exits 0 through the alias" "0" "$RC2"
check_contains "T2: refuses despite the two roots spelling differently" \
  "canonical -> canonical" "$OUT2"
check_eq "T2: codemap.md survives the aliased invocation" \
  "CODEMAP-ORIGINAL" "$(cat "$SBX2/real/.fno/codemap.md" 2>/dev/null)"

# ---------------------------------------------------------------------------
# T3: distinct roots - the guard changes nothing for the real invocation
# ---------------------------------------------------------------------------
SBX3="$(mktemp -d)"
make_canonical "$SBX3/canonical"
mkdir -p "$SBX3/worktree"
OUT3="$(CANONICAL="$SBX3/canonical" WORKTREE="$SBX3/worktree" bash "$SETUP" 2>&1)"
RC3=$?

check_eq "T3: exits 0 on a genuine worktree" "0" "$RC3"
if [ -L "$SBX3/worktree/.fno/codemap.md" ]; then
  echo "PASS: T3: codemap.md was linked into the worktree"
  pass=$((pass+1))
else
  echo "FAIL: T3: codemap.md was NOT linked (guard over-fired): $OUT3"
  fail=$((fail+1))
fi
check_eq "T3: the link reads through to canonical" \
  "CODEMAP-ORIGINAL" "$(cat "$SBX3/worktree/.fno/codemap.md" 2>/dev/null)"
check_eq "T3: canonical is untouched" \
  "CODEMAP-ORIGINAL" "$(cat "$SBX3/canonical/.fno/codemap.md" 2>/dev/null)"

rm -rf "$SBX1" "$SBX2" "$SBX3"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "Results: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
