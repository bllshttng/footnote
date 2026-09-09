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
from typing import Any, Optional, Sequence

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
        b_p1 = sum(1 for r in b if r.get("pass") is True) / len(b)
        v_p1 = sum(1 for r in v if r.get("pass") is True) / len(v)
        delta = v_p1 - b_p1
        verdict = "improved" if delta > 0 else "regressed" if delta < 0 else "unchanged"
        tasks[tid] = {"baseline": {"runs": len(b), "pass_at_1": round(b_p1, 4)},
                      "variant": {"runs": len(v), "pass_at_1": round(v_p1, 4)},
                      "delta": round(delta, 4), "verdict": verdict}
    return {
        "variant": variant,
        "tasks": tasks,
        "missing_in_variant": missing_in_variant,
        "missing_in_baseline": missing_in_baseline,
        "baseline_rev": baseline_rev,
        "variant_rev": variant_rev,
    }


@dataclass(frozen=True)
class CohortSpec:
    """One predeclared comparison-group member (x-fd52 wave 2).

    Declared BEFORE the first run: a row joins a cohort only by its recorded
    ``experiment_id`` (wave 1), never by post-hoc grouping. ``fixture_rev``
    and ``observation_window`` are the predeclared bounds; a run that drifts
    from them is flagged, never silently absorbed.
    """

    id: str
    repeats: int
    fixture_rev: str = ""
    observation_window: str = ""
    stopping_rule: str = ""
    budget_usd: Optional[float] = None


def _duration_stats(values: list[float]) -> Optional[dict[str, float]]:
    if not values:
        return None
    return {
        "min": round(min(values), 3),
        "max": round(max(values), 3),
        "mean": round(sum(values) / len(values), 3),
    }


def _fold_usage(rows: list[dict[str, object]]) -> Optional[list[dict[str, object]]]:
    """Sum usage by (source, unit) - distinct units never averaged together.

    ``None`` means no row in the cohort carried usage (unobserved, not zero).
    """
    by_key: dict[tuple[str, str], float] = {}
    for r in rows:
        usage = r.get("usage")
        if not isinstance(usage, dict):
            continue
        source = str(usage.get("source", "unknown"))
        unit = str(usage.get("unit", "unknown"))
        amount = usage.get("amount")
        if isinstance(amount, (int, float)):
            by_key[(source, unit)] = by_key.get((source, unit), 0.0) + float(amount)
    if not by_key:
        return None
    return [
        {"source": s, "unit": u, "amount": round(v, 6)}
        for (s, u), v in sorted(by_key.items())
    ]


def compare_cohorts(
    rows: list[dict[str, object]],
    cohorts: Sequence[CohortSpec],
    *,
    promotion_criteria: Optional[dict[str, object]] = None,
) -> dict[str, Any]:
    """Compare predeclared configuration cohorts (x-fd52 AC2-HP/AC2-EDGE).

    A row with no ``experiment_id`` match or no ``requested_lane`` is
    legacy/unattributed and is never folded into a cohort's score - it is
    counted separately (AC2-EDGE). Only ``lane_status == "ok"`` rows count
    toward sample/reliability; a ``substituted`` or ``unavailable`` row is
    excluded and its count reported explicitly, never silently dropped.

    Missing review or usage evidence reads as ``unobserved``/``None``, never
    as clean or zero-cost (AC2-EDGE). Review findings and usage are folded
    only from whatever a row already carries (an eval worker's own mechanical
    grade is the only success marker this module ever invents - review/usage
    evidence is attached by whatever caller has it, per docs/evals.md).
    """
    by_cohort: dict[str, list[dict[str, object]]] = {c.id: [] for c in cohorts}
    unattributed = 0
    for r in rows:
        cid = r.get("experiment_id")
        bucket = by_cohort.get(cid) if isinstance(cid, str) else None
        if bucket is not None and r.get("requested_lane"):
            bucket.append(r)
        else:
            unattributed += 1

    out_cohorts: dict[str, Any] = {}
    for spec in cohorts:
        crows = by_cohort[spec.id]
        scored = [r for r in crows if r.get("lane_status") == "ok"]
        lanes = sorted({str(l) for r in crows if (l := r.get("requested_lane"))})
        revs = {str(v) for r in crows if (v := r.get("bank_rev"))}
        runs = len(scored)
        passes = sum(1 for r in scored if r.get("pass") is True)
        durations = [float(d) for r in scored
                     if isinstance(d := r.get("duration_s"), (int, float))]
        review_rows = [r for r in scored if isinstance(r.get("review"), dict)]
        review_evidence = (
            "observed" if review_rows and len(review_rows) == runs
            else "partial" if review_rows else "unobserved"
        )
        out_cohorts[spec.id] = {
            "lanes": lanes,
            "declared_repeats": spec.repeats,
            "observation_window": spec.observation_window,
            "stopping_rule": spec.stopping_rule,
            "budget_usd": spec.budget_usd,
            "sample_count": runs,
            "pass_at_1": round(passes / runs, 4) if runs else None,
            "pass_k": runs > 0 and passes == runs,
            "duration_s": _duration_stats(durations),
            "excluded_substituted": sum(1 for r in crows if r.get("lane_status") == "substituted"),
            "excluded_unavailable": sum(1 for r in crows if r.get("lane_status") == "unavailable"),
            "fixture_revisions": sorted(revs),
            "mixed_fixture_revisions": len(revs) > 1,
            "fixture_rev_mismatch": bool(spec.fixture_rev) and revs not in ({spec.fixture_rev}, set()),
            "review_evidence": review_evidence,
            "review_findings": (
                sum(r["review"].get("findings", 0) for r in review_rows)  # type: ignore[attr-defined]
                if review_rows else None
            ),
            "usage": _fold_usage(scored),
        }

    promotion = (
        _recommend_promotion(out_cohorts, promotion_criteria)
        if promotion_criteria else None
    )
    return {"cohorts": out_cohorts, "unattributed_count": unattributed, "promotion": promotion}


