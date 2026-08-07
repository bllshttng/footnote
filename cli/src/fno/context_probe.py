"""Context-window usage probe: the single implementation behind ``fno whoami``,
the hidden ``fno context`` verb, and the skill-local shim (x-7685).

Ported VERBATIM from ``skills/target/scripts/context-probe.sh``: the token sum
(input + cache_creation + cache_read off the LAST assistant line carrying a
usage block) and the model->window allowlist table, comment included. One
implementation in the repo; a second copy is the defect this exists to prevent
(AC4). ``None`` is the single failure value and means "unreadable", which every
caller already treats as "no pressure" (fail-safe) - so ``fno whoami`` (the
confused-agent recovery verb) gains no failure mode from this enrichment.
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import typer

from fno.agents.self_stamp import (
    _EXPANDED_TAIL_BYTES,
    _TAIL_BYTES,
    _complete_lines,
    _owned_ident,
    resolve_own_transcript,
)


@dataclass(frozen=True)
class ContextReading:
    """One context-window reading. Field-for-field the shell probe's JSON."""

    used_tokens: int
    window_tokens: int
    used_pct: int
    model: str


# Window size by model family. Ported VERBATIM from the shell probe. The
# "[1m]" suffix is a zai/GLM routing marker; no Anthropic model id carries it,
# so matching on it ALONE put every Claude model on the 200K branch - a flat 5x
# inflation (21% real read as 108%) that made the caller's pressure trigger
# fire at a fifth of the intended usage. 1M is an ALLOWLIST, never a
# catch-all. Only ids known to have a 1M window get one; everything else - an
# older Claude, a future id, a non-Claude model - falls to 200K. That direction
# is deliberate and asymmetric: a too-small denominator overstates pressure and
# fires the handoff early, which costs one extra succession, while a too-large
# one understates it and lets the session run out of context, which loses the
# run. A `claude-*` catch-all would put every legacy 200K model (Opus 4.5,
# Sonnet 4.5) on the losing side of that trade. A literal table, not a lookup:
# it changes once per model launch.
def _window_for(model: str) -> int:
    if "[1m]" in model:
        return 1_000_000  # zai/GLM 1M routing marker
    # The provider API echoes the bare GLM id and drops the [1m] routing marker
    # the deployment carries: one real transcript held 12567 "glm-5.2" records
    # against 2 "glm-5.2[1m]". _last_usage reads the LAST assistant line, which
    # is almost always the bare form, so the 1M generation needs an explicit
    # bare-id entry or the reading silently falls to 200K and overstates pressure
    # ~5x (the read that halted a target run at 26% real as 133%).
    if "glm-5.2" in model:
        return 1_000_000
    if "haiku" in model:
        return 200_000  # Haiku 4.5 is 200K
    if "opus-5" in model or "sonnet-5" in model or "fable-5" in model:
        return 1_000_000
    if "opus-4-8" in model or "opus-4-7" in model or "opus-4-6" in model:
        return 1_000_000
    if "sonnet-4-6" in model:
        return 1_000_000
    return 200_000  # unlisted -> conservative


def _last_usage(path: Path) -> Optional[tuple[str, int, int, int]]:
    """``(model, input_tokens, cache_creation, cache_read)`` off the LAST
    assistant line carrying a usage block, or None. Mirrors the shell probe's
    tail scan via the shared bounded-read helper, so a multi-MB transcript does
    not get read whole. No sidechain skip: the shell probe does not, and this
    must match it byte-for-byte for the shim's regression suite to hold."""
    try:
        size = path.stat().st_size
    except OSError:
        return None
    for max_bytes in (_TAIL_BYTES, _EXPANDED_TAIL_BYTES, None):
        lines = _complete_lines(path, max_bytes, drop_unterminated_tail=False)
        if lines is None:
            return None
        for raw in reversed(lines):
            try:
                record = json.loads(raw)
            except (UnicodeDecodeError, ValueError):
                continue
            if not isinstance(record, dict) or record.get("type") != "assistant":
                continue
            message = record.get("message")
            if not isinstance(message, dict):
                continue
            usage = message.get("usage")
            if not isinstance(usage, dict):
                continue
            model = message.get("model")
            model = model if isinstance(model, str) else ""
            try:
                tokens = (
                    int(usage.get("input_tokens") or 0),
                    int(usage.get("cache_creation_input_tokens") or 0),
                    int(usage.get("cache_read_input_tokens") or 0),
                )
            except (TypeError, ValueError):
                continue
            return (model, *tokens)
        # This window spanned the whole file; a larger one would re-read
        # identical bytes for an identical result, so stop escalating.
        if max_bytes is None or size <= max_bytes:
            break
    return None


def probe_context(transcript_path: Optional[Path] = None) -> Optional[ContextReading]:
    """One context-window reading, or None (unreadable).

    With ``transcript_path`` it reads that file - the door hooks use, which are
    handed an authoritative path in their payload. Without one it self-resolves
    through the same harness-aware locator the model resolver uses, so ``fno
    whoami`` and ``fno context`` answer from the same transcript. The number is
    derived, never stored: the transcript is the only source.
    """
    if transcript_path is None:
        ident = _owned_ident()
        if not ident.session_id or not ident.harness:
            return None
        transcript_path = resolve_own_transcript(ident.session_id, ident.harness)
        if transcript_path is None:
            return None
    usage = _last_usage(transcript_path)
    if usage is None:
        return None
    model, input_tokens, cache_create, cache_read = usage
    used_tokens = input_tokens + cache_create + cache_read
    window_tokens = _window_for(model)
    # Integer percent, round-half-up, matching the shell probe's
    # (used * 100 + window/2) / window integer arithmetic exactly.
    used_pct = (used_tokens * 100 + window_tokens // 2) // window_tokens
    return ContextReading(
        used_tokens=used_tokens,
        window_tokens=window_tokens,
        used_pct=used_pct,
        model=model,
    )


def context_command(
    transcript: Optional[Path] = typer.Option(
        None, "--transcript", help="probe this transcript jsonl instead of self-resolving"
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="emit the reading as one JSON line"
    ),
) -> None:
    """Context-window usage for this session, or a given transcript.

    Hidden: hooks are handed a ``transcript_path`` and probe THAT file rather
    than whatever this process's env resolves to, which ``fno whoami`` cannot
    do without a flag that makes no sense on an orientation verb. Not a second
    model-facing surface; it is what makes the number reachable from a codex,
    agy, or opencode worker and from every hook. Exits 3 when unreadable,
    matching the shell probe's contract.
    """
    reading = probe_context(transcript_path=transcript)
    if reading is None:
        raise typer.Exit(code=3)
    if json_output:
        typer.echo(
            json.dumps(
                {
                    "used_tokens": reading.used_tokens,
                    "window_tokens": reading.window_tokens,
                    "used_pct": reading.used_pct,
                    "model": reading.model,
                }
            )
        )
        return
    typer.echo(
        f"{reading.used_pct}% used ({reading.used_tokens:,} of "
        f"{reading.window_tokens:,} tokens), model {reading.model}"
    )
