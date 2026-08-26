"""Human-approved law proposal storage and the engine consent seam."""

from __future__ import annotations

import hashlib
import hmac
import json
import os
import re
import secrets
import tempfile
import fcntl
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from collections.abc import Iterator
from pathlib import Path
from typing import Any, Iterable

import typer

PROPOSAL_TTL = timedelta(minutes=15)
CONSENT_TTL = timedelta(minutes=2)
RETENTION_TTL = timedelta(days=1)
PROMPTING_MODES = frozenset({"default", "acceptEdits", "plan"})
PROPOSAL_ID_RE = re.compile(r"^lp-[0-9a-f]{12}$")
DECISION_ID_RE = re.compile(r"^d-[0-9a-f]{8}$")
COORDINATION_MARKERS = (
    "this pr",
    "this node",
    "this target",
    "temporary",
    "until merge",
    "for this change",
)


class ProposalError(RuntimeError):
    """Base class for safe proposal failures."""


class ProposalNotFoundError(ProposalError):
    """The requested staged proposal does not exist."""


class ProposalValidationError(ProposalError):
    """The proposal is not a durable law statement."""


class InvalidOperatorConsentError(ProposalError):
    """A consent record is absent, expired, forged, or already consumed."""


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _iso(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat()


def _parse_time(value: str) -> datetime:
    try:
        return datetime.fromisoformat(value).astimezone(timezone.utc)
    except (TypeError, ValueError) as exc:
        raise ProposalError("proposal timestamp is invalid") from exc


def _canonical_fields(
    *,
    subject: str,
    decision: str,
    rationale: str | None,
    options: Iterable[str] | None,
    supersedes: str | None,
) -> dict[str, Any]:
    normalized_subject = subject.strip()
    normalized_decision = decision.strip()
    normalized_rationale = rationale.strip() if rationale else ""
    normalized_options = [item.strip() for item in (options or []) if item.strip()]
    normalized_supersedes = supersedes.strip() if supersedes else None
    if not normalized_subject or not normalized_decision:
        raise ProposalValidationError("subject and decision are required")
    if not normalized_rationale:
        raise ProposalValidationError("rationale is required for durable law")
    lowered = normalized_decision.casefold()
    if any(marker in lowered for marker in COORDINATION_MARKERS):
        raise ProposalValidationError(
            "the statement is coordination, not durable law"
        )
    if normalized_supersedes and not DECISION_ID_RE.fullmatch(normalized_supersedes):
        raise ProposalValidationError("supersedes must be a decision id")
    if normalized_supersedes:
        from fno.decide import list_decisions

        _, decisions, _ = list_decisions(limit=None)
        if normalized_supersedes not in {
            str(row.get("decision_id") or "") for row in decisions
        }:
            raise ProposalValidationError(
                f"supersedes decision {normalized_supersedes} does not exist"
            )
    return {
        "subject": normalized_subject,
        "decision": normalized_decision,
        "rationale": normalized_rationale,
        "options": normalized_options,
        "supersedes": normalized_supersedes,
    }


def _content_hash(fields: dict[str, Any]) -> str:
    canonical = json.dumps(fields, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def _receipt_hash(receipt: str) -> str:
    return hashlib.sha256(receipt.encode("utf-8")).hexdigest()


def proposal_path(proposal_id: str) -> Path:
    if not PROPOSAL_ID_RE.fullmatch(proposal_id):
        raise ProposalValidationError("proposal id is invalid")
    from fno import paths

    return paths.law_proposals_dir() / f"{proposal_id}.json"


@contextmanager
def proposal_lock(proposal_id: str) -> Iterator[None]:
    """Serialize validation, claim, and durable writes for one proposal."""
    lock_path = proposal_path(proposal_id).with_suffix(".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+", encoding="utf-8") as handle:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def write_proposal(proposal: dict[str, Any]) -> None:
    path = proposal_path(str(proposal.get("proposal_id") or ""))
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(proposal, sort_keys=True, indent=2) + "\n"
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, prefix=".proposal-", delete=False
    ) as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
        temporary = Path(handle.name)
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)


def load_proposal(proposal_id: str) -> dict[str, Any]:
    path = proposal_path(proposal_id)
    try:
        proposal = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise ProposalNotFoundError(f"proposal {proposal_id} does not exist") from exc
    except (OSError, json.JSONDecodeError) as exc:
        raise ProposalError(f"proposal {proposal_id} is unreadable") from exc
    if not isinstance(proposal, dict):
        raise ProposalError(f"proposal {proposal_id} is not an object")
    return proposal


