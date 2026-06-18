"""Reporting logic for ``fno evals report`` and ``fno evals diff``.

Kept separate from cli.py so it can be unit-tested without Typer.

Public entry points
-------------------
``render_report(rows, *, staleness_days, task_filter, window) -> str``
    Produce the text output for ``fno evals report``.

``render_diff(rows_a, rows_b, *, label_a, label_b) -> tuple[str, int]``
    Produce ``(text, exit_code)`` for ``fno evals diff``.
    Exit code 0 when a comparison was produced; 1 when no common tasks.

Both functions accept a flat list of rows loaded by
``history.iter_rows_tolerant`` - they do their own latest-per-key grouping.
"""
from __future__ import annotations

from datetime import datetime, timezone, timedelta
from typing import Optional


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

# Termination reasons that indicate a "clean" completion suitable for a plain
# PASS display.  Anything else with passing assertions is flagged PASS*.
_GOOD_TERMINATION = frozenset({"DoneAdvisory", "DonePRGreen"})

# Default staleness window in days (Open Question 2 from the design doc).
DEFAULT_STALENESS_DAYS = 14

# Number of recent runs to include in the trend window.
TREND_WINDOW = 5


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _parse_ts(ts_str: str) -> Optional[datetime]:
    """Parse an ISO-8601 UTC timestamp string. Returns None on failure."""
    if not ts_str:
        return None
    try:
        # Handle trailing Z or +00:00
        normalized = ts_str.replace("Z", "+00:00")
        return datetime.fromisoformat(normalized)
    except (ValueError, AttributeError):
        return None


def _latest_per(rows: list[dict], key_fields: tuple) -> dict[tuple, dict]:
    """Return the latest row (by ts) per unique key_fields combination.

    When timestamps are equal or unparseable, later rows in file order win.
    """
    result: dict[tuple, dict] = {}
    for row in rows:
        key = tuple(row.get(f) for f in key_fields)
        existing = result.get(key)
        if existing is None:
            result[key] = row
            continue
        # Compare timestamps; later wins.
        # When ts_new is unparseable (None), later file order wins per docstring.
        ts_new = _parse_ts(row.get("ts", ""))
        ts_old = _parse_ts(existing.get("ts", ""))
        if ts_new is not None and ts_old is not None and ts_new <= ts_old:
            continue
        result[key] = row
    return result


def _pass_status(row: dict) -> str:
    """Return the display status string for a row.

    - FAIL  when passed is False
    - PASS* when passed is True but termination_reason is not a good reason
    - PASS  when passed is True and termination_reason is good
    """
    if not row.get("passed", False):
        return "FAIL"
    reason = row.get("termination_reason", "")
    if reason not in _GOOD_TERMINATION:
        return "PASS*"
    return "PASS"


def _format_cost(cost_usd: object) -> str:
    if cost_usd is None:
        return "-"
    try:
        return f"${float(cost_usd):.2f}"
    except (TypeError, ValueError):
        return str(cost_usd)


def _format_tokens(tokens: object) -> str:
    if tokens is None:
        return "-"
    try:
        v = int(tokens)
        return f"{v:,}"
    except (TypeError, ValueError):
        return str(tokens)


# ---------------------------------------------------------------------------
# Report renderer
# ---------------------------------------------------------------------------


