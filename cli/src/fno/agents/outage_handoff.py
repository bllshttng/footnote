"""Claim-safe handoff for a positively proved provider outage."""
from __future__ import annotations

import hashlib
import json
import os
import signal
import subprocess
import time
import uuid
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Callable, Literal

from fno.state.io import atomic_write


TerminalPhase = Literal["committed", "parked"]


@dataclass(frozen=True)
class SourceRow:
    row_id: str
    name: str
    harness: str
    cwd: str
    harness_session_id: str | None = None
    pid: int | None = None
    pid_start_time: int | None = None
    mux: dict[str, object] | None = None


@dataclass(frozen=True)
class HandoffRequest:
    node: str
    outage_epoch: str
    source_row_id: str
    destination_harness: str
    destination_provider: str
    destination_account: str
    destination_account_env: dict[str, str] = field(default_factory=dict, repr=False)
    quorum_evidence_count: int = 0


@dataclass(frozen=True)
class HandoffSnapshot:
    node: str
    outage_epoch: str
    source: SourceRow
    claim_holder: str
    node_status: str
    plan_path: str
    owner_cwd: str
    branch: str
    head: str
    worktree_id: str
    manifest_hash: str


@dataclass(frozen=True)
class StopProof:
    confirmed_dead: bool
    kind: str
    evidence_count: int
    reason: str


@dataclass(frozen=True)
class ArchiveReceipt:
    path: str
    content_hash: str


@dataclass(frozen=True)
class SpawnReceipt:
    row_id: str
    name: str


@dataclass(frozen=True)
class SuccessorProof:
    executable: bool
    same_cwd: bool
    same_branch: bool
    exact_claim: bool
    fresh_manifest: bool
    unique: bool
    evidence_count: int

    @property
    def complete(self) -> bool:
        return all((
            self.executable,
            self.same_cwd,
            self.same_branch,
            self.exact_claim,
            self.fresh_manifest,
            self.unique,
        ))


@dataclass(frozen=True)
class HandoffDependencies:
    acquire_dispatch: Callable[[str, str], bool]
    refresh_dispatch: Callable[[str, str], bool]
    release_dispatch: Callable[[str, str], None]
    read_snapshot: Callable[[HandoffRequest], HandoffSnapshot]
    stop_source: Callable[[SourceRow], StopProof]
    archive_manifest: Callable[[HandoffSnapshot, str], ArchiveReceipt]
    release_node_claim: Callable[[str, str], bool]
    spawn_successor: Callable[[HandoffSnapshot, HandoffRequest], SpawnReceipt]
    verify_successor: Callable[[HandoffSnapshot, SpawnReceipt], SuccessorProof]
    stop_partial_successor: Callable[[SpawnReceipt], bool]


@dataclass(frozen=True)
class HandoffResult:
    node: str
    outage_epoch: str
    attempt: str
    phase: TerminalPhase
    failed_phase: str | None = None
    reason: str = ""
    counts: dict[str, int] = field(default_factory=dict)
    archive_path: str | None = None
    successor_row_id: str | None = None
    replayed: bool = False

    def to_dict(self) -> dict[str, object]:
        return asdict(self)


