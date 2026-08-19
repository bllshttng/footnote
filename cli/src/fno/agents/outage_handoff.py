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
from typing import Any, Callable, Literal

from fno.state.io import atomic_write


ResultPhase = Literal["committed", "parked", "refused"]


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
    destination_model: str
    destination_account: str
    source_provider: str = ""
    source_account: str = ""
    evidence_fingerprints: tuple[str, ...] = ()
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
class EvidenceProof:
    verified: bool
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
    read_dispatch_holder: Callable[[str], str | None]
    refresh_dispatch: Callable[[str, str], bool]
    release_dispatch: Callable[[str, str], None]
    read_snapshot: Callable[[HandoffRequest], HandoffSnapshot]
    revalidate_evidence: Callable[[HandoffRequest, HandoffSnapshot], EvidenceProof]
    stop_source: Callable[[SourceRow], StopProof]
    prepare_handoff: Callable[[HandoffSnapshot, str], ArchiveReceipt]
    spawn_successor: Callable[[HandoffSnapshot, HandoffRequest], SpawnReceipt]
    verify_successor: Callable[[HandoffSnapshot, SpawnReceipt], SuccessorProof]
    stop_partial_successor: Callable[[SpawnReceipt], bool]


@dataclass(frozen=True)
class HandoffResult:
    node: str
    outage_epoch: str
    attempt: str
    phase: ResultPhase
    failed_phase: str | None = None
    reason: str = ""
    counts: dict[str, int] = field(default_factory=dict)
    archive_path: str | None = None
    successor_row_id: str | None = None
    replayed: bool = False

    def to_dict(self) -> dict[str, object]:
        return asdict(self)


def registry_entry_executable(
    entry: object,
    *,
    pane_probe: Callable[[dict[str, object]], bool | None] | None = None,
    pid_probe: Callable[[int, int | None], bool | None] | None = None,
) -> bool:
    """Require positive transport evidence; registry status is never enough."""
    mux = getattr(entry, "mux", None)
    if mux is not None:
        if pane_probe is None:
            from fno.agents.mux_spawn import _mux_pane_alive

            pane_probe = _mux_pane_alive
        return pane_probe(mux) is True
    pid = getattr(entry, "pid", None)
    started = getattr(entry, "pid_start_time", None)
    if pid and started is not None:
        if pid_probe is None:
            from fno.agents.spawn_gate import _pid_alive

            pid_probe = _pid_alive
        return pid_probe(pid, started) is True
    return False


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
        "--bounded-placement",
        "--cwd",
        snapshot.owner_cwd,
        "--node",
        request.node,
        f"--recorded-provider={request.destination_provider}",
        "--model",
        request.destination_model,
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
    row_id = str(
        receipt.get("short_id")
        or receipt.get("harness_session_id")
        or receipt.get("session_id")
        or ""
    )
    if not row_id:
        raise RuntimeError("spawn returned no successor row id")
    return SpawnReceipt(row_id=row_id, name=str(receipt["name"]))


def _revalidate_persisted_and_raw_evidence(
    request: HandoffRequest,
    snapshot: HandoffSnapshot,
    *,
    path: Path,
    transcript_path_for: Callable[[Any], Path | None] | None = None,
    now_s: float | None = None,
) -> EvidenceProof:
    """Re-read the breaker journal and the source transcript under the lease."""
    from fno.agents.provider_outage import EvidenceIdentity, collect_transcript_evidence

    expected = tuple(sorted(set(request.evidence_fingerprints)))
    if not request.source_provider or not request.source_account or not expected:
        return EvidenceProof(False, 0, "source route or breaker fingerprints are unknown")
    try:
        state = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, TypeError, ValueError):
        return EvidenceProof(False, 0, "persisted breaker evidence is unknown")
    breakers = state.get("breakers") if isinstance(state, dict) else None
    matches = [
        item for item in (breakers or [])
        if isinstance(item, dict)
        and str(item.get("outage_epoch")) == request.outage_epoch
        and item.get("provider") == request.source_provider
        and item.get("account") == request.source_account
    ]
    if len(matches) != 1:
        return EvidenceProof(False, 0, "breaker route or quorum epoch drifted")
    persisted_fingerprints = tuple(sorted(set(matches[0].get("fingerprints") or [])))
    if persisted_fingerprints != expected:
        return EvidenceProof(False, len(persisted_fingerprints), "breaker fingerprint drift")
    evidence = [
        item for item in (state.get("evidence") or [])
        if isinstance(item, dict) and item.get("row_id") == request.source_row_id
    ]
    source_persisted = {
        str(item.get("content_fingerprint") or item.get("fingerprint") or "")
        for item in evidence
        if item.get("provider") == request.source_provider
        and item.get("account") == request.source_account
    }
    expected_source = source_persisted.intersection(expected)
    if not expected_source:
        return EvidenceProof(False, 0, "persisted source evidence is unknown")
    session_id = snapshot.source.harness_session_id
    if not session_id:
        return EvidenceProof(False, len(expected_source), "source transcript identity is unknown")
    records, refusals = collect_transcript_evidence(
        [EvidenceIdentity(
            row_id=request.source_row_id,
            harness=snapshot.source.harness,
            provider=request.source_provider,
            account=request.source_account,
            session_id=session_id,
            cwd=snapshot.source.cwd,
        )],
        now_s=time.time() if now_s is None else now_s,
        transcript_path_for=transcript_path_for,
    )
    raw_source = {record.fingerprint for record in records}.intersection(expected)
    if refusals or raw_source != expected_source:
        return EvidenceProof(False, len(raw_source), "raw source evidence drifted or is unknown")
    return EvidenceProof(True, len(raw_source), "exact persisted and raw evidence match")