def render_report(
    rows: list[dict],
    *,
    staleness_days: int = DEFAULT_STALENESS_DAYS,
    task_filter: Optional[str] = None,
    window: int = TREND_WINDOW,
) -> str:
    """Render the text output for ``fno evals report``.

    Args:
        rows:           All history rows (tolerant-loaded, already filtered for
                        valid dicts).
        staleness_days: Print a staleness warning when newest row is older than
                        this many days.
        task_filter:    When set, include only the task matching this slug.
        window:         Number of recent runs to include in the trend.

    Returns:
        Formatted report string.
    """
    if not rows:
        return "No eval history found. Run `fno evals run` to generate results.\n"

    # Apply task filter
    if task_filter:
        rows = [r for r in rows if r.get("task") == task_filter]
        if not rows:
            return f"No history rows found for task {task_filter!r}.\n"

    lines: list[str] = []

    # Staleness check: look at the newest ts across all rows
    all_ts = [_parse_ts(r.get("ts", "")) for r in rows]
    valid_ts = [t for t in all_ts if t is not None]
    now = datetime.now(timezone.utc)
    has_staleness_warning = False
    if valid_ts:
        newest = max(valid_ts)
        age_days = (now - newest).days
        if age_days >= staleness_days:
            lines.append(
                f"WARNING: suite is stale - newest run is {age_days} day(s) old "
                f"(threshold: {staleness_days} days). Run `fno evals run` to refresh.\n"
            )
            has_staleness_warning = True

    # Group all rows by task for trend computation, then pick latest per task
    tasks_order: list[str] = []
    rows_by_task: dict[str, list[dict]] = {}
    for row in rows:
        t = row.get("task", "")
        if t not in rows_by_task:
            rows_by_task[t] = []
            tasks_order.append(t)
        rows_by_task[t].append(row)

    # Sort each task's rows by ts for trend window
    for t in tasks_order:
        rows_by_task[t].sort(key=lambda r: _parse_ts(r.get("ts", "")) or datetime.min.replace(tzinfo=timezone.utc))

    # Latest row per task (for the main table)
    latest_by_task: dict[str, dict] = {
        t: rows_by_task[t][-1] for t in tasks_order
    }

    # ---- Main table ----
    col_w = {"task": 28, "status": 8, "pass_total": 10, "term": 18, "cost": 10, "isolation": 12, "ts": 22}
    header = (
        f"{'task':<{col_w['task']}} "
        f"{'status':<{col_w['status']}} "
        f"{'pass/total':<{col_w['pass_total']}} "
        f"{'termination':<{col_w['term']}} "
        f"{'cost':<{col_w['cost']}} "
        f"{'isolation':<{col_w['isolation']}} "
        f"{'ts':<{col_w['ts']}}"
    )
    lines.append("=" * len(header))
    lines.append("Latest result per task")
    lines.append("=" * len(header))
    lines.append(header)
    lines.append("-" * len(header))

    has_bad_term = False
    for t in tasks_order:
        row = latest_by_task[t]
        status = _pass_status(row)
        if status == "PASS*":
            has_bad_term = True
        pass_total = f"{sum(1 for v in row.get('assertions', {}).values() if v)}/{row.get('total', 0)}"
        cost_str = _format_cost(row.get("cost_usd"))
        isolation = row.get("isolation", "-")
        ts = (row.get("ts") or "")[:19]  # trim to YYYY-MM-DDTHH:MM:SS
        term = row.get("termination_reason", "-")
        line = (
            f"{t:<{col_w['task']}} "
            f"{status:<{col_w['status']}} "
            f"{pass_total:<{col_w['pass_total']}} "
            f"{term:<{col_w['term']}} "
            f"{cost_str:<{col_w['cost']}} "
            f"{isolation:<{col_w['isolation']}} "
            f"{ts:<{col_w['ts']}}"
        )
        lines.append(line)

    lines.append("-" * len(header))

    if has_bad_term:
        lines.append("* PASS* = assertions passed but termination was not DoneAdvisory/DonePRGreen")

    # ---- Trend section ----
    lines.append("")
    lines.append("Trend (last %d runs per task)" % window)
    lines.append("-" * 50)

    for t in tasks_order:
        task_rows = rows_by_task[t]
        recent = task_rows[-window:] if len(task_rows) >= window else task_rows
        pass_count = sum(1 for r in recent if r.get("passed", False))
        pass_rate = f"{pass_count}/{len(recent)}"

        # Cost direction: compare oldest vs newest in window.  Coerce to
        # float so a hand-edited / stringified cost never crashes the
        # report (tolerant-reader contract).
        costs = []
        for r in recent:
            val = r.get("cost_usd")
            if val is None:
                continue
            try:
                costs.append(float(val))
            except (TypeError, ValueError):
                continue
        if len(costs) >= 2:
            delta = costs[-1] - costs[0]
            if delta > 0.01:
                cost_dir = f"cost +${delta:.2f}"
            elif delta < -0.01:
                cost_dir = f"cost -${abs(delta):.2f}"
            else:
                cost_dir = "cost ~="
        elif len(costs) == 1:
            cost_dir = f"cost {_format_cost(costs[0])}"
        else:
            cost_dir = "cost -"

        total_runs = len(task_rows)
        lines.append(f"  {t}: {pass_rate} pass in last {len(recent)} runs  {cost_dir}  ({total_runs} total)")

    lines.append("")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# Diff renderer
# ---------------------------------------------------------------------------


