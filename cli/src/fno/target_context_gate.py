"""Task-context gate: a DECLARED required binding (``FNO_TASK_CONTEXT_FILE``)
revalidates natively before init acquires the node claim. Undeclared is
ordinary behavior; every enforced decision lives in the Rust verifier."""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Optional

TASK_CONTEXT_ENV = "FNO_TASK_CONTEXT_FILE"


class TaskContextGateRefused(RuntimeError):
    """A declared binding did not revalidate; ``reason`` is the machine name,
    ``detail`` the native answer."""

    def __init__(self, reason: str, detail: str = "") -> None:
        super().__init__(reason)
        self.reason = reason
        self.detail = detail


def gate_declared_task_context(
    node_id: str,
    worktree: str,
    *,
    env: Optional[dict] = None,
    binding: Optional[dict[str, Any]] = None,
    expect: Optional[dict] = None,
) -> Optional[dict[str, Any]]:
    """Revalidate natively. With no ``binding`` in hand, the declared env path
    is loaded; nothing declared -> None. Ok -> the native answer, else
    TaskContextGateRefused whose reason carries the ``context_`` prefix."""
    if binding is None:
        source = env if env is not None else os.environ
        path = (source.get(TASK_CONTEXT_ENV) or "").strip()
        if not path:
            return None
        try:
            binding = json.loads(Path(path).expanduser().read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise TaskContextGateRefused("context_binding_unreadable", str(exc)) from exc
        if not isinstance(binding, dict):
            raise TaskContextGateRefused("context_binding_unreadable", "binding file is not a JSON object")
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        answer = verb_call(
            "task-context-revalidate",
            {"binding": binding, "expect": {"node": node_id, **(expect or {})}, "root": worktree},
        )
    except VerbUnavailable as exc:
        raise TaskContextGateRefused("context_native_verifier_unavailable", str(exc)) from exc
    if not answer.get("ok"):
        base = (answer.get("reason") or "refused").split(":", 1)[0]
        raise TaskContextGateRefused(
            base if base.startswith("context_") else f"context_{base}",
            json.dumps(answer),
        )
    return answer
