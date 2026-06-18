"""Megatron mission state file: filelock-protected I/O with monotonicity.

Each mission owns a state file at ``~/.fno/fleet/{slug}/state.md``
with YAML frontmatter that the commander reads and rewrites atomically.
The lock lives at ``state.md.lock`` (sibling) so two commanders cannot
race; a stale lock from a crashed commander resolves on next start
because the lock is fcntl-based and released when the process exits.

Allowed status transitions:
    pending  -> running | cancelled
    running  -> paused | complete | cancelled
    paused   -> running | cancelled
    complete -> (terminal)
    cancelled -> (terminal)

Backwards or sideways transitions raise ``MissionStateRegression``.
Corrupt frontmatter is renamed to ``state.md.bak`` so the operator can
inspect it; ``read_state`` raises ``MissionStateCorrupt``.
"""
from __future__ import annotations

import json
import os
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

import yaml
from filelock import FileLock, Timeout

from fno.megatron._constants import TERMINAL_STATUSES


class MissionStateError(Exception):
    """Base class for mission-state errors."""


class MissionStateCorrupt(MissionStateError):
    """Raised when a state file's YAML frontmatter is unparseable."""


class MissionStateRegression(MissionStateError):
    """Raised when a write attempts a forbidden status transition."""


class CommanderAlreadyRunning(MissionStateError):
    """Raised when the filelock cannot be acquired within timeout."""


_LOCK_TIMEOUT_SECONDS = 5.0
_FRONTMATTER_DELIM = "---"

_ALLOWED_TRANSITIONS: dict[str, set[str]] = {
    "pending": {"pending", "running", "cancelled", "failed"},
    "running": {"running", "paused", "complete", "cancelled", "failed"},
    "paused": {"paused", "running", "cancelled", "failed"},
    "complete": {"complete"},
    "cancelled": {"cancelled"},
    "failed": {"failed"},
}

# TERMINAL_STATUSES is single-sourced in _constants.py; update_status
# reads it directly to decide when to call write_mission_artifact.


@dataclass
class MissionState:
    mission_id: str
    status: str
    created_at: Optional[str] = None
    paused_reason: Optional[str] = None
    budget_cap_usd: Optional[float] = None
    sent_msg_ids: dict[str, list[str]] = field(default_factory=dict)
    # received_completes is no longer a stored field; see the @property
    # below. Use ``_received_completes_override`` for test fixtures that
    # need to inject completion records without touching the filesystem.
    _received_completes_override: Optional[list[dict]] = field(default=None)
    # Manifest immutability checksum. Lazy-init at first dispatch; once set
    # to a non-None value, never changes unless the operator manually clears
    # the field (re-baseline path). See loop.run_iteration for the compare.
    manifest_sha256: Optional[str] = None
    manifest_sha256_first_set_at: Optional[str] = None  # ISO-8601 UTC
    body: str = ""
    extra: dict = field(default_factory=dict)
    # Filesystem-derived; populated by ``read_state`` from ``path.parent.name``.
    # Never serialized into state.md frontmatter, so the on-disk schema is
    # unchanged. None when MissionState is constructed in-memory without a
    # backing file (tests, in-process callers).
    slug: Optional[str] = None
    # Filesystem-derived: forwarded by ``read_state`` so the property can
    # walk a non-default fleet root (test fixtures, sandboxed runs).
    _fleet_root_override: Optional[Path] = field(default=None)

    @property
    def received_completes(self) -> list[dict]:
        """Filesystem-derived list of wave completion records.

        Walks ``{fleet_root}/{slug}/completions/wave-*/*.json`` on every
        access. Returns ``[]`` when ``slug`` is ``None`` (in-memory state
        with no backing fleet dir). Tests can inject a synthetic list via
        ``_received_completes_override`` to avoid filesystem I/O.
        """
        if self._received_completes_override is not None:
            return self._received_completes_override
        if not self.slug:
            return []
        return _rebuild_received_completes_from_filesystem(
            self.slug,
            fleet_root=self._fleet_root_override,
        )


def _lock_path_for(state_path: Path) -> Path:
    return state_path.with_name(state_path.name + ".lock")


