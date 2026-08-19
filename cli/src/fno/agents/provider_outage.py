"""Durable provider-outage evidence and quorum-backed breaker folding."""
from __future__ import annotations

import hashlib
import json
import os
import tempfile
import re
from collections import Counter, defaultdict
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Iterable

import filelock


FUP_QUORUM_WINDOW_S = 5 * 60
OVERLOAD_QUORUM_WINDOW_S = 10 * 60
OVERLOAD_PERSISTENCE_S = 2 * 60
PANE_FRESHNESS_S = 2 * 60
_JOURNAL_VERSION = 1
_MAX_EVIDENCE = 512
_MAX_PANE_SNAPSHOT_BYTES = 64 * 1024
_MAX_PANE_SNAPSHOTS = 512
HEALTH_MARKER = "FNO_PROVIDER_" + "HEALTH_OK"
_HEALTH_FRAGMENTS = ("FNO_", "PROVIDER_", "HEALTH_", "OK")


@dataclass(frozen=True)
class CanaryProof:
    """One persisted provider response, separate from route policy."""

    source: str
    content: str
    observed_at: float
    persisted: bool
    assistant_role: bool
    pane_id: str | None = None
    stopped: bool = True
    holds_node_claim: bool = False


@dataclass(frozen=True)
class RouteCandidate:
    """One explicit harness, vendor, account, and account-record destination."""

    record_id: str | None
    harness: str | None
    provider: str | None
    account: str | None
    account_env: dict[str, str] | None
    canary: CanaryProof | None
    model: str | None = None
    route_env: dict[str, str] | None = None
    breaker_open: bool = False
    runtime_exhausted: bool = False
    harness_installed: bool = True
    pane_supported: bool = True
    pane_count: int = 0


@dataclass(frozen=True)
class EvidenceIdentity:
    """Machine-recorded axes required to join one transcript to a route."""

    row_id: str
    harness: str
    provider: str | None
    account: str | None
    session_id: str
    cwd: str


def _message_text(value: Any) -> str:
    if isinstance(value, str):
        return value.strip()
    if not isinstance(value, list):
        return ""
    return " ".join(
        str(part.get("text") or "").strip()
        for part in value
        if isinstance(part, dict) and part.get("type") == "text"
    ).strip()


def _record_epoch(value: Any) -> float | None:
    if isinstance(value, (int, float)):
        return float(value)
    if not isinstance(value, str) or not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def collect_transcript_evidence(
    identities: Iterable[EvidenceIdentity], *, now_s: float,
    transcript_path_for: Callable[[EvidenceIdentity], Path | None] | None = None,
    evidence_freshness_s: float = 600,
) -> tuple[list["OutageEvidence"], list[dict[str, Any]]]:
    """Read raw assistant records without consulting liveness projections."""
    if transcript_path_for is None:
        from fno.provenance.observed import resolve_transcript_path

        def transcript_path_for(identity: EvidenceIdentity) -> Path | None:
            return resolve_transcript_path(
                identity.harness, identity.session_id, identity.cwd
            )

    records: list[OutageEvidence] = []
    refusals: list[dict[str, Any]] = []
    for identity in identities:
        if not all((identity.row_id, identity.harness, identity.provider, identity.account)):
            refusals.append({
                "row_id": identity.row_id,
                "reason": "unknown_route_identity",
                "count": 1,
            })
            continue
        try:
            path = transcript_path_for(identity)
            if path is None:
                lines = []
            else:
                with Path(path).open("rb") as handle:
                    size = handle.seek(0, os.SEEK_END)
                    handle.seek(max(0, size - 256 * 1024))
                    raw_tail = handle.read()
                lines = raw_tail.decode("utf-8").splitlines()
                if size > 256 * 1024 and lines:
                    lines = lines[1:]
        except (OSError, UnicodeDecodeError):
            lines = []
        if not lines:
            refusals.append({
                "row_id": identity.row_id,
                "reason": "transcript_unreadable",
                "count": 1,
            })
            continue
        for line in lines:
            try:
                raw = json.loads(line)
            except (TypeError, ValueError):
                continue
            if not isinstance(raw, dict) or raw.get("type") != "assistant":
                continue
            message = raw.get("message")
            if not isinstance(message, dict) or message.get("role") != "assistant":
                continue
            observed_at = _record_epoch(raw.get("timestamp"))
            if observed_at is None:
                continue
            age = now_s - observed_at
            if age < 0 or age > evidence_freshness_s:
                continue
            content = _message_text(message.get("content"))
            if not content:
                continue
            is_error = raw.get("isApiErrorMessage") is True
            raw_status = raw.get("apiErrorStatus") if is_error else None
            if is_error and not isinstance(raw_status, int):
                continue
            records.append(OutageEvidence(
                source="transcript",
                observed_at=observed_at,
                row_id=identity.row_id,
                harness=identity.harness,
                provider=identity.provider,
                account=identity.account,
                role="assistant",
                raw_status=raw_status,
                raw_kind="api_error" if is_error else "content",
                content=content,
            ))
    return records, refusals


