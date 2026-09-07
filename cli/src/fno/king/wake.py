"""The rolling 24h wake ledger on the king manifest.

``wake_times`` is a comma-joined stamp list pruned to the trailing 24h at
every read and write, so the count, the debounce clock, and the window all
come from one field. A pruned list, not an anchor-plus-counter: an anchor
that never advances is a lifetime cap wearing a window's name, and a cap of
any size strands a long-lived reign. Every function takes its ceiling,
debounce, and store path from its caller; keyed on a path, not a crown.
"""

from __future__ import annotations

import fcntl
import os
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

#: The trailing window a stamp counts inside. A stamp exactly 24h old is
#: OUTSIDE it (the roll test advances past it and expects the next wake
#: allowed), so the boundary belongs to the aged-out side.
WINDOW = timedelta(hours=24)

#: Byte bound on one frontmatter line, not a rate bound (that is
#: ``should_wake``); it stops a bypassed ledger growing without limit.
DEFAULT_KEEP = 32

_STAMP_FMT = "%Y-%m-%dT%H:%M:%SZ"


@dataclass(frozen=True)
class WakeVerdict:
    """One gate read: allow, or refuse naming the word that refused."""

    refusal: str  # "" = allow | "debounce" | "ceiling"
    count: int

    @property
    def allowed(self) -> bool:
        return not self.refusal


def _parse_stamp(raw: str) -> datetime | None:
    try:
        parsed = datetime.strptime(raw.strip(), _STAMP_FMT)
    except ValueError:
        return None
    return parsed.replace(tzinfo=timezone.utc)


def _format_stamp(stamp: datetime) -> str:
    return stamp.astimezone(timezone.utc).strftime(_STAMP_FMT)


def read_wakes(path: Path, *, now: datetime) -> list[datetime]:
    """The stamps inside the trailing 24h, oldest first. An unreadable store
    reads as absent; an unparseable stamp is dropped, never a crash."""
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError:
        return []
    stamps: list[datetime] = []
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("wake_times:"):
            continue
        for raw in line.split(":", 1)[1].split(","):
            raw = raw.strip().strip('"')
            if not raw:
                continue
            parsed = _parse_stamp(raw)
            if parsed is not None and now - parsed < WINDOW:
                stamps.append(parsed)
    return sorted(stamps)


def should_wake(path: Path, *, now: datetime, ceiling: int, debounce_s: int) -> WakeVerdict:
    """Allow one wake, or refuse naming ``debounce`` or ``ceiling``. A pure
    read; the dispatcher must use ``admit_wake``, which bills under one lock."""
    stamps = read_wakes(path, now=now)
    if stamps and (now - stamps[-1]).total_seconds() < debounce_s:
        return WakeVerdict(refusal="debounce", count=len(stamps))
    if ceiling > 0 and len(stamps) >= ceiling:
        return WakeVerdict(refusal="ceiling", count=len(stamps))
    return WakeVerdict(refusal="", count=len(stamps))


def _rewrite_wake_times(path: Path, stamps: list[datetime]) -> None:
    """Replace the ``wake_times`` line only; caller holds the lock. A manifest
    from before the field existed gets it inserted after the last king field."""
    try:
        content = path.read_text(encoding="utf-8")
    except OSError:
        return
    joined = ",".join(_format_stamp(s) for s in stamps)
    out_lines: list[str] = []
    replaced = False
    for line in content.splitlines():
        if not replaced and line.strip().startswith("wake_times:"):
            out_lines.append(f"wake_times: {joined}")
            replaced = True
        else:
            out_lines.append(line)
    if not replaced:
        anchor_at = -1
        for idx, line in enumerate(content.splitlines()):
            if line.split(":", 1)[0].strip() in (
                "fno_id",
                "respawn_count",
                "respawn_ceiling",
            ):
                anchor_at = idx
        out_lines.insert(anchor_at + 1, f"wake_times: {joined}")
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text("\n".join(out_lines) + "\n", encoding="utf-8")
    os.replace(str(tmp), str(path))


@contextmanager
def _with_manifest_lock(path: Path):
    """The ``<scope>.md.lock`` flock the arming, bump, and bill paths share."""
    lock_path = path.with_suffix(path.suffix + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    handle = lock_path.open("a+")
    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
    try:
        yield
    finally:
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()


def admit_wake(
    path: Path, *, now: datetime, ceiling: int, debounce_s: int, keep: int = DEFAULT_KEEP
) -> WakeVerdict:
    """Decide AND bill one wake under the manifest lock: ``allowed`` means the
    bill landed, so two overlapping ticks cannot both dispatch. A ceiling of 0
    is the unbounded spelling.
    """
    if not Path(path).exists():
        # No ledger, no wake; the caller's walk construction is the error
        # surface for a missing manifest.
        return WakeVerdict(refusal="debounce", count=0)
    with _with_manifest_lock(path):
        stamps = read_wakes(path, now=now)
        if stamps and (now - stamps[-1]).total_seconds() < debounce_s:
            return WakeVerdict(refusal="debounce", count=len(stamps))
        if ceiling > 0 and len(stamps) >= ceiling:
            return WakeVerdict(refusal="ceiling", count=len(stamps))
        stamps.append(now.astimezone(timezone.utc))
        # The ledger must be able to HOLD what the gate may count: a ceiling
        # above the default keep would otherwise be unreachable, because the
        # prune would cap the stored stamps below the configured cap and the
        # gate would read len(stamps) < ceiling forever.
        if ceiling > 0:
            keep = max(keep, ceiling)
        stamps = sorted(stamps)[-max(1, keep) :]
        _rewrite_wake_times(path, stamps)
        return WakeVerdict(refusal="", count=len(stamps))


def bill_wake(path: Path, *, now: datetime, keep: int = DEFAULT_KEEP) -> int:
    """Append ``now``, prune, rewrite only the ``wake_times`` line. An
    unconditional bill for callers that already decided; the dispatcher uses
    ``admit_wake``. Takes the same flock as the Rust arming and bump paths.
    Returns the count inside the window.
    """
    path = Path(path)
    if not path.exists():
        return 0  # no manifest, no ledger to bill
    with _with_manifest_lock(path):
        stamps = list(read_wakes(path, now=now))
        stamps.append(now.astimezone(timezone.utc))
        stamps = sorted(stamps)[-max(1, keep) :]
        _rewrite_wake_times(path, stamps)
        return len(stamps)
