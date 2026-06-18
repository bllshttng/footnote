#!/usr/bin/env bash
# test_megawalk_args.sh -- guardrails around the 2026-04-20 megawalk surface reduction.
#
# Covers the parts we CAN exercise mechanically: the roadmap-tasks.py
# argument parsing that the skill delegates to, and the shared graph-ID
# resolver that all three shipping skills source before dispatch. The
# SKILL.md prose itself is LLM-interpreted and cannot be asserted here.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

TMP=$(mktemp -d -t megawalk-args.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

export HOME="$TMP"
mkdir -p "$HOME/.fno"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

# Seed a tiny sandbox graph.json that maps ab-12345678 -> plans/test.md.
cat > "$HOME/.fno/graph.json" <<'JSON'
{"entries": [
  {"id": "ab-12345678", "title": "Test", "plan_path": "plans/test.md", "_status": "ready"}
]}
JSON

# Case 1: roadmap-tasks.py intake --batch is rejected with a helpful redirect.
OUT=$(python3 "$REPO_ROOT/scripts/roadmap-tasks.py" intake --batch plans/foo 2>&1 || true)
if echo "$OUT" | grep -qiE "removed|multi-path|shell glob"; then
    pass "intake --batch emits redirect"
else
    fail "intake --batch didn't redirect" "got: $OUT"
fi

# Case 2: resolver returns plan_path for a known ab- ID.
source "$REPO_ROOT/scripts/lib/graph-resolve.sh"
result=$(resolve_arg "ab-12345678")
[[ "$result" == "plans/test.md" ]] && pass "resolver returns plan_path" \
    || fail "resolver returns plan_path" "got: '$result'"

# Case 3: resolver passes non-ID arguments through unchanged.
result=$(resolve_arg "plans/existing.md")
[[ "$result" == "plans/existing.md" ]] && pass "resolver passes paths" \
    || fail "resolver passes paths" "got: '$result'"

# Case 4: resolver soft-fails on unknown IDs (echo arg, stderr warning, rc=0).
result=$(resolve_arg "ab-deadbeef" 2>/dev/null)
[[ "$result" == "ab-deadbeef" ]] && pass "unknown id passes through" \
    || fail "unknown id passes through" "got: '$result'"

# Case 5: `get` command works for a seeded node (smoke-check phase 2 integration).
get_out=$(python3 "$REPO_ROOT/scripts/roadmap-tasks.py" get "ab-12345678" --field plan_path 2>&1)
get_rc=$?
if [[ "$get_rc" -eq 0 && "$get_out" == "plans/test.md" ]]; then
    pass "roadmap-tasks.py get --field plan_path works"
else
    fail "roadmap-tasks.py get" "rc=$get_rc out=$get_out"
fi

echo
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