def _pane_status(content: str) -> int | None:
    match = re.search(
        r"API Error[^\r\n]{0,80}?(?:\b(429|529)\b|\((429|529)\))",
        content,
        re.IGNORECASE,
    )
    return int(match.group(1) or match.group(2)) if match else None


def collect_pane_evidence(
    identities: Iterable[EvidenceIdentity], *, mux_by_row: dict[str, dict[str, Any]],
    now_s: float, snapshot_dir: Path,
    pane_read_fn: Callable[[str, Any], str],
) -> tuple[list["OutageEvidence"], list[dict[str, Any]]]:
    """Persist exact mux screens before returning any pane-backed vote."""
    from fno.state.io import atomic_write

    snapshot_dir.mkdir(parents=True, exist_ok=True)
    records: list[OutageEvidence] = []
    refusals: list[dict[str, Any]] = []
    for identity in identities:
        mux = mux_by_row.get(identity.row_id)
        session = mux.get("session") if isinstance(mux, dict) else None
        pane_id = mux.get("pane_id") if isinstance(mux, dict) else None
        if not session or pane_id is None:
            refusals.append({
                "row_id": identity.row_id,
                "reason": "pane_identity_missing",
                "count": 1,
            })
            continue
        try:
            content = pane_read_fn(str(session), pane_id)
        except Exception as exc:  # noqa: BLE001 - an unreadable screen cannot vote
            refusals.append({
                "row_id": identity.row_id,
                "reason": "pane_read_failed",
                "detail": repr(exc)[:400],
                "count": 1,
            })
            continue
        if not isinstance(content, str) or not content.strip():
            refusals.append({
                "row_id": identity.row_id,
                "reason": "pane_read_failed",
                "count": 1,
            })
            continue
        bounded = content.encode("utf-8")[-_MAX_PANE_SNAPSHOT_BYTES:].decode(
            "utf-8", errors="ignore"
        ).strip()
        status = _pane_status(bounded)
        record = OutageEvidence(
            source="pane",
            observed_at=now_s,
            row_id=identity.row_id,
            harness=identity.harness,
            provider=identity.provider,
            account=identity.account,
            role="assistant",
            raw_status=status,
            raw_kind="api_error" if status is not None else "content",
            content=bounded,
            pane_id=str(pane_id),
            persisted=True,
            snapshot_at=now_s,
        )
        snapshot = {
            "observed_at": now_s,
            "mux_session": str(session),
            "pane_id": str(pane_id),
            "row_id": identity.row_id,
            "harness": identity.harness,
            "provider": identity.provider,
            "account": identity.account,
            "fingerprint": record.fingerprint,
            "content": bounded,
        }
        target = snapshot_dir / f"{record.fingerprint}.json"
        try:
            atomic_write(target, json.dumps(snapshot, sort_keys=True))
            persisted = json.loads(target.read_text(encoding="utf-8"))
        except (OSError, ValueError, UnicodeDecodeError):
            refusals.append({
                "row_id": identity.row_id,
                "reason": "pane_snapshot_unpersisted",
                "count": 1,
            })
            continue
        if persisted.get("fingerprint") != record.fingerprint:
            refusals.append({
                "row_id": identity.row_id,
                "reason": "pane_snapshot_unpersisted",
                "count": 1,
            })
            continue
        records.append(record)

    snapshots = sorted(
        snapshot_dir.glob("*.json"), key=lambda item: item.stat().st_mtime,
        reverse=True,
    )
    for stale in snapshots[_MAX_PANE_SNAPSHOTS:]:
        try:
            stale.unlink()
        except OSError:
            pass
    return records, refusals


