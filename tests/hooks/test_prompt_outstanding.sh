#!/usr/bin/env bash
# hooks/prompt-outstanding.sh, driven end to end against a stubbed fno-agents.
# The hook's whole job is one pipe: pass the payload to `fno-agents hook
# prompt` under a 2s bound and print its envelope (or nothing). The skip
# rules and cache-age policy are Rust-side; this pins the wrapper contract.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
HOOK="hooks/prompt-outstanding.sh"
[[ -f "$HOOK" ]] || { echo "FAIL: $HOOK not found from $(pwd)"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STUB="$TMP/bin"
mkdir -p "$STUB"

pass=0
fail=0

run_with_stub() {
    local body="$1" payload="$2" out
    { printf '#!/usr/bin/env bash\n'; printf '%s\n' "$body"; } > "$STUB/fno-agents"
    chmod +x "$STUB/fno-agents"
    out="$(printf '%s' "$payload" | PATH="$STUB:$PATH" bash "$HOOK" 2>/dev/null)"
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

echo "=== prompt-outstanding hook ==="

PAYLOAD='{"hook_event_name":"UserPromptSubmit","prompt":"what is still open for me?"}'

out="$(run_with_stub \
    'echo "{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"Waiting on you (2)\"}}"' \
    "$PAYLOAD")"
check "the envelope reaches stdout, exactly" \
    '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"Waiting on you (2)"}}' \
    "$out"

out="$(run_with_stub 'exit 0' "$PAYLOAD")"
check "a silent verb renders nothing" "" "$out"

out="$(run_with_stub 'exit 3' "$PAYLOAD")"
check "a failing verb renders nothing" "" "$out"

rm "$STUB/fno-agents"
out="$(printf '%s' "$PAYLOAD" | PATH="$STUB:$PATH" bash "$HOOK" 2>/dev/null)"
check "a missing fno-agents renders nothing" "" "$out"

echo
echo "Results: $pass passed, $fail failed"
[[ $fail -eq 0 ]] || exit 1