def _expire(proposal: dict[str, Any]) -> dict[str, Any]:
    if proposal.get("status") in {"consumed", "expired"}:
        return proposal
    if _utc_now() >= _parse_time(str(proposal.get("expires_at") or "")):
        proposal = dict(proposal)
        proposal["status"] = "expired"
        proposal["expired_at"] = _iso(_utc_now())
        write_proposal(proposal)
    return proposal


def _prune() -> None:
    from fno import paths

    directory = paths.law_proposals_dir()
    if not directory.is_dir():
        return
    cutoff = _utc_now() - RETENTION_TTL
    for path in directory.glob("lp-*.json"):
        try:
            proposal = json.loads(path.read_text(encoding="utf-8"))
            finished = proposal.get("consumed_at") or proposal.get("expired_at")
            if proposal.get("status") in {"consumed", "expired"} and finished:
                if _parse_time(str(finished)) < cutoff:
                    path.unlink()
        except (OSError, json.JSONDecodeError, ProposalError):
            continue


def prepare_proposal(
    *,
    subject: str,
    decision: str,
    rationale: str | None = None,
    options: Iterable[str] | None = None,
    supersedes: str | None = None,
) -> dict[str, Any]:
    _prune()
    fields = _canonical_fields(
        subject=subject,
        decision=decision,
        rationale=rationale,
        options=options,
        supersedes=supersedes,
    )
    from fno.decide import list_decisions

    _, existing, _ = list_decisions(fields["subject"], limit=None, lane="law")
    now = _utc_now()
    proposal = {
        "schema": 1,
        "proposal_id": f"lp-{secrets.token_hex(6)}",
        **fields,
        "content_hash": _content_hash(fields),
        "existing_law_ids": [str(row.get("decision_id")) for row in existing],
        "status": "pending",
        "created_at": _iso(now),
        "expires_at": _iso(now + PROPOSAL_TTL),
    }
    write_proposal(proposal)
    return proposal


def _arm_proposal_from_hook(
    proposal_id: str,
    *,
    content_hash: str,
    session_id: str,
    permission_mode: str,
    tool_input: str,
) -> dict[str, Any]:
    with proposal_lock(proposal_id):
        return _arm_proposal_from_hook_unlocked(
            proposal_id,
            content_hash=content_hash,
            session_id=session_id,
            permission_mode=permission_mode,
            tool_input=tool_input,
        )


def _arm_proposal_from_hook_unlocked(
    proposal_id: str,
    *,
    content_hash: str,
    session_id: str,
    permission_mode: str,
    tool_input: str,
) -> dict[str, Any]:
    _prune()
    proposal = _expire(load_proposal(proposal_id))
    if proposal.get("status") == "expired":
        raise InvalidOperatorConsentError("proposal is expired")
    if proposal.get("status") == "consumed":
        raise InvalidOperatorConsentError("proposal is consumed")
    if proposal.get("status") == "consuming":
        decision_id = proposal.get("consuming_decision_id") or "unknown"
        raise InvalidOperatorConsentError(
            f"proposal is consuming decision {decision_id}; recover it before retrying"
        )
    if proposal.get("status") == "armed":
        consent_expires_at = proposal.get("consent_expires_at")
        if consent_expires_at and _utc_now() >= _parse_time(str(consent_expires_at)):
            reset = dict(proposal)
            reset["status"] = "pending"
            for key in (
                "armed_at",
                "consent_expires_at",
                "armed_session_id",
                "armed_permission_mode",
                "armed_tool_input",
                "approval_receipt_hash",
            ):
                reset.pop(key, None)
            write_proposal(reset)
            proposal = reset
        else:
            raise InvalidOperatorConsentError("proposal is already armed")
    if content_hash != proposal.get("content_hash"):
        raise InvalidOperatorConsentError("content hash mismatch")
    if not session_id:
        raise InvalidOperatorConsentError("session id is required")
    if permission_mode not in PROMPTING_MODES:
        raise InvalidOperatorConsentError("permission mode cannot approve law")
    expected_tool = f"fno inbox law enact --proposal {proposal_id} --hash {content_hash}"
    if tool_input != expected_tool:
        raise InvalidOperatorConsentError("tool input is not the canonical enact command")
    approval_receipt = secrets.token_urlsafe(32)
    armed_tool_input = f"{tool_input} --receipt {approval_receipt}"
    armed = dict(proposal)
    armed.update(
        {
            "status": "armed",
            "armed_at": _iso(_utc_now()),
            "consent_expires_at": _iso(_utc_now() + CONSENT_TTL),
            "armed_session_id": session_id,
            "armed_permission_mode": permission_mode,
            "approval_receipt_hash": _receipt_hash(approval_receipt),
        }
    )
    write_proposal(armed)
    return {
        **armed,
        "approval_receipt": approval_receipt,
        "armed_tool_input": armed_tool_input,
    }


