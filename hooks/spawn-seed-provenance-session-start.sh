#!/usr/bin/env bash
# SessionStart hook: attribute the spawn seed (node x-3a64).
#
# The seed is the one message that defines a worker's entire task, and it was
# the one message a worker could not attribute: `fno agents mail send` wraps every a2a
# message in <fno_mail>, while the seed arrived as bare payload text,
# indistinguishable from the operator typing.
#
# The envelope cannot ride the payload. `skills/agent/scripts/normalize.sh:710`
# classifies by a LEADING slash, and the harness REPL is a second reader we do
# not control, so anything in front of `/fno:target x-1234` breaks routing and
# anything behind it may be swallowed into the verb's arguments. So the prompt
# stays byte-identical and the attribution arrives beside it, here.
#
# Startup only. `resume` and `compact` fire SessionStart too, and emitting there
# would put a second copy of the same envelope in the transcript and make
# `grep '</fno_mail>'` overcount one spawn as several.
#
# Failure mode: hooks must NEVER block session start. Every failure path prints
# an empty object and exits 0. Silence is also the correct answer for a
# hand-started session: no seed fields means no peer sender, and inventing one
# would be the same lie as omitting a real one.

set -uo pipefail

HOOK_INPUT="$(cat 2>/dev/null || true)"

# Fail CLOSED on the source gate. An unreadable source is not "startup" -- if we
# cannot tell a startup from a compaction we must not emit, because the wrong
# guess duplicates the envelope on every compaction for the rest of the session.
if ! command -v jq >/dev/null 2>&1; then
    echo '{}'
    exit 0
fi
# Two different absences, and only one of them is safe to treat as a start.
# Unparseable input means the hook cannot tell a startup from a compaction, and
# guessing wrong duplicates the envelope on every compaction for the rest of the
# session, so that one fails closed.
if ! printf '%s' "$HOOK_INPUT" | jq -e . >/dev/null 2>&1; then
    echo '{}'
    exit 0
fi
# A payload that PARSES and carries no `.source` is the codex shape: `.source`
# is a claude field, and codex routes compaction to PostCompact, a separate
# event this hook is not registered for. So a codex SessionStart is always a
# real start. Requiring the literal "startup" silenced the sidecar on the one
# harness whose head-8 is a clock bucket, which is the collision that made a
# full reply address necessary in the first place.
SOURCE="$(printf '%s' "$HOOK_INPUT" | jq -r '.source // empty' 2>/dev/null || true)"
if [[ -n "$SOURCE" && "$SOURCE" != "startup" ]]; then
    echo '{}'
    exit 0
fi

# The cheap gate before the subprocess: no seed, nothing to attribute.
if [[ -z "${FNO_SEED_PROV_SEED_B64:-}" ]]; then
    echo '{}'
    exit 0
fi

# FNO_BIN, the same override every other fno subprocess caller honors, so a
# split install or a source checkout renders with the fno it means to.
FNO_BIN="${FNO_BIN:-fno}"
command -v "$FNO_BIN" >/dev/null 2>&1 || { echo '{}'; exit 0; }

ENVELOPE="$("$FNO_BIN" mail seed-provenance 2>/dev/null)" || {
    echo "spawn-seed-provenance: renderer refused; the seed stays unattributed" >&2
    echo '{}'
    exit 0
}
[[ -n "$ENVELOPE" ]] || { echo '{}'; exit 0; }

# Drop the C0 controls the escaper below does not cover, keeping TAB, LF and CR
# because it does. A seed pasted from a terminal capture carries ESC, and one
# raw control byte makes the emitted object invalid JSON, at which point the
# harness discards the WHOLE payload. The sidecar would not degrade, it would
# vanish, and silently. Stripping is right rather than escaping: this text is
# rendered into a prompt, where a control byte has nothing to say.
ENVELOPE="$(printf '%s' "$ENVELOPE" | LC_ALL=C tr -d '\000-\010\013\014\016-\037\177')"

# JSON-escape via bash parameter substitution, the same single-pass pattern
# session-start-using-fno.sh uses.
escape_for_json() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    printf '%s' "$s"
}

CONTEXT="$(escape_for_json "$ENVELOPE")"

if [[ -n "${CURSOR_PLUGIN_ROOT:-}" ]]; then
    printf '{\n  "additional_context": "%s"\n}\n' "${CONTEXT}"
elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]] && [[ -z "${COPILOT_CLI:-}" ]]; then
    printf '{\n  "hookSpecificOutput": {\n    "hookEventName": "SessionStart",\n    "additionalContext": "%s"\n  }\n}\n' "${CONTEXT}"
else
    printf '{\n  "additionalContext": "%s"\n}\n' "${CONTEXT}"
fi

exit 0
