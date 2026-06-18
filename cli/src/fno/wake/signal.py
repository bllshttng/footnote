"""Wake-signal substrate: small JSON envelopes dropped by background workers
to surface across-channel events into active sessions.

A signal is a one-shot record at <repo>/.fno/wake-signals/wake-{id}.json.
Three readers consume them: SessionStart hook, UserPromptSubmit hook, and the
target stop hook. The first reader to find a signal deletes it.

Today the only signal source is inbox-drain (Phase 4 + 5) emitting on
kind=question messages. Future writers (supervisor sweep, daily brief)
share this channel."""
from __future__ import annotations

import json
import os
import secrets
import sys
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Literal


SignalKind = Literal["question", "lesson", "supervisor", "brief"]


@dataclass(frozen=True)
class WakeSignal:
    source: str        # e.g. "inbox-drain", "supervisor-sweep", "daily-brief"
    kind: SignalKind   # filter predicate for readers
    msg_id: str        # back-reference to inbox msg-id when source is inbox
    from_project: str
    summary: str
    ts: datetime
    signal_id: str = field(default_factory=lambda: f"wake-{secrets.token_hex(4)}")

    def to_json(self) -> dict:
        d = asdict(self)
        d["ts"] = self.ts.astimezone(timezone.utc).isoformat()
        return d


def signals_dir(repo_root: Path) -> Path:
    return repo_root / ".fno" / "wake-signals"


def drop_signal(repo_root: Path, signal: WakeSignal) -> Path:
    """Atomically write a signal to <repo>/.fno/wake-signals/.

    Uses tmp-file-plus-rename to avoid partial reads from concurrent readers.
    Creates the parent directory if missing."""
    dest_dir = signals_dir(repo_root)
    dest_dir.mkdir(parents=True, exist_ok=True)

    final = dest_dir / f"{signal.signal_id}.json"
    tmp = dest_dir / f".tmp.{os.getpid()}.{signal.signal_id}"
    tmp.write_text(json.dumps(signal.to_json(), indent=2), encoding="utf-8")
    os.rename(tmp, final)
    return final


def read_signals(repo_root: Path, kind: SignalKind | None = None) -> list[dict]:
    """Read all signals (optionally filtered by kind). Does NOT delete.

    Used for non-destructive previews. Hook readers use drain_signals."""
    out = []
    d = signals_dir(repo_root)
    if not d.is_dir():
        return out
    for f in sorted(d.glob("wake-*.json")):
        try:
            payload = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if kind is not None and payload.get("kind") != kind:
            continue
        payload["_path"] = str(f)
        out.append(payload)
    return out


def drain_signals(repo_root: Path, kind: SignalKind | None = None) -> list[dict]:
    """Read AND delete signals matching kind. Used by hook readers.

    Returns the payloads (with ephemeral _path stripped) sorted by ts asc."""
    payloads = read_signals(repo_root, kind=kind)
    drained = []
    for p in payloads:
        path = Path(p.pop("_path"))
        try:
            path.unlink()
        except OSError as exc:
            print(f"wake.signal: unlink failed for {path}: {exc!r}", file=sys.stderr)
            continue
        drained.append(p)
    return sorted(drained, key=lambda x: x.get("ts", ""))