def _validate_operator_consent(
    consent: Any,
    *,
    expected: dict[str, Any],
) -> dict[str, Any]:
    _prune()
    proposal = _expire(load_proposal(str(consent.proposal_id)))
    status = proposal.get("status")
    if status != "armed":
        raise InvalidOperatorConsentError(f"proposal is {status or 'not armed'}")
    if _utc_now() >= _parse_time(str(proposal.get("consent_expires_at") or "")):
        expired = dict(proposal)
        expired["status"] = "expired"
        expired["expired_at"] = _iso(_utc_now())
        write_proposal(expired)
        raise InvalidOperatorConsentError("consent is expired")
    if consent.content_hash != proposal.get("content_hash"):
        raise InvalidOperatorConsentError("content hash mismatch")
    if consent.session_id != proposal.get("armed_session_id"):
        raise InvalidOperatorConsentError("session mismatch")
    if consent.permission_mode != proposal.get("armed_permission_mode"):
        raise InvalidOperatorConsentError("permission mode mismatch")
    expected_tool_prefix = (
        f"fno inbox law enact --proposal {proposal['proposal_id']} "
        f"--hash {proposal['content_hash']} --receipt "
    )
    if not consent.tool_input.startswith(expected_tool_prefix):
        raise InvalidOperatorConsentError("tool input mismatch")
    receipt = consent.tool_input[len(expected_tool_prefix) :]
    receipt_hash = proposal.get("approval_receipt_hash")
    if not receipt or " " in receipt:
        raise InvalidOperatorConsentError("approval receipt is invalid")
    if not isinstance(receipt_hash, str) or not hmac.compare_digest(
        _receipt_hash(receipt), receipt_hash
    ):
        raise InvalidOperatorConsentError("approval receipt is invalid")
    fields = {key: proposal.get(key) for key in ("subject", "decision", "rationale", "options", "supersedes")}
    if fields != expected or _content_hash(fields) != consent.content_hash:
        raise InvalidOperatorConsentError("proposal content mismatch")
    return proposal


def validate_operator_consent(
    consent: Any,
    *,
    expected: dict[str, Any],
) -> dict[str, Any]:
    """Validate consent without consuming it or changing proposal state."""
    return _validate_operator_consent(consent, expected=expected)


def claim_operator_consent(
    consent: Any,
    *,
    expected: dict[str, Any],
    decision_id: str,
) -> dict[str, Any]:
    proposal = _validate_operator_consent(consent, expected=expected)
    claimed = dict(proposal)
    claimed["status"] = "consuming"
    claimed["consuming_decision_id"] = decision_id
    claimed["consuming_at"] = _iso(_utc_now())
    write_proposal(claimed)
    return claimed


def release_operator_consent(consent: Any, *, decision_id: str) -> dict[str, Any]:
    proposal = load_proposal(str(consent.proposal_id))
    if (
        proposal.get("status") != "consuming"
        or proposal.get("consuming_decision_id") != decision_id
    ):
        raise InvalidOperatorConsentError("consent claim is not current")
    released = dict(proposal)
    released["status"] = "armed"
    released.pop("consuming_decision_id", None)
    released.pop("consuming_at", None)
    write_proposal(released)
    return released


def consume_operator_consent(
    consent: Any,
    *,
    expected: dict[str, Any],
    decision_id: str | None = None,
) -> dict[str, Any]:
    if decision_id is None:
        proposal = _validate_operator_consent(consent, expected=expected)
    else:
        proposal = load_proposal(str(consent.proposal_id))
        if (
            proposal.get("status") != "consuming"
            or proposal.get("consuming_decision_id") != decision_id
        ):
            raise InvalidOperatorConsentError("consent claim is not current")
    consumed = dict(proposal)
    consumed["status"] = "consumed"
    consumed["consumed_at"] = _iso(_utc_now())
    consumed.pop("consuming_decision_id", None)
    consumed.pop("consuming_at", None)
    write_proposal(consumed)
    return consumed


