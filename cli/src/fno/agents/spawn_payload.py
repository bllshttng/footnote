"""Portable output-compression guidance for spawned workers."""

from __future__ import annotations

import os


BREVITY_MARKER = "<fno_relay_compression>"
BREVITY_END_MARKER = "</fno_relay_compression>"
BREVITY_INSTRUCTION = (
    "Keep reports and handoffs at 80 words or fewer unless this payload requires a "
    "longer artifact or exact output schema. Think fully; write only the requested "
    "result, essential reason, and next action. Drop filler, pleasantries, hedges, "
    "repeated context, and articles where clear. Fragments work. Keep technical terms, "
    "commands, errors, numbers, units, negation, and code blocks exact. Put long detail "
    "in durable artifacts when available; return a path or link."
)
BREVITY_BLOCK = f"{BREVITY_MARKER}\n{BREVITY_INSTRUCTION}\n{BREVITY_END_MARKER}"

# The spawner arms the current attempt's bound binding here (absolute path to
# the bound JSON under the existing artifact root). Both launch substrates go
# through prepare_spawn_payload, so pane and non-pane carries stay identical.
TASK_CONTEXT_ENV = "FNO_TASK_CONTEXT_FILE"

TASK_CONTEXT_TAG = "<task-context "

# Rendered natively (task-context-payload): a binding only exists where the
# native verifier runs, so a missing binary means no block, never a Python
# re-implementation of the render.
_TASK_CONTEXT_VERB = "task-context-payload"


def enrich_spawn_payload(message: str) -> str:
    """Append first-party brevity guidance once to a non-empty spawn payload."""
    if not message or BREVITY_BLOCK in message:
        return message
    return f"{message}\n\n{BREVITY_BLOCK}"


def prepare_spawn_payload(message: str) -> tuple[str, dict]:
    """ONE payload-preparation entry for both launch substrates (x-59b0).

    Preserves the original message as the prefix, appends the brevity guidance
    once, then the natively rendered task-context pointer once. payload_bytes
    measures THIS payload, never the sources' bytes.
    """
    payload = enrich_spawn_payload(message)
    measures: dict = {"task_context": False, "payload_bytes": len(payload.encode("utf-8"))}
    path = os.environ.get(TASK_CONTEXT_ENV) or ""
    if path.strip():
        from fno.rust_binary import VerbUnavailable, verb_call

        try:
            block = verb_call(_TASK_CONTEXT_VERB, {"path": path}).get("block")
        except VerbUnavailable:
            block = None
        if block and TASK_CONTEXT_TAG not in payload:
            payload = f"{payload}\n\n{block}"
            measures = {"task_context": True, "payload_bytes": len(payload.encode("utf-8"))}
    return payload, measures