def _fresh_canary(proof: CanaryProof | None, now_s: float) -> bool:
    if proof is None or not proof.persisted or not proof.stopped:
        return False
    if proof.holds_node_claim or proof.content.strip() != HEALTH_MARKER:
        return False
    age = now_s - proof.observed_at
    if age < 0 or age > PANE_FRESHNESS_S:
        return False
    if proof.source == "transcript":
        return proof.assistant_role
    if proof.source == "pane":
        return bool(proof.pane_id)
    return False


def select_healthy_destination(
    candidates: Iterable[RouteCandidate], *, broken_provider: str,
    now_s: float, pinned_record_id: str | None = None,
) -> RouteCandidate | None:
    """Return the first explicitly identified, positively proved candidate."""
    for candidate in candidates:
        if pinned_record_id is not None and candidate.record_id != pinned_record_id:
            continue
        if not all((candidate.record_id, candidate.harness, candidate.provider, candidate.account)):
            continue
        if candidate.provider == broken_provider:
            continue
        if (
            candidate.breaker_open
            or candidate.runtime_exhausted
            or not candidate.harness_installed
            or not candidate.pane_supported
            or candidate.pane_count >= 4
        ):
            continue
        if _fresh_canary(candidate.canary, now_s):
            return candidate
    return None


def _node_claim_snapshot() -> set[str]:
    from fno.claims.core import list_claims
    from fno.claims.io import global_claims_root

    return {
        str(item.get("key"))
        for item in list_claims(prefix="node:", root=global_claims_root())
        if item.get("key")
    }


def run_health_canary(
    destination: RouteCandidate,
    *,
    canary_cwd: Path,
    node_cwd: Path,
    now_s: float,
    collect_proof: Callable[[Any], CanaryProof | None],
    stop: Callable[[Any], bool],
    spawn: Callable[..., Any] | None = None,
    claim_snapshot: Callable[[], set[str]] = _node_claim_snapshot,
) -> CanaryProof | None:
    """Run, stop, and verify one neutral canary without entering node state."""
    if destination.harness not in {"codex", "opencode", "agy"}:
        return None
    resolved_canary = canary_cwd.resolve()
    resolved_node = node_cwd.resolve()
    if resolved_canary == resolved_node or resolved_canary.is_relative_to(resolved_node):
        return None
    launcher = spawn
    if launcher is None:
        from fno.agents.mux_spawn import dispatch_spawn_bounded_pane

        launcher = dispatch_spawn_bounded_pane
    prompt = (
        "Reply with only the concatenation of these four fragments, with no "
        f"spaces or punctuation: {' | '.join(_HEALTH_FRAGMENTS)}"
    )
    before_claims = claim_snapshot()
    spawned = launcher(
        name=f"provider-health-{os.getpid()}",
        message=prompt,
        provider=destination.harness,
        cwd=resolved_canary,
        account_env=destination.account_env,
        route_env=destination.route_env,
        route_provider_id=destination.provider,
        model_name=destination.model,
        account_record_id=destination.record_id,
        model=destination.model,
    )
    proof: CanaryProof | None = None
    stopped = False
    try:
        proof = collect_proof(spawned)
    finally:
        stopped = stop(spawned)
    new_node_claim = bool(claim_snapshot() - before_claims)
    if proof is None:
        return None
    verified = CanaryProof(
        source=proof.source,
        content=proof.content,
        observed_at=proof.observed_at,
        persisted=proof.persisted,
        assistant_role=proof.assistant_role,
        pane_id=proof.pane_id,
        stopped=stopped,
        holds_node_claim=new_node_claim,
    )
    return verified if _fresh_canary(verified, now_s) else None


