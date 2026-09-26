#!/usr/bin/env bash
# hooks/law-stage-inject.sh, driven end to end against a stubbed `fno-agents`.
#
# The hook's whole job is one pipe: wrap the raw hook payload as a law-match
# stage request, run it, and print the answer's hook_output object. Its
# failure mode is printing SOMETHING when the answer carried no block (a
# stray `null` or a stderr leak onto stdout would inject garbage context
# after every Skill call), so the silent cases assert EMPTY stdout, and the
# one happy case asserts the exact compact object.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
HOOK="hooks/law-stage-inject.sh"
[[ -f "$HOOK" ]] || { echo "FAIL: $HOOK not found from $(pwd)"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STUB="$TMP/bin"
mkdir -p "$STUB"

pass=0
fail=0

# Build a stub `fno-agents` whose body is $1, run the real hook with it on
# PATH, feeding $2 as the hook payload.
run_with_stub() {
    local body="$1" payload="$2" out
    { printf '#!/usr/bin/env bash\n'; printf '%s\n' "$body"; } > "$STUB/fno-agents"
    chmod +x "$STUB/fno-agents"
    out="$(printf '%s' "$payload" | PATH="$STUB:$PATH" bash "$HOOK" 2>/dev/null)"
    # Command substitution strips trailing newlines on both sides of the
    # comparison, so the exactness here is on content, not the final newline.
    printf '%s' "$out"
}

check() {
    local name="$1" want="$2" got="$3"
    if [[ "$got" == "$want" ]]; then
        echo "  PASS: $name"
        pass=$((pass + 1))
    else
        echo "  FAIL: $name"
        echo "    wanted: $want"
        echo "    got: ${got:-<empty>}"
        fail=$((fail + 1))
    fi
}

echo "=== law-stage-inject hook ==="

SKILL_PAYLOAD='{"hook_event_name":"PostToolUse","tool_name":"Skill","tool_input":{"skill":"fno:review","args":"low"}}'
PROMPT_PAYLOAD='{"hook_event_name":"UserPromptSubmit","prompt":"/fno:review low"}'

# AC3-HP: the answer's hook_output object reaches stdout as compact JSON,
# nothing else. Both happy stubs read stdin and answer only a "mode":"stage"
# request, so a wrapper that drops stdin (the async-job /dev/null defect
# measured 2026-09-22) fails here instead of passing on an answer that was
# never fed.
out="$(run_with_stub \
    'read -r req
     [[ "$req" == *"mode\":\"stage\""* ]] && echo "{\"ok\":true,\"stage\":\"review\",\"hook_output\":{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"additionalContext\":\"x\"}}}"' \
    "$SKILL_PAYLOAD")"
check "the hook_output object is stdout, exactly" \
    '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"x"}}' \
    "$out"

out="$(run_with_stub \
    'read -r req
     [[ "$req" == *"mode\":\"stage\""* ]] && echo "{\"ok\":true,\"stage\":\"review\",\"hook_output\":{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"## Law governing review\"}}}"' \
    "$PROMPT_PAYLOAD")"
check "a prompt payload carries the block through" \
    '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"## Law governing review"}}' \
    "$out"

# AC7-HP: an Edit payload rides the file it is about to change as `paths`,
# so the verb answers the edit read (path laws) instead of the classifier.
EDIT_PAYLOAD='{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"crates/x.rs","old_string":"a","new_string":"b"}}'
out="$(run_with_stub \
    'read -r req
     [[ "$req" == *"paths\":[\"crates/x.rs\"]"* ]] && echo "{\"ok\":true,\"stage\":\"edit\",\"hook_output\":{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"edit-paths-ok\"}}}"' \
    "$EDIT_PAYLOAD")"
check "an Edit payload rides the file as paths" \
    '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"edit-paths-ok"}}' \
    "$out"

# AC8-HP: a prompt payload builds the request exactly as before, with no
# `paths` key anywhere on it.
out="$(run_with_stub \
    'read -r req
     [[ "$req" != *"paths"* ]] && echo "{\"ok\":true,\"stage\":null,\"hook_output\":{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"prompt-no-paths-ok\"}}}"' \
    "$PROMPT_PAYLOAD")"
check "a prompt payload carries no paths key" \
    '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"prompt-no-paths-ok"}}' \
    "$out"

# AC3-ERR: every degraded path renders NOTHING. An absent binary, a crashed
# verb, and an answer with no block are all the same silence.
out="$(run_with_stub 'exit 0' "$SKILL_PAYLOAD")"
check "a stub that prints nothing renders nothing" "" "$out"

out="$(run_with_stub 'exit 2' "$SKILL_PAYLOAD")"
check "a failing verb renders nothing" "" "$out"

out="$(run_with_stub 'echo "{\"ok\":true,\"stage\":null,\"hook_output\":null}"' \
    "$SKILL_PAYLOAD")"
check "a null hook_output renders nothing" "" "$out"

rm "$STUB/fno-agents"
# With the wrapper passing stdin through, a real fno-agents elsewhere on PATH
# would answer here, so the missing-binary case must confine PATH to system
# dirs. jq is symlinked in because the hook needs it before its binary guard.
mkdir -p "$TMP/sys"
ln -sf "$(command -v jq)" "$TMP/sys/jq"
out="$(printf '%s' "$SKILL_PAYLOAD" | PATH="$TMP/sys:/usr/bin:/bin" bash "$HOOK" 2>/dev/null)"
check "a missing fno-agents renders nothing" "" "$out"

echo
echo "Results: $pass passed, $fail failed"
[[ $fail -eq 0 ]] || exit 1
