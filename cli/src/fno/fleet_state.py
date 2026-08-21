"""Fleet-sweep watermark: is the failover host alive, and where is each node
in its fallback chain.

One file, ``~/.fno/fleet-sweep-state.json``, written as the fleet leg's last
act on every daemon tick. It exists because a status line is not evidence:
``fno do pr watch status`` reported the agent loaded while nothing had ticked for
six hours and eighteen minutes against a 600s interval, and ``failover_swapped``
had never been emitted once. A freshness check against this file's mtime
answers "did the trigger run" without trusting anyone's self-report.

Schema::

    {
      "ts": float,                 # epoch of the last completed fleet leg
      "candidates": int,           # rows the recovery sweep considered
      "refused": int,              # worker_refused events emitted this tick
      "silent": int,               # worker_silent events emitted this tick
      "chains": {node_id: {"links": [link, ...], "at": float}}
    }

``chains`` is the walk's memory: a node re-dispatched onto codex must not be
re-dispatched onto codex again next tick. A chain that loops is a worse failure
than a chain that ends.

Not single-writer. The fleet leg runs BEFORE ``_tick`` acquires the daemon
lock, and it runs whether or not that lock turns out to be held, so a manual
``fno do pr watch tick`` overlapping the launchd one gives two concurrent
read-modify-write cycles on this file. The loser's link would be dropped and
the chain would relap the vendor it just tried, which is precisely the loop
this file exists to prevent. Every mutation therefore takes a sidecar filelock,
the same shape ``runtime_state`` uses, and writes atomically through a temp
file plus rename.
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import tempfile
import time
from pathlib import Path
from typing import Any

import filelock

from fno.paths import state_dir

log = logging.getLogger(__name__)

_FILENAME = "fleet-sweep-state.json"
# A node that has walked its whole chain keeps its row until the node is done.
# Cap the stored walk so a pathological config cannot grow the file without
# bound; a chain longer than this is a config problem, not a state problem.
_MAX_LINKS_PER_NODE = 16
#: How long a node's walk is remembered. Without an age-out, one systematic
#: spawn failure burns the chain link by link and the node then emits
#: `failover_exhausted: all-tried` forever - including long after every
#: provider has recovered. A day is longer than any measured window and short
#: enough that a stuck node frees itself by the next morning.
_WALK_TTL_S = 24 * 3600


#: Seconds to wait for the sidecar lock. Short on purpose: this is a watermark
#: and a memo, so losing the write costs one repeated chain link, while
#: blocking the tick costs the failover pass.
_LOCK_TIMEOUT_S = 5


@contextlib.contextmanager
def _lock(path: Path | None = None):
    """Serialize a read-modify-write. A contended lock degrades to unlocked.

    Failing the write outright would cost the whole fleet leg; proceeding
    unlocked costs at most one lost link, which the next tick re-records.
    """
    target = path or fleet_state_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        with filelock.FileLock(str(target) + ".lock", timeout=_LOCK_TIMEOUT_S):
            yield
    except filelock.Timeout:
        log.warning("fleet_state: lock contention on %s; writing unlocked", target)
        yield


def fleet_state_path() -> Path:
    """``~/.fno/fleet-sweep-state.json`` (or the configured state root)."""
    return state_dir() / _FILENAME


def read_fleet_state(path: Path | None = None) -> dict[str, Any]:
    """The stored payload, or an empty dict.

    Fail-open on every read failure: a corrupt watermark must degrade to "no
    memory" rather than break the tick that would have refreshed it. The cost
    of an empty read is one repeated chain link; the cost of a raise is the
    failover trigger not running at all.
    """
    p = path or fleet_state_path()
    try:
        raw = json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return {}
    return raw if isinstance(raw, dict) else {}


def _write(payload: dict[str, Any], path: Path | None = None) -> Path:
    p = path or fleet_state_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=str(p.parent), prefix=".fleet-sweep-", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=2, sort_keys=True)
        os.replace(tmp, p)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return p


def write_heartbeat(
    *,
    candidates: int,
    refused: int = 0,
    silent: int = 0,
    now: float | None = None,
    path: Path | None = None,
) -> Path:
    """Stamp this tick's fleet-leg watermark, preserving the chain memory."""
    with _lock(path):
        payload = read_fleet_state(path)
        payload.update({
            "ts": time.time() if now is None else now,
            "candidates": int(candidates),
            "refused": int(refused),
            "silent": int(silent),
        })
        payload.setdefault("chains", {})
        return _write(payload, path)


def _walk(entry: Any, now: float) -> list[str]:
    """One node's stored walk, or [] when absent, malformed, or aged out."""
    if not isinstance(entry, dict):
        return []
    at = entry.get("at")
    if not isinstance(at, (int, float)) or now - float(at) > _WALK_TTL_S:
        return []
    links = entry.get("links")
    return [str(x) for x in links if isinstance(x, str)] if isinstance(links, list) else []


def links_tried(
    node_id: str, path: Path | None = None, now: float | None = None
) -> list[str]:
    """Fallback links already spent on ``node_id``, within the walk's TTL."""
    chains = read_fleet_state(path).get("chains")
    if not isinstance(chains, dict):
        return []
    return _walk(chains.get(node_id), time.time() if now is None else now)


def record_link(
    node_id: str, link: str, path: Path | None = None, now: float | None = None
) -> list[str]:
    """Append ``link`` to ``node_id``'s walk and return the full list.

    Written BEFORE the spawn, not after: a link whose spawn dies half-way must
    still count as tried, or the next tick tries the same failing vendor again.
    Every write restamps the walk, so the TTL measures time since the last
    attempt rather than since the first.
    """
    stamp = time.time() if now is None else now
    with _lock(path):
        payload = read_fleet_state(path)
        chains = payload.get("chains")
        if not isinstance(chains, dict):
            chains = {}
        got = _walk(chains.get(node_id), stamp)
        if link not in got:
            got.append(link)
        chains[node_id] = {"links": got[-_MAX_LINKS_PER_NODE:], "at": stamp}
        payload["chains"] = chains
        _write(payload, path)
        return chains[node_id]["links"]


def clear_node(node_id: str, path: Path | None = None) -> None:
    """Forget a node's walk (it shipped, or an operator reset it)."""
    with _lock(path):
        payload = read_fleet_state(path)
        chains = payload.get("chains")
        if isinstance(chains, dict) and node_id in chains:
            del chains[node_id]
            payload["chains"] = chains
            _write(payload, path)


def silent_seen(path: Path | None = None) -> set[str]:
    """Handles already reported silent, so the report fires on the TRANSITION.

    Without this a worker idle past its deadline re-emits every tick for as
    long as it stays idle, which both bloats the event log and destroys the
    event's value as a "something changed" signal.
    """
    got = read_fleet_state(path).get("silent_seen")
    return {str(x) for x in got} if isinstance(got, list) else set()


def set_silent_seen(handles, path: Path | None = None) -> None:
    """Replace the reported-silent set. Absent handles re-arm by construction."""
    with _lock(path):
        payload = read_fleet_state(path)
        payload["silent_seen"] = sorted({str(h) for h in handles})
        _write(payload, path)
