#!/usr/bin/env python3
"""Token Doctor - Session diagnostic for Claude Code.

Analyzes the current session's transcript for cache breaks, idle gaps,
resume bug indicators, and cost attribution. Outputs a diagnostic report.

Usage:
    python3 diagnose.py                    # auto-detect current session
    python3 diagnose.py <session-id>       # specific session
    python3 diagnose.py --json             # JSON output
"""

import argparse
import json
import os
import sys
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

# Import shared pricing module. Resolve repo root via git rev-parse so the
# script is location-independent (was previously assumed to live three deep
# under skills/<name>/scripts/; now lives under scripts/diagnostics/).
SCRIPT_DIR = Path(__file__).resolve().parent
try:
    import subprocess as _sp  # local alias; main subprocess import is below
    FNO_ROOT = Path(
        _sp.check_output(
            ["git", "rev-parse", "--show-toplevel"],
            stderr=_sp.DEVNULL,
            text=True,
        ).strip()
    )
except Exception:
    FNO_ROOT = SCRIPT_DIR.parent.parent
# cost_tracker moved into the fno package (cli/src/fno/cost/cost_tracker.py).
sys.path.insert(0, str(FNO_ROOT / "cli" / "src"))

try:
    from fno.cost.cost_tracker import model_tier, calculate_cost, estimate_cache_miss_cost, format_tokens, format_cost
except ImportError:
    print("Error: fno.cost.cost_tracker not found. Run from fno plugin root.", file=sys.stderr)
    sys.exit(1)

SESSION_CONTEXT_PATH = Path.home() / ".claude" / ".session-context.json"
PROJECTS_DIR = Path.home() / ".claude" / "projects"

# Cache break: if cache_read drops below this ratio of previous turn's cache_read
CACHE_BREAK_THRESHOLD = 0.10
# Idle gap: seconds between turns that indicates cache expiry risk
IDLE_GAP_5M = 300
IDLE_GAP_1H = 3600


@dataclass
class TurnData:
    turn_number: int
    timestamp: str
    model: str
    input_tokens: int = 0
    output_tokens: int = 0
    cache_read: int = 0
    cache_create: int = 0
    cache_1h: int = 0
    cache_5m: int = 0
    cost_usd: float = 0.0
    is_subagent: bool = False
    speed: str | None = None
    gap_from_prev_sec: float = 0.0


@dataclass
class CacheBreak:
    turn_number: int
    timestamp: str
    reason: str
    prev_cache_read: int
    curr_cache_read: int
    cost_impact: float


@dataclass
class IdleGap:
    turn_number: int
    start_time: str
    end_time: str
    gap_seconds: float
    tier_expired: str


@dataclass
class DiagnosticReport:
    session_id: str
    total_cost: float = 0.0
    duration_minutes: float = 0.0
    primary_model: str = ""
    cache_ratio: float = 0.0
    total_input: int = 0
    total_output: int = 0
    total_cache_read: int = 0
    total_cache_create: int = 0
    total_context: int = 0
    turns: int = 0
    subagent_turns: int = 0
    subagent_cost: float = 0.0
    main_thread_cost: float = 0.0
    top_expensive_turns: list = field(default_factory=list)
    cache_breaks: list = field(default_factory=list)
    idle_gaps: list = field(default_factory=list)
    bugs_detected: list = field(default_factory=list)
    recommendations: list = field(default_factory=list)
    models_used: dict = field(default_factory=dict)


def find_session_id() -> str | None:
    """Read current session ID from CC's session context file."""
    if not SESSION_CONTEXT_PATH.exists():
        return None
    try:
        data = json.loads(SESSION_CONTEXT_PATH.read_text())
        return data.get("session_id")
    except (json.JSONDecodeError, OSError):
        return None


def find_transcript(session_id: str) -> Path | None:
    """Search for transcript JSONL across all project dirs."""
    if not PROJECTS_DIR.is_dir():
        return None
    for project_dir in PROJECTS_DIR.iterdir():
        if not project_dir.is_dir():
            continue
        candidate = project_dir / f"{session_id}.jsonl"
        if candidate.exists():
            return candidate
    return None


