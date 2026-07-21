#!/usr/bin/env python3
"""Extract cost and usage metrics from Claude Code session transcripts.

In-package module (formerly scripts/metrics/session-cost.py). Run via
``python3 -m fno.cost._session_cost`` or imported in-package.

Usage:
    # Single session
    python3 -m fno.cost._session_cost <session-id>

    # Filter by branch (attributes only tokens from messages on that branch)
    python3 -m fno.cost._session_cost --branch feature/my-branch <session-id>

    # Multiple sessions (summed)
    python3 -m fno.cost._session_cost <session-id-1> <session-id-2>

    # JSON output
    python3 -m fno.cost._session_cost --json <session-id>

    # Show branch breakdown for a session
    python3 -m fno.cost._session_cost --branches <session-id>

    # Backfill tasks.md (dry run) — uses branch field for attribution
    python3 -m fno.cost._session_cost --backfill --dry-run

    # Backfill tasks.md (apply)
    python3 -m fno.cost._session_cost --backfill

Finds transcripts in ~/.claude/projects/ by session ID.
Each transcript entry has a gitBranch field, enabling per-branch cost attribution
even when a single session spans multiple features.
"""

import argparse
import json
import os
import re
import sys
import tempfile
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

# Import shared pricing from the in-package cost_tracker. Resolving it as a
# sibling module (not a repo-relative sys.path hack) is the whole point of the
# move: run from the installed wheel in /tmp with no repo on disk, this binds
# the in-package cost_tracker, never a stray repo copy.
from fno import paths as _paths
from fno.cost.cost_tracker import (
    FALLBACK_MODELS_SEEN,
    PRICING,
    model_tier as _shared_model_tier,
)

# Dedup key for transcript lines: Claude Code writes one JSONL line per
# content block, and every line of the same API message repeats identical
# `message.usage` (verified live: 502 assistant lines -> 185 unique pairs,
# 0 duplicate groups with differing usage). Counting usage once per unique
# (message.id, requestId) matches ccusage semantics.
DedupKey = tuple[str, str]

UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")


@dataclass
class SessionMetrics:
    session_id: str
    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_create_tokens: int = 0
    assistant_messages: int = 0
    user_messages: int = 0
    subagent_messages: int = 0
    compaction_count: int = 0
    models: dict = field(default_factory=dict)
    first_timestamp: str = ""
    last_timestamp: str = ""
    cost_usd: float = 0.0

    @property
    def total_tokens(self) -> int:
        return (
            self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_create_tokens
        )

    @property
    def duration_minutes(self) -> float:
        if not self.first_timestamp or not self.last_timestamp:
            return 0.0
        try:
            fmt = "%Y-%m-%dT%H:%M:%S"
            t1 = self.first_timestamp[:19]
            t2 = self.last_timestamp[:19]
            d1 = datetime.strptime(t1, fmt)
            d2 = datetime.strptime(t2, fmt)
            return (d2 - d1).total_seconds() / 60
        except (ValueError, TypeError) as e:
            print(f"Warning: could not parse timestamps: {e}", file=sys.stderr)
            return 0.0

    @property
    def primary_model(self) -> str:
        if not self.models:
            return "unknown"
        return max(self.models, key=self.models.get)


def model_tier(model_name: str, speed: str | None = None) -> str:
    """Map model ID + optional speed to pricing tier. Delegates to shared cost_tracker."""
    return _shared_model_tier(model_name, speed)


def calculate_cost(metrics: SessionMetrics) -> float:
    """Calculate cost from token counts, handling mixed models."""
    if not metrics.models:
        tier = "sonnet"
        prices = PRICING[tier]
        return (
            metrics.input_tokens * prices["input"]
            + metrics.output_tokens * prices["output"]
            + metrics.cache_read_tokens * prices["cache_read"]
            + metrics.cache_create_tokens * prices["cache_create"]
        ) / 1_000_000

    # Per-model tracking not available at token level,
    # use primary model for pricing (good enough - sessions rarely mix models)
    tier = model_tier(metrics.primary_model)
    prices = PRICING[tier]
    return (
        metrics.input_tokens * prices["input"]
        + metrics.output_tokens * prices["output"]
        + metrics.cache_read_tokens * prices["cache_read"]
        + metrics.cache_create_tokens * prices["cache_create"]
    ) / 1_000_000


