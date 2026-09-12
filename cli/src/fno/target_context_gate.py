"""Task-context gate for target init (x-59b0).

Extracted from target_cli so the oversized module keeps shrinking. For a
DECLARED required binding - ``FNO_TASK_CONTEXT_FILE`` naming a bound binding
JSON - the gate runs the NATIVE revalidation (validation, digest, live source
bytes) BEFORE init acquires the node claim or the target seed is submitted.
Every enforced decision lives in the Rust verifier; this module is transport
plus the named refusal.

An absent or undeclared binding is ordinary behavior: no env var, no gate.
A DECLARED-but-unreadable binding refuses: naming a binding and then failing
to prove it must never read as absent. The gate never mutates claims - init
simply exits before the init script runs, so an existing owner is preserved.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Optional

TASK_CONTEXT_ENV = "FNO_TASK_CONTEXT_FILE"


class TaskContextGateRefused(RuntimeError):
    """A declared required binding did not revalidate. ``reason`` is the
    machine-readable name (context_stale_source, context_missing_source,
    context_binding_unreadable, ...); ``detail`` carries the native answer."""

    def __init__(self, reason: str, detail: str = "") -> None:
        super().__init__(reason)
        self.reason = reason
        self.detail = detail


def gate_declared_task_context(
    node_id: str,
    worktree: str,
    *,
    env: Optional[dict] = None,
    expect_extra: Optional[dict] = None,
) -> Optional[dict[str, Any]]:
    """Revalidate the declared binding natively. Ok -> the native answer;
    no binding declared -> None (ordinary behavior); anything else -> refusal.
    """
    source = env if env is not None else os.environ
    path = (source.get(TASK_CONTEXT_ENV) or "").strip()
    if not path:
        return None
    try:
        binding = json.loads(Path(path).expanduser().read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise TaskContextGateRefused("context_binding_unreadable", str(exc)) from exc
    if not isinstance(binding, dict):
        raise TaskContextGateRefused(
            "context_binding_unreadable", "binding file is not a JSON object"
        )
    from fno.rust_binary import VerbUnavailable, verb_call

    expect = {"node": node_id}
    expect.update(expect_extra or {})
    try:
        answer = verb_call(
            "task-context-revalidate",
            {"binding": binding, "expect": expect, "root": worktree},
        )
    except VerbUnavailable as exc:
        raise TaskContextGateRefused("context_native_verifier_unavailable", str(exc)) from exc
    if not answer.get("ok"):
        reason = answer.get("reason") or "refused"
        # The native reason may carry detail after a colon ("stale_source:
        # PLAN.md"); the refusal NAME is the code before it.
        base = reason.split(":", 1)[0]
        raise TaskContextGateRefused(
            base if base.startswith("context_") else f"context_{base}",
            json.dumps(answer),
        )
    return answer
