#!/usr/bin/env bash
# test_init_node_guard_tokenize.sh
#
# init-target-state.sh: the node guard must tokenize INITIAL_INPUT so a
# modifier-prefixed input ("beast mode <id>") still resolves and claims its
# node, instead of the old anchored whole-string match that only saw a bare id.
# It must stay fail-safe on zero or ambiguous matches, and must never lose a
# claim silently (one stderr line names an unresolved-but-non-empty input).
#
# Covers:
#   AC1-HP : "beast mode <id>" resolves and claims; identical to the bare id.
#   AC3-ERR: two distinct ids -> ambiguous, no claim; free-text -> no claim;
#            empty input -> no claim AND no diagnostic line at all.
#   AC4-FR : a non-empty input that resolves to no node prints exactly one line.
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[guard-tokenize] %s\n' "$*"; }
fail() { printf '[guard-tokenize] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[guard-tokenize] PASS: %s\n' "$*"; }
skip() { printf '[guard-tokenize] SKIP: %s\n' "$*" >&2; exit 77; }

command -v git     &>/dev/null || skip "git not on PATH"
command -v python3 &>/dev/null || skip "python3 not on PATH"
command -v fno     &>/dev/null || skip "fno not on PATH (modern claim required)"
[[ -f "$INIT" ]]   || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

_ALL_TMPS=()
trap 'rm -rf "${_ALL_TMPS[@]}"' EXIT

make_repo() {
  local _varname="$1" _dir
  _dir="$(mktemp -d -t guard-tok.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  (cd "$_dir" && git init -q && mkdir -p .fno home/.fno) || fail "repo setup failed in $_dir"
  printf '# isolated\n' > "${_dir}/.fno/config.toml"
  printf '# isolated global\n' > "${_dir}/home/.fno/config.toml"
  cat > "${_dir}/home/.fno/graph.json" <<'JSON'
{"entries":[
  {"id":"tst-aa00aa00","title":"first guard node","session_id":null},
  {"id":"tst-bb00bb00","title":"second guard node","session_id":null}
]}
JSON
}

# $1 = tmp repo dir, $2 = TARGET_INPUT ; sets STATE / ERRLOG globals
run_init() {
  local _dir="$1" _input="$2"
  STATE="${_dir}/.fno/target-state.md"
  ERRLOG="${_dir}/stderr.log"
  (cd "$_dir" && \
    HOME="${_dir}/home" \
    TARGET_START=1 \
    TARGET_INPUT="$_input" \
    TARGET_LOCATION_OK="main-acknowledged" \
    bash "$INIT" >/dev/null 2>"$ERRLOG") \
    || fail "init exited non-zero for input '$_input' (see $ERRLOG)"
  [[ -f "$STATE" ]] || fail "target-state.md not created for input '$_input'"
}

claim_key_of() { grep '^target_claim_key:' "$1" 2>/dev/null | sed 's/^target_claim_key:[[:space:]]*//' | tr -d '"\r' ; }

# ── AC1-HP: modifier-prefixed input resolves and claims like the bare id ──
log "AC1-HP: 'beast mode tst-aa00aa00' resolves + claims == bare 'tst-aa00aa00'"

make_repo TMP1; _ALL_TMPS+=("$TMP1")
run_init "$TMP1" "beast mode tst-aa00aa00"
CK1="$(claim_key_of "$STATE")"
[[ "$CK1" == "node:tst-aa00aa00" ]] \
  || fail "AC1-HP: modifier-prefixed input did not claim the node (got '${CK1}')"
pass "AC1-HP: modifier-prefixed input claimed node:tst-aa00aa00"

make_repo TMP1b; _ALL_TMPS+=("$TMP1b")
run_init "$TMP1b" "tst-aa00aa00"
CK1b="$(claim_key_of "$STATE")"
[[ "$CK1b" == "$CK1" ]] \
  || fail "AC1-HP: bare-id claim '${CK1b}' != modifier-prefixed claim '${CK1}'"
pass "AC1-HP: bare id produces an identical claim key"

# ── AC3-ERR: two distinct ids => ambiguous, no claim, one ambiguity line ──
log "AC3-ERR: 'tst-aa00aa00 tst-bb00bb00' is ambiguous => no claim, one line"

make_repo TMP2; _ALL_TMPS+=("$TMP2")
run_init "$TMP2" "tst-aa00aa00 tst-bb00bb00"
CK2="$(claim_key_of "$STATE")"
[[ -z "$CK2" ]] || fail "AC3-ERR: ambiguous input still claimed '${CK2}'"
grep -qi 'ambiguous' "$ERRLOG" || fail "AC3-ERR: no ambiguity line printed"
pass "AC3-ERR: ambiguous input claims nothing and prints an ambiguity line"

# ── AC3-ERR: free-text description => no claim, no ambiguity line ──────────
log "AC3-ERR: 'add dark mode' => no claim, no ambiguity line"

make_repo TMP3; _ALL_TMPS+=("$TMP3")
run_init "$TMP3" "add dark mode"
CK3="$(claim_key_of "$STATE")"
[[ -z "$CK3" ]] || fail "AC3-ERR: free-text input claimed '${CK3}'"
grep -qi 'ambiguous' "$ERRLOG" && fail "AC3-ERR: free-text wrongly printed an ambiguity line"
pass "AC3-ERR: free-text input claims nothing and prints no ambiguity line"

# ── AC4-FR: a non-empty unresolved input prints exactly one guard line ─────
log "AC4-FR: 'add dark mode' prints exactly one 'no backlog node resolved' line"
_n="$(grep -ciE 'no backlog node resolved' "$ERRLOG" || true)"
[[ "$_n" == "1" ]] \
  || fail "AC4-FR: expected exactly one 'no backlog node resolved' line, got ${_n}"
pass "AC4-FR: exactly one guard-diagnostic line for a non-empty unresolved input"

# ── AC3-ERR: empty input => no claim AND no guard-diagnostic line at all ───
log "AC3-ERR: empty input => no claim, and NO guard-diagnostic line"

make_repo TMP4; _ALL_TMPS+=("$TMP4")
run_init "$TMP4" ""
CK4="$(claim_key_of "$STATE")"
[[ -z "$CK4" ]] || fail "AC3-ERR: empty input claimed '${CK4}'"
if grep -qiE 'ambiguous|no backlog node resolved' "$ERRLOG"; then
  fail "AC3-ERR: empty input printed a guard-diagnostic line ($(cat "$ERRLOG"))"
fi
pass "AC3-ERR: empty input prints no guard-diagnostic line"

log "All node-guard tokenize scenarios passed"