def find_transcript(session_id: str) -> str | None:
    """Find transcript JSONL by session ID across all project dirs."""
    if not UUID_RE.match(session_id):
        return None
    base = Path.home() / ".claude" / "projects"
    if not base.is_dir():
        return None
    for project_dir in base.iterdir():
        if not project_dir.is_dir():
            continue
        candidate = project_dir / f"{session_id}.jsonl"
        if candidate.exists():
            return str(candidate)
    return None


def _accumulate_entry(
    obj: dict,
    metrics: SessionMetrics,
    prev_context_size: list[float | None] | None = None,
    seen: set[DedupKey] | None = None,
):
    """Accumulate a single JSONL log entry into metrics.

    Shared by parse_transcript and get_branch_breakdown to avoid duplicating
    the token extraction, message counting, and timestamp tracking logic.

    Args:
        obj: Parsed JSON object from a transcript line.
        metrics: SessionMetrics instance to update in place.
        prev_context_size: Mutable single-element list tracking last context size
            for compaction detection. Pass None to skip compaction detection.
        seen: Set of (message.id, requestId) pairs already counted. Usage and
            message counts accumulate once per unique key; lines missing either
            field are counted as-is (no false dedup, matching ccusage). Pass
            None to skip dedup. Mutated in place so callers can share one set
            across transcripts (resumed sessions copy history lines, with
            usage, into the new file).
    """
    ts = obj.get("timestamp")
    if ts:
        if not metrics.first_timestamp:
            metrics.first_timestamp = ts
        metrics.last_timestamp = ts

    msg_type = obj.get("type")

    if msg_type == "user":
        metrics.user_messages += 1

    elif msg_type == "assistant":
        msg = obj.get("message", {})

        if seen is not None:
            msg_id = msg.get("id")
            request_id = obj.get("requestId")
            # isinstance guards make DedupKey = tuple[str, str] true at
            # runtime: a non-string id from a future format drift counts
            # as-is (over-count toward old behavior, never silent drop).
            if (
                msg_id
                and request_id
                and isinstance(msg_id, str)
                and isinstance(request_id, str)
            ):
                key = (msg_id, request_id)
                if key in seen:
                    # Duplicate content-block line of an already-counted API
                    # message. Duplicates carry identical usage, so skipping
                    # them does not change the compaction context-size series.
                    return
                seen.add(key)

        metrics.assistant_messages += 1
        if obj.get("isSidechain"):
            metrics.subagent_messages += 1

        model = msg.get("model", "unknown")
        metrics.models[model] = metrics.models.get(model, 0) + 1

        usage = msg.get("usage", {})
        input_t = usage.get("input_tokens", 0) or 0
        output_t = usage.get("output_tokens", 0) or 0
        cache_read = usage.get("cache_read_input_tokens", 0) or 0
        cache_create = usage.get("cache_creation_input_tokens", 0) or 0

        metrics.input_tokens += input_t
        metrics.output_tokens += output_t
        metrics.cache_read_tokens += cache_read
        metrics.cache_create_tokens += cache_create

        # Detect compaction: significant context drop between main-chain messages
        if prev_context_size is not None and not obj.get("isSidechain"):
            context_size = input_t + cache_read + cache_create
            if context_size > 0 and prev_context_size[0] and prev_context_size[0] > 0:
                ratio = context_size / prev_context_size[0]
                if ratio < 0.5:
                    metrics.compaction_count += 1
            if context_size > 0:
                prev_context_size[0] = context_size