def _recommend_promotion(
    cohorts: dict[str, Any], criteria: dict[str, object]
) -> dict[str, Any]:
    """Score one candidate against one baseline by predeclared criteria only.

    Never a silent green light: any missing cohort, any unmet threshold, a
    fixture-revision mismatch, or (when required) unobserved review blocks
    promotion with a stated reason. This never touches production lane
    config - it only recommends; promotion is always a separate, reviewed act.
    """
    baseline_id = criteria.get("baseline")
    candidate_id = criteria.get("candidate")
    reasons: list[str] = []
    baseline = cohorts.get(baseline_id) if isinstance(baseline_id, str) else None
    candidate = cohorts.get(candidate_id) if isinstance(candidate_id, str) else None
    if baseline is None or candidate is None:
        reasons.append("baseline or candidate cohort missing from this comparison")
        return {"baseline": baseline_id, "candidate": candidate_id,
                "recommended": False, "reasons": reasons}

    min_pass_at_1 = criteria.get("min_pass_at_1")
    if candidate["pass_at_1"] is None:
        reasons.append("candidate has no scored samples")
    elif isinstance(min_pass_at_1, (int, float)) and candidate["pass_at_1"] < min_pass_at_1:
        reasons.append(f"candidate pass_at_1 {candidate['pass_at_1']} < required {min_pass_at_1}")
    if baseline["pass_at_1"] is not None and candidate["pass_at_1"] is not None:
        if candidate["pass_at_1"] < baseline["pass_at_1"]:
            reasons.append("candidate regresses against baseline pass_at_1")
    if candidate["mixed_fixture_revisions"] or baseline["mixed_fixture_revisions"]:
        reasons.append("comparison is not like-for-like: mixed fixture revisions")
    if candidate["fixture_rev_mismatch"] or baseline["fixture_rev_mismatch"]:
        reasons.append("observed fixture revision does not match the predeclared one")
    if criteria.get("require_review") and candidate["review_evidence"] != "observed":
        reasons.append("required review evidence is unobserved for the candidate")
    return {
        "baseline": baseline_id, "candidate": candidate_id,
        "recommended": not reasons, "reasons": reasons,
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
    report = build_report(rows)
    if report["no_data"]:
        return None
    reg = report["tiers"].get("regression")
    if stale_days is None:
        try:
            stale_days = int(load_settings().evals.stale_days)
        except Exception:  # noqa: BLE001 - the summary never raises
            stale_days = 7
    if now is None:
        now = datetime.now(timezone.utc)
    reg_ts = [dt for r in rows if r.get("tier") == "regression"
              and (dt := _parse_ts(r.get("ts"))) is not None]
    newest_dt = max(reg_ts) if reg_ts else None
    never_ran = reg is None
    age_days = None if newest_dt is None else round(
        (now - newest_dt).total_seconds() / 86400, 3)
    stale = never_ran is False and age_days is not None and age_days > stale_days
    return {
        "regression_pass_rate": reg["pass_rate"] if reg else None,
        "flake_count": len(report["flakes"]),
        "regression_alarm": report["regression_alarm"],
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
