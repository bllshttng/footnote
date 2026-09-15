"""Eval history reads, graduation, and the health summary.

The report FOLD lives native (fno-agents evals-trend; law d-b6cc1a2a): one
denominator authority, no Python second leg. Python keeps the row reader,
the graduation file rewrite, and the health summary, which reads the native
summary's alarm, `regressed`, pass rate, and flake count.
"""
from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import json

from fno.evals import history as _history
from fno.config import load_settings
from fno.evals.runner import BASELINE


def load_rows(
    history_path: Path, *, since: Optional[int] = None, variant: Optional[str] = "baseline"
) -> list[dict[str, object]]:
    """History rows in order: one round by default (missing key = baseline), ``None`` = all."""
    rows = [r for _, r in _history.iter_rows_tolerant(history_path)
            if variant is None or (r.get("variant") or BASELINE) == variant]
    if since is not None and since >= 0:
        rows = rows[-since:]
    return rows


def _parse_ts(value: object) -> Optional[datetime]:
    """Parse a history row's ``ts`` (ISO-8601, Z or offset), or None."""
    if not isinstance(value, str) or not value.strip():
        return None
    try:
        return datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError:
        return None


def evals_health_summary(
    history_path: Path,
    *,
    stale_days: Optional[int] = None,
    now: Optional[datetime] = None,
    native_reads: bool = True,
) -> Optional[dict[str, Any]]:
    """One-line evals health for triage health and doctor; the demand row.

    None when no history or no rows; never raises. The alarm, regressed set,
    pass rate, and flake count all come from the native summary (one
    denominator authority); an unreachable door degrades to empty/None, never
    a second Python fold.
    """
    if not history_path.exists():
        return None
    rows = load_rows(history_path)
    if not rows:
        return None
    if stale_days is None:
        try:
            stale_days = int(load_settings().evals.stale_days)
        except Exception:  # noqa: BLE001 - the summary never raises
            stale_days = 7
    if now is None:
        now = datetime.now(timezone.utc)
    reg_ts = [dt for r in rows if r.get("tier") == "regression"
              and (dt := _parse_ts(r.get("ts"))) is not None]
    never_ran = not reg_ts and not any(
        r.get("tier") == "regression" for r in rows
    )
    newest_dt = max(reg_ts, default=None)
    age_days = None if newest_dt is None else round(
        (now - newest_dt).total_seconds() / 86400, 3)
    stale = not never_ran and age_days is not None and age_days > stale_days
    alarm, regressed, pass_rate, flake_count = (
        _native_summary_reads(history_path, stale_days)
        if native_reads else ([], [], None, 0)
    )
    return {
        "regression_pass_rate": pass_rate,
        "flake_count": flake_count,
        "regression_alarm": alarm,
        "regressed": regressed,
        "window_days": stale_days,
        "age_days": age_days,
        "stale": stale,
        "never_ran": never_ran,
    }


def _native_summary_reads(
    history_path: Path, stale_days: int
) -> tuple[list[str], list[str], Optional[float], int]:
    """Alarm, `regressed`, tier pass rate, and flake count, read from the
    native evals-trend summary (which owns the denominators); an absent
    binary or a failed read degrades to empty/None/0."""
    import subprocess

    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        return [], [], None, 0
    try:
        proc = subprocess.run(
            [str(binary), "evals-trend", "--mode", "summary",
             "--history", str(history_path), "--stale-days", str(stale_days)],
            capture_output=True, text=True, timeout=30, check=False,
        )
        payload = (
            json.loads(proc.stdout.strip().splitlines()[-1])
            if proc.returncode == 0 and proc.stdout.strip() else {}
        )
        return (
            list(payload.get("regression_alarm") or []),
            list(payload.get("regressed") or []),
            payload.get("regression_pass_rate"),
            int(payload.get("flake_count") or 0),
        )
    except Exception:  # noqa: BLE001 - the summary never raises
        return [], [], None, 0


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