def parse_transcript(
    path: str,
    session_id: str,
    branch_filter: str | None = None,
    since: datetime | None = None,
    seen: set[DedupKey] | None = None,
) -> SessionMetrics:
    """Parse a transcript JSONL and extract all metrics.

    Args:
        path: Path to the transcript JSONL file.
        session_id: Session identifier for labeling.
        branch_filter: If set, only count tokens from messages on this branch.
            Supports partial matching (e.g. "cost-and-scale" matches
            "feature/cost-and-scale").
        since: If set, skip entries whose `timestamp` field is earlier than
            this datetime. Entries without a `timestamp` field are also
            skipped when `since` is set (a session's own log entries always
            carry timestamps; the no-timestamp shape is a sentinel for
            transcript-internal records that don't belong to the session).
            When `since` is None, behavior is unchanged.
        seen: Optional shared dedup set of (message.id, requestId) pairs,
            mutated in place. Callers summing multiple transcripts (main(),
            backfill) pass one set per logical sum so history lines copied
            into resumed-session files are not re-counted. When None, a
            fresh per-file set is used (single-file dedup still applies).
    """
    metrics = SessionMetrics(session_id=session_id)
    prev_context_size = [None]
    if seen is None:
        seen = set()

    def branch_matches(git_branch: str) -> bool:
        if not branch_filter:
            return True
        return branch_filter in git_branch or git_branch == branch_filter

    def _parse_ts(raw: str | None) -> datetime | None:
        if not raw or not isinstance(raw, str):
            return None
        try:
            parsed = datetime.fromisoformat(raw.rstrip("Z").rstrip())
        except (ValueError, TypeError):
            return None
        # Normalize to naive-UTC so comparisons against `since` (also
        # parsed via this strip-then-fromisoformat path) never mix
        # naive and aware datetimes. Real Claude transcripts use `Z`
        # consistently today (rstrip leaves a naive datetime); this
        # path defends against a future `+HH:MM` shape drift by
        # converting to UTC before dropping tzinfo.
        if parsed.tzinfo is not None:
            parsed = parsed.astimezone(timezone.utc).replace(tzinfo=None)
        return parsed

    skipped_lines = 0
    unparseable_ts_count = 0
    with open(path) as f:
        for line in f:
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                skipped_lines += 1
                continue

            git_branch = obj.get("gitBranch", "")
            if not branch_matches(git_branch):
                continue

            if since is not None:
                raw_ts = obj.get("timestamp")
                entry_ts = _parse_ts(raw_ts)
                if entry_ts is None:
                    # A non-empty raw timestamp that failed to parse is a
                    # shape-drift signal (e.g. a future producer emits a
                    # form _parse_ts doesn't understand). Without surfacing
                    # it, --since silently drops every such entry and the
                    # hook would record $0.00 with no diagnostic. Empty /
                    # missing timestamps are the documented "skip me"
                    # sentinel and stay silent.
                    if raw_ts:
                        unparseable_ts_count += 1
                    continue
                if entry_ts < since:
                    continue

            _accumulate_entry(obj, metrics, prev_context_size, seen=seen)

    if skipped_lines > 0:
        print(f"Warning: {skipped_lines} malformed lines in {path}", file=sys.stderr)
    if unparseable_ts_count > 0:
        print(
            f"Warning: {unparseable_ts_count} entries with unparseable "
            f"timestamps skipped under --since in {path}",
            file=sys.stderr,
        )

    metrics.cost_usd = calculate_cost(metrics)
    return metrics


def get_branch_breakdown(path: str, session_id: str) -> dict[str, SessionMetrics]:
    """Parse a transcript and return metrics grouped by gitBranch."""
    branches: dict[str, SessionMetrics] = {}
    skipped_lines = 0
    # One dedup set across all branches: every content-block line of an API
    # message carries the same gitBranch, so the message lands on exactly
    # one branch and is counted once.
    seen: set[DedupKey] = set()

    with open(path) as f:
        for line in f:
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                skipped_lines += 1
                continue

            git_branch = obj.get("gitBranch", "") or "unknown"
            if git_branch not in branches:
                branches[git_branch] = SessionMetrics(
                    session_id=f"{session_id}:{git_branch}"
                )

            _accumulate_entry(obj, branches[git_branch], seen=seen)

    if skipped_lines > 0:
        print(f"Warning: {skipped_lines} malformed lines in {path}", file=sys.stderr)

    for m in branches.values():
        m.cost_usd = calculate_cost(m)

    return branches


def merge_metrics(all_metrics: list[SessionMetrics]) -> SessionMetrics:
    """Merge multiple session metrics into a combined view."""
    if len(all_metrics) == 1:
        return all_metrics[0]

    combined = SessionMetrics(
        session_id=", ".join(m.session_id[:12] for m in all_metrics)
    )
    for m in all_metrics:
        combined.input_tokens += m.input_tokens
        combined.output_tokens += m.output_tokens
        combined.cache_read_tokens += m.cache_read_tokens
        combined.cache_create_tokens += m.cache_create_tokens
        combined.assistant_messages += m.assistant_messages
        combined.user_messages += m.user_messages
        combined.subagent_messages += m.subagent_messages
        combined.compaction_count += m.compaction_count
        for model, count in m.models.items():
            combined.models[model] = combined.models.get(model, 0) + count

    combined.first_timestamp = min(
        (m.first_timestamp for m in all_metrics if m.first_timestamp), default=""
    )
    combined.last_timestamp = max(
        (m.last_timestamp for m in all_metrics if m.last_timestamp), default=""
    )
    combined.cost_usd = sum(m.cost_usd for m in all_metrics)
    return combined


