"""Scope-keyed king manifests and the freshness read.

Why a separate file rather than a ``driver: king`` field on the target manifest.
A king runs in the canonical checkout, where a target manifest may also exist,
and a manifest whose name says target while its contents say king is how two
sessions come to share one discriminator. The stop-hook shim already searches a
candidate list, so a second candidate is the smaller real cost.

Each crown scope owns ``.fno/kings/<scope>.md``. The registry row is authority:
a leftover file with no live crown is inert, while a resumed session finds the
same file through its current registry row instead of an unstable session id.

``last_run_is_fresh`` is the second done-probe. A file test would be vacuous: it
would pass the moment the manifest existed and say nothing about whether a walk
ever terminated. This reads the events journal for the newest king
``loop_terminated`` and asks whether it falls inside a window, which is a claim
about the world rather than about a file.
"""
from __future__ import annotations

import json
import os
import re
import secrets
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

#: Iteration ceiling a king walk is allowed before it terminates on Budget.
#: A non-converging king should cost a ceiling, not a night.
DEFAULT_MAX_ITERATIONS = 40

#: Both king arms end a walk, so both satisfy the freshness probe. The walk arm
#: emits the runtime's ``loop_terminated``; the in-session stop arm emits
#: ``termination``. Reading only one would report "no king walk in 24h" right
#: after a king had in fact drained its board and exited.
_TERMINAL_TYPES = frozenset({"loop_terminated", "termination"})

_WINDOW = re.compile(r"^(\d+)([smhd]?)$")
_UNIT_S = {"s": 1, "m": 60, "h": 3600, "d": 86400, "": 1}


class KingLoopDisabled(RuntimeError):
    """`config.king.enabled` is false, so no manifest is written."""


def king_loop_enabled() -> bool:
    """Resolve ``config.king.enabled``, fail-safe to OFF.

    An unreadable config resolves an autonomous loop to off, matching every
    other gate resolver here.
    """
    try:
        from fno.config import load_settings

        return bool(load_settings().king.enabled)
    except Exception:  # noqa: BLE001
        return False


class KingManifestExists(RuntimeError):
    """Raised when init would overwrite a manifest that is already there."""


def king_manifest_path(scope: str, *, state_root: Path = Path(".fno")) -> Path:
    """Return the manifest path for one canonical crown scope.

    Scope is registry data, but it becomes a filename here. Refuse path syntax
    instead of normalizing it: two spellings of one scope must never select two
    files, and no scope may escape the state root.
    """
    scope = scope.strip()
    if not scope or ".." in scope or "/" in scope or "\\" in scope or "\0" in scope:
        raise ValueError(f"unsafe king scope for manifest path: {scope!r}")
    return Path(state_root) / "kings" / f"{scope}.md"


def resolve_king_manifest_path(
    harness_session_id: str,
    harness: Optional[str],
    *,
    state_root: Path = Path(".fno"),
    registry=None,
) -> Optional[Path]:
    """Resolve this live session's crowned scope to its existing manifest.

    The row, not file presence, proves authority. Any unreadable, missing,
    terminal, uncrowned, or unsafe reading returns ``None`` so stale state can
    never capture an unrelated session.
    """
    if not harness_session_id:
        return None
    try:
        from fno.agents.registry import TERMINAL_STATUSES, load_registry
        from fno.agents.whoami import _find_by_session

        rows = load_registry() if registry is None else registry
        row = _find_by_session(rows, harness_session_id, harness or None)
    except Exception:  # noqa: BLE001 - an unproved crown has no authority
        return None
    if row is None or getattr(row, "status", None) in TERMINAL_STATUSES:
        return None
    scope = getattr(row, "crown_scope", None)
    if not isinstance(scope, str) or not scope.strip():
        return None
    try:
        path = king_manifest_path(scope, state_root=state_root)
    except ValueError:
        return None
    return path if path.is_file() else None


def _utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def mint_fno_id(pid: Optional[int] = None) -> str:
    """``{ts}-kg{pid}-{6hex}``.

    Three dash-separated segments, matching the target manifest's shape, because
    ``split('-')[0]`` consumers depend on that count. The provenance infix goes
    inside segment two rather than becoming a fourth segment, for the same
    reason.
    """
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"{stamp}-kg{pid or os.getpid()}-{secrets.token_hex(3)}"


def write_manifest(
    path: Path,
    *,
    scope: str,
    harness_session_id: str = "",
    max_iterations: int = DEFAULT_MAX_ITERATIONS,
    force: bool = False,
) -> dict[str, str]:
    """Write the manifest once. Raises :class:`KingManifestExists` if it is there."""
    path = Path(path)
    if path.exists() and not force:
        raise KingManifestExists(
            f"{path} already exists; the king manifest is write-once. "
            "Pass --force to re-init deliberately."
        )
    fields = {
        "fno_id": mint_fno_id(),
        "created_at": _utc_now(),
        "scope": scope,
        "harness": os.environ.get("FNO_HARNESS", "claude"),
        "harness_session_id": harness_session_id,
        "owner_pid": str(os.getpid()),
        "owner_cwd": str(Path.cwd()),
        "budget_max_iterations": str(max_iterations),
    }
    body = "---\n" + "".join(f"{k}: {_dump(v)}\n" for k, v in fields.items()) + "---\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(body, encoding="utf-8")
    os.replace(str(tmp), str(path))
    return fields


def _dump(value: str) -> str:
    return json.dumps(value) if value != value.strip() or '"' in value else value


def parse_manifest(path: Path) -> dict[str, str]:
    """Read the frontmatter fields. An unreadable manifest reads as absent."""
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError:
        return {}
    out: dict[str, str] = {}
    for line in text.splitlines():
        if line.strip() == "---":
            continue
        if ":" not in line:
            continue
        key, _, raw = line.partition(":")
        raw = raw.strip()
        if raw.startswith('"'):
            try:
                raw = json.loads(raw)
            except json.JSONDecodeError:
                raw = raw.strip('"')
        out[key.strip()] = raw
    return out


def parse_window(window: str) -> int:
    """``24h`` / ``90m`` / ``7d`` / ``30s`` / a bare second count -> seconds."""
    match = _WINDOW.match(window.strip())
    if not match:
        raise ValueError(f"unparseable window {window!r}; use e.g. 24h, 90m, 7d")
    return int(match.group(1)) * _UNIT_S[match.group(2)]


def _parse_ts(raw: object) -> Optional[float]:
    if not isinstance(raw, str):
        return None
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def last_run_is_fresh(
    events_path: Path,
    *,
    since_s: int,
    now_iso: Optional[str] = None,
) -> bool:
    """True when a king ``loop_terminated`` landed inside the window.

    Every failure answers False. A missing journal, a corrupt line, and a walk
    that never ran are all "no evidence a king walk terminated recently", which
    is what the probe asks. A corrupt line skips itself rather than the file, so
    one bad row cannot hide a real termination underneath it.
    """
    now = _parse_ts(now_iso) if now_iso else datetime.now(timezone.utc).timestamp()
    if now is None:
        return False
    try:
        text = Path(events_path).read_text(encoding="utf-8")
    except OSError:
        return False
    newest: Optional[float] = None
    for line in text.splitlines():
        line = line.strip()
        if not line or not any(t in line for t in _TERMINAL_TYPES):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") not in _TERMINAL_TYPES:
            continue
        if (event.get("data") or {}).get("driver") != "king":
            continue
        ts = _parse_ts(event.get("ts"))
        if ts is not None and (newest is None or ts > newest):
            newest = ts
    if newest is None:
        return False
    return (now - newest) <= since_s
