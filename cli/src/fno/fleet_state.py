"""Fleet-sweep watermark: is the failover host alive, and where is each node
in its fallback chain.

One file, ``~/.fno/fleet-sweep-state.json``, written as the fleet leg's last
act on every daemon tick. It exists because a status line is not evidence:
``fno pr-watch status`` reported the agent loaded while nothing had ticked for
six hours and eighteen minutes against a 600s interval, and ``failover_swapped``
had never been emitted once. A freshness check against this file's mtime
answers "did the trigger run" without trusting anyone's self-report.

Schema::

    {
      "ts": float,                 # epoch of the last completed fleet leg
      "candidates": int,           # rows the recovery sweep considered
      "refused": int,              # worker_refused events emitted this tick
      "silent": int,               # worker_silent events emitted this tick
      "chains": {node_id: [link, ...]}   # fallback links already spent per node
    }

``chains`` is the walk's memory: a node re-dispatched onto codex must not be
re-dispatched onto codex again next tick. A chain that loops is a worse failure
than a chain that ends.

Single-writer by construction - every writer runs inside the pr-watch tick,
which already holds the daemon lock - so this takes no lock of its own and
writes atomically through a temp file plus rename.
"""

from __future__ import annotations

import json
import logging
import os
import tempfile
import time
from pathlib import Path
from typing import Any

from fno.paths import state_dir

log = logging.getLogger(__name__)

_FILENAME = "fleet-sweep-state.json"
# A node that has walked its whole chain keeps its row until the node is done.
# Cap the stored walk so a pathological config cannot grow the file without
# bound; a chain longer than this is a config problem, not a state problem.
_MAX_LINKS_PER_NODE = 16


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
    payload = read_fleet_state(path)
    payload.update({
        "ts": time.time() if now is None else now,
        "candidates": int(candidates),
        "refused": int(refused),
        "silent": int(silent),
    })
    payload.setdefault("chains", {})
    return _write(payload, path)


def links_tried(node_id: str, path: Path | None = None) -> list[str]:
    """Fallback links already spent on ``node_id`` this run."""
    chains = read_fleet_state(path).get("chains")
    if not isinstance(chains, dict):
        return []
    got = chains.get(node_id)
    return [str(x) for x in got] if isinstance(got, list) else []


def record_link(node_id: str, link: str, path: Path | None = None) -> list[str]:
    """Append ``link`` to ``node_id``'s walk and return the full list.

    Written BEFORE the spawn, not after: a link whose spawn dies half-way must
    still count as tried, or the next tick tries the same failing vendor again.
    """
    payload = read_fleet_state(path)
    chains = payload.get("chains")
    if not isinstance(chains, dict):
        chains = {}
    got = [str(x) for x in chains.get(node_id, []) if isinstance(x, str)]
    if link not in got:
        got.append(link)
    chains[node_id] = got[-_MAX_LINKS_PER_NODE:]
    payload["chains"] = chains
    _write(payload, path)
    return chains[node_id]


def clear_node(node_id: str, path: Path | None = None) -> None:
    """Forget a node's walk (it shipped, or an operator reset it)."""
    payload = read_fleet_state(path)
    chains = payload.get("chains")
    if isinstance(chains, dict) and node_id in chains:
        del chains[node_id]
        payload["chains"] = chains
        _write(payload, path)