@dataclass(frozen=True)
class OutageEvidence:
    """One explicit raw record; provider and account are resolved upstream."""

    source: str
    observed_at: float
    row_id: str
    harness: str
    provider: str | None
    account: str | None
    role: str
    raw_status: int | None
    raw_kind: str
    content: str
    content_fingerprint: str = ""
    pane_id: str | None = None
    persisted: bool = True
    snapshot_at: float | None = None

    def __post_init__(self) -> None:
        if self.content_fingerprint:
            return
        raw = json.dumps(
            [self.source, self.observed_at, self.row_id, self.harness,
             self.provider, self.account, self.role, self.raw_status,
             self.raw_kind, self.content, self.pane_id],
            separators=(",", ":"), ensure_ascii=False,
        )
        object.__setattr__(self, "content_fingerprint", hashlib.sha256(raw.encode()).hexdigest())

    @property
    def fingerprint(self) -> str:
        """Compatibility shorthand for the explicit content fingerprint."""
        return self.content_fingerprint


def empty_report() -> dict[str, Any]:
    return {"instrument": "measured", "breakers": [], "counts": {}, "refusals": []}


def journal_path() -> Path:
    from fno.paths import state_dir

    return state_dir() / "recovery" / "provider-outages.json"


def _refusal(record: OutageEvidence, reason: str) -> dict[str, Any]:
    return {
        "row_id": record.row_id,
        "source": record.source,
        "fingerprint": record.fingerprint,
        "reason": reason,
    }


def _validate(record: OutageEvidence, now_s: float, pane_freshness_s: float) -> str | None:
    if record.source not in {"transcript", "pane"}:
        return "unknown_source"
    if not record.row_id or not record.harness:
        return "unknown_record_identity"
    if not record.provider or not record.account:
        return "unknown_route_identity"
    if record.role != "assistant":
        return "not_assistant_api_record"
    if record.source == "pane":
        if not record.pane_id:
            return "unknown_pane_identity"
        if not record.persisted or record.snapshot_at is None:
            return "pane_not_persisted"
        if record.snapshot_at > now_s or now_s - record.snapshot_at > pane_freshness_s:
            return "pane_snapshot_stale"
    if record.raw_kind == "content" and record.raw_status is None:
        return None
    if record.raw_kind != "api_error" or not isinstance(record.raw_status, int):
        return "not_assistant_api_record"
    return None


def _evidence_from(value: Any) -> OutageEvidence | None:
    if not isinstance(value, dict):
        return None
    try:
        return OutageEvidence(**value)
    except (TypeError, ValueError):
        return None


def _initial_state(prior_state: dict[str, Any] | None) -> dict[str, Any]:
    prior = prior_state if isinstance(prior_state, dict) else {}
    evidence = [e for item in prior.get("evidence", []) if (e := _evidence_from(item))]
    breakers = [dict(item) for item in prior.get("breakers", []) if isinstance(item, dict)]
    snapshots = [dict(item) for item in prior.get("pane_snapshots", []) if isinstance(item, dict)]
    return {
        "version": _JOURNAL_VERSION,
        "evidence": evidence,
        "breakers": breakers,
        "pane_snapshots": snapshots,
    }


