"""Read-side projections for `fno backlog provenance`.

graph/cli.py is shrink-only, so the roster renderer lives here, extended with
the two columns the one-call answer was still missing: each session row's
registry status (live, exited, ... reaped) and its observed model. The
registry read itself sits in fno.provenance.registry_liveness - the graph
layer does not import the agents runtime.
"""
from __future__ import annotations

from typing import Any, Optional

_LIFECYCLE_PHASES = ("think", "blueprint", "do", "review", "ship")


def registry_status_of(
    session_id: Optional[str], status_index: "dict[str, str] | None"
) -> Optional[str]:
    """The status word one roster row renders, or ``None`` when unannotated."""
    if status_index is None or not session_id:
        return None
    return status_index.get(session_id) or "reaped"


def observed_model_of(row: dict) -> Optional[str]:
    """The model string a row recorded, only when actually observed."""
    obs = row.get("observed_model")
    if isinstance(obs, dict) and obs.get("kind") == "observed":
        model = obs.get("model")
        return str(model) if model else None
    return None


def lifecycle_roster(
    sessions: list, status_index: "dict[str, str] | None" = None
) -> "tuple[list[str], dict]":
    """Per-phase lifecycle roster: start, end, duration per row, and an honest
    node total. Returns ``(human_lines, summary_dict)``.

    Honesty is the acceptance criterion: a phase with no row renders 'not
    recorded'; a row with an end but no start renders 'end only'; neither
    renders as a duration, and the total states how many of the lifecycle
    phases contributed a duration rather than summing silently over gaps.
    Start reads ``started_at`` (canonical) with ``claimed_at`` as the legacy
    fallback. When ``status_index`` is a mapping, each row is annotated with
    its registry status (``reaped`` when the machine holds no row) and the
    observed model it recorded.
    """
    from datetime import datetime

    by_phase: "dict[str, list[dict]]" = {p: [] for p in _LIFECYCLE_PHASES}
    for s in sessions or []:
        ph = s.get("phase") if isinstance(s, dict) else None
        if ph in by_phase:
            by_phase[ph].append(s)

    def _start(row: dict) -> "str | None":
        return row.get("started_at") or row.get("claimed_at")

    def _end(row: dict) -> "str | None":
        return row.get("ended_at") or row.get("at")

    def _honest(row: dict) -> bool:
        # A duration is honest only when both CANONICAL names are present.
        # Legacy rows (claimed_at/at) hold stamp-fire time, not phase boundaries
        # - their span is the whole session - so they render 'end only' and are
        # never summed, named, or displayed as a phase duration.
        return "started_at" in row and "ended_at" in row

    def _dur(row: dict) -> "float | None":
        if not _honest(row):
            return None
        try:
            sp = datetime.fromisoformat(row["started_at"].replace("Z", "+00:00"))
            ep = datetime.fromisoformat(row["ended_at"].replace("Z", "+00:00"))
        except (ValueError, AttributeError):
            return None
        sec = (ep - sp).total_seconds()
        # An inverted window (started_at after ended_at) is a backfill typo or
        # a clock skew, not a phase duration. Render it as end-only rather than
        # summing a negative span into the node total.
        if sec < 0:
            return None
        return sec

    def _fmt(sec: float) -> str:
        sec = int(round(sec))
        if sec < 60:
            return f"{sec}s"
        if sec < 3600:
            return f"{sec // 60}m"
        return f"{sec // 3600}h{(sec % 3600) // 60}m"

    def _annot(row: dict) -> str:
        bits = []
        status = registry_status_of(row.get("session_id"), status_index)
        if status:
            bits.append(f"[{status}]")
        model = observed_model_of(row)
        if model:
            bits.append(model)
        return (" " + " ".join(bits)) if bits else ""

    lines: list[str] = []
    phases: list[dict] = []
    total = 0.0
    phases_with_window = 0
    for ph in _LIFECYCLE_PHASES:
        rows = by_phase[ph]
        if not rows:
            lines.append(f"    {ph:<9} not recorded")
            phases.append({"phase": ph, "recorded": False})
            continue
        phase_has_window = False
        for row in rows:
            st, en, dur = _start(row), _end(row), _dur(row)
            head = f"    {ph:<9} {row.get('harness', '?')}:{row.get('session_id', '?')}"
            if dur is not None:
                lines.append(f"{head} {row['started_at']} -> {row['ended_at']} ({_fmt(dur)}){_annot(row)}")
                total += dur
                phase_has_window = True
            elif en:
                lines.append(f"{head} end only @ {en}{_annot(row)}")
            elif st:
                lines.append(f"{head} in progress (since {st}){_annot(row)}")
            else:
                lines.append(head.rstrip() + _annot(row))
            phases.append(
                {
                    "phase": ph,
                    "recorded": True,
                    "harness": row.get("harness"),
                    "session_id": row.get("session_id"),
                    "start": st,
                    "end": en,
                    "duration_seconds": dur,
                    "registry_status": registry_status_of(
                        row.get("session_id"), status_index
                    ),
                    "observed_model": observed_model_of(row),
                }
            )
        if phase_has_window:
            phases_with_window += 1

    # Predicate on phases_with_window, not total: a zero-second window
    # (started_at == ended_at) or any out-of-order stamp leaves total at 0
    # while a phase still contributed a window. Keying on total would drop
    # the duration line and report total_duration_seconds: null despite a
    # real recorded window.
    if phases_with_window > 0:
        lines.append(
            f"    total     {_fmt(total)} "
            f"({phases_with_window} of {len(_LIFECYCLE_PHASES)} phases recorded)"
        )
    else:
        lines.append(
            f"    total     {phases_with_window} of {len(_LIFECYCLE_PHASES)} phases recorded"
        )

    summary = {
        "phases": phases,
        "total_duration_seconds": total if phases_with_window > 0 else None,
        "phases_recorded": phases_with_window,
        "phases_total": len(_LIFECYCLE_PHASES),
    }
    return lines, summary


def pr_block(entry_or_sidecar: Any) -> dict:
    """The PR projection every provenance shape carries; accepts dict or sidecar."""
    def _num(o: Any) -> Optional[int]:
        return getattr(o, "pr_number", None) if not isinstance(o, dict) else o.get("pr_number")

    def _url(o: Any) -> Optional[str]:
        return getattr(o, "pr_url", None) if not isinstance(o, dict) else o.get("pr_url")

    return {"number": _num(entry_or_sidecar), "url": _url(entry_or_sidecar)}


def render_pr_line(pr: dict) -> str:
    """The human ``pr:`` line: both when both, the one present; never omitted."""
    num, url = pr.get("number"), pr.get("url")
    if num is None and not url:
        return "  pr: (none)"
    if num is None:
        return f"  pr: {url}"
    if not url:
        return f"  pr: #{num}"
    return f"  pr: #{num} {url}"