def format_tokens(n: int) -> str:
    """Format token count for display."""
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}K"
    return str(n)


def print_metrics(metrics: SessionMetrics, as_json: bool = False):
    """Print metrics in human-readable or JSON format."""
    if as_json:
        payload = {
            "session_id": metrics.session_id,
            "cost_usd": round(metrics.cost_usd, 2),
            "tokens": {
                "input": metrics.input_tokens,
                "output": metrics.output_tokens,
                "cache_read": metrics.cache_read_tokens,
                "cache_create": metrics.cache_create_tokens,
                "total": metrics.total_tokens,
            },
            "messages": {
                "user": metrics.user_messages,
                "assistant": metrics.assistant_messages,
                "subagent": metrics.subagent_messages,
            },
            "compactions": metrics.compaction_count,
            "duration_minutes": round(metrics.duration_minutes, 1),
            "primary_model": metrics.primary_model,
            "models": metrics.models,
        }
        # Stop-hook stderr is swallowed; surface the pricing fallback
        # machine-visibly so drift is observable in the ledger. Optional
        # field: present only when the fallback fired for a model in this
        # session (existing JSON keys stay unchanged).
        fallback = sorted(set(metrics.models) & FALLBACK_MODELS_SEEN)
        if fallback:
            payload["pricing_fallback_models"] = fallback
        print(json.dumps(payload, indent=2))
        return

    print(f"{'─' * 50}")
    print(f"  Session:      {metrics.session_id}")
    print(f"  Model:        {metrics.primary_model}")
    print(f"  Duration:     {metrics.duration_minutes:.0f} min")
    print(f"  Cost:         ${metrics.cost_usd:.2f}")
    print(f"{'─' * 50}")
    print(f"  Input:        {format_tokens(metrics.input_tokens):>10}")
    print(f"  Output:       {format_tokens(metrics.output_tokens):>10}")
    print(f"  Cache read:   {format_tokens(metrics.cache_read_tokens):>10}")
    print(f"  Cache create: {format_tokens(metrics.cache_create_tokens):>10}")
    print(f"  Total:        {format_tokens(metrics.total_tokens):>10}")
    print(f"{'─' * 50}")
    print(f"  Messages:     {metrics.assistant_messages} assistant, {metrics.user_messages} user")
    if metrics.subagent_messages:
        print(f"  Subagents:    {metrics.subagent_messages}")
    print(f"  Compactions:  {metrics.compaction_count}")
    print(f"{'─' * 50}")


# --- Backfill support ---

# Single resolution path: the ledger is cross-project, so it always routes
# through the pinned-global paths.ledger_json(). Resolved lazily (not at module
# load) so importing this module stays pydantic-free - the bare-python metric
# harnesses (tests/metrics/*.py) import it without fno's deps installed.
def _ledger_json() -> Path:
    return _paths.ledger_json()


def _ledger_md() -> Path:
    return _ledger_json().with_suffix(".md")