def _session_summaries(records: list[OutageEvidence]) -> tuple[dict[str, dict[str, Any]], list[dict[str, Any]]]:
    sessions: dict[str, dict[str, Any]] = {}
    votes: list[dict[str, Any]] = []
    by_row: dict[str, list[OutageEvidence]] = defaultdict(list)
    for record in records:
        by_row[record.row_id].append(record)
    for row_id, row_records in by_row.items():
        consecutive: list[OutageEvidence] = []
        terminal: OutageEvidence | None = None
        for record in sorted(row_records, key=lambda item: item.observed_at):
            if record.raw_kind == "content":
                consecutive = []
                continue
            if record.raw_status == 529:
                consecutive.append(record)
                continue
            consecutive = []
            if record.raw_status == 429 and "fair usage policy" in record.content.lower():
                terminal = record
        if terminal is not None:
            sessions[row_id] = {
                "state": "terminal", "kind": "fair_usage_policy", "consecutive": 1,
                "reset_at": None, "manual_restoration": True,
            }
            votes.append({"record": terminal, "kind": "fair_usage_policy", "at": terminal.observed_at})
            continue
        span = consecutive[-1].observed_at - consecutive[0].observed_at if consecutive else 0
        if len(consecutive) >= 3 and span >= OVERLOAD_PERSISTENCE_S:
            sessions[row_id] = {
                "state": "session_persistent", "kind": "overloaded_529",
                "consecutive": len(consecutive), "reset_at": None,
                "manual_restoration": False,
            }
            votes.append({"record": consecutive[-1], "kind": "overloaded_529", "at": consecutive[-1].observed_at})
        elif consecutive:
            sessions[row_id] = {
                "state": "retrying", "kind": "overloaded_529",
                "consecutive": len(consecutive), "reset_at": None,
                "manual_restoration": False,
            }
    return sessions, votes


def _breaker_key(provider: str, account: str, kind: str) -> str:
    return json.dumps([provider, account, kind], separators=(",", ":"))


def _breakers(votes: list[dict[str, Any]], prior: list[dict[str, Any]], now_s: float) -> list[dict[str, Any]]:
    prior_by_key = {
        _breaker_key(str(item.get("provider")), str(item.get("account")), str(item.get("kind"))): item
        for item in prior if item.get("provider") and item.get("account") and item.get("kind")
    }
    grouped: dict[tuple[str, str, str], list[dict[str, Any]]] = defaultdict(list)
    for vote in votes:
        record = vote["record"]
        grouped[(record.provider, record.account, vote["kind"])].append(vote)
    out: list[dict[str, Any]] = []
    for (provider, account, kind), got in grouped.items():
        newest_by_row: dict[str, dict[str, Any]] = {}
        for vote in got:
            row_id = vote["record"].row_id
            if row_id not in newest_by_row or vote["at"] > newest_by_row[row_id]["at"]:
                newest_by_row[row_id] = vote
        distinct = sorted(newest_by_row.values(), key=lambda item: item["at"])
        window_s = FUP_QUORUM_WINDOW_S if kind == "fair_usage_policy" else OVERLOAD_QUORUM_WINDOW_S
        if len(distinct) < 2 or distinct[-1]["at"] - distinct[0]["at"] > window_s:
            continue
        key = _breaker_key(provider, account, kind)
        previous = prior_by_key.get(key, {})
        epoch = previous.get("outage_epoch")
        if not isinstance(epoch, (int, float)):
            epoch = distinct[0]["at"]
        out.append({
            "provider": provider,
            "account": account,
            "kind": kind,
            "outage_epoch": epoch,
            "opened_at": previous.get("opened_at", now_s),
            "row_ids": sorted(item["record"].row_id for item in distinct),
            "fingerprints": sorted(item["record"].fingerprint for item in distinct),
            "reset_at": None,
            "manual_restoration": kind == "fair_usage_policy",
            "basis": f"positive quorum=2 across {len(distinct)} distinct rows",
        })
    current_keys = {
        _breaker_key(item["provider"], item["account"], item["kind"]) for item in out
    }
    for item in prior:
        key = _breaker_key(str(item.get("provider")), str(item.get("account")), str(item.get("kind")))
        if item.get("manual_restoration") is True and key not in current_keys:
            out.append(dict(item))
    return sorted(out, key=lambda item: (item["provider"], item["account"], item["kind"]))


