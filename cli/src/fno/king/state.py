"""Scope-keyed king manifests and the freshness read.

One manifest per crown scope at ``<space>/kings/<scope>.md``, separate from
any target manifest: one file naming both drivers is how two sessions share
a discriminator. The manifest is the durable crown record and a registry
row its cache, so a leftover file is never inert: it is the record heal
restores a lost row from. ``last_run_is_fresh`` reads the events journal,
not file mtimes.
"""

from __future__ import annotations

import fcntl
import json
import os
import re
import secrets
import uuid
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

#: Iteration ceiling before a walk terminates on Budget.
DEFAULT_MAX_ITERATIONS = 40

#: Respawns allowed before the walk refuses another king session.
DEFAULT_RESPAWN_CEILING = 4

#: Both king arms end a walk: the walk arm emits ``loop_terminated``, the
#: in-session stop arm ``termination``.
_TERMINAL_TYPES = frozenset({"loop_terminated", "termination"})

_WINDOW = re.compile(r"^(\d+)([smhd]?)$")
_UNIT_S = {"s": 1, "m": 60, "h": 3600, "d": 86400, "": 1}


class KingLoopDisabled(RuntimeError):
    """`config.king.enabled` is false, so no manifest is written."""


def king_loop_enabled() -> bool:
    """Resolve ``config.king.enabled``, fail-safe to OFF like every gate here."""
    try:
        from fno.config import load_settings

        return bool(load_settings().king.enabled)
    except Exception:  # noqa: BLE001
        return False


class KingManifestExists(RuntimeError):
    """Raised when init would overwrite a manifest that is already there."""


def king_state_root(cwd: Path | None = None) -> Path:
    """The canonical-keyed coordination root, so a crown never lands in a
    disposable linked worktree however ``cwd`` is spelled."""
    from fno.paths import space_dir

    return space_dir(cwd)


def king_manifest_path(scope: str, *, state_root: Optional[Path] = None) -> Path:
    """The manifest path for one scope. Path syntax refuses (never
    normalizes): two spellings must never select two files."""
    scope = scope.strip()
    if not scope or ".." in scope or "/" in scope or "\\" in scope or "\0" in scope:
        raise ValueError(f"unsafe king scope for manifest path: {scope!r}")
    root = state_root if state_root is not None else king_state_root()
    return Path(root) / "kings" / f"{scope}.md"


def resolve_king_manifest_path(
    harness_session_id: str,
    harness: Optional[str],
    *,
    state_root: Optional[Path] = None,
    registry=None,
) -> Optional[Path]:
    """This live session's crowned manifest. The row, not file presence,
    proves authority; any unreadable or terminal reading returns None."""
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


def _transcript_matchable_session_id(value: str) -> bool:
    """Whether the stop hook's owner guard can match this id against a
    transcript basename: full 36-char uuids only (uuid.UUID() alone also
    accepts 8-hex short forms, which match no transcript)."""
    try:
        return len(value) == 36 and str(uuid.UUID(value)) == value.lower()
    except (ValueError, AttributeError):
        return False


def arm_king_manifest(
    scope: str,
    harness_session_id: str,
    *,
    state_root: Optional[Path] = None,
    owner_pid: Optional[int] = None,
    owner_cwd: Optional[str] = None,
    crown_level: Optional[int] = None,
    crown_scope: Optional[str] = None,
    crown_grantor: Optional[str] = None,
    row: Any = None,
) -> Optional[Path]:
    """Refresh loop state at the moment a crown becomes authoritative."""
    if row is not None:
        owner_pid = owner_pid or getattr(row, "pid", None)
        owner_cwd = owner_cwd or getattr(row, "cwd", None)
        crown_level = crown_level if crown_level is not None else getattr(row, "crown_level", None)
        crown_scope = crown_scope if crown_scope is not None else getattr(row, "crown_scope", None)
        crown_grantor = crown_grantor if crown_grantor is not None else getattr(row, "crown_grantor", None)
    if state_root is None:
        state_root = _owner_state_root(owner_cwd)
    if not king_loop_enabled():
        remove_king_manifest(scope, state_root=state_root)
        return None
    if not harness_session_id.strip():
        raise ValueError("a crowned king manifest needs a harness session id")
    if not _transcript_matchable_session_id(harness_session_id):
        raise ValueError(
            f"a crowned king manifest needs a full session uuid, got "
            f"{harness_session_id!r}: the stop hook matches the manifest id "
            "against the transcript basename, and a short id or row name "
            "matches no transcript, so the gate would arm dead. Re-arm with "
            "fno agents crown once the session has self-identified."
        )
    path = king_manifest_path(scope, state_root=state_root)
    with _manifest_lock(path):
        write_manifest(
            path,
            scope=scope,
            harness_session_id=harness_session_id,
            force=True,
            owner_pid=owner_pid,
            owner_cwd=owner_cwd,
            crown_level=crown_level,
            crown_scope=crown_scope,
            crown_grantor=crown_grantor,
        )
        path.with_suffix(".cancelled").unlink(missing_ok=True)
    return path


def _owner_state_root(owner_cwd: Optional[str]) -> Path:
    return king_state_root(Path(owner_cwd) if owner_cwd else None)


