"""Eval history graduation and the health summary.

The FOLD lives native (fno-agents evals-trend; law d-b6cc1a2a): one
denominator authority, no Python second leg. Python keeps the graduation
file rewrite and the health summary, a thin read of the native summary's
JSON.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Optional

from fno.config import load_settings


def evals_health_summary(
    history_path: Path,
    *,
    stale_days: Optional[int] = None,
) -> Optional[dict[str, Any]]:
    """One-line evals health for triage health and doctor; the demand row.

    None when no history exists, no baseline rows are in it, or the native
    door is unreachable; never raises. Every field is the native summary's
    (one denominator authority); Python never re-folds.
    """
    if not history_path.exists():
        return None
    if stale_days is None:
        try:
            stale_days = int(load_settings().evals.stale_days)
        except Exception:  # noqa: BLE001 - the summary never raises
            stale_days = 7
    payload = _native_summary(history_path, stale_days)
    if payload is None or not payload.get("row_count"):
        return None
    return {
        "regression_pass_rate": payload.get("regression_pass_rate"),
        "flake_count": int(payload.get("flake_count") or 0),
        "regression_alarm": list(payload.get("regression_alarm") or []),
        "regressed": list(payload.get("regressed") or []),
        "window_days": stale_days,
        "age_days": payload.get("age_days"),
        "stale": bool(payload.get("stale") or False),
        "never_ran": bool(payload.get("never_ran") or False),
    }


def _native_summary(history_path: Path, stale_days: int) -> Optional[dict[str, Any]]:
    """The native summary payload (fno-agents evals-trend, stdin summary
    op); None when the door is unreachable or answers a non-dict."""
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        payload = verb_call("evals-trend", {
            "op": "summary", "history": str(history_path), "stale_days": stale_days,
        })
    except VerbUnavailable:
        return None
    return payload if isinstance(payload, dict) else None


def _native_qualification(history_path: Path, manifest_path: Path) -> Optional[dict[str, Any]]:
    """The native qualification projection (fno-agents evals-trend, stdin
    qualification op); None when the door is unreachable or answers a
    non-dict. A missing history file is valid input: the fold answers with
    every scenario missing, which is the honest nothing-ran report."""
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        payload = verb_call("evals-trend", {
            "op": "qualification",
            "history": str(history_path),
            "qualification": str(manifest_path),
        })
    except VerbUnavailable:
        return None
    return payload if isinstance(payload, dict) else None


def qualification_summary(
    manifest_path: Path,
    history_path: Optional[Path] = None,
) -> Optional[dict[str, Any]]:
    """One read of the release qualification projection: the declared
    expected set joined against eval history by the native fold. None when
    the native door is unreachable; never raises, never re-folds."""
    if history_path is None:
        from fno import paths as _paths

        history_path = _paths.evals_history()
    return _native_qualification(history_path, manifest_path)


class GraduateError(ValueError):
    """The task cannot be graduated (not found, or not capability-tier)."""


def graduate_task_file(task_path: Path) -> None:
    """Rewrite *task_path*'s ``tier: capability`` to ``tier: regression`` in place.

    A line-level rewrite (not a YAML round-trip) so comments and formatting
    survive. Raises :class:`GraduateError` if the file is not capability-tier.
    """
    import re

    text = task_path.read_text(encoding="utf-8")
    new_text, count = re.subn(
        r"(?m)^(\s*tier:\s*)capability(\s*(?:#.*)?)$",
        r"\1regression\2",
        text,
    )
    if count == 0:
        raise GraduateError(
            f"{task_path}: no `tier: capability` line to graduate "
            f"(already regression, or non-standard formatting)"
        )
    task_path.write_text(new_text, encoding="utf-8")
