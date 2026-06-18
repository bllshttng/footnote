"""fno worker blueprint - signal that LLM work is needed.

The CLI does NOT write feature code. It emits a dispatch action so the
skill layer can invoke the appropriate Agent tool, then resume via
`fno runtime register-worker`.
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
