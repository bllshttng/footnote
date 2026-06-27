"""Group 2 of the cross-session agent relay (x-908b / x-e4ac): a PERSISTENT
session registry. Maps ``session_id -> {provider, pid, cwd, inject_handle,
status}`` so a peer is addressable by its session id and discovery survives a
restart (cmux and herdr both fail here by staying in-memory; the design's
Architecture/Registry section makes persistence the fix).

Two sources, two durability models:

- **Discovered claude sessions.** Read live from ``~/.claude/sessions/<pid>.json``
  via :func:`fno.agents.discover.discover_live_sessions` (already robust to the
  7000-entry / ``.sync-conflict-*`` / mid-write reality). These are NOT persisted
  here: pid-liveness is the live truth, and persisting a dead session row is the
  exact cmux/herdr flaw inverted. They re-derive from disk on every read, so they
  "survive restarts" for free.
- **footnote-owned relay peers.** A peer the daemon spawns as interactive claude
  (E4.1) is the routable participant, but nothing else on disk records it. THIS is
  what the registry file persists. Its
  ``inject_handle`` is a durable pointer (``pty:<pid>``) the G3 daemon will resolve
  to the live PTY fd; the fd itself is process-local and not persistable, so it is
  deliberately not stored here.

:func:`index` returns the union (persisted peers win on a session-id clash, since
they carry the real ``inject_handle``). No daemon, no live injection -- that is G3.
"""
from __future__ import annotations

import fcntl
import json
import os
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterator, Optional

from fno import paths
from fno.agents.discover import discover_live_sessions

_SCHEMA_VERSION = 1


@dataclass(frozen=True)
class RegistryEntry:
    """One addressable session. ``inject_handle`` is None for a discovered
    session footnote does not own a PTY for (injection into it is refused
    downstream per the design's Failure Modes); a footnote-owned peer carries a
    durable ``pty:<pid>`` handle."""

    session_id: str
    provider: str
    pid: int
    cwd: Optional[str] = None
    inject_handle: Optional[str] = None
    status: Optional[str] = None
    name: Optional[str] = None
    transcript_path: Optional[str] = None  # the jsonl the OUT leg tails (G2 capture)


def registry_path() -> Path:
    """``~/.fno/relay/registry.json`` (resolved against the active state dir)."""
    return paths.state_dir() / "relay" / "registry.json"


def transcript_path_for(
    session_id: str, *, projects_dir: Optional[Path] = None
) -> Optional[str]:
    """Resolve a session's transcript jsonl -- the OUT-leg capture source.

    claude encodes the ``projects/`` subdir by replacing BOTH ``/`` and ``.`` in
    the cwd with ``-``, so glob by the ``<session_id>.jsonl`` filename rather than
    deriving the path from cwd (the naive ``/``->``-`` derivation misses the dot,
    proven in the x-e4ac probe). Returns None when no transcript exists yet --
    which on this host means the peer was spawned without scrubbing the parent's
    ``CLAUDE_CODE_*`` env (the daemon spawn recipe, E4.1, applies that scrub)."""
    base = projects_dir or (Path.home() / ".claude" / "projects")
    try:
        hits = sorted(base.glob(f"*/{session_id}.jsonl"))
    except OSError:
        return None
    return str(hits[0]) if hits else None


def _atomic_write_json(target: Path, data: dict) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    # Per-pid temp so two concurrent writers never collide on the temp path.
    tmp = target.with_suffix(target.suffix + f".{os.getpid()}.tmp")
    tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
    os.replace(tmp, target)  # atomic; a kill -9 mid-write cannot corrupt target


@contextmanager
def _lock(path: Path) -> Iterator[None]:
    """Serialize a registry read-modify-write across processes. The G3 daemon
    spawns + registers peers concurrently, so an unlocked load->mutate->write
    would let the later writer clobber the earlier peer (lost entry). Mirrors
    the graph store's flock-on-a-sidecar pattern (codex P2 on PR #43)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = path.with_suffix(path.suffix + ".lock")
    fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def load(path: Optional[Path] = None) -> dict[str, RegistryEntry]:
    """Read the persisted footnote-owned peers. A missing, corrupt, or
    wrong-version file yields ``{}`` -- a junk registry must never deny lookup,
    it just means no peers were persisted yet."""
    path = path or registry_path()
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError, OSError, UnicodeDecodeError):
        return {}
    if not isinstance(raw, dict) or raw.get("schema_version") != _SCHEMA_VERSION:
        return {}
    sessions = raw.get("sessions")
    if not isinstance(sessions, dict):
        return {}
    out: dict[str, RegistryEntry] = {}
    for sid, row in sessions.items():
        if not isinstance(row, dict) or "provider" not in row or "pid" not in row:
            continue  # skip a single malformed row, keep the healthy ones
        out[sid] = RegistryEntry(
            session_id=sid,
            provider=row["provider"],
            pid=row["pid"],
            cwd=row.get("cwd"),
            inject_handle=row.get("inject_handle"),
            status=row.get("status"),
            name=row.get("name"),
            transcript_path=row.get("transcript_path"),
        )
    return out


def _write(entries: dict[str, RegistryEntry], path: Path) -> None:
    sessions = {sid: {k: v for k, v in asdict(e).items() if k != "session_id"}
                for sid, e in entries.items()}
    _atomic_write_json(path, {"schema_version": _SCHEMA_VERSION, "sessions": sessions})


def register(entry: RegistryEntry, path: Optional[Path] = None) -> None:
    """Persist a footnote-owned peer (upsert on ``session_id``)."""
    path = path or registry_path()
    with _lock(path):
        entries = load(path)
        entries[entry.session_id] = entry
        _write(entries, path)


def unregister(session_id: str, path: Optional[Path] = None) -> None:
    """Drop a peer. Silent no-op if it was not registered."""
    path = path or registry_path()
    with _lock(path):
        entries = load(path)
        if entries.pop(session_id, None) is not None:
            _write(entries, path)


def index(
    path: Optional[Path] = None,
    *,
    include_discovered: bool = True,
) -> dict[str, RegistryEntry]:
    """The full live index: persisted footnote peers folded over live-discovered
    claude sessions. Persisted peers win a session-id clash (they carry the real
    ``inject_handle``); discovery refreshes ``status``/``cwd`` for everything
    else."""
    merged: dict[str, RegistryEntry] = {}
    if include_discovered:
        for s in discover_live_sessions():
            merged[s.session_id] = RegistryEntry(
                session_id=s.session_id,
                provider=s.agent,
                pid=s.pid,
                cwd=s.cwd,
                inject_handle=None,  # hand-started: footnote owns no PTY for it
                status=s.status,
                name=s.handle,
                transcript_path=transcript_path_for(s.session_id),
            )
    merged.update(load(path))  # persisted peers win
    return merged
