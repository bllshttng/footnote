"""The ``fno worker blueprint`` dispatch action: the CLI does NOT write
feature code itself - it emits a structured action the skill layer turns into
an Agent tool call, then the session resumes via
``fno agents workspace register-worker``.

The territory feed is native Rust (crates/fno-agents/src/territory.rs): the
supervisor reads ideas, spawns the standing worker, and mails delivery itself.
"""
from __future__ import annotations

from typing import Any


def blueprint(plan_path: str) -> dict[str, Any]:
    """Return an llm_blueprint dispatch action.

    Args:
        plan_path: Path to the plan file or folder.

    Returns:
        {"action": "llm_blueprint", "plan_path": str, "next_step": str}
    """
    return {
        "action": "llm_blueprint",
        "plan_path": plan_path,
        "next_step": "re-enter after skill dispatch",
    }
