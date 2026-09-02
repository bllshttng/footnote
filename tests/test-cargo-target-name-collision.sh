#!/usr/bin/env bash
# Pins the target-name collision behind the worktrees.md cargo note: three
# source dirs are literally named target, so a name-only match is destructive,
# while CACHEDIR.TAG and crates/*/target discriminate. CACHEDIR.TAG is written
# by cargo into every target dir it owns, so its presence is the positive
# marker; a name is not. Measured 2026-09-02: a name-based sweep deleted 66
# source dirs across 26 worktrees, and the same sweep's du counted 84
# name-matched dirs where 18 were cargo, 4.7x inflated - the name match also
# poisons measurement.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

SOURCE_DIRS=("cli/src/fno/target" "skills/target" "tests/target")

echo "== name-only match hits the source dirs (the documented hazard) =="
name_matched="$(find "$ROOT" -type d -name target -not -path '*/.git/*')" || fail "find" "name-only sweep errored; treat any pass below as unproven"
for rel in "${SOURCE_DIRS[@]}"; do
  if grep -qxF "$ROOT/$rel" <<<"$name_matched"; then
    pass "name-only match hits source dir $rel"
  else
    fail "name-only match" "$rel not matched; collision facts changed, revisit the worktrees.md cargo note"
  fi
done

echo "== CACHEDIR.TAG discriminates source from cargo =="
for rel in "${SOURCE_DIRS[@]}"; do
  if [[ -e "$ROOT/$rel/CACHEDIR.TAG" ]]; then
    fail "positive marker" "$rel carries CACHEDIR.TAG; the marker no longer separates source from cargo"
  else
    pass "source dir $rel carries no CACHEDIR.TAG"
  fi
done

echo "== crates/*/target and the marker describe the same cargo dirs =="
glob_dirs="$(for d in "$ROOT"/crates/*/target; do [[ -d "$d" ]] && echo "$d"; done | sort)"
marker_dirs="$(find "$ROOT/crates" -type f -name CACHEDIR.TAG -exec dirname {} \; 2>/dev/null | sort)" || fail "find" "CACHEDIR.TAG sweep errored"
if [[ -z "$glob_dirs" && -z "$marker_dirs" ]]; then
  pass "no cargo build dirs in this checkout (fresh worktree); collision facts above still pinned"
elif [[ "$glob_dirs" == "$marker_dirs" ]]; then
  pass "crates/*/target == CACHEDIR.TAG dirs ($(echo "$glob_dirs" | wc -l | tr -d ' ') cargo dirs, all outside the source dirs)"
else
  fail "cargo dir selection" "glob and marker disagree; the worktrees.md path-shape recipe no longer covers every cargo dir
glob: $glob_dirs
marker: $marker_dirs"
fi
for d in $glob_dirs; do
  safe=true
  for rel in "${SOURCE_DIRS[@]}"; do [[ "$d" == "$ROOT/$rel" ]] && safe=false; done
  if $safe; then pass "path glob excludes source dirs ($d)"; else fail "path glob" "$d matched a source dir"; fi
done

echo
echo "pass=$PASS fail=$FAIL"
if [[ $FAIL -eq 0 ]]; then exit 0; fi
exit 1
