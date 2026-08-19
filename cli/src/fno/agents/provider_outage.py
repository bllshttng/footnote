"""Durable provider-outage evidence and quorum-backed breaker folding."""
from __future__ import annotations

import hashlib
import json
import os
import tempfile
from collections import Counter, defaultdict
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Iterable

import filelock


FUP_QUORUM_WINDOW_S = 5 * 60
OVERLOAD_QUORUM_WINDOW_S = 10 * 60
OVERLOAD_PERSISTENCE_S = 2 * 60
PANE_FRESHNESS_S = 2 * 60
_JOURNAL_VERSION = 1
_MAX_EVIDENCE = 512


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
    if record.raw_kind == "content" and record.raw_status is None:
        return None
    if record.raw_kind != "api_error" or not isinstance(record.raw_status, int):
        return "not_assistant_api_record"
    if record.source == "pane":
        if not record.pane_id:
            return "unknown_pane_identity"
        if not record.persisted or record.snapshot_at is None:
            return "pane_not_persisted"
        if record.snapshot_at > now_s or now_s - record.snapshot_at > pane_freshness_s:
            return "pane_snapshot_stale"
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
