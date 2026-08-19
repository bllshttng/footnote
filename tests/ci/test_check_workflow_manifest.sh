#!/usr/bin/env bash
# tests/ci/test_check_workflow_manifest.sh
#
# Test harness for scripts/ci/check-workflow-manifest.sh.
#
# Scenarios:
#   T01 - manifest matches the live directory  -> PASS, rc=0
#   T02 - a manifest entry has no file on disk -> FAIL, names the missing file, rc=1
#   T03 - a file on disk is not in the manifest -> FAIL, names the unlisted file, rc=1
#   T04 - manifest file itself is missing      -> misuse, rc=2
#   T05 - two missing AND two unlisted at once -> FAIL, names all four (every
#         entry indented, not just the first - a scalar-vs-array printf bug
#         here once let only the first of several names print indented)
#
# Exit codes: 0 pass, 1 fail, 77 skip (missing deps)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CHECK_SCRIPT="${REPO_ROOT}/scripts/ci/check-workflow-manifest.sh"

fail() { printf '[workflow-manifest-test] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[workflow-manifest-test] PASS: %s\n' "$*"; }

[[ -f "${CHECK_SCRIPT}" ]] || fail "script not found at ${CHECK_SCRIPT}"
bash -n "${CHECK_SCRIPT}" || fail "script failed bash -n"

TMP=$(mktemp -d -t workflow-manifest-test-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/repo/.github/workflows" "$TMP/repo/scripts/ci"
(cd "$TMP/repo" && git init -q)

# T01: matching set.
touch "$TMP/repo/.github/workflows/a.yml" "$TMP/repo/.github/workflows/b.yml"
printf 'a.yml\nb.yml\n' > "$TMP/repo/scripts/ci/workflow-manifest.txt"
out="$(cd "$TMP/repo" && bash "${CHECK_SCRIPT}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] || fail "T01: expected rc=0, got $rc: $out"
pass "T01 matching manifest exits 0"

# T02: manifest names a file that does not exist.
printf 'a.yml\nb.yml\nc.yml\n' > "$TMP/repo/scripts/ci/workflow-manifest.txt"
out="$(cd "$TMP/repo" && bash "${CHECK_SCRIPT}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 1 ]] || fail "T02: expected rc=1, got $rc"
echo "$out" | grep -q "c.yml" || fail "T02: missing file not named: $out"
pass "T02 a manifest entry with no file fails, naming it"

# T03: a file on disk is not in the manifest.
printf 'a.yml\nb.yml\n' > "$TMP/repo/scripts/ci/workflow-manifest.txt"
touch "$TMP/repo/.github/workflows/d.yml"
out="$(cd "$TMP/repo" && bash "${CHECK_SCRIPT}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 1 ]] || fail "T03: expected rc=1, got $rc"
echo "$out" | grep -q "d.yml" || fail "T03: unlisted file not named: $out"
pass "T03 an unlisted workflow file fails, naming it"
rm "$TMP/repo/.github/workflows/d.yml"

# T04: manifest missing entirely.
rm "$TMP/repo/scripts/ci/workflow-manifest.txt"
out="$(cd "$TMP/repo" && bash "${CHECK_SCRIPT}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 2 ]] || fail "T04: expected rc=2 (misuse), got $rc: $out"
pass "T04 a missing manifest is misuse (rc=2), not a silent pass"

# T05: two missing and two unlisted, together - the multi-entry case a plain
# scalar printf can silently mis-indent past the first line.
printf 'a.yml\nb.yml\nc.yml\nd.yml\n' > "$TMP/repo/scripts/ci/workflow-manifest.txt"
touch "$TMP/repo/.github/workflows/e.yml" "$TMP/repo/.github/workflows/f.yml"
out="$(cd "$TMP/repo" && bash "${CHECK_SCRIPT}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 1 ]] || fail "T05: expected rc=1, got $rc"
for name in c.yml d.yml e.yml f.yml; do
  echo "$out" | grep -q "  ${name}$" || fail "T05: '$name' not printed indented: $out"
done
pass "T05 multiple missing and unlisted files are all named, each indented"
rm "$TMP/repo/.github/workflows/e.yml" "$TMP/repo/.github/workflows/f.yml"

pass "all scenarios"
