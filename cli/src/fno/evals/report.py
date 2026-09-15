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
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional

from fno.evals import history as _history
from fno.config import load_settings
from fno.evals.runner import BASELINE


@dataclass(frozen=True)
class TaskStat:
    task_id: str
    tier: str
    runs: int
    passes: int

    @property
    def pass_at_1(self) -> float:
        return self.passes / self.runs if self.runs else 0.0

    @property
    def pass_k(self) -> bool:
        return self.runs > 0 and self.passes == self.runs

    @property
    def flake(self) -> bool:
        return 0 < self.passes < self.runs


def load_rows(
    history_path: Path, *, since: Optional[int] = None, variant: Optional[str] = "baseline"
) -> list[dict[str, object]]:
    """History rows in order: one round by default (missing key = baseline), ``None`` = all."""
    rows = [r for _, r in _history.iter_rows_tolerant(history_path)
            if variant is None or (r.get("variant") or BASELINE) == variant]
    if since is not None and since >= 0:
        rows = rows[-since:]
    return rows


def _by_task(rows: list[dict[str, object]]) -> dict[str, list[dict[str, object]]]:
    by_id: dict[str, list[dict[str, object]]] = {}
    for r in rows:
        tid = r.get("task_id")
        if isinstance(tid, str):
            by_id.setdefault(tid, []).append(r)
    return by_id


def _stats(rows: list[dict[str, object]]) -> list[TaskStat]:
    by_id = _by_task(rows)
    stats: list[TaskStat] = []
    for tid in sorted(by_id):
        task_rows = by_id[tid]
        current_tier = str(task_rows[-1].get("tier", "unknown"))
        # Only rows SINCE the latest tier change count toward the task's current
        # stats: a freshly-graduated task's pre-graduation capability failures
        # must not inflate its regression pass rate and fire a false alarm the
        # instant it graduates (each row carries the tier it ran under).
        segment: list[dict[str, object]] = []
        for r in reversed(task_rows):
            if str(r.get("tier", "unknown")) != current_tier:
                break
            segment.append(r)
        passes = sum(1 for r in segment if r.get("pass") is True)
        stats.append(TaskStat(tid, current_tier, len(segment), passes))
    return stats


def build_report(
    rows: list[dict[str, object]],
    *,
    now: Optional[datetime] = None,
    window_days: Optional[int] = None,
) -> dict[str, Any]:
    """Fold *rows* into a JSON-friendly report dict.

    With both *now* and *window_days*, ``regression_alarm`` reads only the
    recent window ``(now - window_days, now]`` - a 50-day-old fixed flake no
    longer fires it. ``tiers``, ``tasks`` and ``flakes`` stay the all-rows
    long view either way.
    """
    stats = _stats(rows)

    tier_runs: dict[str, int] = {}
    tier_passes: dict[str, int] = {}
    for s in stats:
        tier_runs[s.tier] = tier_runs.get(s.tier, 0) + s.runs
        tier_passes[s.tier] = tier_passes.get(s.tier, 0) + s.passes

    tiers = {
        tier: {
            "runs": tier_runs[tier],
            "passes": tier_passes[tier],
            "pass_rate": round(tier_passes[tier] / tier_runs[tier], 4) if tier_runs[tier] else 0.0,
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
        }
        for s in stats
    ]
    flakes = [s.task_id for s in stats if s.flake]
    # Regression alarm: any regression-tier task not at 100%. Windowed when a
    # window is given; the tier-segment rule in _stats still holds inside it.
    if now is not None and window_days is not None:
        window = timedelta(days=window_days)
        alarm_stats = _stats(window_rows(rows, now - window, now))
    else:
        alarm_stats = stats
    regression_alarm = [
        s.task_id for s in alarm_stats if s.tier == "regression" and s.pass_at_1 < 1.0
    ]
    return {
        "no_data": not stats,
        "tiers": tiers,
        "tasks": tasks,
        "flakes": flakes,
        "regression_alarm": regression_alarm,
    }


def graduation_candidates(rows: list[dict[str, object]], *, n: int = 3) -> list[str]:
    """Capability task ids whose last *n* runs were consecutive passes.

    A candidate must have at least *n* recorded runs and every one of its most
    recent *n* runs must be a pass. Only capability-tier tasks graduate.
    """
    by_id = _by_task(rows)
    candidates: list[str] = []
    for tid in sorted(by_id):
        task_rows = by_id[tid]
        if str(task_rows[-1].get("tier")) != "capability":
            continue
        if len(task_rows) < n:
            continue
        if all(r.get("pass") is True for r in task_rows[-n:]):
            candidates.append(tid)
    return candidates


def _common_rev(rs: list[dict[str, object]]) -> Optional[str]:
    revs = [v for r in rs if isinstance(v := r.get("bank_rev"), str)]
    return max(sorted(set(revs)), key=revs.count) if revs else None