def stop_source_exact(
    source: SourceRow,
    *,
    runner: Callable[..., object] = subprocess.run,
    pane_probe: Callable[[dict[str, object]], bool | None] | None = None,
    pid_probe: Callable[[int, int | None], bool | None] | None = None,
    signal_process: Callable[[int, int], None] = os.kill,
    sleep: Callable[[float], None] = time.sleep,
    grace_s: float = 5.0,
) -> StopProof:
    """Stop the exact recorded transport and require positive death proof."""
    if source.mux is not None:
        session = source.mux.get("session")
        pane_id = source.mux.get("pane_id")
        if session in (None, "") or pane_id in (None, ""):
            return StopProof(False, "mux-pane", 0, "mux identity is incomplete")
        from fno import _subprocess_util
        from fno.agents.mux_spawn import _mux_pane_alive

        probe = pane_probe or _mux_pane_alive
        try:
            killed = runner(
                [*_subprocess_util.fno_py_cmd(), "mux", "pane", "kill",
                 "--session", str(session), str(pane_id)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
        except Exception as exc:  # noqa: BLE001 - a failed stop is not death proof
            return StopProof(False, "mux-pane", 0, f"pane kill failed: {exc}")
        state = probe(source.mux)
        if state is False:
            return StopProof(True, "mux-pane", 1, "exact mux pane is absent")
        detail = "unknown" if state is None else "still alive"
        return StopProof(
            False,
            "mux-pane",
            0,
            f"exact mux pane death is {detail} (kill exit {getattr(killed, 'returncode', None)})",
        )

    if not source.pid or source.pid_start_time is None:
        return StopProof(False, "process", 0, "pid and process-start token are required")
    if pid_probe is None:
        from fno.agents.spawn_gate import _pid_alive

        pid_probe = _pid_alive
    initial = pid_probe(source.pid, source.pid_start_time)
    evidence = 1 if initial is True else 0
    if initial is not True:
        reason = "unreadable" if initial is None else "already absent without a stop observation"
        return StopProof(False, "process", evidence, f"source process identity is {reason}")

    def send(sig: int) -> tuple[bool, str | None]:
        state = pid_probe(source.pid, source.pid_start_time)
        if state is False:
            return True, None
        if state is not True:
            return False, "process identity changed before signal"
        try:
            signal_process(source.pid, sig)
        except ProcessLookupError:
            return True, None
        except OSError as exc:
            return False, f"signal failed: {exc}"
        return False, None

    gone, error = send(signal.SIGTERM)
    if error:
        return StopProof(False, "process", evidence, error)
    if gone:
        return StopProof(True, "process", evidence + 1, "recorded process is dead")
    deadline = time.monotonic() + grace_s
    state: bool | None = True
    while time.monotonic() < deadline:
        state = pid_probe(source.pid, source.pid_start_time)
        if state is False:
            return StopProof(True, "process", evidence + 1, "recorded process is dead")
        if state is None:
            return StopProof(False, "process", evidence, "process death probe is unreadable")
        sleep(min(0.1, max(0.0, deadline - time.monotonic())))
    gone, error = send(signal.SIGKILL)
    if error:
        return StopProof(False, "process", evidence, error)
    if gone:
        return StopProof(True, "process", evidence + 1, "recorded process is dead")
    state = pid_probe(source.pid, source.pid_start_time)
    if state is False:
        return StopProof(True, "process", evidence + 1, "recorded process is dead")
    reason = "unreadable" if state is None else "still alive"
    return StopProof(False, "process", evidence, f"process death probe is {reason}")


def spawn_successor_exact(
    snapshot: HandoffSnapshot,
    request: HandoffRequest,
    *,
    runner: Callable[..., object] = subprocess.run,
) -> SpawnReceipt:
    """Spawn one pane successor with explicit destination axes."""
    from fno import _subprocess_util

    command = [
        *_subprocess_util.fno_py_cmd(),
        "agents",
        "spawn",
        "--harness",
        request.destination_harness,
        "--substrate",
        "pane",
        "--cwd",
        snapshot.owner_cwd,
        "--node",
        request.node,
        "--provider",
        request.destination_provider,
        "--dispatch-account",
        request.destination_account,
        f"/fno:target --no-merge {request.node}",
    ]
    proc = runner(
        command,
        cwd=snapshot.owner_cwd,
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
        env={**os.environ, "TARGET_NO_MERGE": "1"},
    )
    if getattr(proc, "returncode", 1) != 0:
        detail = str(getattr(proc, "stderr", "") or "spawn failed").strip()
        raise RuntimeError(detail[:400])
    receipt: dict[str, object] | None = None
    for line in reversed(str(getattr(proc, "stdout", "") or "").splitlines()):
        try:
            value = json.loads(line)
        except (TypeError, ValueError):
            continue
        if isinstance(value, dict):
            receipt = value
            break
    if receipt is None or not receipt.get("name"):
        raise RuntimeError("spawn returned no exact registry identity")
    row_id = str(receipt.get("short_id") or receipt.get("harness_session_id") or "")
    if not row_id:
        raise RuntimeError("spawn returned no successor row id")
    return SpawnReceipt(row_id=row_id, name=str(receipt["name"]))


_PHASES = (
    "observed",
    "destination_healthy",
    "source_stopped",
    "prepared",
    "successor_spawned",
    "committed",
    "parked",
)


def _attempt_path(root: Path, node: str, outage_epoch: str) -> Path:
    digest = hashlib.sha256(f"{node}\0{outage_epoch}".encode()).hexdigest()[:24]
    return root / f"outage-handoff-{digest}.json"


def _read_terminal(path: Path) -> HandoffResult | None:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, ValueError, TypeError):
        return None
    if data.get("phase") not in ("committed", "parked"):
        return None
    result = data.get("result")
    if not isinstance(result, dict):
        return None
    try:
        return HandoffResult(**{**result, "replayed": True})
    except (TypeError, ValueError):
        return None


def _journal(path: Path, *, phase: str, request: HandoffRequest, attempt: str,
             result: HandoffResult | None = None, **details: object) -> None:
    if phase not in _PHASES:
        raise ValueError(f"unknown outage handoff phase: {phase}")
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: dict[str, object] = {
        "version": 1,
        "node": request.node,
        "outage_epoch": request.outage_epoch,
        "attempt": attempt,
        "phase": phase,
        "updated_at_ms": int(time.time() * 1000),
        "destination": {
            "harness": request.destination_harness,
            "provider": request.destination_provider,
            "account": request.destination_account,
        },
        **details,
    }
    if result is not None:
        payload["result"] = result.to_dict()
    atomic_write(path, json.dumps(payload, sort_keys=True) + "\n")


def _validate_snapshot(request: HandoffRequest, snapshot: HandoffSnapshot) -> None:
    if snapshot.node != request.node or snapshot.outage_epoch != request.outage_epoch:
        raise ValueError("snapshot does not match the requested outage attempt")
    if snapshot.source.row_id != request.source_row_id:
        raise ValueError("exact source registry row changed")
    if snapshot.node_status in {"done", "superseded"}:
        raise ValueError(f"node is terminal ({snapshot.node_status})")
    if not all((snapshot.claim_holder, snapshot.plan_path, snapshot.owner_cwd,
                snapshot.branch, snapshot.head, snapshot.worktree_id,
                snapshot.manifest_hash)):
        raise ValueError("snapshot is missing immutable handoff authority")
    if Path(snapshot.owner_cwd).resolve() != Path(snapshot.source.cwd).resolve():
        raise ValueError("source row and manifest owner cwd differ")


def _require_same_authority(
    observed: HandoffSnapshot, refreshed: HandoffSnapshot
) -> None:
    if refreshed != observed:
        raise ValueError("handoff authority changed after observation")


def _park(
    path: Path,
    request: HandoffRequest,
    attempt: str,
    failed_phase: str,
    reason: str,
    counts: dict[str, int],
    *,
    archive_path: str | None = None,
    successor_row_id: str | None = None,
) -> HandoffResult:
    result = HandoffResult(
        node=request.node,
        outage_epoch=request.outage_epoch,
        attempt=attempt,
        phase="parked",
        failed_phase=failed_phase,
        reason=reason,
        counts=counts,
        archive_path=archive_path,
        successor_row_id=successor_row_id,
    )
    _journal(path, phase="parked", request=request, attempt=attempt, result=result)
    return result


def run_outage_handoff(
    request: HandoffRequest,
    *,
    deps: HandoffDependencies,
    journal_root: Path,
) -> HandoffResult:
    """Run one durable handoff attempt; terminal attempts are replay-only."""
    path = _attempt_path(Path(journal_root), request.node, request.outage_epoch)
    terminal = _read_terminal(path)
    if terminal is not None:
        return terminal

    attempt = f"{os.getpid()}-{uuid.uuid4().hex}"
    dispatch_key = f"dispatch:{request.node}"
    lease_holder = f"outage-handoff:{attempt}"
    if not deps.acquire_dispatch(dispatch_key, lease_holder):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            terminal = _read_terminal(path)
            if terminal is not None:
                return terminal
            time.sleep(0.01)
        return HandoffResult(
            node=request.node,
            outage_epoch=request.outage_epoch,
            attempt=attempt,
            phase="parked",
            failed_phase="observed",
            reason="dispatch lease contended and no terminal receipt appeared",
            counts={"lease_contention": 1},
        )

    archive: ArchiveReceipt | None = None
    spawned: SpawnReceipt | None = None
    counts: dict[str, int] = {}
    source_dead = False
    active_phase = "observed"
    try:
        terminal = _read_terminal(path)
        if terminal is not None:
            return terminal
        snapshot = deps.read_snapshot(request)
        _validate_snapshot(request, snapshot)
        observed = snapshot
        _journal(path, phase="observed", request=request, attempt=attempt,
                 source_row_id=snapshot.source.row_id)
        _journal(path, phase="destination_healthy", request=request, attempt=attempt,
                 destination_evidence=1)

        active_phase = "source_stopped"
        if not deps.refresh_dispatch(dispatch_key, lease_holder):
            return _park(path, request, attempt, "source_stopped",
                         "dispatch lease changed before source stop", counts)
        snapshot = deps.read_snapshot(request)
        _validate_snapshot(request, snapshot)
        _require_same_authority(observed, snapshot)
        proof = deps.stop_source(snapshot.source)
        counts["source_stop_evidence"] = proof.evidence_count
        if not proof.confirmed_dead:
            return _park(path, request, attempt, "source_stopped", proof.reason, counts)
        source_dead = True
        _journal(path, phase="source_stopped", request=request, attempt=attempt,
                 stop_kind=proof.kind, source_stop_evidence=proof.evidence_count)

        active_phase = "prepared"
        if not deps.refresh_dispatch(dispatch_key, lease_holder):
            return _park(path, request, attempt, "prepared",
                         "dispatch lease changed before prepare", counts)
        snapshot = deps.read_snapshot(request)
        _validate_snapshot(request, snapshot)
        _require_same_authority(observed, snapshot)
        archive = deps.archive_manifest(snapshot, attempt)
        if archive.content_hash != snapshot.manifest_hash:
            return _park(path, request, attempt, "prepared",
                         "archive content hash differs from observed manifest", counts,
                         archive_path=archive.path)
        if not deps.release_node_claim(f"node:{request.node}", snapshot.claim_holder):
            return _park(path, request, attempt, "prepared",
                         "exact source claim was not released", counts,
                         archive_path=archive.path)
        _journal(path, phase="prepared", request=request, attempt=attempt,
                 archive_path=archive.path, archive_hash=archive.content_hash)

        active_phase = "successor_spawned"
        if not deps.refresh_dispatch(dispatch_key, lease_holder):
            return _park(path, request, attempt, "successor_spawned",
                         "dispatch lease changed before successor spawn", counts,
                         archive_path=archive.path)
        spawned = deps.spawn_successor(snapshot, request)
        _journal(path, phase="successor_spawned", request=request, attempt=attempt,
                 successor_row_id=spawned.row_id)
        active_phase = "committed"
        successor = deps.verify_successor(snapshot, spawned)
        counts["successor_evidence"] = successor.evidence_count
        if not successor.complete:
            deps.stop_partial_successor(spawned)
            return _park(path, request, attempt, "committed",
                         "successor verification incomplete", counts,
                         archive_path=archive.path,
                         successor_row_id=spawned.row_id)

        result = HandoffResult(
            node=request.node,
            outage_epoch=request.outage_epoch,
            attempt=attempt,
            phase="committed",
            counts=counts,
            archive_path=archive.path,
            successor_row_id=spawned.row_id,
        )
        _journal(path, phase="committed", request=request, attempt=attempt, result=result)
        return result
    except Exception as exc:  # noqa: BLE001 - transaction failures become durable parks
        if source_dead and spawned is not None:
            try:
                deps.stop_partial_successor(spawned)
            except Exception:  # noqa: BLE001 - preserve the original failed phase
                pass
        return _park(
            path,
            request,
            attempt,
            active_phase,
            f"{type(exc).__name__}: {exc}",
            {**counts, "errors": 1},
            archive_path=archive.path if archive else None,
            successor_row_id=spawned.row_id if spawned else None,
        )
    finally:
        deps.release_dispatch(dispatch_key, lease_holder)
