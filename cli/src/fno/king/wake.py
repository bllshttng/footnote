"""The rolling 24h wake ledger on the king manifest.

A king woken by ordinary traffic is normal operation, not a failure retry, so
it gets its own budget: ``wake_times``, a comma-joined list of RFC3339 UTC
stamps pruned to the trailing 24 hours at every read and write. One field
carries the three facts a waker needs - the COUNT in the window is
``len(read_wakes(...))``, the DEBOUNCE clock is ``max(...)`` of it, and the
WINDOW rolls by construction because pruning happens on both edges.

Why a pruned list and not an anchor-plus-counter. An anchored window is a
tumbling window in disguise: it admits twice the ceiling across a boundary,
and an implementation whose anchor never advances is a lifetime constant
wearing a window's name - the exact silent stranding this ledger exists to
prevent (a lifetime cap of any size strands a long-lived reign; 32 lifetime
was a three-day budget at measured volume). A pruned list cannot express
either bug.

The ledger is keyed on a store path, never a crown. ``should_wake`` takes its
ceiling and debounce as arguments rather than reaching into a king settings
object, and every function takes the store path from its caller. The king is
not the only spawner this machine has, so whichever component survives can
hold the same bound; that is a parameter list, not an abstraction, and no
second caller is wired in this change.
"""
from __future__ import annotations

import fcntl
import os
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

#: The trailing window a stamp counts inside. A stamp exactly 24h old is
#: OUTSIDE it: AC4's roll test advances past the oldest stamps' 24 hours and
#: expects the next wake allowed, so the boundary belongs to the aged-out side.
WINDOW = timedelta(hours=24)

#: Byte bound on one frontmatter line, not a rate bound. The rate bound is
#: ``should_wake``; this cap only stops a bypassed ledger (a hand-run caller)
#: from growing the line without limit. A respecting caller can never exceed
#: its own ceiling, so the default matches the shipped config default.
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
    """The stamps inside the trailing 24h, oldest first.

    An unreadable store reads as absent, matching :func:`parse_manifest`, and
    an unparseable stamp is DROPPED rather than crashing the tick - a corrupt
    ledger must not wedge the fleet's only waker.
    """
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


def should_wake(
    path: Path, *, now: datetime, ceiling: int, debounce_s: int
) -> WakeVerdict:
    """Allow one wake, or refuse naming ``debounce`` or ``ceiling``.

    The walk this gates is the wake's executor, not its gate: an operator
    running the walk by hand is deliberately bypassing a rate limit, not a
    safety limit. A ``ceiling`` of 0 is the unbounded spelling, mirroring
    ``at_respawn_ceiling`` so the two ceilings read the same way.
    """
    stamps = read_wakes(path, now=now)
    if stamps and (now - stamps[-1]).total_seconds() < debounce_s:
        return WakeVerdict(refusal="debounce", count=len(stamps))
    if ceiling > 0 and len(stamps) >= ceiling:
        return WakeVerdict(refusal="ceiling", count=len(stamps))
    return WakeVerdict(refusal="", count=len(stamps))


def bill_wake(path: Path, *, now: datetime, keep: int = DEFAULT_KEEP) -> int:
    """Append ``now``, prune, rewrite ONLY the ``wake_times`` line.

    Takes the same ``<scope>.md.lock`` flock the Rust arming and bump paths
    take, for the same reason: a concurrent re-crown rewrites the whole
    manifest and a bill must not interleave with it. Returns the count now
    inside the window. Billing happens BEFORE dispatch at the caller, never
    after: a crash between the two costs one wasted slot in a 32-wide window,
    while the reverse costs an unbounded respawn storm.
    """
    path = Path(path)
    lock_path = path.with_suffix(path.suffix + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        try:
            try:
                content = path.read_text(encoding="utf-8")
            except OSError:
                return 0  # no manifest, no ledger to bill; the caller's walk
                # construction is the error surface for that
            stamps = [s for s in read_wakes(path, now=now)]
            stamps.append(now.astimezone(timezone.utc))
            stamps = sorted(stamps)[-max(1, keep):]
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
                # A manifest armed before the field existed carries no line.
                # Insert after the last king field so the frontmatter stays
                # grouped, mirroring bump_respawn_count's fno_id anchor.
                out_lines = []
                anchor_at = -1
                for idx, line in enumerate(content.splitlines()):
                    if line.split(":", 1)[0].strip() in (
                        "fno_id",
                        "respawn_count",
                        "respawn_ceiling",
                    ):
                        anchor_at = len(out_lines)
                    out_lines.append(line)
                out_lines.insert(anchor_at + 1, f"wake_times: {joined}")
            tmp = path.with_suffix(path.suffix + ".tmp")
            tmp.write_text("\n".join(out_lines) + "\n", encoding="utf-8")
            os.replace(str(tmp), str(path))
            return len(stamps)
        finally:
            fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