def compare_variants(rows: list[dict[str, object]], variant: str) -> dict[str, Any]:
    """Score *variant* against baseline at one revision pair (rows from variant=None)."""
    by_id = _by_task(rows)
    baseline_rev = _common_rev([r for r in rows if (r.get("variant") or BASELINE) == BASELINE])
    variant_rev = _common_rev([r for r in rows if (r.get("variant") or BASELINE) == variant])
    tasks: dict[str, Any] = {}
    missing_in_variant: list[str] = []
    missing_in_baseline: list[str] = []
    for tid, task_rows in sorted(by_id.items()):
        b = [r for r in task_rows if (r.get("variant") or BASELINE) == BASELINE
             and r.get("bank_rev") == baseline_rev]
        v = [r for r in task_rows if (r.get("variant") or BASELINE) == variant
             and r.get("bank_rev") == variant_rev]
        if not b:
            missing_in_baseline.append(tid)
        if not v:
            missing_in_variant.append(tid)
        if not b or not v:
            continue
        score = _pair_verdict(b, v)
        tasks[tid] = {"baseline": score["a"], "variant": score["b"],
                      "delta": score["delta"], "verdict": score["verdict"]}
    return {
        "variant": variant,
        "tasks": tasks,
        "missing_in_variant": missing_in_variant,
        "missing_in_baseline": missing_in_baseline,
        "baseline_rev": baseline_rev,
        "variant_rev": variant_rev,
    }


def _pair_verdict(a_rows: list[dict[str, object]],
                  b_rows: list[dict[str, object]]) -> dict[str, Any]:
    """Score one row list against another: runs and pass@1 per side, delta
    (b - a) and the improved/regressed/unchanged verdict. Shared by
    compare_variants (revision axis) and compare_windows (time axis)."""
    a_p1 = sum(1 for r in a_rows if r.get("pass") is True) / len(a_rows)
    b_p1 = sum(1 for r in b_rows if r.get("pass") is True) / len(b_rows)
    delta = b_p1 - a_p1
    verdict = "improved" if delta > 0 else "regressed" if delta < 0 else "unchanged"
    return {"a": {"runs": len(a_rows), "pass_at_1": round(a_p1, 4)},
            "b": {"runs": len(b_rows), "pass_at_1": round(b_p1, 4)},
            "delta": round(delta, 4), "verdict": verdict}


def window_rows(
    rows: list[dict[str, object]], start: datetime, end: datetime
) -> list[dict[str, object]]:
    """Rows whose ``ts`` parses and falls in the half-open ``(start, end]``;
    a row with no parseable ``ts`` lands in no window."""
    return [
        r for r in rows
        if (dt := _parse_ts(r.get("ts"))) is not None and start < dt <= end
    ]


def compare_windows(
    rows: list[dict[str, object]], *, window_days: int, now: datetime
) -> dict[str, Any]:
    """Per-task trend: recent ``(now - W, now]`` against prior ``(now - 2W, now - W]``.

    The time-axis twin of :func:`compare_variants`: weeks, not git refs.
    ``regressed`` names regression-tier tasks (tier of each task's newest row)
    whose verdict is ``regressed``.
    """
    w = timedelta(days=window_days)
    prior = _by_task(window_rows(rows, now - 2 * w, now - w))
    recent = _by_task(window_rows(rows, now - w, now))
    tasks: dict[str, Any] = {}
    missing_in_prior: list[str] = []
    missing_in_recent: list[str] = []
    for tid in sorted(set(prior) | set(recent)):
        p, r = prior.get(tid, []), recent.get(tid, [])
        if not p:
            missing_in_prior.append(tid)
        if not r:
            missing_in_recent.append(tid)
        if p and r:
            s = _pair_verdict(p, r)
            tasks[tid] = {"prior": s["a"], "recent": s["b"],
                          "delta": s["delta"], "verdict": s["verdict"]}
    newest_tier = {tid: str(tr[-1].get("tier", "unknown"))
                   for tid, tr in _by_task(rows).items()}
    regressed = [tid for tid, t in tasks.items()
                 if t["verdict"] == "regressed" and newest_tier.get(tid) == "regression"]
    return {
        "window_days": window_days,
        "prior_start": (now - 2 * w).isoformat(),
        "recent_start": (now - w).isoformat(),
        "tasks": tasks,
        "missing_in_prior": missing_in_prior,
        "missing_in_recent": missing_in_recent,
        "regressed": regressed,
    }


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
) -> Optional[dict[str, Any]]:
    """One-line evals health for triage health and doctor; the demand row.

    None when no history or no rows; never raises. Semantics in docs/evals.md.
    """
    if not history_path.exists():
        return None
    rows = load_rows(history_path)
    if stale_days is None:
        try:
            stale_days = int(load_settings().evals.stale_days)
        except Exception:  # noqa: BLE001 - the summary never raises
            stale_days = 7
    if now is None:
        now = datetime.now(timezone.utc)
    report = build_report(rows, now=now, window_days=stale_days)
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
    try:
        regressed = compare_windows(
            rows, window_days=stale_days, now=now
        )["regressed"]
    except Exception:  # noqa: BLE001 - the summary never raises
        regressed = []
    return {
        "regression_pass_rate": reg["pass_rate"] if reg else None,
        "flake_count": len(report["flakes"]),
        "regression_alarm": report["regression_alarm"],
        "regressed": regressed,
        "window_days": stale_days,
        "age_days": age_days,
        "stale": stale,
        "never_ran": never_ran,
    }


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
