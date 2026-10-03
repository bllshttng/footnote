#!/usr/bin/env bash
# fno hook: agy session-state push - report this session's presence to the
# registry (mail and liveness stop guessing). agy ignores Stop stdout and
# merges PreInvocation injections, so a silent {} is the only safe output;
# the decision contract stays with the Stop adapter.
PATH="${PATH:+$PATH:}/usr/bin:/bin"
export PATH
stdin=$(cat)
sid=$(printf '%s' "$stdin" | sed -n 's/.*"conversationId"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
bin=""
for b in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}"; do
    [[ -n "$b" && -x "$b" ]] || continue
    bin="$b"
    break
done
if [[ -n "$bin" && -n "$sid" ]]; then
    "$bin" report --kind session --harness agy --session-id "$sid" >/dev/null 2>&1 || true
fi
printf '{}\n'
