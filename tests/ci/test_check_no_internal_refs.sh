#!/usr/bin/env bash
# tests/ci/test_check_no_internal_refs.sh
#
# Exercises scripts/ci/check-no-internal-refs.sh (finding A2): the three-class
# prose gate (internal-path / node-id / session-url) with a by-TOKEN node-id
# allowlist. Uses a scratch git repo with fixture prose so `git ls-files` sees
# real tracked files.
#
# Run: bash tests/ci/test_check_no_internal_refs.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GATE="${REPO_ROOT_REAL}/scripts/ci/check-no-internal-refs.sh"

log()  { printf '[internal-refs] %s\n' "$*"; }
fail() { printf '[internal-refs] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[internal-refs] PASS: %s\n' "$*"; }

[[ -f "$GATE" ]] || fail "gate not found at $GATE"

WORK=$(mktemp -d -t internal-refs-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

REPO="$WORK/repo"
mkdir -p "$REPO/docs"
cd "$REPO"
git init -q
git config user.email t@t.t
git config user.name t

commit_all() { git add -A && git commit -q -m "fixtures"; }
# Run the gate from inside the scratch repo (it resolves REPO_ROOT via
# `git rev-parse --show-toplevel`, so it scans THIS repo's tracked prose).
run_gate() { ( cd "$REPO" && bash "$GATE" ) 2>&1; }

# --- clean tree passes -------------------------------------------------------
log "clean tree passes"
printf '# doc\nnothing to see here.\n' > docs/clean.md
commit_all
OUT=$(run_gate); RC=$?
(( RC == 0 )) || fail "clean: expected 0, got $RC ($OUT)"
echo "$OUT" | grep -q "no violations found" || fail "clean: missing clean message"
pass "clean tree exits 0"

# --- node-id leak fails; allowlisted example token stays legal (AC2-EDGE) -----
log "node-id leak fails; allowlisted token legal in same file"
printf 'real leak x-ab12 here.\nexample id ab-1234abcd is fine.\n' > docs/mixed.md
commit_all
OUT=$(run_gate); RC=$?
(( RC == 1 )) || fail "node-id: expected 1, got $RC ($OUT)"
echo "$OUT" | grep -q "\[node-id\] docs/mixed.md:1:real leak x-ab12" \
    || fail "node-id: real id x-ab12 not flagged ($OUT)"
echo "$OUT" | grep -q "ab-1234abcd" \
    && fail "node-id: allowlisted example token ab-1234abcd was flagged ($OUT)"
pass "node-id: real id fails, allowlisted example token exempt"

# --- session-url leak in prose fails -----------------------------------------
log "session-url leak fails"
rm -f docs/mixed.md
printf 'see https://claude.ai/code/session_xyz for details.\n' > docs/url.md
commit_all
OUT=$(run_gate); RC=$?
(( RC == 1 )) || fail "session-url: expected 1, got $RC"
echo "$OUT" | grep -q "\[session-url\] docs/url.md" || fail "session-url: not flagged ($OUT)"
pass "session-url: prose leak fails"

# --- AC2-UI: a file with BOTH an internal/ leak and a node-id leak labels each
log "report distinguishes pattern class"
rm -f docs/url.md
printf 'pointer to internal/foo/bar.md\nand node x-ab12 breadcrumb.\n' > docs/both.md
commit_all
OUT=$(run_gate); RC=$?
(( RC == 1 )) || fail "both: expected 1, got $RC"
echo "$OUT" | grep -q "\[internal-path\] docs/both.md:1" || fail "both: internal-path line unlabeled ($OUT)"
echo "$OUT" | grep -q "\[node-id\] docs/both.md:2" || fail "both: node-id line unlabeled ($OUT)"
pass "AC2-UI: each violation line names its pattern class"

# --- AC2-FR: a `git ls-files` failure is loud, not a vacuous green ------------
# Regression guard on the review #503 fix. A fake `git` on PATH resolves the
# repo root (so REPO_ROOT lands in the scratch repo) but fails `ls-files`, the
# exact failure the capture must turn into exit 1 instead of "no violations".
log "git ls-files failure fails loud"
FAKEBIN="$WORK/bin"; mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/git" <<EOF
#!/usr/bin/env bash
case "\$1" in
  rev-parse) echo "$REPO"; exit 0;;
  ls-files) echo "fatal: simulated git failure" >&2; exit 128;;
  *) exit 0;;
esac
EOF
chmod +x "$FAKEBIN/git"
OUT=$( cd "$REPO" && PATH="$FAKEBIN:$PATH" bash "$GATE" 2>&1 ); RC=$?
(( RC == 1 )) || fail "git-fail: expected 1, got $RC ($OUT)"
echo "$OUT" | grep -q "'git ls-files' failed" || fail "git-fail: missing loud error ($OUT)"
echo "$OUT" | grep -q "no violations found" && fail "git-fail: vacuously passed"
pass "AC2-FR: git ls-files failure exits 1 loudly"

echo "[internal-refs] all check-no-internal-refs tests passed"
exit 0
