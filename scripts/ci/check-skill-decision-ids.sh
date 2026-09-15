#!/usr/bin/env bash
# scripts/ci/check-skill-decision-ids.sh
#
# Shipped skill text must not cite maintainer-local decision ids (d-xxxxxxxx).
# The law content lives in the maintainer's ~/.fno decision store; a public
# clone carries the pointer without the content, so the citation cannot be
# resolved and the instruction's authority cannot be checked. Inline the rule
# instead - the same failure one level up as .claude/rules/oss-fix-not-memory.md:
# the pointer must not ship without what it points at.
#
# The scan is a strict shape match: `d-` followed by 8 lowercase hex, in any
# tracked file under skills/. Case-sensitive on purpose: uppercase format
# illustrations (d-ABCD1234) and shell-default tokens (ID-deadbeef) are not
# citations. No marker hatch: a citation-shaped string under skills/ is a bug
# by this gate's terms; if a real need ever appears, change the gate, not the
# tree it reads.
#
# Two controls, because an absence-only pass has two explanations ("clean"
# and "the instrument never matched anything"):
#   surface  skills/ must resolve to tracked files. A pathspec that matches
#            nothing fails silently, and the scan would read a vacuous zero.
#   tool     --self-test runs the same git grep against a scratch repo whose
#            canary file carries d-cafebabe, then against a clean fixture,
#            so a broken pattern cannot read as a clean tree.
#
# Exit 0 clean; 1 on any hit, on the surface control, or on a failed
# self-test.

set -uo pipefail

PATTERN='d-[0-9a-f]{8}'
SURFACE='skills/'

self_test() {
  local tmp out rc
  tmp=$(mktemp -d) || return 1
  trap 'rm -rf "$tmp"' RETURN
  git init -q "$tmp"
  printf 'law d-cafebabe says inline the rule\n' > "$tmp/canary.md"
  (cd "$tmp" && git add canary.md)
  out=$(cd "$tmp" && git grep -nE "$PATTERN" -- .)
  rc=$?
  if [ "$rc" -ne 0 ] || ! printf '%s\n' "$out" | grep -q 'canary.md.*d-cafebabe'; then
    echo "self-test failed: the canary id was not detected (rc=$rc)" >&2
    return 1
  fi
  printf 'no ids on this line\n' > "$tmp/canary.md"
  (cd "$tmp" && git add canary.md)
  if (cd "$tmp" && git grep -nE "$PATTERN" -- .); then
    echo "self-test failed: the clean fixture matched" >&2
    return 1
  fi
  return 0
}

if [ "${1:-}" = "--self-test" ]; then
  if self_test; then
    echo "self-test ok: the pattern detects a canary id and passes a clean fixture"
    exit 0
  fi
  exit 1
fi

# Surface control: a glob that matches nothing must fail loud, not pass.
if [ -z "$(git ls-files -- "$SURFACE")" ]; then
  echo "surface control failed: no tracked files under $SURFACE" >&2
  exit 1
fi

# git grep over tracked files: case-sensitive by default, ignores untracked
# scratch files, and gives clean exit semantics (0 match, 1 none, 2 error).
hits=$(git grep -nE "$PATTERN" -- "$SURFACE")
rc=$?
if [ "$rc" -eq 0 ]; then
  echo "FAIL: shipped skill text cites maintainer-local decision ids (d-xxxxxxxx):" >&2
  echo "the law content lives in a private decision store a public clone cannot read;" >&2
  echo "inline the rule instead (see .claude/rules/oss-fix-not-memory.md)" >&2
  printf '%s\n' "$hits" >&2
  exit 1
elif [ "$rc" -eq 1 ]; then
  echo "clean: no decision-id citations under $SURFACE"
  exit 0
else
  echo "scan failed (git grep exit $rc)" >&2
  exit 1
fi
