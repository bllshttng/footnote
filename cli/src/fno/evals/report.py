"""Fold evals history into a pass^k reliability report + graduation logic.

Per task, over the folded window:
- ``runs``       = number of recorded runs
- ``passes``     = number that passed
- ``pass_at_1``  = passes / runs (single-run success rate)
- ``pass_k``     = passes == runs (every run passed)
- ``flake``      = 0 < passes < runs (passed sometimes, not always)

Two consumers key off this: the regression alarm (any regression-tier task
below 100%) and graduation (a capability task that passed its last N runs).
"""
from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import json

from fno.evals import history as _history
from fno.config import load_settings
from fno.evals.runner import BASELINE


@dataclass(frozen=True)
class TaskStat:
    task_id: str
    tier: str
    runs: int
    passes: int
    grades: int = 0
    infrastructure: int = 0
    unavailable: int = 0
    ungraded: int = 0
    legacy: int = 0
    legacy_fold: bool = True

    @property
    def grade_denominator(self) -> int:
        """Correctness denominator: valid grades for a modern task, attempts
        for an all-legacy task (the pre-attempt boolean fold)."""
        return self.runs if self.legacy_fold else self.grades

    @property
    def pass_at_1(self) -> float:
        return self.passes / self.grade_denominator if self.grade_denominator else 0.0

    @property
    def pass_k(self) -> bool:
        den = self.grade_denominator
        return den > 0 and self.passes == den

    @property
    def flake(self) -> bool:
        den = self.grade_denominator
        return 0 < self.passes < den


def load_rows(
    history_path: Path, *, since: Optional[int] = None, variant: Optional[str] = "baseline"
) -> list[dict[str, object]]:
    """History rows in order: one round by default (missing key = baseline), ``None`` = all."""
    rows = [r for _, r in _history.iter_rows_tolerant(history_path)
            if variant is None or (r.get("variant") or BASELINE) == variant]
    if since is not None and since >= 0:
        rows = rows[-since:]
    return rows


def _by_task(rows: list[dict[str, object]]) -> dict[str, list[tuple[int, dict[str, object]]]]:
    """Group rows by task id, keeping each row's GLOBAL position (the key the
    native verdicts arrive under)."""
    by_id: dict[str, list[tuple[int, dict[str, object]]]] = {}
    for i, r in enumerate(rows):
        tid = r.get("task_id")
        if isinstance(tid, str):
            by_id.setdefault(tid, []).append((i, r))
    return by_id


def _stats(
    rows: list[dict[str, object]],
    verdicts: Optional[dict[int, dict]] = None,
) -> list[TaskStat]:
    """Per-task stats with the attempt-aware denominators (x-ecda).

    *verdicts* maps each row's position to the NATIVE attempt verdict
    (`{"status": ..., "graded": ...}`). A task with any classified row
    reports correctness over its valid grades; an all-legacy task keeps the
    pre-attempt boolean fold, never reinterpreting legacy evidence.
    """
    by_id = _by_task(rows)
    stats: list[TaskStat] = []
    for tid in sorted(by_id):
        task_rows = by_id[tid]
        current_tier = str(task_rows[-1][1].get("tier", "unknown"))
        # Only rows SINCE the latest tier change count toward the task's current
        # stats: a freshly-graduated task's pre-graduation capability failures
        # must not inflate its regression pass rate and fire a false alarm the
        # instant it graduates (each row carries the tier it ran under).
        segment: list[tuple[int, dict[str, object]]] = []
        for i, r in reversed(task_rows):
            if str(r.get("tier", "unknown")) != current_tier:
                break
            segment.append((i, r))
        passes = 0
        grades = 0
        infra = 0
        unavail = 0
        ungraded = 0
        legacy = 0
        for pos, r in segment:
            v = (verdicts or {}).get(pos)
            status = str(v.get("status")) if isinstance(v, dict) and v.get("status") else "legacy"
            if status == "graded":
                grades += 1
                if r.get("pass") is True:
                    passes += 1
            elif status == "infrastructure":
                infra += 1
            elif status == "unavailable":
                unavail += 1
            elif status == "ungraded":
                ungraded += 1
            else:
                legacy += 1
        all_legacy = grades + infra + unavail + ungraded == 0
        if all_legacy:
            passes = sum(1 for _pos, r in segment if r.get("pass") is True)
        stats.append(TaskStat(
            task_id=tid, tier=current_tier,
            runs=len(segment),
            passes=passes,
            grades=grades,
            infrastructure=infra, unavailable=unavail, ungraded=ungraded,
            legacy=legacy, legacy_fold=all_legacy,
        ))
    return stats


