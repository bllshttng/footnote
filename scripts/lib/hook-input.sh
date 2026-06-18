#!/usr/bin/env bash
# hook-input.sh - extract the last assistant text from a Claude Code
# transcript JSONL file.
#
# Lifted from hooks/target-stop-hook.sh (Phase 3 of stop-hook refactor).
# Behavior is identical to the inline Python heredoc. The surrounding
# shell orchestration (HOOK_INPUT read from stdin, TRANSCRIPT_PATH
# extraction, platform branching) stays inline in the hook because it
# has exits and reads from stdin; lifting just the heredoc gives the
# meaty bit a name and a single place to evolve.
#
# Requires (set by caller):
#   python3 on PATH.
#
# Memory-bounded: streams forward and resets the buffer at every real
# (non-tool_result-only) user message, so memory stays O(one turn)
# instead of O(whole file). Transcripts can grow into hundreds of MB
# on long sessions; loading the whole file caused OOM before the
# streaming rewrite (see memory entry feedback_promise_must_be_last_
# assistant_message for the chain of regressions this code path has
# survived).

# extract_claude_last_assistant_text TRANSCRIPT_PATH
#   Walk the Claude Code transcript JSONL at TRANSCRIPT_PATH and print
#   the most recent assistant turn's text content on stdout (text from
#   contiguous trailing assistant entries, joined by newlines).
#
#   The buffer resets at every real user message (text input, image,
#   document, mixed - anything that isn't purely tool_result wrappers).
#   Tool-result user entries belong to the previous agent turn and do
#   NOT reset. Without this discriminator, an assistant turn that
#   emits <promise> then runs verification tool calls would leak the
#   stale promise into LAST_OUTPUT - the original bug this heredoc
#   was rewritten to fix (Codex P2 on PR #263).
#
#   Empty string + rc 0 on parse failure (file open error, JSON parse
#   error mid-stream, etc.) so the caller can fall through to other
#   detection paths without crashing the hook.
extract_claude_last_assistant_text() {
    local transcript_path="$1"
    python3 - "$transcript_path" <<'PYEOF' 2>/dev/null || echo ""
import json, sys
path = sys.argv[1]
# Stream forward and keep only the current assistant turn's text. Buffer
# resets at every real user message (text input), so memory stays O(one
# turn) instead of O(whole file). Transcripts can grow into hundreds of
# MB on long sessions.
texts = []
try:
    with open(path) as f:
        for line in f:
            if not line.strip():
                continue
            try:
                entry = json.loads(line)
            except Exception:
                continue
            msg = entry.get("message") or {}
            role = msg.get("role") or entry.get("role")
            content = msg.get("content")
            if role == "user":
                # Reset buffer on any real user input. The discriminator is
                # "is this entry purely tool_result wrappers?" — if yes, it
                # belongs to the previous agent turn and must NOT reset.
                # Anything else (text, image, document, mixed) is a fresh
                # user turn. The earlier "only text resets" check missed
                # image-only / document-only prompts (Codex P2 on PR #263):
                # `assistant(<promise>) -> user(image) -> assistant(tool_use)`
                # would leak the stale promise into LAST_OUTPUT.
                is_tool_result_only = False
                if isinstance(content, list) and content:
                    is_tool_result_only = all(
                        isinstance(c, dict) and c.get("type") == "tool_result"
                        for c in content
                    )
                if not is_tool_result_only:
                    texts = []
            elif role == "assistant":
                if isinstance(content, list):
                    for c in content:
                        if isinstance(c, dict) and c.get("type") == "text" and c.get("text"):
                            texts.append(c["text"])
                elif isinstance(content, str) and content:
                    texts.append(content)
except Exception:
    sys.exit(0)
sys.stdout.write("\n".join(texts))
PYEOF
}
