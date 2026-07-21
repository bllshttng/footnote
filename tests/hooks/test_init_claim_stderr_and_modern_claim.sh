#!/usr/bin/env bash
# test_init_claim_stderr_and_modern_claim.sh
#
# init-target-state.sh: let the modern `fno claim` be the authority for
# graph_node_id.
#
# Covers:
#   (a) defect 2: the modern `fno claim acquire` wins => graph_node_id is the
#       node id (NOT null), written exactly once. (The old legacy-claim stderr
#       capture into .fno/.init-claim.log was removed when the graph lock stamp
#       moved off the ambient python3 path; .init-claim.log is now a transient
#       stamp-failure log that is removed on success, so it is no longer
#       asserted here.)
#   (b) Boundary + AC1-FR: graph.json missing entirely => graph_node_id null
#       even though the modern claim succeeds (the `-f $_GRAPH_FILE` guard), and
#       it is written exactly once.
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[init-claim] %s\n' "$*"; }
fail() { printf '[init-claim] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[init-claim] PASS: %s\n' "$*"; }
skip() { printf '[init-claim] SKIP: %s\n' "$*" >&2; exit 77; }

command -v git     &>/dev/null || skip "git not on PATH"
command -v python3 &>/dev/null || skip "python3 not on PATH"
command -v fno     &>/dev/null || skip "fno not on PATH (modern claim required)"
[[ -f "$INIT" ]]   || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

_ALL_TMPS=()
trap 'rm -rf "${_ALL_TMPS[@]}"' EXIT

# An isolated temp repo. Crucially it has NO scripts/roadmap-tasks.py, so the
# legacy `python3 .../scripts/roadmap-tasks.py update` fails exactly like the
# transient flock contention it stands in for -- letting us assert the modern
# claim is the authority.
make_repo() {
  local _varname="$1" _dir
  _dir="$(mktemp -d -t init-claim.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  (cd "$_dir" && git init -q && mkdir -p .fno home/.fno) || fail "repo setup failed in $_dir"
  printf '# isolated\n' > "${_dir}/.fno/config.toml"
  printf '# isolated global\n' > "${_dir}/home/.fno/config.toml"
}

graph_node_id_of() {  # $1 = state file
  grep '^graph_node_id:' "$1" | sed 's/^graph_node_id:[[:space:]]*//' | tr -d '\r'
}

# ── (a) legacy fails, modern wins => node id + captured stderr ────────
log "(a): legacy-claim failure + modern-claim win => graph_node_id=node, stderr captured"

make_repo TMP_A
_ALL_TMPS+=("$TMP_A")
# A claimable node (session_id null => _CURRENT_CLAIM empty => legacy path taken).
cat > "${TMP_A}/home/.fno/graph.json" <<'JSON'
{"entries":[{"id":"tst-11000a","title":"legacy-fail modern-win test node","session_id":null}]}
JSON

(cd "$TMP_A" && \
  HOME="${TMP_A}/home" \
  TARGET_START=1 \
  TARGET_INPUT="tst-11000a" \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(a): init exited non-zero"

STATE_A="${TMP_A}/.fno/target-state.md"
[[ -f "$STATE_A" ]] || fail "(a): target-state.md was not created"

GNID_A="$(graph_node_id_of "$STATE_A")"
[[ "$GNID_A" == "tst-11000a" ]] \
  || fail "(a): expected graph_node_id 'tst-11000a' (modern claim is authority), got '${GNID_A}'"
pass "(a): graph_node_id falls back to node id when modern claim wins"

# exactly one graph_node_id line (AC1-FR)
_count_a="$(grep -c '^graph_node_id:' "$STATE_A")"
[[ "$_count_a" == "1" ]] || fail "(a): graph_node_id written ${_count_a}x, expected 1 (AC1-FR)"
pass "(a): graph_node_id written exactly once"

# ── (b) graph.json missing => null even when modern claim wins ────────
log "(b): graph.json missing => graph_node_id null despite modern-claim win (Boundary guard)"

make_repo TMP_B
_ALL_TMPS+=("$TMP_B")
# No graph.json written. Use an ab-<8hex> id so _NODE_ID is set without a graph.
rm -f "${TMP_B}/home/.fno/graph.json"

(cd "$TMP_B" && \
  HOME="${TMP_B}/home" \
  TARGET_START=1 \
  TARGET_INPUT="ab-1100b0b0" \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(b): init exited non-zero"

STATE_B="${TMP_B}/.fno/target-state.md"
[[ -f "$STATE_B" ]] || fail "(b): target-state.md was not created"

GNID_B="$(graph_node_id_of "$STATE_B")"
[[ "$GNID_B" == "null" ]] \
  || fail "(b): expected graph_node_id 'null' when graph.json missing, got '${GNID_B}'"
pass "(b): graph_node_id null when graph.json missing (Boundary preserved)"

_count_b="$(grep -c '^graph_node_id:' "$STATE_B")"
[[ "$_count_b" == "1" ]] || fail "(b): graph_node_id written ${_count_b}x, expected 1 (AC1-FR)"
pass "(b): graph_node_id written exactly once"

# ── (c) ab-id absent from an existing graph => null (codex P2 guard) ───
log "(c): ab-id not present in an existing graph.json => graph_node_id null"

make_repo TMP_C
_ALL_TMPS+=("$TMP_C")
# Graph exists but does NOT contain the requested ab-id. The legacy claim fails
# ("node not found") yet the modern lock acquires fine; the node-presence grep
# must still keep graph_node_id null so no successor is spawned for a bogus node.
cat > "${TMP_C}/home/.fno/graph.json" <<'JSON'
{"entries":[{"id":"tst-other0","title":"some other node","session_id":null}]}
JSON

(cd "$TMP_C" && \
  HOME="${TMP_C}/home" \
  TARGET_START=1 \
  TARGET_INPUT="ab-deadbeef" \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(c): init exited non-zero"

STATE_C="${TMP_C}/.fno/target-state.md"
[[ -f "$STATE_C" ]] || fail "(c): target-state.md was not created"
GNID_C="$(graph_node_id_of "$STATE_C")"
[[ "$GNID_C" == "null" ]] \
  || fail "(c): expected graph_node_id 'null' for ab-id absent from graph, got '${GNID_C}'"
pass "(c): graph_node_id null when node id is absent from an existing graph"

log "All init-claim scenarios passed"