def fold_provider_outages(
    records: Iterable[OutageEvidence], *, prior_state: dict[str, Any] | None,
    now_s: float, pane_freshness_s: float = PANE_FRESHNESS_S,
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Pure fold over explicit records, prior durable state, and injected time."""
    state = _initial_state(prior_state)
    known = {record.fingerprint: record for record in state["evidence"]}
    refusals: list[dict[str, Any]] = []
    pane_snapshots = {item.get("fingerprint"): item for item in state["pane_snapshots"]}
    for record in records:
        reason = _validate(record, now_s, pane_freshness_s)
        if reason:
            refusals.append(_refusal(record, reason))
            continue
        known.setdefault(record.fingerprint, record)
        if record.source == "pane":
            pane_snapshots[record.fingerprint] = {
                "fingerprint": record.fingerprint,
                "pane_id": record.pane_id,
                "observed_at": record.observed_at,
                "snapshot_at": record.snapshot_at,
                "raw_status": record.raw_status,
                "raw_kind": record.raw_kind,
                "content": record.content,
            }
    accepted = sorted(known.values(), key=lambda item: (item.observed_at, item.fingerprint))[-_MAX_EVIDENCE:]
    sessions, votes = _session_summaries(accepted)
    breakers = _breakers(votes, state["breakers"], now_s)
    counter = Counter(item["state"] for item in sessions.values())
    counts: dict[str, int] = {}
    if accepted:
        counts["accepted"] = len(accepted)
    if refusals:
        counts["refused"] = len(refusals)
    for key in ("terminal", "retrying", "session_persistent"):
        if counter[key]:
            counts[key] = counter[key]
    if breakers:
        counts["open"] = len(breakers)
    report = {
        "instrument": "measured",
        "breakers": breakers,
        "counts": counts,
        "refusals": refusals,
        "sessions": sessions,
    }
    next_state = {
        "version": _JOURNAL_VERSION,
        "fingerprints": sorted(record.fingerprint for record in accepted),
        "evidence": [asdict(record) for record in accepted],
        "breakers": breakers,
        "pane_snapshots": sorted(pane_snapshots.values(), key=lambda item: item["fingerprint"]),
    }
    return report, next_state


def _unknown_journal(path: Path) -> dict[str, Any]:
    return {
        "instrument": "unknown",
        "breakers": [],
        "counts": {"journal_unreadable": 1},
        "refusals": [{"reason": "journal_unreadable", "path": str(path)}],
    }


def _read_journal(path: Path) -> tuple[dict[str, Any], bool]:
    if not path.exists():
        return {}, True
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return {}, False
    return (value, True) if isinstance(value, dict) else ({}, False)


def _write_journal(path: Path, state: dict[str, Any]) -> None:
    fd, temporary = tempfile.mkstemp(dir=str(path.parent), prefix=".provider-outages-", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(state, handle, indent=2, sort_keys=True)
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise


def measure_and_persist(
    records: Iterable[OutageEvidence], *, now_s: float, path: Path | None = None,
    pane_freshness_s: float = PANE_FRESHNESS_S,
) -> dict[str, Any]:
    """Fold and atomically persist deduplication and breaker epochs."""
    target = path or journal_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        with filelock.FileLock(str(target) + ".lock", timeout=5):
            prior, readable = _read_journal(target)
            if not readable:
                return _unknown_journal(target)
            report, state = fold_provider_outages(
                records, prior_state=prior, now_s=now_s,
                pane_freshness_s=pane_freshness_s,
            )
            _write_journal(target, state)
            return report
    except (OSError, filelock.Timeout):
        return _unknown_journal(target)
