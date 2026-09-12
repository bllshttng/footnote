#!/usr/bin/env bash
# test_target_context_binding.sh
#
# Task-context binding on the real carriers (x-59b0):
#   1. The postcompact hook restores a single-FILE plan reference (the -d gate
#      used to drop it exactly when a flat quick plan was in play).
#   2. The postcompact hook emits the current attempt's binding pointer +
#      declared constraints once, and nothing when no binding exists or the
#      file is malformed (an unreadable binding degrades to absent, never to a
#      fabricated pointer).
#   3. Both launch substrates go through ONE payload-preparation entry: the
#      block rides at most once, the original normalized message stays the
#      prefix, and the brevity guidance is untouched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_HOOK="$REPO_ROOT/hooks/target-postcompact-reinject.sh"

[[ -f "$TARGET_HOOK" ]] || { echo "FAIL: target postcompact hook not found at $TARGET_HOOK" >&2; exit 1; }
export CLAUDE_PLUGIN_ROOT="$REPO_ROOT"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t target-context-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

NODE="x-59b0"
SID="sess-context-worker"

# A minimal session repo: manifest owned by $SID, a single-FILE plan, and the
# handoff artifact root the binding lives under.
setup_repo() { # $1 = plan_path_kind (file|dir|missing)
    local repo="$1"
    mkdir -p "$repo/.fno/artifacts/handoff"
    {
        echo 'fno_id: 20260912T055500Z-test99-abc123'
        echo 'input: "bind task context"'
        echo 'harness_session_id: '"$SID"
        echo 'graph_node_id: '"$NODE"
        echo 'plan_path: '"$2"
    } > "$repo/.fno/target-state.md"
}

run_hook() { # $1 = repo (cwd), echoes the hook's stdout
    (cd "$1" && printf '{"source":"compact","session_id":"%s"}' "$SID" \
        | CLAUDE_PLUGIN_ROOT="$REPO_ROOT" bash "$TARGET_HOOK" 2>/dev/null)
}

count_occurrences() { # $1 = haystack, $2 = needle -> count on stdout
    printf '%s' "$1" | grep -o -F "$2" | wc -l | tr -d ' '
}

write_binding() { # $1 = repo - write the node-keyed binding slot
    local f="$1/.fno/artifacts/handoff/task-context-${NODE}.json"
    {
        echo '{'
        echo '  "node": "'"${NODE}"'",'
        echo '  "attempt": "20260912T055500Z-test99-abc123",'
        echo '  "binding_digest": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",'
        echo '  "stage": "prepared",'
        echo '  "required_constraints": ['
        echo '    "Do not widen scope beyond the plan",'
        echo '    "Atomic commits per task"'
        echo '  ]'
        echo '}'
    } > "$f"
}

# ── 1. Single-file plan reference survives compaction ─────────────────────

SINGLE="$TMP/single"
mkdir -p "$SINGLE"
printf '# plan\n' > "$SINGLE/PLAN.md"
setup_repo "$SINGLE" "$SINGLE/PLAN.md"
OUT="$(run_hook "$SINGLE")"
if printf '%s' "$OUT" | grep -qF "**Plan:** $SINGLE/PLAN.md (single file)"; then
    pass "single-file plan path restored after compaction"
else
    fail "single-file plan path restored after compaction; got: $OUT"
fi

# A directory plan keeps its task count (regression guard on the -d branch).
DIRPLAN="$TMP/dirplan/plans"
mkdir -p "$DIRPLAN" "$TMP/dirplan"
printf '### Task 1\nx\n### Task 2\ny\n' > "$DIRPLAN/00-a.md"
setup_repo "$TMP/dirplan" "$DIRPLAN"
OUT="$(run_hook "$TMP/dirplan")"
if printf '%s' "$OUT" | grep -qF "**Plan:** $DIRPLAN (2 tasks)"; then
    pass "directory plan keeps its task count"
else
    fail "directory plan keeps its task count; got: $OUT"
fi

# ── 2. Binding pointer: present exactly once with constraints, absent when ──
#    there is no binding or a malformed one.

BOUND="$TMP/bound"
mkdir -p "$BOUND"
printf '# plan\n' > "$BOUND/PLAN.md"
setup_repo "$BOUND" "$BOUND/PLAN.md"
write_binding "$BOUND"
OUT="$(run_hook "$BOUND")"
if printf '%s' "$OUT" | grep -qF "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"; then
    pass "binding pointer carries the digest"
else
    fail "binding pointer carries the digest; got: $OUT"
fi
if printf '%s' "$OUT" | grep -qF "stage prepared" && printf '%s' "$OUT" | grep -qF -- "- Do not widen scope beyond the plan"; then
    pass "binding pointer carries stage and declared constraints"
else
    fail "binding pointer carries stage and declared constraints; got: $OUT"
fi
N=$(count_occurrences "$OUT" "**Task context:**")
if [[ "$N" == "1" ]]; then
    pass "binding pointer rides exactly once"