def enact_proposal(
    proposal_id: str, content_hash: str, receipt: str | None = None
) -> dict[str, Any]:
    proposal = load_proposal(proposal_id)
    status = proposal.get("status")
    if status != "armed":
        if status == "consuming":
            decision_id = proposal.get("consuming_decision_id") or "unknown"
            raise InvalidOperatorConsentError(
                f"proposal is consuming decision {decision_id}; recover it before retrying"
            )
        raise InvalidOperatorConsentError(f"proposal is {status or 'not armed'}")
    consent = _consent_from_proposal(proposal, content_hash, receipt)
    from fno.decide import record_decision

    result = record_decision(
        subject=proposal["subject"],
        decision=proposal["decision"],
        rationale=proposal["rationale"],
        options=proposal["options"],
        supersedes=proposal["supersedes"],
        # The permission click proves a chat approval, never a human origin
        # (ruling d-15ddfab0: origin_provenance not_exposed), so the row
        # never claims the operator lane.
        authority_source="chat_attested",
        consent=consent,
    )
    return {"proposal_id": proposal_id, "status": "consumed", **result}


def _consent_from_proposal(
    proposal: dict[str, Any], content_hash: str, receipt: str | None
) -> Any:
    if content_hash != proposal.get("content_hash"):
        raise InvalidOperatorConsentError("content hash mismatch")
    if not receipt:
        raise InvalidOperatorConsentError("approval receipt is missing")
    receipt_hash = proposal.get("approval_receipt_hash")
    if not isinstance(receipt_hash, str) or not hmac.compare_digest(
        _receipt_hash(receipt), receipt_hash
    ):
        raise InvalidOperatorConsentError("approval receipt is invalid")
    from fno.decide import OperatorConsent

    tool_input = (
        f"fno inbox law enact --proposal {proposal['proposal_id']} "
        f"--hash {content_hash} --receipt {receipt}"
    )

    return OperatorConsent(
        proposal_id=str(proposal["proposal_id"]),
        content_hash=content_hash,
        session_id=str(proposal.get("armed_session_id") or ""),
        permission_mode=str(proposal.get("armed_permission_mode") or ""),
        tool_input=tool_input,
    )


def _json(value: Any) -> None:
    typer.echo(json.dumps(value, sort_keys=True, separators=(",", ":")))


law_app = typer.Typer(help="Compose and enact human-approved project law.")


@law_app.command("prepare")
def prepare_command(
    subject: str = typer.Option(..., "--subject"),
    decision: str = typer.Option(..., "--decision"),
    rationale: str = typer.Option(..., "--rationale"),
    option: list[str] = typer.Option([], "--option"),
    supersedes: str | None = typer.Option(None, "--supersedes"),
) -> None:
    _json(
        prepare_proposal(
            subject=subject,
            decision=decision,
            rationale=rationale,
            options=option,
            supersedes=supersedes,
        )
    )


@law_app.command("enact")
def enact_command(
    proposal: str = typer.Option(..., "--proposal"),
    content_hash: str = typer.Option(..., "--hash"),
    receipt: str = typer.Option(..., "--receipt"),
) -> None:
    from fno.decide import IndexWriteError

    try:
        result = enact_proposal(proposal, content_hash, receipt)
    except ProposalError as exc:
        typer.echo(f"fno law: refused: {exc}", err=True)
        raise typer.Exit(3) from exc
    except IndexWriteError as exc:
        typer.echo(
            f"fno law: recorded {exc.decision_id} to the project journal, but "
            f"the decision index write failed: {exc}. Run `fno backlog "
            "decide-reindex`; do not retry enact.",
            err=True,
        )
        raise typer.Exit(4) from exc
    _json({"decision_id": result["decision_id"], "proposal_id": proposal})


@law_app.command("resume")
def resume_command(proposal: str = typer.Argument(...)) -> None:
    current = _expire(load_proposal(proposal))
    if current.get("status") in {"expired", "consumed"}:
        raise typer.BadParameter(f"proposal is {current['status']}")
    _json(current)


@law_app.command("inspect")
def inspect_command() -> None:
    from fno import paths

    _prune()
    rows = []
    for path in sorted(paths.law_proposals_dir().glob("lp-*.json")):
        try:
            rows.append(_expire(json.loads(path.read_text(encoding="utf-8"))))
        except (OSError, json.JSONDecodeError, ProposalError):
            continue
    _json(rows)
