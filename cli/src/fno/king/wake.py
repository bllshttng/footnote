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
from contextlib import contextmanager
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

    A PURE read with no billing: tests and diagnostics use it to ask what the
    gate would say. The wake dispatcher itself must call :func:`admit_wake`
    only - deciding here and billing separately reopens the two-reader race
    admit_wake exists to close.
    """
    stamps = read_wakes(path, now=now)
    if stamps and (now - stamps[-1]).total_seconds() < debounce_s:
        return WakeVerdict(refusal="debounce", count=len(stamps))
    if ceiling > 0 and len(stamps) >= ceiling:
        return WakeVerdict(refusal="ceiling", count=len(stamps))
    return WakeVerdict(refusal="", count=len(stamps))


def _rewrite_wake_times(path: Path, stamps: list[datetime]) -> None:
    """Replace the ``wake_times`` line with ``stamps``. Caller holds the lock.

    Every other line passes through byte-identical. A manifest armed before
    the field existed gets the line inserted after the last king field,
    mirroring bump_respawn_count's anchor.
    """
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
    """Decide AND bill one wake under a single lock.

    ``allowed`` means the bill already landed: the read, the gate, the append,
    and the rewrite happen inside one ``<scope>.md.lock`` critical section, so
    two overlapping tick processes cannot both read an empty ledger and both
    dispatch - the loser sees the winner's stamp inside the lock and takes the
    debounce refusal. The caller dispatches only on ``allowed``. A ``ceiling``
    of 0 is the unbounded spelling, mirroring ``at_respawn_ceiling``.
    """
    if not Path(path).exists():
        return WakeVerdict(refusal="debounce", count=0)  # no ledger, no wake;
        # the caller's walk construction is the error surface for a missing
        # manifest
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
        stamps = sorted(stamps)[-max(1, keep):]
        _rewrite_wake_times(path, stamps)
        return WakeVerdict(refusal="", count=len(stamps))


def bill_wake(path: Path, *, now: datetime, keep: int = DEFAULT_KEEP) -> int:
    """Append ``now``, prune, rewrite ONLY the ``wake_times`` line.

    An UNCONDITIONAL bill for callers that have already decided (tests
    pre-filling a ledger, a hand-run bypass). The wake dispatcher uses
    :func:`admit_wake`, which decides and bills under one lock. Both take the
    same ``<scope>.md.lock`` flock the Rust arming and bump paths take: a
    concurrent re-crown rewrites the whole manifest and a bill must not
    interleave with it. Returns the count now inside the window.
    """
    path = Path(path)
    if not path.exists():
        return 0  # no manifest, no ledger to bill; the caller's walk
        # construction is the error surface for that
    with _with_manifest_lock(path):
        stamps = list(read_wakes(path, now=now))
        stamps.append(now.astimezone(timezone.utc))
        stamps = sorted(stamps)[-max(1, keep):]
        _rewrite_wake_times(path, stamps)
        return len(stamps)


#: Byte bound on one rendered board diff. The diff rides the walk's argv as
#: ``--wake-detail``, so an unbounded render is an unbounded command line; a
#: scope whose refill exceeds the cap names its elided row count instead.
MAX_DETAIL_CHARS = 2000


def render_board_diff(old_rows, new_rows, *, cap: int = MAX_DETAIL_CHARS) -> str:
    """The board diff between two wake observations, as prompt text.

    Rows are the ``(id, status, column, priority)`` tuples the wake sidecar
    stores beside its hash. Only ADDED, CHANGED, and REMOVED rows appear: a
    king woken on a two-row refill reads those two rows, not the board. A
    removed row is named rather than hidden - a node leaving the scope is a
    change the king may need to react to, and a diff that dropped it would
    lie by omission at the exact moment the hash says something moved.
    """
    old = {str(row[0]): tuple(str(f) for f in row) for row in old_rows or ()}
    new = {str(row[0]): tuple(str(f) for f in row) for row in new_rows or ()}
    lines: list[str] = []
    for row_id in sorted(set(old) - set(new)):
        lines.append(f"removed: {row_id} ({_row_label(old[row_id])})")
    for row_id in sorted(set(new) - set(old)):
        lines.append(f"added: {row_id} ({_row_label(new[row_id])})")
    for row_id in sorted(set(new) & set(old)):
        if new[row_id] != old[row_id]:
            lines.append(
                f"changed: {row_id} {_row_label(old[row_id])} -> {_row_label(new[row_id])}"
            )
    if not lines:
        return ""
    if len("\n".join(lines)) <= cap:
        return "\n".join(lines)
    shown: list[str] = []
    used = 0
    for line in lines:
        if used + len(line) + 1 > cap - 32:
            break
        shown.append(line)
        used += len(line) + 1
    elided = len(lines) - len(shown)
    return "\n".join(shown) + f"\n...(+{elided} more rows elided)"


def _row_label(row: tuple) -> str:
    return "/".join(part for part in row[1:] if part)
