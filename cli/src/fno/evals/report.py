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
from pathlib import Path
from typing import Any, Optional

from fno.evals import history as _history


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


def load_rows(history_path: Path, *, since: Optional[int] = None) -> list[dict[str, object]]:
    """Return history rows in file order.

    ``since`` folds only the most recent N runs (the last N history lines);
    ``None`` folds everything.
    """
    rows = [r for _, r in _history.iter_rows_tolerant(history_path)]
    if since is not None and since >= 0:
        rows = rows[-since:]
    return rows


def _stats(rows: list[dict[str, object]]) -> list[TaskStat]:
    by_id: dict[str, list[dict[str, object]]] = {}
    for r in rows:
        tid = r.get("task_id")
        if isinstance(tid, str):
            by_id.setdefault(tid, []).append(r)
    stats: list[TaskStat] = []
    for tid in sorted(by_id):
        task_rows = by_id[tid]
        # Tier is taken from the most recent row (a graduated task's later rows
        # carry the new tier).
        tier = str(task_rows[-1].get("tier", "unknown"))
        passes = sum(1 for r in task_rows if r.get("pass") is True)
        stats.append(TaskStat(tid, tier, len(task_rows), passes))
    return stats


def build_report(rows: list[dict[str, object]]) -> dict[str, Any]:
    """Fold *rows* into a JSON-friendly report dict."""
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
    # Regression alarm: any regression-tier task not at 100%.
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


def graduation_candidates(rows: list[dict[str, object]], *, n: int = 3) -> list[str]:
    """Capability task ids whose last *n* runs were consecutive passes.

    A candidate must have at least *n* recorded runs and every one of its most
    recent *n* runs must be a pass. Only capability-tier tasks graduate.
    """
    by_id: dict[str, list[dict[str, object]]] = {}
    for r in rows:
        tid = r.get("task_id")
        if isinstance(tid, str):
            by_id.setdefault(tid, []).append(r)
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