def production_handoff_dependencies(
    *,
    outage_evidence_path: Path | None = None,
    transcript_path_for: Callable[[Any], Path | None] | None = None,
    now_s: Callable[[], float] = time.time,
) -> HandoffDependencies:
    """Build the real claim, registry, manifest, stop, and spawn transaction."""
    from fno import paths
    from fno.agents.registry import load_registry
    from fno.claims.core import (
        ClaimHeldByOther,
        acquire_claim,
        claim_status,
        refresh_claim,
        release_claim,
    )
    from fno.claims.io import claims_root_for
    from fno.graph.store import read_graph
    from fno.agents.provider_outage import journal_path as provider_outage_journal_path
    from fno.state.outage_handoff import (
        ManifestAuthority,
        inspect_target_manifest,
        prepare_target_handoff,
    )

    def acquire(key: str, holder: str) -> bool:
        try:
            acquire_claim(
                key, holder=holder, ttl_ms=60_000,
                root=claims_root_for(key), reason="provider outage handoff",
            )
            return True
        except ClaimHeldByOther:
            return False

    def dispatch_holder(key: str) -> str | None:
        status = claim_status(key, root=claims_root_for(key))
        holder = status.get("holder")
        return str(holder) if holder else None

    def refresh(key: str, holder: str) -> bool:
        try:
            refresh_claim(key, holder=holder, root=claims_root_for(key))
            return True
        except Exception:
            return False

    def release(key: str, holder: str) -> None:
        release_claim(key, holder=holder, root=claims_root_for(key), strict=True)

    def read_snapshot(request: HandoffRequest) -> HandoffSnapshot:
        matches = [
            entry for entry in load_registry()
            if str(entry.harness_session_id or entry.short_id) == request.source_row_id
        ]
        if len(matches) != 1:
            raise ValueError(
                f"source registry identity resolved {len(matches)} rows"
            )
        entry = matches[0]
        inspection = inspect_target_manifest(Path(entry.cwd))
        authority = inspection.authority
        claim = claim_status(
            f"node:{request.node}", root=claims_root_for(f"node:{request.node}")
        )
        if claim.get("state") not in {"live", "suspect"}:
            raise ValueError("source node claim is not live")
        if claim.get("holder") != authority.claim_holder:
            raise ValueError("source node claim holder differs from manifest")
        graph = read_graph(paths.graph_json())
        node = next((item for item in graph if item.get("id") == request.node), None)
        if node is None:
            raise ValueError("source node is absent from the graph")
        return HandoffSnapshot(
            node=authority.node,
            outage_epoch=request.outage_epoch,
            source=SourceRow(
                row_id=request.source_row_id,
                name=entry.name,
                harness=entry.harness,
                cwd=entry.cwd,
                harness_session_id=entry.harness_session_id,
                pid=entry.pid,
                pid_start_time=entry.pid_start_time,
                mux=entry.mux,
            ),
            claim_holder=authority.claim_holder,
            node_status=str(node.get("status") or ""),
            plan_path=authority.plan_path,
            owner_cwd=authority.owner_cwd,
            branch=authority.branch,
            head=authority.head,
            worktree_id=authority.worktree_id,
            manifest_hash=inspection.content_hash,
        )

    def prepare(snapshot: HandoffSnapshot, attempt: str) -> ArchiveReceipt:
        authority = ManifestAuthority(
            node=snapshot.node,
            claim_holder=snapshot.claim_holder,
            owner_cwd=snapshot.owner_cwd,
            plan_path=snapshot.plan_path,
            branch=snapshot.branch,
            head=snapshot.head,
            worktree_id=snapshot.worktree_id,
            harness_session_id=snapshot.source.harness_session_id or "",
        )
        receipt = prepare_target_handoff(Path(snapshot.owner_cwd), attempt, authority)
        return ArchiveReceipt(receipt.path, receipt.content_hash)

    def revalidate(request: HandoffRequest, snapshot: HandoffSnapshot) -> EvidenceProof:
        return _revalidate_persisted_and_raw_evidence(
            request,
            snapshot,
            path=outage_evidence_path or provider_outage_journal_path(),
            transcript_path_for=transcript_path_for,
            now_s=now_s(),
        )

    def verify(snapshot: HandoffSnapshot, spawned: SpawnReceipt) -> SuccessorProof:
        entries = load_registry()
        successors = [
            entry for entry in entries
            if str(entry.harness_session_id or entry.short_id) == spawned.row_id
        ]
        successor_executable = len(successors) == 1 and registry_entry_executable(successors[0])
        same_cwd = successor_executable and Path(successors[0].cwd).resolve() == Path(snapshot.owner_cwd)
        try:
            inspection = inspect_target_manifest(Path(snapshot.owner_cwd))
            exact_claim = claim_status(
                f"node:{snapshot.node}", root=claims_root_for(f"node:{snapshot.node}")
            ).get("holder") == inspection.authority.claim_holder
            fresh_manifest = (
                inspection.authority.node == snapshot.node
                and inspection.authority.plan_path == snapshot.plan_path
                and inspection.content_hash != snapshot.manifest_hash
            )
            same_branch = inspection.authority.branch == snapshot.branch
        except Exception:
            exact_claim = fresh_manifest = same_branch = False
        executable_same_worktree = [
            entry for entry in entries
            if Path(entry.cwd).resolve() == Path(snapshot.owner_cwd)
            and registry_entry_executable(entry)
        ]
        unique = (
            len(successors) == 1
            and len(executable_same_worktree) == 1
            and successors[0] in executable_same_worktree
        )
        facts = [successor_executable, same_cwd, same_branch, exact_claim, fresh_manifest, unique]
        return SuccessorProof(
            executable=successor_executable,
            same_cwd=bool(same_cwd),
            same_branch=bool(same_branch),
            exact_claim=bool(exact_claim),
            fresh_manifest=bool(fresh_manifest),
            unique=unique,
            evidence_count=sum(bool(value) for value in facts),
        )

    def stop_partial(spawned: SpawnReceipt) -> bool:
        entries = [
            entry for entry in load_registry()
            if str(entry.harness_session_id or entry.short_id) == spawned.row_id
        ]
        if len(entries) != 1:
            return False
        entry = entries[0]
        proof = stop_source_exact(SourceRow(
            row_id=spawned.row_id,
            name=entry.name,
            harness=entry.harness,
            cwd=entry.cwd,
            harness_session_id=entry.harness_session_id,
            pid=entry.pid,
            pid_start_time=entry.pid_start_time,
            mux=entry.mux,
        ))
        return proof.confirmed_dead

    return HandoffDependencies(
        acquire_dispatch=acquire,
        read_dispatch_holder=dispatch_holder,
        refresh_dispatch=refresh,
        release_dispatch=release,
        read_snapshot=read_snapshot,
        revalidate_evidence=revalidate,
        stop_source=stop_source_exact,
        prepare_handoff=prepare,
        spawn_successor=spawn_successor_exact,
        verify_successor=verify,
        stop_partial_successor=stop_partial,
    )


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
            "model": request.destination_model,
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
        live_holder = deps.read_dispatch_holder(dispatch_key) or "unknown"
        return HandoffResult(
            node=request.node,
            outage_epoch=request.outage_epoch,
            attempt=attempt,
            phase="refused",
            failed_phase="observed",
            reason=f"dispatch lease held by {live_holder}; no handoff action taken",
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
        evidence = deps.revalidate_evidence(request, snapshot)
        counts["source_evidence"] = evidence.evidence_count
        if not evidence.verified:
            return HandoffResult(
                node=request.node,
                outage_epoch=request.outage_epoch,
                attempt=attempt,
                phase="refused",
                failed_phase="source_stopped",
                reason=evidence.reason,
                counts=counts,
            )
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
        archive = deps.prepare_handoff(snapshot, attempt)
        if archive.content_hash != snapshot.manifest_hash:
            return _park(path, request, attempt, "prepared",
                         "archive content hash differs from observed manifest", counts,
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
        failure_archive = archive.path if archive else getattr(exc, "archive_path", None)
        return _park(
            path,
            request,
            attempt,
            active_phase,
            f"{type(exc).__name__}: {exc}",
            {**counts, "errors": 1},
            archive_path=failure_archive,
            successor_row_id=spawned.row_id if spawned else None,
        )
    finally:
        deps.release_dispatch(dispatch_key, lease_holder)
