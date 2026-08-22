"""Portable output-compression guidance for spawned workers."""

from __future__ import annotations


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


def enrich_spawn_payload(message: str) -> str:
    """Append first-party brevity guidance once to a non-empty spawn payload."""
    if not message or BREVITY_BLOCK in message:
        return message
    return f"{message}\n\n{BREVITY_BLOCK}"