else
    fail "binding pointer rides exactly once (got $N)"
fi

# The SECOND carrier shape: the codex lane delivers the same pointer through
# systemMessage (claude rides hookSpecificOutput.additionalContext).
OUT_CODEX="$( (cd "$BOUND" && printf '{"session_id":"%s"}' "$SID" \
    | FNO_PLATFORM="codex" CLAUDE_PLUGIN_ROOT="$REPO_ROOT" bash "$TARGET_HOOK" 2>/dev/null) )"
if printf '%s' "$OUT_CODEX" | grep -qF '"systemMessage"' \
    && printf '%s' "$OUT_CODEX" | grep -qF "**Task context:**" \
    && printf '%s' "$OUT_CODEX" | grep -qF -- "- Do not widen scope beyond the plan"; then
    pass "codex carrier delivers the same pointer through systemMessage"
else
    fail "codex carrier delivers the same pointer through systemMessage; got: $OUT_CODEX"
fi

# No binding file: no pointer, hook still exits 0 with the goal line.
NOBIND="$TMP/nobind"
mkdir -p "$NOBIND"
printf '# plan\n' > "$NOBIND/PLAN.md"
setup_repo "$NOBIND" "$NOBIND/PLAN.md"
OUT="$(run_hook "$NOBIND")"
if [[ "$OUT" != *"Task context:"* ]] && printf '%s' "$OUT" | grep -qF "**Goal:** bind task context"; then
    pass "no binding file means no pointer, goal still reinjected"
else
    fail "no binding file means no pointer, goal still reinjected; got: $OUT"
fi

# Malformed binding file: degrades to absent, never to a fabricated pointer.
MAL="$TMP/malformed"
mkdir -p "$MAL"
printf '# plan\n' > "$MAL/PLAN.md"
setup_repo "$MAL" "$MAL/PLAN.md"
printf '{not json' > "$MAL/.fno/artifacts/handoff/task-context-${NODE}.json"
OUT="$(run_hook "$MAL")"
if [[ "$OUT" != *"Task context:"* ]]; then
    pass "malformed binding degrades to absent (no fabricated pointer)"
else
    fail "malformed binding degrades to absent; got: $OUT"
fi

# ── 3. One payload-preparation entry for both substrates ──────────────────

PAYLOAD_CHECK="$(REPO_ROOT="$REPO_ROOT" FNO_TASK_CONTEXT_FILE="$BOUND/.fno/artifacts/handoff/task-context-${NODE}.json" \
PYTHONPATH="$REPO_ROOT/cli/src" python3 - "$BOUND/PLAN.md" <<'PY'
import json, sys
from fno.agents.spawn_payload import (
    BREVITY_MARKER,
    prepare_spawn_payload,
    task_context_block,
    load_task_context,
)

original = "Work the node; report at the boundary."
binding = load_task_context()
assert binding is not None, "binding failed to load from env"

payload, measures = prepare_spawn_payload(original)
assert payload.startswith(original), "original normalized message must stay the prefix"
assert BREVITY_MARKER in payload, "brevity guidance still rides once"
assert payload.count("<task-context ") == 1, "binding block must ride exactly once"
assert "- Do not widen scope beyond the plan" in payload, "declared constraint missing"
assert measures["task_context"] is True, "measures must record the carry"
assert measures["payload_bytes"] == len(payload.encode()), "payload bytes must measure the actual payload"

# The same entry serves the pane substrate: identical output, byte for byte.
payload2, measures2 = prepare_spawn_payload(original)
assert payload2 == payload and measures2 == measures, "pane/non-pane payloads diverged"

# A payload that already carries the block never grows a second one.
again, again_measures = prepare_spawn_payload(payload)
assert again.count("<task-context ") == 1, "block duplicated on re-preparation"

# Without a binding, the entry degrades to the historical brevity-only shape.
sys.argv = ["x"]
import os
os.environ.pop("FNO_TASK_CONTEXT_FILE", None)
plain, plain_measures = prepare_spawn_payload(original)
assert "<task-context " not in plain and plain_measures["task_context"] is False

# A corrupt file loads as UNSET (no block), never as a binding.
os.environ["FNO_TASK_CONTEXT_FILE"] = "/nonexistent/task-context.json"
broken = load_task_context()
assert broken is None, "unreadable binding must load as unset"

# Source contents never ride the payload.
assert "plan bytes" not in payload and "# plan" not in payload, "source contents leaked"
print("PAYLOAD_OK")
PY
)"
if [[ "$PAYLOAD_CHECK" == *"PAYLOAD_OK"* ]]; then
    pass "one payload entry: once-only carry, prefix preserved, corrupt=unset"
else
    fail "one payload entry contract; output: $PAYLOAD_CHECK"
fi

echo
echo "passed: $PASS  failed: $FAIL"
[[ "$FAIL" == "0" ]] || exit 1
exit 0
