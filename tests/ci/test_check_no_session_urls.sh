#!/usr/bin/env bash
# tests/ci/test_check_no_session_urls.sh
#
# Exercises scripts/ci/check-no-session-urls.sh (finding A1): the session-URL
# gate for a PR's commit messages, title, and body. Uses a scratch git repo so
# the commit-range scan is real (not mocked) and diff-scoping is provable.
#
# Run: bash tests/ci/test_check_no_session_urls.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GATE="${REPO_ROOT_REAL}/scripts/ci/check-no-session-urls.sh"

log()  { printf '[session-url] %s\n' "$*"; }
fail() { printf '[session-url] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[session-url] PASS: %s\n' "$*"; }

[[ -f "$GATE" ]] || fail "gate not found at $GATE"

WORK=$(mktemp -d -t session-url-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

REPO="$WORK/repo"
mkdir -p "$REPO"
cd "$REPO"
git init -q
git config user.email t@t.t
git config user.name t
export GIT_AUTHOR_DATE="2026-01-01T00:00:00" GIT_COMMITTER_DATE="2026-01-01T00:00:00"

LEAK_URL="https://claude.ai/code/session_deadbeef"

# NOTE: the maintainer's global commit-msg hook strips a literal
# `Claude-Session:` trailer at commit time, so this test seeds the leak as an
# INLINE mention. The gate matches the `claude.ai/code` substring regardless of
# whether it arrives as a trailer or inline, so the scan path is identical.

# base commit (this stands in for origin/main); carries the leak inline so we
# also prove the scan does NOT reach commits at/behind base (AC1-EDGE).
echo a > a.txt; git add a.txt
git commit -q -m "base commit (ref $LEAK_URL)"
BASE=$(git rev-parse HEAD)

run_gate() { PR_BASE_SHA="$1" PR_HEAD_SHA="$2" PR_TITLE="$3" PR_BODY="$4" bash "$GATE" 2>&1; }

# --- AC1-HP: clean commit + empty body passes --------------------------------
log "AC1-HP: clean PR passes"
echo b > b.txt; git add b.txt; git commit -q -m "feat: clean commit"
HEAD_CLEAN=$(git rev-parse HEAD)
OUT=$(run_gate "$BASE" "$HEAD_CLEAN" "clean title" ""); RC=$?
(( RC == 0 )) || fail "AC1-HP: expected exit 0, got $RC ($OUT)"
echo "$OUT" | grep -q "no violations found" || fail "AC1-HP: missing clean message ($OUT)"
pass "AC1-HP: clean PR exits 0"

# --- AC1-EDGE: a session trailer on the BASE commit does NOT fire -------------
# The base commit carries a trailer; a clean range base..head must still pass.
log "AC1-EDGE: history at/behind base is not scanned"
echo "$OUT" | grep -q "$LEAK_URL" && fail "AC1-EDGE: scan reached the base commit's trailer"
pass "AC1-EDGE: diff-scoped range excludes base-and-older commits"

# --- AC1-ERR: a leaky commit in-range fails, naming the SHA + line ------------
log "AC1-ERR: leaky commit fails with SHA + line"
echo c > c.txt; git add c.txt
git commit -q -m "feat: work (see $LEAK_URL)"
HEAD_LEAK=$(git rev-parse HEAD)
SHORT=${HEAD_LEAK:0:12}
OUT=$(run_gate "$BASE" "$HEAD_LEAK" "t" ""); RC=$?
(( RC == 1 )) || fail "AC1-ERR: expected exit 1, got $RC ($OUT)"
echo "$OUT" | grep -q "\[commit ${SHORT}\]" || fail "AC1-ERR: report did not name commit $SHORT ($OUT)"
echo "$OUT" | grep -q "$LEAK_URL" || fail "AC1-ERR: report did not quote the matching line"
pass "AC1-ERR: leaky commit fails, names SHA and line"

# --- AC1-UI: a leaky body fails, naming 'PR body' + remediation --------------
log "AC1-UI: leaky PR body fails with field named"
OUT=$(run_gate "$BASE" "$HEAD_CLEAN" "t" "please review $LEAK_URL"); RC=$?
(( RC == 1 )) || fail "AC1-UI: expected exit 1, got $RC"
echo "$OUT" | grep -q "\[PR body\]" || fail "AC1-UI: did not name 'PR body' ($OUT)"
echo "$OUT" | grep -q "edit the field" || fail "AC1-UI: missing 'edit the field' remediation"
pass "AC1-UI: leaky body fails, names field + remediation"

# --- AC1-FR: unfetched base fails loud (not a vacuous green) ------------------
log "AC1-FR: unfetched base fails loud"
MISSING="deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
OUT=$(run_gate "$MISSING" "$HEAD_CLEAN" "t" ""); RC=$?
(( RC == 1 )) || fail "AC1-FR: expected exit 1, got $RC ($OUT)"
echo "$OUT" | grep -q "not present in this checkout" || fail "AC1-FR: missing loud infra error ($OUT)"
echo "$OUT" | grep -q "no violations found" && fail "AC1-FR: vacuously passed on unfetched base"
pass "AC1-FR: unfetched base fails loud"

# --- empty base is an infra error too ----------------------------------------
log "empty base fails loud"
OUT=$(run_gate "" "$HEAD_CLEAN" "t" ""); RC=$?
(( RC == 1 )) || fail "empty base: expected exit 1, got $RC"
echo "$OUT" | grep -q "PR_BASE_SHA is empty" || fail "empty base: missing loud error ($OUT)"
pass "empty base: fails loud"

echo "[session-url] all check-no-session-urls tests passed"
exit 0