def parse_timestamp(ts: str) -> datetime | None:
    """Parse ISO timestamp from transcript."""
    if not ts:
        return None
    try:
        return datetime.fromisoformat(ts[:19].replace("Z", ""))
    except (ValueError, TypeError):
        return None


def analyze_session(transcript_path: Path, session_id: str) -> DiagnosticReport:
    """Parse transcript and produce diagnostic report."""
    report = DiagnosticReport(session_id=session_id)
    turns: list[TurnData] = []
    model_counts: dict[str, int] = {}
    prev_timestamp: datetime | None = None
    turn_num = 0
    is_resumed = False

    with open(transcript_path) as f:
        for line_num, line in enumerate(f):
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue

            # Check for resume indicator on first entries
            if line_num < 5:
                if entry.get("type") == "resume" or entry.get("resumedFrom"):
                    is_resumed = True

            if entry.get("type") != "assistant":
                continue

            message = entry.get("message", {})
            usage = message.get("usage")
            if not usage:
                continue

            turn_num += 1
            model = message.get("model", "unknown")
            model_counts[model] = model_counts.get(model, 0) + 1

            ts = entry.get("timestamp", "")
            current_time = parse_timestamp(ts)

            gap_sec = 0.0
            if prev_timestamp and current_time:
                gap_sec = (current_time - prev_timestamp).total_seconds()

            cache_creation = usage.get("cache_creation", {})

            turn = TurnData(
                turn_number=turn_num,
                timestamp=ts,
                model=model,
                input_tokens=usage.get("input_tokens", 0) or 0,
                output_tokens=usage.get("output_tokens", 0) or 0,
                cache_read=usage.get("cache_read_input_tokens", 0) or 0,
                cache_create=usage.get("cache_creation_input_tokens", 0) or 0,
                cache_1h=cache_creation.get("ephemeral_1h_input_tokens", 0) or 0,
                cache_5m=cache_creation.get("ephemeral_5m_input_tokens", 0) or 0,
                is_subagent=entry.get("isSidechain", False),
                speed=usage.get("speed"),
                gap_from_prev_sec=gap_sec,
            )
            turn.cost_usd = calculate_cost(usage, model)

            turns.append(turn)
            if current_time:
                prev_timestamp = current_time

    if not turns:
        report.recommendations.append("No assistant messages found in transcript.")
        return report

    # Aggregate stats
    report.turns = len(turns)
    report.total_cost = sum(t.cost_usd for t in turns)
    report.total_input = sum(t.input_tokens for t in turns)
    report.total_output = sum(t.output_tokens for t in turns)
    report.total_cache_read = sum(t.cache_read for t in turns)
    report.total_cache_create = sum(t.cache_create for t in turns)
    report.total_context = report.total_input + report.total_cache_read + report.total_cache_create
    report.models_used = model_counts

    if report.total_context > 0:
        report.cache_ratio = report.total_cache_read / report.total_context * 100

    # Primary model
    if model_counts:
        report.primary_model = max(model_counts, key=model_counts.get)

    # Duration
    first_ts = parse_timestamp(turns[0].timestamp)
    last_ts = parse_timestamp(turns[-1].timestamp)
    if first_ts and last_ts:
        report.duration_minutes = (last_ts - first_ts).total_seconds() / 60

    # Subagent attribution
    subagent_turns = [t for t in turns if t.is_subagent]
    report.subagent_turns = len(subagent_turns)
    report.subagent_cost = sum(t.cost_usd for t in subagent_turns)
    report.main_thread_cost = report.total_cost - report.subagent_cost

    # Top 5 most expensive turns
    sorted_by_cost = sorted(turns, key=lambda t: t.cost_usd, reverse=True)
    report.top_expensive_turns = sorted_by_cost[:5]

    # Detect cache breaks
    for i in range(1, len(turns)):
        prev = turns[i - 1]
        curr = turns[i]
        if prev.cache_read > 0 and curr.cache_read < prev.cache_read * CACHE_BREAK_THRESHOLD:
            reason = "unknown"
            if curr.gap_from_prev_sec > IDLE_GAP_1H:
                reason = "idle_gap_1h"
            elif curr.gap_from_prev_sec > IDLE_GAP_5M:
                reason = "idle_gap_5m"
            elif is_resumed:
                reason = "resume_bug"

            cost_impact = estimate_cache_miss_cost(prev.cache_read, curr.model)
            report.cache_breaks.append(CacheBreak(
                turn_number=curr.turn_number,
                timestamp=curr.timestamp,
                reason=reason,
                prev_cache_read=prev.cache_read,
                curr_cache_read=curr.cache_read,
                cost_impact=cost_impact,
            ))

    # Detect idle gaps
    for turn in turns:
        if turn.gap_from_prev_sec > IDLE_GAP_5M:
            tier = "5m" if turn.gap_from_prev_sec < IDLE_GAP_1H else "1h"
            report.idle_gaps.append(IdleGap(
                turn_number=turn.turn_number,
                start_time=turns[turn.turn_number - 2].timestamp if turn.turn_number > 1 else "",
                end_time=turn.timestamp,
                gap_seconds=turn.gap_from_prev_sec,
                tier_expired=tier,
            ))

    # Bug detection
    if is_resumed:
        low_cache_turns = [t for t in turns if t.cache_read == 0 and t.input_tokens > 1000]
        if len(low_cache_turns) > len(turns) * 0.3:
            report.bugs_detected.append(
                "Possible --resume cache prefix bug: session was resumed and "
                f"{len(low_cache_turns)}/{len(turns)} turns had zero cache reads. "
                "Consider using npx @anthropic-ai/claude-code or starting fresh sessions."
            )

    # Recommendations
    if report.idle_gaps:
        total_gap_cost = sum(
            estimate_cache_miss_cost(
                turns[g.turn_number - 2].cache_read if g.turn_number > 1 else 0,
                report.primary_model
            )
            for g in report.idle_gaps
        )
        report.recommendations.append(
            f"{len(report.idle_gaps)} idle gap(s) detected (>{IDLE_GAP_5M // 60} min). "
            f"Estimated extra cost from cache misses: {format_cost(total_gap_cost)}. "
            "Use /fno:cache-keepalive to prevent this."
        )

    if report.cache_ratio < 80:
        report.recommendations.append(
            f"Cache hit ratio is {report.cache_ratio:.1f}% (healthy is >90%). "
            "Check for --resume flag usage, frequent /compact, or billing content in chat."
        )

    if report.subagent_cost > report.total_cost * 0.5:
        report.recommendations.append(
            f"Subagents account for {format_cost(report.subagent_cost)} "
            f"({report.subagent_cost / report.total_cost * 100:.0f}% of total). "
            "Consider routing more subagent tasks to Sonnet or Haiku."
        )

    if not report.recommendations:
        report.recommendations.append("Session looks healthy. No issues detected.")

    return report