def _split_frontmatter(text: str) -> tuple[str, str]:
    """Return (frontmatter_yaml, body) or raise MissionStateCorrupt."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != _FRONTMATTER_DELIM:
        raise MissionStateCorrupt("missing opening '---' frontmatter delimiter")
    end_index = None
    for i in range(1, len(lines)):
        if lines[i].strip() == _FRONTMATTER_DELIM:
            end_index = i
            break
    if end_index is None:
        raise MissionStateCorrupt("missing closing '---' frontmatter delimiter")
    fm = "\n".join(lines[1:end_index])
    body = "\n".join(lines[end_index + 1:]).lstrip("\n")
    return fm, body


def _state_from_dict(data: dict, body: str) -> MissionState:
    if not isinstance(data, dict):
        raise MissionStateCorrupt("frontmatter must be a YAML mapping")
    mission_id = data.get("mission_id")
    status = data.get("status")
    if not mission_id or not status:
        raise MissionStateCorrupt(
            "frontmatter missing required field(s): mission_id, status"
        )
    sent_raw = data.get("sent_msg_ids") or {}
    if not isinstance(sent_raw, dict):
        raise MissionStateCorrupt("'sent_msg_ids' must be a mapping")
    sent: dict[str, list[str]] = {}
    for key, value in sent_raw.items():
        if not isinstance(value, list):
            raise MissionStateCorrupt(f"'sent_msg_ids.{key}' must be a list")
        sent[str(key)] = [str(v) for v in value]

    # ``slug`` is filesystem-derived (path.parent.name) and stamped onto the
    # dataclass by ``read_state``. If a stale state.md frontmatter carries a
    # ``slug:`` key, drop it here so the in-memory value cannot be shadowed
    # by serialized data the writer is about to discard anyway.
    # ``received_completes`` is now a @property on MissionState backed by a
    # filesystem walk; if a legacy state.md carries the key, silently drop
    # it (the property would ignore a stored value anyway).
    extra = {
        k: v for k, v in data.items()
        if k not in (
            "mission_id",
            "status",
            "created_at",
            "paused_reason",
            "budget_cap_usd",
            "sent_msg_ids",
            "received_completes",
            "manifest_sha256",
            "manifest_sha256_first_set_at",
            "slug",
        )
    }
    return MissionState(
        mission_id=str(mission_id),
        status=str(status),
        created_at=str(data["created_at"]) if data.get("created_at") is not None else None,
        paused_reason=str(data["paused_reason"]) if data.get("paused_reason") is not None else None,
        budget_cap_usd=float(data["budget_cap_usd"]) if data.get("budget_cap_usd") is not None else None,
        sent_msg_ids=sent,
        manifest_sha256=str(data["manifest_sha256"]) if data.get("manifest_sha256") is not None else None,
        manifest_sha256_first_set_at=str(data["manifest_sha256_first_set_at"]) if data.get("manifest_sha256_first_set_at") is not None else None,
        body=body,
        extra=extra,
    )


def _state_to_dict(state: MissionState) -> dict:
    out: dict[str, Any] = {
        "mission_id": state.mission_id,
        "status": state.status,
    }
    if state.created_at is not None:
        out["created_at"] = state.created_at
    if state.paused_reason is not None:
        out["paused_reason"] = state.paused_reason
    if state.budget_cap_usd is not None:
        out["budget_cap_usd"] = state.budget_cap_usd
    out["sent_msg_ids"] = dict(state.sent_msg_ids)
    # received_completes is filesystem-derived; never written to state.md.
    # It is rebuilt from disk by read_state on every load.
    # state.slug is intentionally NOT serialized: it is filesystem-derived
    # and stamped at read time. Persisting it would create a redundant
    # source of truth and let on-disk drift mask filesystem moves.
    # Manifest immutability fields - serialized whenever non-None so that a
    # state.md round-trip preserves them. Operator clears them by hand-editing
    # the file (the re-baseline recovery path).
    if state.manifest_sha256 is not None:
        out["manifest_sha256"] = state.manifest_sha256
    if state.manifest_sha256_first_set_at is not None:
        out["manifest_sha256_first_set_at"] = state.manifest_sha256_first_set_at
    for k, v in state.extra.items():
        out.setdefault(k, v)
    return out


def _serialize(state: MissionState) -> str:
    payload = _state_to_dict(state)
    yaml_text = yaml.safe_dump(payload, sort_keys=False)
    body = state.body.rstrip("\n")
    if body:
        return f"---\n{yaml_text}---\n\n{body}\n"
    return f"---\n{yaml_text}---\n"


def _rebuild_received_completes_from_filesystem(
    slug: Optional[str], fleet_root: Optional[Path] = None
) -> list[dict]:
    """Walk the fleet completions tree and return all completion dicts.

    Reads every ``*.json`` file under
    ``{fleet_root}/{slug}/completions/wave-*/`` sorted by (wave-dir, filename)
    for determinism. Corrupt files are silently skipped (the loop layer
    already emitted ``completion_file_corrupt`` events when it encountered
    them).

    Best-effort: returns ``[]`` when:
    - ``slug`` is None or empty, or
    - ``fleet_root`` doesn't resolve, or
    - the completions dir doesn't exist yet.
    """
    if not slug:
        return []
    if fleet_root is None:
        from fno import paths as _paths
        fleet_root = _paths.fleet_dir()
    resolved_root = fleet_root
    completions_root = resolved_root / slug / "completions"
    if not completions_root.is_dir():
        return []

    results: list[dict] = []
    try:
        wave_dirs = sorted(completions_root.glob("wave-*"))
    except OSError:
        # Permission denied / vanished dir: return [] per the best-effort
        # contract above. The wave-complete predicate sees an empty list
        # and returns False, so the loop keeps waiting rather than
        # advancing on stale state.
        return results
    for wave_dir in wave_dirs:
        if not wave_dir.is_dir():
            continue
        try:
            json_files = sorted(wave_dir.glob("*.json"))
        except OSError:
            continue
        for json_file in json_files:
            try:
                text = json_file.read_text(encoding="utf-8")
                data = json.loads(text)
            except (json.JSONDecodeError, OSError):
                # Skip silently; the loop layer emitted telemetry already.
                continue
            # Mirror the loop-layer guard: a non-mapping top-level is malformed
            # for downstream consumers (artifact aggregator walks .get on each
            # entry). Skip rather than append a record that would crash.
            if not isinstance(data, dict):
                continue
            results.append(data)
    return results


def read_state(
    path: Path | str, *, fleet_root: Optional[Path] = None
) -> MissionState:
    path = Path(path)
    text = path.read_text(encoding="utf-8")
    try:
        fm_text, body = _split_frontmatter(text)
    except MissionStateCorrupt:
        _back_up_corrupt(path)
        raise
    try:
        data = yaml.safe_load(fm_text)
    except yaml.YAMLError as exc:
        _back_up_corrupt(path)
        raise MissionStateCorrupt(f"YAML parse error: {exc}") from exc
    state = _state_from_dict(data or {}, body)
    # Slug is filesystem-derived (parent dir name) and never written back
    # into state.md; _state_to_dict skips it.
    state.slug = path.parent.name
    # received_completes is a @property: the filesystem walk happens on
    # every read via state.received_completes. Forward fleet_root so the
    # property can find the right completions dir for sandboxed runs.
    state._fleet_root_override = fleet_root
    return state


def _back_up_corrupt(path: Path) -> None:
    backup = path.with_name(path.name + ".bak")
    try:
        os.replace(path, backup)
    except FileNotFoundError:  # pragma: no cover - file vanished mid-call
        return


def _check_transition(current: str, new: str) -> None:
    if current == new:
        return
    allowed = _ALLOWED_TRANSITIONS.get(current)
    if allowed is None:
        raise MissionStateRegression(
            f"unknown current status {current!r}; refusing write"
        )
    if new not in allowed:
        raise MissionStateRegression(
            f"status {current} -> {new} not allowed"
        )


def _atomic_write(path: Path, text: str) -> None:
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def write_state(path: Path | str, state: MissionState) -> None:
    path = Path(path)
    lock = FileLock(str(_lock_path_for(path)), timeout=_LOCK_TIMEOUT_SECONDS)
    try:
        with lock:
            if path.exists():
                current = read_state(path)
                _check_transition(current.status, state.status)
            _atomic_write(path, _serialize(state))
    except Timeout as exc:
        raise CommanderAlreadyRunning(
            f"mission {state.mission_id} has another commander; refusing to start"
        ) from exc


_UPDATE_STATE_FIELD_ALLOWED: tuple[str, ...] = (
    "manifest_sha256",
    "manifest_sha256_first_set_at",
)


def stamp_manifest_sha(
    path: Path | str,
    sha: str,
    first_set_at: str,
    *,
    fleet_root: Optional[Path] = None,
) -> None:
    """Atomically write manifest_sha256 + manifest_sha256_first_set_at
    together inside one filelock acquisition.

    Solves the partial-write window where two sequential
    ``update_state_field`` calls could crash between the writes and leave
    ``manifest_sha256`` set with ``manifest_sha256_first_set_at`` still
    None - a state the lazy-init guard
    (``if state.manifest_sha256 is None``) would never recover.

    Both fields must be written together so the joint invariant "either
    both fields are None or both are non-None" holds even on process
    interruption between writes. ``fleet_root`` is forwarded to
    ``read_state`` for consistency with other state-management helpers
    so the in-memory state object's filesystem-derived fields populate
    correctly during the read-modify-write cycle.
    """
    path = Path(path)
    lock = FileLock(str(_lock_path_for(path)), timeout=_LOCK_TIMEOUT_SECONDS)
    try:
        with lock:
            state = read_state(path, fleet_root=fleet_root)
            state.manifest_sha256 = sha
            state.manifest_sha256_first_set_at = first_set_at
            _atomic_write(path, _serialize(state))
    except Timeout as exc:
        raise MissionStateError(
            f"stamp_manifest_sha: filelock timed out on {_lock_path_for(path)}"
        ) from exc


def update_state_field(
    path: Path | str,
    field_name: str,
    value: Any,
    *,
    allowed_fields: tuple[str, ...] = _UPDATE_STATE_FIELD_ALLOWED,
    fleet_root: Optional[Path] = None,
) -> None:
    """Atomically stamp a single MissionState field under filelock.

    Read-modify-write happens entirely inside the lock so no caller-held
    state object can race with concurrent writers. The allowed_fields
    allowlist prevents callers from using this helper to flip status
    (which has its own dedicated path with transition checks).
    ``fleet_root`` is forwarded to ``read_state`` for consistency with
    other state-management helpers (``update_status``).
    """
    if field_name not in allowed_fields:
        raise MissionStateError(
            f"update_state_field: refusing to write field {field_name!r}; "
            f"allowed: {allowed_fields}"
        )
    path = Path(path)
    lock = FileLock(str(_lock_path_for(path)), timeout=_LOCK_TIMEOUT_SECONDS)
    try:
        with lock:
            state = read_state(path, fleet_root=fleet_root)
            setattr(state, field_name, value)
            _atomic_write(path, _serialize(state))
    except Timeout as exc:
        raise MissionStateError(
            f"update_state_field: filelock timed out on {_lock_path_for(path)}"
        ) from exc


def append_sent_msg_id(path: Path | str, wave: int, msg_id: str) -> None:
    """Order-preserving append of msg_id to ``sent_msg_ids[wave_{wave}]``.

    Filelock-protected so concurrent commanders cannot tear the list.
    Idempotent: appending the same msg_id twice is a no-op.
    """
    path = Path(path)
    key = f"wave_{wave}"
    lock = FileLock(str(_lock_path_for(path)), timeout=_LOCK_TIMEOUT_SECONDS)
    try:
        with lock:
            state = read_state(path)
            existing = state.sent_msg_ids.setdefault(key, [])
            if msg_id not in existing:
                existing.append(msg_id)
            _atomic_write(path, _serialize(state))
    except Timeout as exc:
        raise MissionStateError(
            f"append_sent_msg_id: filelock timed out on {_lock_path_for(path)}"
        ) from exc


def _append_received_complete_for_test(
    state_path: Path | str,
    *,
    from_project: str,
    msg_id: str,
    reply_to: Optional[str],
    wave: int,
    fleet_root: Optional[Path] = None,
    commit_sha: Optional[str] = None,
    discoveries: Optional[str] = None,
) -> None:
    """TEST-ONLY HELPER. Write a completion JSON file to the fleet dir.

    Production state population happens via filesystem completion files
    written by target-stop-hook (Wave 3). This helper exists so tests can
    seed the filesystem path that ``_completes_for_wave`` reads.

    Idempotent: writing the same ``{from_project}.json`` overwrites the
    previous file atomically (same filesystem rename).

    ``fleet_root`` defaults to the parent of ``state_path`` so the helper
    works with the common test pattern of placing state.md inside the
    fleet dir (``{fleet_root}/{slug}/state.md``).
    """
    import datetime as _dt

    state_path = Path(state_path)
    # Derive fleet_dir from state_path: state_path is {fleet_root}/{slug}/state.md
    # so fleet_dir = state_path.parent.
    fleet_dir = state_path.parent if fleet_root is None else fleet_root / state_path.parent.name
    completions_dir = fleet_dir / "completions" / f"wave-{wave}"
    completions_dir.mkdir(parents=True, exist_ok=True)

    # Read mission_id from the state file for the payload.
    try:
        fm_text, _ = _split_frontmatter(state_path.read_text(encoding="utf-8"))
        data = yaml.safe_load(fm_text) or {}
        mission_id = str(data.get("mission_id", ""))
    except Exception:
        mission_id = ""

    payload = {
        "project": from_project,
        "wave": wave,
        "mission_id": mission_id,
        "pr_url": None,
        "pr_status": None,
        "commit_sha": commit_sha,
        "completed_at": _dt.datetime.now(_dt.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "reply_to_msg_id": reply_to,
        "msg_id": msg_id,
        "discoveries": discoveries,
    }
    tmp = completions_dir / f".{from_project}.json.tmp"
    tmp.write_text(json.dumps(payload), encoding="utf-8")
    tmp.replace(completions_dir / f"{from_project}.json")


# Public alias kept for backwards compatibility with test call sites.
# New code should use ``_append_received_complete_for_test`` directly.
append_received_complete = _append_received_complete_for_test


# State-machine status -> events-schema status. Explicit dict prevents
# the parallel-structure drift the type-design panel flagged: if a future
# state (e.g. "failed") gets added to _ALLOWED_TRANSITIONS, adding the
# matching event mapping here is one line and the test fixture forces
# the conscious decision. The schema's mission_complete.status enum is
# [done, failed, cancelled]; "complete" maps to "done" because the state
# machine and the schema use different vocabularies for success.
_TERMINAL_EVENT_STATUSES: dict[str, str] = {
    "complete": "done",
    "failed": "failed",
    "cancelled": "cancelled",
}


def _emit_status_event(mission_id: str, prev_status: str, new_status: str) -> None:
    """Emit a megatron lifecycle event for a status transition.

    Three transition shapes emit events:
      - any -> running (first time): mission_started
      - any -> terminal status in _TERMINAL_EVENT_STATUSES: mission_complete
        with the mapped schema-side status

    Telemetry only - failures are swallowed so a broken events.jsonl
    cannot block a critical state write. The emit lands in
    ``.fno/events.jsonl`` relative to cwd; if that directory
    does not exist (test fixtures, non-repo cwds), the call is a
    silent no-op.
    """
    try:
        # Anchor via resolve_repo_root so this telemetry survives
        # invocation from arbitrary cwds. Pre-fix: Path(".fno")
        # silently no-op'd when the user ran `fno megatron run` from
        # outside the repo. BUG-MT-008.
        from fno.megatron._telemetry import resolve_events_path

        events_path = resolve_events_path()
        if events_path is None:
            return

        # Lazy import: keeps the megatron package from forcing a YAML
        # schema load at import time when callers only need state I/O.
        from fno import events as _events

        if new_status == "running" and prev_status != "running":
            _events.append_event(
                _events.mission_started(mission_id=mission_id),
                events_path=events_path,
            )
            return

        event_status = _TERMINAL_EVENT_STATUSES.get(new_status)
        if event_status is not None:
            _events.append_event(
                _events.mission_complete(mission_id=mission_id, status=event_status),
                events_path=events_path,
            )
    except Exception:
        # Telemetry must never block a state write. Swallow.
        pass


def update_status(
    path: Path | str,
    new_status: str,
    *,
    paused_reason: Optional[str] = None,
    fleet_root: Optional[Path] = None,
) -> None:
    """Atomically flip the mission status under filelock.

    Read-modify-write happens entirely inside the lock so no caller-
    held state object can race with concurrent writers (the drain
    handler appending received_completes, another commander appending
    sent_msg_ids). Use this in place of write_state when you only
    need to change the status field.

    Emits a matching megatron lifecycle event after the write commits:
    ``mission_started`` on first transition into ``running``,
    ``mission_complete`` on terminal status (``complete`` -> done,
    ``cancelled`` -> cancelled). Emit failures are swallowed.

    ``fleet_root`` is forwarded to ``read_state`` so the completion-file
    rebuild uses the correct directory (important in tests and when the
    fleet lives outside ``~/.fno/fleet``).
    """
    path = Path(path)
    lock = FileLock(str(_lock_path_for(path)), timeout=_LOCK_TIMEOUT_SECONDS)
    try:
        with lock:
            state = read_state(path, fleet_root=fleet_root)
            _check_transition(state.status, new_status)
            prev_status = state.status
            state.status = new_status
            if paused_reason is not None:
                state.paused_reason = paused_reason
            _atomic_write(path, _serialize(state))

            # Mission-scope artifact (forensic, not gating). Inside the
            # lock so readers cannot observe terminal state.md without
            # the corresponding artifact. Failure is logged + swallowed
            # inside write_mission_artifact; never raised here. Defense-
            # in-depth: a try/except wraps the call too, in case the
            # writer is monkeypatched to raise.
            if new_status in TERMINAL_STATUSES:
                try:
                    # Local import avoids circular dependency: artifact.py
                    # imports MissionState from this module.
                    from fno.megatron.artifact import write_mission_artifact

                    write_mission_artifact(state, path.parent)
                except Exception:
                    # State-flip is the source of truth; artifact failure
                    # must not propagate.
                    pass

                # Aggregated completion artifact: walks filesystem completion
                # files + ledger.json for a richer, per-project artifact.
                # Best-effort: failures emit completion_artifact_write_failed
                # event but never block the already-committed state flip.
                try:
                    from fno.megatron.artifact import write_mission_complete

                    manifest_path = path.parent / "00-INDEX.md"
                    # Default ledger to ~/.fno/ledger.json so the
                    # aggregator can populate per-project cost rollups.
                    # Without this default the aggregator was always
                    # getting ledger_path=None and rendering every cost
                    # as "unknown" (Codex review on PR #254). Caller can
                    # still pass a non-default ledger via direct invocation.
                    default_ledger = (
                        Path(os.path.expanduser("~"))
                        / ".fno"
                        / "ledger.json"
                    )
                    ledger_path = default_ledger if default_ledger.exists() else None
                    write_mission_complete(
                        manifest_path=manifest_path,
                        state_path=path,
                        fleet_dir=path.parent,
                        ledger_path=ledger_path,
                    )
                except Exception as _wmc_exc:
                    try:
                        # Anchor via resolve_repo_root (BUG-MT-008).
                        from fno.megatron._telemetry import resolve_events_path

                        _ep = resolve_events_path()
                        if _ep is not None:
                            import datetime as _dt_mod

                            from fno import events as _events

                            _events.append_event(
                                {
                                    "type": "completion_artifact_write_failed",
                                    "ts": _dt_mod.datetime.now(
                                        _dt_mod.timezone.utc
                                    ).strftime("%Y-%m-%dT%H:%M:%SZ"),
                                    "source": "megatron",
                                    "data": {
                                        "mission_id": state.mission_id,
                                        "reason": str(_wmc_exc),
                                    },
                                },
                                events_path=_ep,
                            )
                    except Exception:
                        pass
    except Timeout as exc:
        raise MissionStateError(
            f"update_status: filelock timed out on {_lock_path_for(path)}"
        ) from exc

    try:
        _emit_status_event(state.mission_id, prev_status, new_status)
    except Exception:
        # Defense-in-depth: even if the emit helper itself raises (e.g.
        # someone monkeypatches a broken implementation), the state
        # write must still be considered complete.
        pass


def resolve_mission_directory(mission_id: str, fleet_root: Optional[Path] = None) -> Optional[Path]:
    """Walk fleet root looking for a state.md whose frontmatter mission_id matches.

    Returns the slug-named directory or None. Extracted from cli.py /
    drain.py duplicates per the Gemini MEDIUM finding on PR #216.
    """
    if fleet_root is None:
        from fno import paths as _paths
        fleet_root = _paths.fleet_dir()
    if not fleet_root.exists():
        return None
    for slug_dir in sorted(fleet_root.iterdir()):
        if not slug_dir.is_dir():
            continue
        state_path = slug_dir / "state.md"
        if not state_path.exists():
            continue
        try:
            text = state_path.read_text(encoding="utf-8")
        except OSError:
            continue
        in_fm = False
        for line in text.splitlines():
            stripped = line.strip()
            if not in_fm:
                if stripped == "---":
                    in_fm = True
                continue
            if stripped == "---":
                break
            if stripped.startswith("mission_id:"):
                if stripped.split(":", 1)[1].strip() == mission_id:
                    return slug_dir
                break
    return None