def build_report(
    rows: list[dict[str, object]],
    attempt_verdicts: Optional[dict[int, dict]] = None,
) -> dict[str, Any]:
    """Fold *rows* into a JSON-friendly report dict (all-rows alarm; the
    windowed read lives in the native evals-trend fold, d-b6cc1a2a).
    *attempt_verdicts* optionally carries the native per-row verdicts keyed
    by row position; absent verdicts read as legacy rows."""
    stats = _stats(rows, attempt_verdicts)

    tier_runs: dict[str, int] = {}
    tier_passes: dict[str, int] = {}
    tier_dens: dict[str, int] = {}
    for s in stats:
        tier_runs[s.tier] = tier_runs.get(s.tier, 0) + s.runs
        tier_passes[s.tier] = tier_passes.get(s.tier, 0) + s.passes
        tier_dens[s.tier] = tier_dens.get(s.tier, 0) + s.grade_denominator

    tiers = {
        tier: {
            "runs": tier_runs[tier],
            "passes": tier_passes[tier],
            "pass_rate": round(tier_passes[tier] / tier_dens[tier], 4) if tier_dens[tier] else 0.0,
        }
        for tier in sorted(tier_runs)
    }

    tasks = [
        {
            "task_id": s.task_id,
            "tier": s.tier,
            "runs": s.runs,
            "passes": s.passes,
            "pass_at_1": round(s.pass_at_1, 4),
            "pass_k": s.pass_k,
            "flake": s.flake,
            "grades": s.grades,
            "attempts": {
                "infrastructure": s.infrastructure,
                "unavailable": s.unavailable,
                "ungraded": s.ungraded,
                "legacy": s.legacy,
            },
            "legacy_fold": s.legacy_fold,
        }
        for s in stats
    ]
    flakes = [s.task_id for s in stats if s.flake]
    # Alarm: any regression-tier task not at 100% (the windowed read is native).
    regression_alarm = [
        s.task_id for s in stats if s.tier == "regression" and s.pass_at_1 < 1.0
    ]
    return {
        "no_data": not stats,
        "tiers": tiers,
        "tasks": tasks,
        "flakes": flakes,
        "regression_alarm": regression_alarm,
    }


def _parse_ts(value: object) -> Optional[datetime]:
    """Parse a history row's ``ts`` (ISO-8601, Z or offset), or None."""
    if not isinstance(value, str) or not value.strip():
        return None
    try:
        return datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError:
        return None


def _native_attempt_verdicts(history_path: Path) -> dict[int, dict]:
    """Line-keyed native attempt verdicts for a history file; {} when the
    native door is unreachable (the fold then reads every row as legacy -
    exactly the pre-attempt semantics)."""
    import subprocess

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None or not history_path.exists():
        return {}
    try:
        proc = subprocess.run(
            [str(binary), "evals-attempt", "--rows", str(history_path)],
            capture_output=True, text=True, timeout=30, check=False,
        )
        if proc.returncode != 0:
            return {}
        payload = json.loads(proc.stdout.strip().splitlines()[-1])
        return {int(v["line"]): v for v in payload}
    except Exception:  # noqa: BLE001 - the summary never raises
        return {}


def evals_health_summary(
    history_path: Path,
    *,
    stale_days: Optional[int] = None,
    now: Optional[datetime] = None,
    native_reads: bool = True,
) -> Optional[dict[str, Any]]:
    """One-line evals health for triage health and doctor; the demand row.

    None when no history or no rows; never raises. Semantics in docs/evals.md.
    """
    if not history_path.exists():
        return None
    rows = load_rows(history_path)
    verdicts: dict[int, dict] = {}
    if native_reads:
        native = _native_attempt_verdicts(history_path)
        if native:
            baseline_lines = [
                ln for ln, r in _history.iter_rows_tolerant(history_path)
                if (r.get("variant") or BASELINE) == BASELINE
            ]
            verdicts = {
                pos: native[ln] for pos, ln in enumerate(baseline_lines) if ln in native
            }
    if stale_days is None:
        try:
            stale_days = int(load_settings().evals.stale_days)
        except Exception:  # noqa: BLE001 - the summary never raises
            stale_days = 7
    if now is None:
        now = datetime.now(timezone.utc)
    report = build_report(rows, verdicts)
    if report["no_data"]:
        return None
    reg = report["tiers"].get("regression")
    reg_ts = [dt for r in rows if r.get("tier") == "regression"
              and (dt := _parse_ts(r.get("ts"))) is not None]
    never_ran = reg is None
    newest_dt = max(reg_ts, default=None)
    age_days = None if newest_dt is None else round(
        (now - newest_dt).total_seconds() / 86400, 3)
    stale = not never_ran and age_days is not None and age_days > stale_days
    alarm, regressed = (
        _native_summary_reads(history_path, stale_days) if native_reads else ([], [])
    )
    return {
        "regression_pass_rate": reg["pass_rate"] if reg else None,
        "flake_count": len(report["flakes"]),
        "regression_alarm": alarm,
        "regressed": regressed,
        "window_days": stale_days,
        "age_days": age_days,
        "stale": stale,
        "never_ran": never_ran,
    }


def _native_summary_reads(history_path: Path, stale_days: int) -> tuple[list[str], list[str]]:
    """The windowed alarm and `regressed`, read from the native evals-trend
    fold; an absent binary or a failed read degrades to empty lists."""
    import subprocess

    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        return [], []
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
        return list(payload.get("regression_alarm") or []), list(payload.get("regressed") or [])
    except Exception:  # noqa: BLE001 - the summary never raises
        return [], []


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