def render_tasks_md(entries: list[dict]) -> str:
    """Render tasks.json entries as a human-readable markdown file."""
    lines = [
        "# Do-Target Task Registry",
        "",
        "> Global audit trail of all target pipeline runs across projects.",
        "> Source of truth: `ledger.json` - this file is a derived view.",
        "",
    ]

    # Provenance: historical costs were corrected once by
    # backfill-cost-recompute.py (transcript dedup + version-aware pricing,
    # ab-c0f92987). Anything that consumed pre-correction totals (retro
    # docs, per-feature comparisons) silently shifted meaning; this note
    # lives in the renderer so it survives re-renders.
    if any(e.get("cost_backfill") for e in entries):
        lines.insert(
            4,
            "> Costs corrected by `backfill-cost-recompute.py` "
            "(per-entry `cost_backfill` markers: recomputed / pricing_only / "
            "no_transcript).",
        )

    for i, e in enumerate(entries):
        if i > 0:
            lines.append("---")
            lines.append("")

        # Title with PR link
        pr_link = f"[#{e.get('pr_number') or '?'}]({e['pr_url']})" if e.get("pr_url") else f"#{e.get('pr_number') or '?'}"
        lines.append(f"## {e.get('title', 'Untitled')}")
        lines.append("")

        # Summary as blockquote
        if e.get("summary"):
            lines.append(f"> {e['summary']}")
            lines.append("")

        # Identity
        plan_path = e.get("plan_path") or "—"
        lines.append(f"Plan: {plan_path}")
        lines.append(f"PR: {pr_link}")
        lines.append(f"Branch: `{e.get('branch', '—')}`")
        lines.append(f"Project: `{e.get('project', '—')}` · `{e.get('root_path', '—')}`")
        if e.get("worktree"):
            lines.append(f"Worktree: `{e['worktree']}`")
        lines.append("")

        # Timing
        lines.append(f"Started: {e.get('started', '—')}")
        lines.append(f"Completed: {e.get('completed', '—')}")
        duration = e.get("duration_minutes")
        lines.append(f"Duration: {duration} min" if duration else "Duration: —")
        lines.append(f"Iterations: {e.get('iterations', '—')}")
        compactions = e.get("compactions")
        lines.append(f"Compactions: {compactions}" if compactions is not None else "Compactions: —")
        lines.append("")

        # Cost
        cost = e.get("cost_usd")
        lines.append(f"Cost: ${cost:.2f}" if cost is not None else "Cost: —")
        tokens = e.get("tokens_total")
        cache_read = e.get("cache_read_tokens")
        if tokens is not None:
            token_str = f"Tokens: {format_tokens(tokens)}"
            if cache_read is not None:
                pct = round(cache_read / tokens * 100) if tokens > 0 else 0
                token_str += f" ({pct}% cached)"
            lines.append(token_str)
        else:
            lines.append("Tokens: —")
        lines.append(f"Model: {e.get('model', '—')}")
        sessions = e.get("sessions", [])
        lines.append(f"Sessions: {', '.join(f'`{s}`' for s in sessions)}" if sessions else "Sessions: —")
        lines.append("")

        # Execution
        phases_done = e.get("phases_completed", [])
        phases_skip = e.get("phases_skipped", [])
        lines.append(f"Phases: {', '.join(phases_done)}" if phases_done else "Phases: —")
        lines.append(f"Skipped: {', '.join(phases_skip)}" if phases_skip else "Skipped: —")
        points = e.get("points")
        lines.append(f"Points: {points}" if points is not None else "Points: —")
        lines.append("")

        # Notes
        if e.get("notes"):
            lines.append(f"Notes: {e['notes']}")
            lines.append("")

    return "\n".join(lines)



def backfill_tasks_json(dry_run: bool = True):
    """Recalculate costs for all tasks.json entries using branch-level attribution."""
    if not _ledger_json().exists():
        print(f"No ledger.json found at {_ledger_json()}")
        return

    try:
        data = json.loads(_ledger_json().read_text())
    except json.JSONDecodeError as e:
        print(f"Error: ledger.json is corrupt: {e}", file=sys.stderr)
        sys.exit(1)
    entries = data if isinstance(data, list) else data.get("entries", [])

    for entry in entries:
        sessions = entry.get("sessions", [])
        branch = entry.get("branch")
        all_metrics = []
        # One dedup set per ledger entry (the logical sum): resumed sessions
        # copy history lines into new transcript files, so per-file dedup
        # alone would re-count history across this entry's sessions.
        entry_seen: set[DedupKey] = set()

        for sid in sessions:
            path = find_transcript(sid)
            if not path:
                continue
            m = None
            if branch:
                # Trial parse on a copy: if the branch filter matches nothing,
                # the fallback full parse below must not see the trial's keys.
                trial_seen = set(entry_seen)
                branch_metrics = parse_transcript(
                    path, sid, branch_filter=branch, seen=trial_seen
                )
                if branch_metrics.assistant_messages > 0:
                    m = branch_metrics
                    entry_seen.update(trial_seen)
            if m is None:
                m = parse_transcript(path, sid, seen=entry_seen)
            all_metrics.append(m)

        if not all_metrics:
            print(f"  SKIP: {entry.get('title', 'Untitled')} — no transcripts found")
            continue

        combined = merge_metrics(all_metrics)
        attribution = f"branch:{branch}" if branch else "full session"

        print(f"\n  {entry.get('title', 'Untitled')}")
        print(f"    Attribution: {attribution}")
        print(f"    Cost: ${combined.cost_usd:.2f} (was: ${entry.get('cost_usd') or 0:.2f})")
        print(f"    Tokens: {format_tokens(combined.total_tokens)} | Compactions: {combined.compaction_count}")

        if not dry_run:
            entry["cost_usd"] = round(combined.cost_usd, 2)
            entry["tokens_total"] = combined.total_tokens
            entry["cache_read_tokens"] = combined.cache_read_tokens
            entry["compactions"] = combined.compaction_count

    if dry_run:
        print("\n  Dry run — no changes written. Use --backfill without --dry-run to apply.")
        return

    # Atomic write: temp file then rename to prevent corruption on interrupt
    tmp_fd, tmp_path = tempfile.mkstemp(
        dir=_ledger_json().parent, suffix=".tmp"
    )
    try:
        with os.fdopen(tmp_fd, "w") as tmp_f:
            tmp_f.write(json.dumps(data, indent=2) + "\n")
        os.replace(tmp_path, _ledger_json())
    except Exception:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
    print(f"\n  Updated {_ledger_json()}")

    # Render MD from JSON
    md_content = render_tasks_md(entries)
    _ledger_md().write_text(md_content + "\n")
    print(f"  Rendered {_ledger_md()}")


