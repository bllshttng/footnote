#!/usr/bin/env bash
# tests/ci/test_draft_release_notes.sh
#
# Table test for scripts/release/draft-release-notes.py: bucket math,
# title cleaning, the retired-command override, and the written draft's
# shape. Runs against a throwaway repo under mktemp -d; the real checkout
# is only read. Needs git and python3 on PATH; gh is never called (the PR
# list is injected with --pr-file).
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

root="$(pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fails=0
check() {
  if [ "$2" = "0" ]; then
    echo "  ok: $1"
  else
    echo "  FAIL: $1"
    fails=$((fails + 1))
  fi
}
check_rc() { # check_rc <desc> <actual> <expected>
  if [ "$2" -eq "$3" ]; then
    echo "  ok: $1"
  else
    echo "  FAIL: $1 (got exit $2, want $3)"
    fails=$((fails + 1))
  fi
}

# ------------------------------------------------------------- unit asserts
python3 scripts/release/draft-release-notes.py --self-test >/dev/null 2>&1
check_rc "self-test passes" "$?" "0"

# ------------------------------------------------------------- e2e draft
rv="$work/repo"
git init -q -b main "$rv"
git -C "$rv" config user.email t@example.com
git -C "$rv" config user.name t
git -C "$rv" remote add origin https://github.com/test-owner/test-repo.git
: > "$rv/placeholder" && git -C "$rv" add placeholder
# The tag's creatordate is the draft's since-boundary: pin the commit date
# so the fixture PR dates sit firmly on both sides of it.
GIT_COMMITTER_DATE="2026-01-01T00:00:00Z" git -C "$rv" commit -qm init
git -C "$rv" tag v0.4.0
mkdir -p "$rv/scripts/ci"
printf '# fixture registry\nfno dispatch|fno agents spawn --node <id>|fixture\n' > "$rv/scripts/ci/retired-commands.txt"
mkdir -p "$rv/scripts/release"
cp scripts/release/draft-release-notes.py "$rv/scripts/release/"

cat > "$work/prs.json" <<'EOF'
[
  {"number": 101, "title": "feat(mux): portals open operator windows", "mergedAt": "2026-09-25T10:00:00Z"},
  {"number": 102, "title": "fix: wheel smoke flake on clean machines", "mergedAt": "2026-09-25T11:00:00Z"},
  {"number": 103, "title": "docs: rewrite the install page", "mergedAt": "2026-09-25T12:00:00Z"},
  {"number": 104, "title": "fno dispatch retires in favor of spawn", "mergedAt": "2026-09-25T13:00:00Z"},
  {"number": 105, "title": "chore: tidy the lockfile", "mergedAt": "2026-09-25T14:00:00Z"},
  {"number": 106, "title": "unprefixed board fix", "mergedAt": "2026-09-25T15:00:00Z"},
  {"number": 107, "title": "feat(old): predates the base tag", "mergedAt": "2020-01-01T00:00:00Z"}
]
EOF

run_draft() { (cd "$rv" && python3 scripts/release/draft-release-notes.py "$@"); }

mkdir -p "$work/out"
run_draft v0.4.1rc1 --since v0.4.0 --pr-file "$work/prs.json" --out "$work/out" >"$work/o" 2>"$work/e"
check_rc "draft exits 0" "$?" "0"
grep -q "wrote.*v0.4.1rc1.md" "$work/o"
check "success line names the draft" $?

notes="$work/out/v0.4.1rc1.md"
grep -q "^6 merged pull requests since v0.4.0\.$" "$notes"
check "count line says 6 (the 2020 PR is filtered)" $?
grep -q "### Features (1 PRs)" "$notes"
check "Features section lands with its count" $?
grep -q "Portals open operator windows (#101)" "$notes"
check "conventional prefix stripped from the bullet" $?
grep -q "### Fixes (1 PRs)" "$notes"
check "Fixes section lands" $?
grep -q "### Before you upgrade" "$notes"
check "retired-command title buckets to Before you upgrade" $?
grep -q "Fno dispatch retires in favor of spawn (#104)" "$notes"
check "the retired-command bullet keeps its text" $?
grep -q "test-owner/test-repo/compare/v0.4.0...v0.4.1rc1" "$notes"
check "Full Changelog link carries the slug and range" $?
grep -q "<summary>Internal (1 PRs)</summary>" "$notes"
check "internal work hides in the collapsed block" $?

# Refusals: an existing draft needs --force; a missing out dir refuses to
# invent a home.
run_draft v0.4.1rc1 --since v0.4.0 --pr-file "$work/prs.json" --out "$work/out" >"$work/o" 2>"$work/e"
check_rc "rerun without --force refuses (exit 1)" "$?" "1"
grep -q "pass --force" "$work/e"
check "the refusal names --force" $?
run_draft v0.4.2rc1 --since v0.4.0 --pr-file "$work/prs.json" --out "$work/nope" >"$work/o" 2>"$work/e"
check_rc "missing out dir refuses (exit 1)" "$?" "1"
grep -q "vault checkout" "$work/e"
check "the refusal names the vault checkout" $?

if [ "$fails" -eq 0 ]; then
  echo "test_draft_release_notes: ALL PASS"
fi
exit "$fails"