def render_diff(
    all_rows: list[dict],
    *,
    label_a: str,
    label_b: str,
) -> tuple[str, int]:
    """Render text for ``fno evals diff --label A --label B``.

    Returns ``(text, exit_code)``.  Exit code is:
    - 0 when a comparison was produced (even if all regressions)
    - 1 when no comparison is possible (no common tasks, or a label is missing)

    Args:
        all_rows:  All history rows from ``iter_rows_tolerant``.
        label_a:   The "before" label.
        label_b:   The "after" label.

    Returns:
        (output_text, exit_code)
    """
    # Partition by label
    rows_a = [r for r in all_rows if r.get("label") == label_a]
    rows_b = [r for r in all_rows if r.get("label") == label_b]

    # Validate labels exist
    missing_labels = []
    if not rows_a:
        missing_labels.append(label_a)
    if not rows_b:
        missing_labels.append(label_b)
    if missing_labels:
        names = ", ".join(repr(m) for m in missing_labels)
        return (f"Error: label(s) {names} not found in history.\n", 1)

    # Latest row per (task, label)
    latest_a = {k[0]: v for k, v in _latest_per(rows_a, ("task",)).items()}  # task -> row
    latest_b = {k[0]: v for k, v in _latest_per(rows_b, ("task",)).items()}

    tasks_a = set(latest_a.keys())
    tasks_b = set(latest_b.keys())
    common_tasks = tasks_a & tasks_b
    only_in_a = tasks_a - tasks_b
    only_in_b = tasks_b - tasks_a

    if not common_tasks:
        parts = [
            f"No common tasks between label {label_a!r} and label {label_b!r}.\n",
            f"  {label_a!r} tasks: {sorted(tasks_a) or '(none)'}\n",
            f"  {label_b!r} tasks: {sorted(tasks_b) or '(none)'}\n",
        ]
        return ("".join(parts), 1)

    lines: list[str] = []
    lines.append(f"fno evals diff: {label_a!r} vs {label_b!r}")
    lines.append("=" * 60)

    for task in sorted(common_tasks):
        row_a = latest_a[task]
        row_b = latest_b[task]
        lines.append(f"\nTask: {task}")
        lines.append("-" * 40)

        # Assertion flips
        assertions_a = row_a.get("assertions", {})
        assertions_b = row_b.get("assertions", {})
        all_assertions = sorted(set(assertions_a) | set(assertions_b))

        flips: list[str] = []
        for name in all_assertions:
            val_a = assertions_a.get(name)
            val_b = assertions_b.get(name)
            if val_a == val_b:
                continue  # no change
            # Regression: was ok, now fail
            if val_a is True and val_b is False:
                flips.append(f"  REGRESS  {name}: ok->FAIL")
            # Improvement: was fail, now ok
            elif val_a is False and val_b is True:
                flips.append(f"  IMPROVE  {name}: fail->OK")
            # Added assertion
            elif val_a is None and val_b is True:
                flips.append(f"  NEW(ok)  {name}: (new)->ok")
            elif val_a is None and val_b is False:
                flips.append(f"  NEW(fail) {name}: (new)->FAIL")
            # Removed assertion
            elif val_b is None and val_a is True:
                flips.append(f"  REMOVED  {name}: ok->(removed)")
            elif val_b is None and val_a is False:
                flips.append(f"  REMOVED  {name}: FAIL->(removed)")

        if flips:
            lines.append("  Assertion flips:")
            lines.extend(flips)
        else:
            lines.append("  Assertions: no changes")

        # Termination reason change
        term_a = row_a.get("termination_reason", "-")
        term_b = row_b.get("termination_reason", "-")
        if term_a != term_b:
            lines.append(f"  Termination: {term_a} -> {term_b}")
        else:
            lines.append(f"  Termination: {term_a} (unchanged)")

        # Token and cost delta
        tok_a = row_a.get("tokens_total")
        tok_b = row_b.get("tokens_total")
        cost_a = row_a.get("cost_usd")
        cost_b = row_b.get("cost_usd")

        if tok_a is not None and tok_b is not None:
            try:
                tok_delta = int(tok_b) - int(tok_a)
                sign = "+" if tok_delta >= 0 else ""
                lines.append(f"  Tokens:   {int(tok_a):,} -> {int(tok_b):,}  ({sign}{tok_delta:,})")
            except (TypeError, ValueError):
                pass
        if cost_a is not None and cost_b is not None:
            try:
                cost_delta = float(cost_b) - float(cost_a)
                sign = "+" if cost_delta >= 0 else ""
                lines.append(f"  Cost:     ${float(cost_a):.2f} -> ${float(cost_b):.2f}  ({sign}${cost_delta:.2f})")
            except (TypeError, ValueError):
                pass

    # Asymmetric tasks
    if only_in_a or only_in_b:
        lines.append("\nAsymmetric tasks:")
    for task in sorted(only_in_a):
        lines.append(f"  missing in {label_b!r}: {task}")
    for task in sorted(only_in_b):
        lines.append(f"  missing in {label_a!r}: {task}")

    lines.append("")
    return ("\n".join(lines) + "\n", 0)
