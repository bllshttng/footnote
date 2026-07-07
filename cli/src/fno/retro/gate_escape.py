"""Aggregate gate_escape events for the retro autonomy-debt summary (x-f894).

Reads the canonical events.jsonl (where reconcile lands gate_escapes) plus the
durable emit-failure counter beside it, and renders a ranked-by-reason block.
The deliverable: escape counts by reason say which reliability fix pays first
("dead-bot: 7, spawn-cap: 4" -> fixing the wedged bot pays before the rest).
"""
from __future__ import annotations

import json
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


@dataclass
class GateEscapeSummary:
    total: int = 0
    # (reason, count) most-frequent first; ties broken by reason name.
    by_reason: list[tuple[str, int]] = field(default_factory=list)
    prs_by_reason: dict[str, list[int]] = field(default_factory=dict)
    nodes_by_reason: dict[str, list[str]] = field(default_factory=dict)
    # Fail-open emit failures logged over the window (AC7): a non-zero count
    # means the metric may under-report, so it is surfaced, never hidden.
    emit_failures: int = 0


def summarize_gate_escapes(
    events_path: Path, failure_log_path: Optional[Path] = None
) -> GateEscapeSummary:
    """Count gate_escape events by reason. A missing/unreadable log aggregates
    to a clean empty summary (AC: first-ever run is '0 by reason', not an error)."""
    counts: Counter[str] = Counter()
    prs: dict[str, list[int]] = defaultdict(list)
    nodes: dict[str, list[str]] = defaultdict(list)
    try:
        text = events_path.read_text(encoding="utf-8")
    except OSError:
        text = ""
    for line in text.splitlines():
        if '"gate_escape"' not in line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") != "gate_escape":
            continue
        data = ev.get("data") or {}
        reason = data.get("reason") or "other"
        counts[reason] += 1
        pr = data.get("pr")
        if isinstance(pr, int) and pr not in prs[reason]:
            prs[reason].append(pr)
        node = data.get("graph_node_id")
        if isinstance(node, str) and node and node not in nodes[reason]:
            nodes[reason].append(node)

    # Most-frequent first; ties broken by reason name for a stable render.
    ranked = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))

    if failure_log_path is None:
        failure_log_path = events_path.parent / "gate_escape_emit_failures.jsonl"
    try:
        ftext = failure_log_path.read_text(encoding="utf-8")
        failures = sum(1 for ln in ftext.splitlines() if ln.strip())
    except OSError:
        failures = 0

    return GateEscapeSummary(
        total=sum(counts.values()),
        by_reason=ranked,
        prs_by_reason={r: prs[r] for r, _ in ranked},
        nodes_by_reason={r: nodes[r] for r, _ in ranked},
        emit_failures=failures,
    )


def render_gate_escapes(summary: GateEscapeSummary) -> list[str]:
    """Human-readable lines. '0 by reason' on an empty log (a clean zero, not an
    error). A non-zero emit-failure count is surfaced as 'may under-report'."""
    lines: list[str] = []
    if summary.total == 0:
        lines.append("gate_escapes: 0 by reason")
    else:
        parts = []
        for reason, n in summary.by_reason:
            prs = summary.prs_by_reason.get(reason) or []
            attribution = (
                " (" + ", ".join(f"PR #{p}" for p in prs) + ")" if prs else ""
            )
            parts.append(f"{reason}={n}{attribution}")
        lines.append(
            f"gate_escapes ({summary.total} total, most-frequent first): "
            + " ".join(parts)
        )
    if summary.emit_failures > 0:
        lines.append(
            f"WARN gate_escape metric may under-report: {summary.emit_failures} "
            "emit failure(s) logged (fail-open); a broken counter reads LOW"
        )
    return lines