def render_tasks_from_json():
    """Render tasks.md from tasks.json without recalculating costs."""
    if not _ledger_json().exists():
        print(f"No ledger.json found at {_ledger_json()}")
        return

    try:
        data = json.loads(_ledger_json().read_text())
    except json.JSONDecodeError as e:
        print(f"Error: ledger.json is corrupt: {e}", file=sys.stderr)
        sys.exit(1)
    entries = data if isinstance(data, list) else data.get("entries", [])
    md_content = render_tasks_md(entries)
    _ledger_md().write_text(md_content + "\n")
    print(f"  Rendered {len(entries)} entries to {_ledger_md()}")


def print_branch_breakdown(path: str, session_id: str):
    """Print cost breakdown by branch for a session."""
    branches = get_branch_breakdown(path, session_id)
    total_cost = sum(m.cost_usd for m in branches.values())

    print(f"{'─' * 60}")
    print(f"  Branch breakdown for {session_id[:12]}...")
    print(f"{'─' * 60}")
    print(f"  {'Branch':<45} {'Msgs':>5} {'Cost':>10}")
    print(f"  {'─' * 55}")

    for branch_name in sorted(branches, key=lambda b: -branches[b].cost_usd):
        m = branches[branch_name]
        pct = (m.cost_usd / total_cost * 100) if total_cost > 0 else 0
        print(
            f"  {branch_name:<45} {m.assistant_messages:>5} "
            f"${m.cost_usd:>8.2f} ({pct:.0f}%)"
        )

    print(f"  {'─' * 55}")
    print(f"  {'TOTAL':<45} {sum(m.assistant_messages for m in branches.values()):>5} ${total_cost:>8.2f}")
    print(f"{'─' * 60}")


def print_by_provider(ledger_path: Path | None = None) -> None:
    """Print a cost breakdown by provider_id from ledger.json.

    Entries without a provider_id field are bucketed under "unattributed"
    for backward compatibility with pre-substrate sessions.
    """
    path = ledger_path or _ledger_json()
    if not path.exists():
        print(f"No ledger.json found at {path}")
        return

    try:
        data = json.loads(path.read_text())
    except json.JSONDecodeError as e:
        print(f"Error: ledger.json is corrupt: {e}", file=sys.stderr)
        sys.exit(1)

    entries = data if isinstance(data, list) else data.get("entries", [])

    # Bucket entries by provider_id; missing key -> "unattributed"
    buckets: dict[str, list[dict]] = {}
    for entry in entries:
        bucket_key = entry.get("provider_id") or "unattributed"
        if bucket_key not in buckets:
            buckets[bucket_key] = []
        buckets[bucket_key].append(entry)

    # Sort: unattributed last, providers alphabetically
    sorted_keys = sorted(
        buckets.keys(),
        key=lambda k: ("\xff" if k == "unattributed" else k),
    )

    total_cost = sum(
        float(e.get("cost_usd") or 0) for e in entries
    )

    print(f"{'─' * 60}")
    print(f"  Provider cost breakdown ({len(entries)} entries total)")
    print(f"{'─' * 60}")
    print(f"  {'Provider':<35} {'Entries':>7} {'Cost':>12}")
    print(f"  {'─' * 55}")

    for key in sorted_keys:
        bucket_entries = buckets[key]
        bucket_cost = sum(float(e.get("cost_usd") or 0) for e in bucket_entries)
        pct = (bucket_cost / total_cost * 100) if total_cost > 0 else 0
        print(
            f"  {key:<35} {len(bucket_entries):>7} "
            f"${bucket_cost:>9.2f} ({pct:.0f}%)"
        )

    print(f"  {'─' * 55}")
    print(f"  {'TOTAL':<35} {len(entries):>7} ${total_cost:>9.2f}")
    print(f"{'─' * 60}")