def format_report(report: DiagnosticReport) -> str:
    """Render diagnostic report as markdown."""
    lines = []
    lines.append("# Token Doctor - Session Diagnostic")
    lines.append("")
    lines.append(f"**Session:** `{report.session_id[:12]}...`")
    lines.append(f"**Model:** {report.primary_model}")
    lines.append(f"**Duration:** {report.duration_minutes:.0f} min")
    lines.append(f"**Turns:** {report.turns} ({report.subagent_turns} subagent)")
    lines.append("")

    # Cost summary
    lines.append("## Cost Summary")
    lines.append("")
    lines.append(f"| Metric | Value |")
    lines.append(f"|--------|-------|")
    lines.append(f"| Total cost | {format_cost(report.total_cost)} |")
    lines.append(f"| Main thread | {format_cost(report.main_thread_cost)} |")
    lines.append(f"| Subagents | {format_cost(report.subagent_cost)} |")
    lines.append(f"| Cache ratio | {report.cache_ratio:.1f}% |")
    lines.append("")

    # Token breakdown
    lines.append("## Tokens")
    lines.append("")
    lines.append(f"| Type | Count |")
    lines.append(f"|------|-------|")
    lines.append(f"| Input (uncached) | {format_tokens(report.total_input)} |")
    lines.append(f"| Output | {format_tokens(report.total_output)} |")
    lines.append(f"| Cache read | {format_tokens(report.total_cache_read)} |")
    lines.append(f"| Cache create | {format_tokens(report.total_cache_create)} |")
    lines.append("")

    # Models used
    if len(report.models_used) > 1:
        lines.append("## Models Used")
        lines.append("")
        for model, count in sorted(report.models_used.items(), key=lambda x: -x[1]):
            lines.append(f"- {model}: {count} turns")
        lines.append("")

    # Top expensive turns
    if report.top_expensive_turns:
        lines.append("## Most Expensive Turns")
        lines.append("")
        lines.append(f"| Turn | Time | Cost | Cache Read | Uncached |")
        lines.append(f"|------|------|------|------------|----------|")
        for t in report.top_expensive_turns:
            ts_short = t.timestamp[11:19] if len(t.timestamp) > 19 else t.timestamp
            lines.append(
                f"| {t.turn_number} | {ts_short} | {format_cost(t.cost_usd)} "
                f"| {format_tokens(t.cache_read)} | {format_tokens(t.input_tokens)} |"
            )
        lines.append("")

    # Cache breaks
    if report.cache_breaks:
        lines.append("## Cache Breaks Detected")
        lines.append("")
        for cb in report.cache_breaks:
            ts_short = cb.timestamp[11:19] if len(cb.timestamp) > 19 else cb.timestamp
            lines.append(
                f"- **Turn {cb.turn_number}** ({ts_short}): {cb.reason} - "
                f"cache dropped from {format_tokens(cb.prev_cache_read)} to "
                f"{format_tokens(cb.curr_cache_read)}. "
                f"Extra cost: ~{format_cost(cb.cost_impact)}"
            )
        lines.append("")

    # Idle gaps
    if report.idle_gaps:
        lines.append("## Idle Gaps")
        lines.append("")
        for gap in report.idle_gaps:
            gap_min = gap.gap_seconds / 60
            lines.append(
                f"- **Turn {gap.turn_number}**: {gap_min:.0f} min idle "
                f"({gap.tier_expired} tier at risk)"
            )
        lines.append("")

    # Bugs
    if report.bugs_detected:
        lines.append("## Bugs Detected")
        lines.append("")
        for bug in report.bugs_detected:
            lines.append(f"- {bug}")
        lines.append("")

    # Recommendations
    lines.append("## Recommendations")
    lines.append("")
    for rec in report.recommendations:
        lines.append(f"- {rec}")
    lines.append("")

    return "\n".join(lines)


