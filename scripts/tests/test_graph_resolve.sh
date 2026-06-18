#!/usr/bin/env bash
# test_graph_resolve.sh -- unit tests for scripts/lib/graph-resolve.sh
#
# Sandboxes ~/.fno/graph.json via a temp HOME override so the caller's
# real backlog is never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LIB="$REPO_ROOT/scripts/lib/graph-resolve.sh"

if [[ ! -f "$LIB" ]]; then
    echo "test_graph_resolve: cannot find $LIB" >&2
    exit 2
fi

TMP=$(mktemp -d -t graph-resolve.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

# Seed a sandbox graph.json. Caller can point GRAPH_JSON at it.
cat > "$TMP/graph.json" <<'JSON'
{
  "entries": [
    {
      "id": "ab-12345678",
      "title": "Seeded node",
      "plan_path": "internal/plans/foo.md"
    },
    {
      "id": "ab-aaaaaaaa",
      "title": "Node without plan_path",
      "plan_path": null
    }
  ]
}
JSON

# Source the library once; helpers are idempotent so multiple tests share it.
# shellcheck source=/dev/null
source "$LIB"

run_test() {
    local name="$1"; shift
    local want_rc="$1"; shift
    local want_stdout="$1"; shift
    local want_stderr_pattern="$1"; shift
    local got_stdout got_stderr got_rc
    local tmpstderr
    tmpstderr=$(mktemp)
    got_stdout=$("$@" 2>"$tmpstderr")
    got_rc=$?
    got_stderr=$(cat "$tmpstderr"); rm -f "$tmpstderr"

    if [[ "$got_rc" -ne "$want_rc" ]]; then
        fail "$name" "rc=$got_rc (want $want_rc) stdout=$got_stdout stderr=$got_stderr"
        return
    fi
    if [[ -n "$want_stdout" && "$got_stdout" != "$want_stdout" ]]; then
        fail "$name" "stdout='$got_stdout' (want '$want_stdout')"
        return
    fi
    if [[ -n "$want_stderr_pattern" && ! "$got_stderr" =~ $want_stderr_pattern ]]; then
        fail "$name" "stderr does not match /$want_stderr_pattern/: $got_stderr"
        return
    fi
    pass "$name"
}

# 1. passes through non-ID arg
GRAPH_JSON="$TMP/graph.json" run_test "pass-through: plain file path" 0 "plans/foo.md" "" resolve_arg "plans/foo.md"

# 2. passes through description / sentence
GRAPH_JSON="$TMP/graph.json" run_test "pass-through: description" 0 "add login feature" "" resolve_arg "add login feature"

# 3. resolves a known ID to its plan_path
GRAPH_JSON="$TMP/graph.json" run_test "resolve: known ID -> plan_path" 0 "internal/plans/foo.md" "" resolve_arg "ab-12345678"

# 4. unknown ID soft-fails (echoes arg + stderr warning)
GRAPH_JSON="$TMP/graph.json" run_test "unknown ID soft-fails" 0 "ab-deadbeef" "unknown id" resolve_arg "ab-deadbeef"

# 5. unknown ID under RESOLVE_STRICT=1 returns nonzero
tmpstderr5=$(mktemp)
got5=$(RESOLVE_STRICT=1 GRAPH_JSON="$TMP/graph.json" resolve_arg "ab-deadbeef" 2>"$tmpstderr5")
rc5=$?
err5=$(cat "$tmpstderr5"); rm -f "$tmpstderr5"
if [[ "$rc5" -ne 0 && "$err5" =~ unknown\ id ]]; then
    pass "strict mode: unknown ID exits nonzero"
else
    fail "strict mode: unknown ID exits nonzero" "rc=$rc5 stdout=$got5 stderr=$err5"
fi

# 6. missing graph file soft-fails (echoes arg + stderr warning)
GRAPH_JSON="$TMP/does-not-exist.json" run_test "missing graph soft-fails" 0 "ab-12345678" "missing" resolve_arg "ab-12345678"

# 7. pattern not-quite-right passes through unchanged (too short / non-hex)
GRAPH_JSON="$TMP/graph.json" run_test "bad pattern: too-short" 0 "ab-notvalid" "" resolve_arg "ab-notvalid"

# 8. injection guard: shell metacharacters in arg never reach python because regex rejects them
GRAPH_JSON="$TMP/graph.json" run_test "injection guard: shell metachars pass through" 0 "ab-12345678; rm -rf /" "" resolve_arg "ab-12345678; rm -rf /"

# 9. node with null plan_path soft-fails with distinct message
GRAPH_JSON="$TMP/graph.json" run_test "node has no plan_path soft-fails" 0 "ab-aaaaaaaa" "no plan_path" resolve_arg "ab-aaaaaaaa"

echo
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
