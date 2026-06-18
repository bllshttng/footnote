"""Per-turn provider attribution sidecar.

Phase 02 of provider rotation failover (ab-9728b70b). The transcript
jsonl is owned by Claude Code and fno cannot modify what Claude
Code writes. The stamp lands instead in a sidecar fno owns:
``.fno/turn-attribution.jsonl``. One line per assistant turn,
written by the dispatch layer when it spawns the subprocess for that
turn.

Format::

    {"turn_index": 0, "ts": "2026-05-05T01:23:45Z",
     "provider_id": "claude-anthropic", "error_class": null}

Reader fall-back: legacy sessions predating this sidecar return an empty
iterator, and per-segment cost math falls back to ``active provider at
compute time`` (the existing cost.py behavior).

Writes are **non-blocking**: if the sidecar can't be written (disk full,
permission denied, parent-dir gone), the writer logs WARNING and returns.
The stamp is observability data; a missing entry must NEVER block the
turn from completing.
"""
from __future__ import annotations

import dataclasses
import fcntl
import json
import logging
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterator

logger = logging.getLogger(__name__)

SIDECAR_FILENAME = "turn-attribution.jsonl"


@dataclasses.dataclass(frozen=True)
class TurnAttribution:
    """One row from the per-turn sidecar."""

    turn_index: int
    ts: str
    provider_id: str
    error_class: str | None


def record_turn(
    *,
    sidecar_path: Path,
    turn_index: int,
    ts: str,
    provider_id: str,
    error_class: str | None,
) -> None:
    """Append one turn-attribution line to the sidecar.

    Best-effort: any exception (parent dir unwritable, disk full, etc.) is
    logged and swallowed. The turn must NOT fail because the stamp can't
    be written.

    fcntl.LOCK_EX serializes appends across processes/threads so the
    JSONL stays line-atomic even under concurrent dispatch.
    """
    sidecar_path = Path(sidecar_path)
    entry: dict[str, Any] = {
        "turn_index": int(turn_index),
        "ts": str(ts),
        "provider_id": str(provider_id),
        "error_class": error_class if error_class is None else str(error_class),
    }
    line = json.dumps(entry, separators=(",", ":")) + "\n"

    try:
        sidecar_path.parent.mkdir(parents=True, exist_ok=True)
        with open(sidecar_path, "a", encoding="utf-8") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            try:
                f.write(line)
                f.flush()
            finally:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    except OSError as exc:
        # Non-blocking: log and move on. The dispatch layer must not
        # propagate this exception to the caller because the per-turn
        # stamp is observability, not load-bearing.
        logger.warning(
            "turn-attribution sidecar write failed (%s): %s. "
            "Turn proceeds; per-segment attribution will fall back to "
            "active-at-compute.",
            sidecar_path,
            exc,
        )


def iter_turn_attributions(
    *,
    sidecar_path: Path,
) -> Iterator[TurnAttribution]:
    """Yield parsed TurnAttribution records from the sidecar.

    Skips malformed lines silently (returns only the rows that parsed).
    Returns an empty iterator if the sidecar is missing.
    """
    sidecar_path = Path(sidecar_path)
    if not sidecar_path.is_file():
        return iter(())

    def _gen() -> Iterator[TurnAttribution]:
        try:
            text = sidecar_path.read_text(encoding="utf-8")
        except OSError:
            return
        for line in text.splitlines():
            stripped = line.strip()
            if not stripped:
                continue
            try:
                obj = json.loads(stripped)
                yield TurnAttribution(
                    turn_index=int(obj["turn_index"]),
                    ts=str(obj["ts"]),
                    provider_id=str(obj["provider_id"]),
                    error_class=(
                        None if obj.get("error_class") is None
                        else str(obj["error_class"])
                    ),
                )
            except (json.JSONDecodeError, KeyError, TypeError, ValueError):
                continue

    return _gen()


def summarize_per_provider(
    *,
    sidecar_path: Path,
) -> dict[str, dict[str, int]]:
    """Roll up the sidecar into a ``{provider_id: {turns, errors}}`` map.

    Cites what-if finding #9 (per-provider attribution mismatch). The math
    that turns this rollup into per-provider USD is Spec 2.5; v0 only
    confirms the breakdown data is present and structured.

    Returns an empty dict when the sidecar is missing or empty.
    """
    summary: dict[str, dict[str, int]] = defaultdict(lambda: {"turns": 0, "errors": 0})
    for record in iter_turn_attributions(sidecar_path=sidecar_path):
        summary[record.provider_id]["turns"] += 1
        if record.error_class is not None:
            summary[record.provider_id]["errors"] += 1
    return dict(summary)