def main():
    parser = argparse.ArgumentParser(
        description="Extract cost and usage metrics from Claude Code session transcripts.",
        epilog=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )

    # Modes (mutually exclusive)
    mode_group = parser.add_mutually_exclusive_group()
    mode_group.add_argument("--render", action="store_true",
                            help="Render tasks.md from tasks.json without recalculating")
    mode_group.add_argument("--backfill", action="store_true",
                            help="Recalculate costs for all tasks.json entries")
    mode_group.add_argument("--branches", action="store_true",
                            help="Show per-branch cost breakdown")
    mode_group.add_argument("--by-provider", action="store_true",
                            help="Show per-provider cost breakdown from ledger.json")

    # Options
    parser.add_argument("--json", dest="as_json", action="store_true",
                        help="Output in JSON format")
    parser.add_argument("--branch", dest="branch_filter", metavar="BRANCH",
                        help="Filter tokens to a specific branch (supports partial match)")

    def _parse_since(raw: str) -> datetime:
        # Mirrors ledger-dedup-lookup.py:42-50: strip trailing Z because
        # datetime.fromisoformat predates Z-suffix support pre-Python 3.11.
        # Normalize tz the same way parse_transcript._parse_ts does so
        # naive/aware mixing never crashes the comparison.
        try:
            parsed = datetime.fromisoformat(raw.rstrip("Z").rstrip())
        except (ValueError, TypeError) as exc:
            raise argparse.ArgumentTypeError(
                f"--since must be ISO8601 (e.g. 2026-05-13T22:00:00Z): {raw!r}"
            ) from exc
        if parsed.tzinfo is not None:
            parsed = parsed.astimezone(timezone.utc).replace(tzinfo=None)
        return parsed

    parser.add_argument("--since", dest="since", metavar="ISO8601",
                        type=_parse_since, default=None,
                        help="Skip transcript entries earlier than this timestamp")
    parser.add_argument("--dry-run", dest="dry_run", action="store_true",
                        help="Preview backfill changes without writing")

    # Positional
    parser.add_argument("session_ids", nargs="*", metavar="SESSION_ID",
                        help="Session UUIDs to analyze")

    args = parser.parse_args()

    if args.render:
        render_tasks_from_json()
        return

    if args.backfill:
        backfill_tasks_json(dry_run=args.dry_run)
        return

    if args.by_provider:
        print_by_provider()
        return

    if not args.session_ids:
        parser.error("provide at least one session ID")

    # Branch breakdown mode
    if args.branches:
        for sid in args.session_ids:
            path = find_transcript(sid)
            if not path:
                print(f"Warning: transcript not found for {sid}", file=sys.stderr)
                continue
            print_branch_breakdown(path, sid)
        return

    all_metrics = []
    # One dedup set across all requested transcripts: a resumed session's
    # file contains copied history (with usage) from its predecessor, so a
    # multi-session sum must count each unique API message exactly once.
    shared_seen: set[DedupKey] = set()
    for sid in args.session_ids:
        path = find_transcript(sid)
        if not path:
            print(f"Warning: transcript not found for {sid}", file=sys.stderr)
            continue
        metrics = parse_transcript(
            path, sid,
            branch_filter=args.branch_filter,
            since=args.since,
            seen=shared_seen,
        )
        all_metrics.append(metrics)

    if not all_metrics:
        print("Error: no transcripts found", file=sys.stderr)
        sys.exit(1)

    if len(all_metrics) == 1:
        print_metrics(all_metrics[0], as_json=args.as_json)
    else:
        if not args.as_json:
            for m in all_metrics:
                print_metrics(m)
                print()
            print("COMBINED:")
        combined = merge_metrics(all_metrics)
        print_metrics(combined, as_json=args.as_json)


if __name__ == "__main__":
    main()