@contextmanager
def _manifest_lock(path: Path):
    lock = path.with_suffix(path.suffix + ".lock")
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a+") as handle:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def remove_king_manifest(
    scope: str,
    *,
    state_root: Optional[Path] = None,
    owner_cwd: Optional[str] = None,
    expected_harness_session_id: Optional[str] = None,
) -> bool:
    """Best-effort cleanup; live registry authority never depends on this."""
    try:
        if state_root is None:
            state_root = _owner_state_root(owner_cwd)
        path = king_manifest_path(scope, state_root=state_root)
        with _manifest_lock(path):
            if expected_harness_session_id:
                current = parse_manifest(path).get("harness_session_id")
                if current != expected_harness_session_id:
                    return False
            path.unlink(missing_ok=True)
        return True
    except (OSError, ValueError):
        return False


def _utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def mint_fno_id(pid: Optional[int] = None) -> str:
    """``{ts}-kg{pid}-{6hex}``: three dash-separated segments like the target
    manifest, because ``split('-')[0]`` consumers depend on that count."""
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"{stamp}-kg{pid or os.getpid()}-{secrets.token_hex(3)}"


def write_manifest(
    path: Path,
    *,
    scope: str,
    harness_session_id: str = "",
    max_iterations: int = DEFAULT_MAX_ITERATIONS,
    respawn_ceiling: int = DEFAULT_RESPAWN_CEILING,
    force: bool = False,
    owner_pid: Optional[int] = None,
    owner_cwd: Optional[str] = None,
    shape: str = "pass",
    crown_level: Optional[int] = None,
    crown_scope: Optional[str] = None,
    crown_grantor: Optional[str] = None,
) -> dict[str, str]:
    """Write the manifest once; raises KingManifestExists if it is there.

    ``respawn_count`` and ``wake_times`` start at 0 (a successor coronation
    is a new reign generation). ``shape`` rides from birth.
    """
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
        "shape": shape if shape in ("pass", "court") else "pass",
        "harness": os.environ.get("FNO_HARNESS", "claude"),
        "harness_session_id": harness_session_id,
        "owner_pid": str(owner_pid or os.getpid()),
        "owner_cwd": owner_cwd or str(Path.cwd()),
        "budget_max_iterations": str(max_iterations),
        "respawn_count": "0",
        "respawn_ceiling": str(respawn_ceiling),
        "wake_times": "",
    }
    if crown_scope:
        fields.update(
            crown_level=str(crown_level),
            crown_scope=crown_scope,
            crown_grantor=crown_grantor or "human",
        )
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


def at_respawn_ceiling(path: Path) -> bool:
    """Whether the manifest's respawn budget is spent - the wake phase's
    pre-check; the Rust walk's own read is the authority. Ceiling 0 is the
    unbounded spelling; an unreadable manifest reads as under it."""
    manifest = parse_manifest(path)

    def _int(key: str) -> int:
        try:
            return int(manifest.get(key) or 0)
        except (TypeError, ValueError):
            return 0

    ceiling = _int("respawn_ceiling")
    return ceiling > 0 and _int("respawn_count") >= ceiling


@dataclass
class ReignState:
    """One read of who reigns, over what, and is it live. Unknowns answer
    None with ``unknown_reason``, never a clean False."""

    crowned: Optional[bool] = None
    scope: Optional[str] = None
    shape: Optional[str] = None
    manifest_session: Optional[str] = None
    manifest_path: Optional[str] = None
    crown_on_manifest: Optional[bool] = None
    registry_session: Optional[str] = None
    live: Optional[bool] = None
    split: Optional[bool] = None
    unknown_reason: Optional[str] = None


def _unknown_state(scope: Optional[str], reason: str) -> ReignState:
    return ReignState(scope=scope, unknown_reason=reason)


def reign_state(
    scope: Optional[str] = None,
    session_id: Optional[str] = None,
    *,
    state_root: Optional[Path] = None,
) -> ReignState:
    """Ask the Rust reign reader (``fno-agents reign-state``) who reigns;
    Python never derives its own answer. Failures answer unknown, named.
    """
    import subprocess

    from fno import paths
    from fno.rust_binary import resolve_binary

    harness = None
    if scope is None and not session_id:
        from fno.agents.self_stamp import resolve_self_identity

        ident = resolve_self_identity()
        session_id = ident.session_id or ""
        harness = ident.harness or None

    root = state_root if state_root is not None else _owner_state_root(None)
    # The Rust reader joins <root>/kings itself; a suffixed root is a phantom path.
    binary = resolve_binary()
    if binary is None:
        return _unknown_state(
            scope,
            "fno-agents binary not found: the reign reader lives in Rust. "
            "Reinstall fno, run `fno doctor update --rust`, or set FNO_AGENTS_BIN.",
        )
    argv = [str(binary), "reign-state", "--root", str(root)]
    if scope:
        argv += ["--scope", scope]
    if session_id:
        argv += ["--session", session_id]
    if harness:
        argv += ["--harness", harness]
    try:
        argv += ["--registry", str(paths.agents_registry_path())]
        proc = subprocess.run(argv, capture_output=True, text=True, check=False, timeout=30)
    except (OSError, subprocess.SubprocessError) as exc:
        return _unknown_state(scope, f"reign reader failed to run: {exc}")
    if proc.returncode != 0:
        detail = proc.stderr.strip() or proc.stdout.strip()
        return _unknown_state(scope, f"reign reader exited {proc.returncode}: {detail}")
    try:
        payload = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        return _unknown_state(scope, f"reign reader emitted no JSON: {exc}")
    keys = (
        "crowned", "shape", "manifest_session", "manifest_path",
        "crown_on_manifest", "registry_session", "live", "split", "unknown_reason",
    )
    return ReignState(
        **{k: payload.get(k) for k in keys},
        scope=payload.get("scope") or scope,
    )


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
    """True when a king ``loop_terminated`` landed inside the window. Every
    failure answers False (no evidence); a corrupt line skips itself."""
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
