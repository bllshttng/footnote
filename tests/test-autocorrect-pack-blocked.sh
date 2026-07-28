#!/usr/bin/env bash
# test-autocorrect-pack-blocked.sh
#
# autocorrect-pack.sh emits a BLOCKED-state section read out of graph.json.
# It read `.nodes`, but the entry list lives under `.entries`, so the selector
# ran against an empty list on every invocation and the packet shipped an empty
# section for its whole life. jq's stderr was discarded too, so a read that
# never matched anything was indistinguishable from a read that found nothing.
#
# This is the check that would have caught it: a fixture graph with a known
# blocked node must show up in the packet.
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PACK="${REPO_ROOT}/scripts/autocorrect-pack.sh"

pass() { printf '[autocorrect-blocked] PASS: %s\n' "$*"; }
fail() { printf '[autocorrect-blocked] FAIL: %s\n' "$*" >&2; exit 1; }
skip() { printf '[autocorrect-blocked] SKIP: %s\n' "$*" >&2; exit 77; }

command -v jq &>/dev/null || skip "jq not on PATH"
[[ -f "$PACK" ]] || fail "not found: $PACK"

TMP="$(mktemp -d)" || fail "mktemp failed"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/claude" "$TMP/fno"
# One in-window S1 event so the packet gets past its empty-log guard. The log
# lives under FNO_HOME, not CLAUDE_DIR (footnote state never sits under .claude/).
printf '%s | S1 | test | test.md | fixture event\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$TMP/fno/corrections.log"

# Two blocked nodes, one via `status` and one via blocked_count, plus a node
# that must NOT appear.
cat > "$TMP/graph.json" <<'JSON'
{"entries":[
  {"id":"tst-block01","title":"blocked by status","status":"blocked","blocked_count":0,
   "last_blocked_reason":"waiting on upstream"},
  {"id":"tst-block02","title":"blocked by count","status":"ready","blocked_count":3,
   "last_blocked_reason":null},
  {"id":"tst-clear01","title":"not blocked at all","status":"ready","blocked_count":0}
]}
JSON

OUT="$TMP/packet.yaml"
CLAUDE_DIR_OVERRIDE="$TMP/claude" FNO_HOME="$TMP/fno" FNO_GRAPH_PATH="$TMP/graph.json" \
  bash "$PACK" --dry-run > "$OUT" 2>"$TMP/err.log" \
  || fail "autocorrect-pack exited non-zero (stderr: $(cat "$TMP/err.log"))"

grep -q 'tst-block01' "$OUT" \
  || fail "status==blocked node missing from packet (this is the .nodes/.entries bug)"
pass "a status==blocked node reaches the packet"

grep -q 'tst-block02' "$OUT" \
  || fail "blocked_count>0 node missing from packet"
pass "a blocked_count>0 node reaches the packet"

grep -q 'tst-clear01' "$OUT" \
  && fail "an unblocked node leaked into the packet"
pass "an unblocked node is excluded"

grep -q 'waiting on upstream' "$OUT" \
  || fail "last_blocked_reason did not survive into the packet"
pass "last_blocked_reason survives into the packet"

printf '[autocorrect-blocked] All blocked-section scenarios passed\n'