def report_to_dict(report: DiagnosticReport) -> dict:
    """Convert report to JSON-serializable dict."""
    return {
        "session_id": report.session_id,
        "total_cost": round(report.total_cost, 4),
        "duration_minutes": round(report.duration_minutes, 1),
        "primary_model": report.primary_model,
        "cache_ratio": round(report.cache_ratio, 1),
        "tokens": {
            "input": report.total_input,
            "output": report.total_output,
            "cache_read": report.total_cache_read,
            "cache_create": report.total_cache_create,
            "total_context": report.total_context,
        },
        "turns": report.turns,
        "subagent_turns": report.subagent_turns,
        "subagent_cost": round(report.subagent_cost, 4),
        "main_thread_cost": round(report.main_thread_cost, 4),
        "cache_breaks": len(report.cache_breaks),
        "idle_gaps": len(report.idle_gaps),
        "bugs_detected": report.bugs_detected,
        "recommendations": report.recommendations,
        "models_used": report.models_used,
    }


def main():
    parser = argparse.ArgumentParser(description="Token Doctor - Session diagnostic")
    parser.add_argument("session_id", nargs="?", help="Session ID (auto-detects if omitted)")
    parser.add_argument("--json", action="store_true", help="JSON output")
    args = parser.parse_args()

    session_id = args.session_id or find_session_id()
    if not session_id:
        print("Error: could not find session ID. Pass it as an argument or ensure CC is running.", file=sys.stderr)
        sys.exit(1)

    transcript = find_transcript(session_id)
    if not transcript:
        print(f"Error: transcript not found for session {session_id}", file=sys.stderr)
        sys.exit(1)

    report = analyze_session(transcript, session_id)

    if args.json:
        print(json.dumps(report_to_dict(report), indent=2))
    else:
        print(format_report(report))


if __name__ == "__main__":
    main()
