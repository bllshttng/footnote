"""Portable output-compression guidance for spawned workers."""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Optional


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
# the bound JSON under the existing artifact root). Both launch substrates read
# the same variable through prepare_spawn_payload, so pane and non-pane carries
# stay identical by construction.
TASK_CONTEXT_ENV = "FNO_TASK_CONTEXT_FILE"

# Bounded by declaration: at most this many declared constraints ride a
# payload. Source CONTENTS never ride a payload at all.
MAX_PAYLOAD_CONSTRAINTS = 10

TASK_CONTEXT_TAG = "<task-context "


def enrich_spawn_payload(message: str) -> str:
    """Append first-party brevity guidance once to a non-empty spawn payload."""
    if not message or BREVITY_BLOCK in message:
        return message
    return f"{message}\n\n{BREVITY_BLOCK}"


def load_task_context(env: Optional[str] = None) -> Optional[dict]:
    """The spawner's bound binding, when FNO_TASK_CONTEXT_FILE names a readable one.

    A missing, unreadable, or malformed file loads as UNSET (no block rides the
    payload); it never renders as a binding. A declared-but-corrupt binding is
    the native verifier's refusal to report, not this loader's guess.
    """
    path = (env if env is not None else os.environ.get(TASK_CONTEXT_ENV)) or ""
    if not path.strip():
        return None
    try:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return raw if isinstance(raw, dict) else None


def task_context_block(binding: dict) -> Optional[str]:
    """The bounded pointer block: identity + digest + declared constraints.

    Never source contents, never a second skill preamble. The pointer is not a
    read: having this block in a payload proves nothing about what the
    recipient has read (the stage field stays the honest record).
    """
    node = binding.get("node")
    attempt = binding.get("attempt")
    digest = binding.get("binding_digest")
    if not (isinstance(node, str) and node and isinstance(attempt, str) and attempt):
        return None
    if not (isinstance(digest, str) and digest):
        return None
    stage = binding.get("stage") or "prepared"
    constraints = [
        c
        for c in binding.get("required_constraints", [])
        if isinstance(c, str) and c.strip()
    ][:MAX_PAYLOAD_CONSTRAINTS]
    lines = [
        f'{TASK_CONTEXT_TAG}node="{node}" attempt="{attempt}" '
        f'binding_digest="{digest}" stage="{stage}">'
    ]
    if constraints:
        lines.append("Required constraints:")
        lines.extend(f"- {c}" for c in constraints)
    lines.append("</task-context>")
    return "\n".join(lines)


def prepare_spawn_payload(
    message: str, task_context: Optional[dict] = None
) -> tuple[str, dict]:
    """ONE payload-preparation entry for both launch substrates (x-59b0).

    Preserves the original (normalized) message as the prefix, appends the
    brevity guidance once, then the bounded task-context pointer once. Returns
    (payload, measures); measures keeps the two budgets separate -
    payload_bytes is what THIS submission carries, not the sources' bytes.
    """
    payload = enrich_spawn_payload(message)
    binding = task_context if task_context is not None else load_task_context()
    measures: dict = {"task_context": False, "payload_bytes": len(payload.encode("utf-8"))}
    if binding:
        block = task_context_block(binding)
        if block and TASK_CONTEXT_TAG not in payload:
            payload = f"{payload}\n\n{block}"
            measures["task_context"] = True
        measures["payload_bytes"] = len(payload.encode("utf-8"))
    return payload, measures
