"""AC-A10-UI: the operator surface is explicit and inert."""

from __future__ import annotations

import datetime as _dt
import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.approvals import (
    AdapterCapability,
    ApprovalRequest,
    DecisionKind,
    EffectState,
    EffectStore,
    action_digest,
)
from fno.approvals.cli import approvals_app
from fno.approvals.policy import ConfigAuthority

runner = CliRunner()

FOUNDER = "principal:founder"
INTERN = "principal:intern"
NOW = _dt.datetime(2026, 8, 2, 12, 0, tzinfo=_dt.timezone.utc)


def _request(**overrides: object) -> ApprovalRequest:
    fields: dict[str, object] = {
        "request_id": "req-1",
        "principal_id": FOUNDER,
        "work_order_id": "x-1111",
        "attempt_id": "attempt-1",
        "effect_id": "effect-1",
        "effect_class": "communication.external",
        "destination": "email:customer@example.com",
        "action_digest": action_digest({"body": "original"}),
        "created_at": NOW,
        "expires_at": NOW + _dt.timedelta(days=3650),
    }
    fields.update(overrides)
    return ApprovalRequest(**fields)  # type: ignore[arg-type]


@pytest.fixture
def policy(monkeypatch: pytest.MonkeyPatch) -> ConfigAuthority:
    authority = ConfigAuthority({"*": [FOUNDER]})
    monkeypatch.setattr("fno.approvals.cli.load_authority", lambda: authority)
    return authority


@pytest.fixture
def db(tmp_path: Path, policy: ConfigAuthority) -> Path:
    path = tmp_path / "approvals.db"
    with EffectStore(path, authority=policy, events_path=tmp_path / "events.jsonl") as store:
        store.submit(_request())
    return path


def _invoke(*args: str):
    return runner.invoke(approvals_app, list(args))


def test_ac10_ui_show_names_every_bound_field(db: Path) -> None:
    digest = _request().request_digest
    result = _invoke("show", digest, "--db", str(db))

    assert result.exit_code == 0, result.output
    for expected in (
        FOUNDER,
        "x-1111",
        "attempt-1",
        "effect-1",
        "communication.external",
        "email:customer@example.com",
        _request().action_digest,
        "pending",
    ):
        assert expected in result.output


def test_ac10_ui_show_json_carries_every_bound_field(db: Path) -> None:
    digest = _request().request_digest
    result = _invoke("show", digest, "--json", "--db", str(db))

    assert result.exit_code == 0, result.output
    record = json.loads(result.output)
    assert record["principal_id"] == FOUNDER
    assert record["work_order_id"] == "x-1111"
    assert record["work_order_attempt"] == "attempt-1"
    assert record["effect_class"] == "communication.external"
    assert record["destination"] == "email:customer@example.com"
    assert record["action_digest"] == _request().action_digest
    assert record["decision"] is None
    assert record["effect_attempts"] == []


def test_ac10_ui_inspection_never_executes_the_effect(db: Path, tmp_path: Path) -> None:
    digest = _request().request_digest
    _invoke("ls", "--db", str(db))
    _invoke("show", digest, "--db", str(db))

    events = (tmp_path / "events.jsonl").read_text().splitlines()
    types = [json.loads(line)["type"] for line in events if line.strip()]
    assert types == ["approval_requested"]

    with EffectStore(db, authority=ConfigAuthority()) as store:
        assert store.attempts_for_request(digest) == []


def test_ac10_ui_decision_records_without_claiming_acknowledgment(db: Path) -> None:
    digest = _request().request_digest
    result = _invoke("decide", digest, "--as", FOUNDER, "--approve", "--db", str(db))

    assert result.exit_code == 0, result.output
    assert "approved" in result.output
    assert "not executed" in result.output
    assert "not acknowledged" in result.output

    with EffectStore(db, authority=ConfigAuthority()) as store:
        decision = store.get_decision(digest)
        assert decision is not None and decision.decision is DecisionKind.APPROVED
        assert store.attempts_for_request(digest) == []


def test_ac10_ui_decision_json_is_explicit_about_what_did_not_happen(db: Path) -> None:
    digest = _request().request_digest
    result = _invoke("decide", digest, "--as", FOUNDER, "--approve", "--json", "--db", str(db))

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["result"] == "approved"
    assert payload["executed"] is False
    assert payload["acknowledged"] is False
    assert payload["authority_source"] == "config.approvals.authorized_principals"


def test_ac10_ui_unauthorized_principal_gets_a_stable_refusal(db: Path) -> None:
    digest = _request().request_digest
    result = _invoke("decide", digest, "--as", INTERN, "--approve", "--db", str(db))

    assert result.exit_code == 2
    assert "REFUSED [unauthorized_principal]" in result.output
    assert "config.approvals.authorized_principals" in result.output

    with EffectStore(db, authority=ConfigAuthority()) as store:
        assert store.get_decision(digest) is None


def test_ac10_ui_refusal_json_is_machine_readable(db: Path) -> None:
    digest = _request().request_digest
    result = _invoke("decide", digest, "--as", INTERN, "--approve", "--json", "--db", str(db))

    assert result.exit_code == 2
    payload = json.loads(result.output)
    assert payload["result"] == "refused"
    assert payload["reason"] == "unauthorized_principal"
    assert payload["authority_source"] == "config.approvals.authorized_principals"


def test_ac10_ui_replayed_decision_is_refused(db: Path) -> None:
    digest = _request().request_digest
    assert _invoke("decide", digest, "--as", FOUNDER, "--decline", "--db", str(db)).exit_code == 0

    result = _invoke("decide", digest, "--as", FOUNDER, "--approve", "--db", str(db))
    assert result.exit_code == 2
    assert "REFUSED [replay]" in result.output


def test_ac10_ui_unconfigured_policy_refuses_every_decision(
    db: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("fno.approvals.cli.load_authority", lambda: ConfigAuthority())
    result = _invoke("decide", _request().request_digest, "--as", FOUNDER, "--approve", "--db", str(db))

    assert result.exit_code == 2
    assert "no approval policy is configured" in result.output


def test_ac10_ui_decide_requires_exactly_one_verb(db: Path) -> None:
    digest = _request().request_digest
    for args in (("--approve", "--decline"), ()):
        result = _invoke("decide", digest, "--as", FOUNDER, *args, "--db", str(db))
        assert result.exit_code == 2
        assert "exactly one of --approve or --decline" in result.output


def test_ac10_ui_ls_pending_hides_decided_requests(db: Path) -> None:
    digest = _request().request_digest
    assert "pending" in _invoke("ls", "--pending", "--db", str(db)).output

    _invoke("decide", digest, "--as", FOUNDER, "--approve", "--db", str(db))
    result = _invoke("ls", "--pending", "--db", str(db))
    assert "no pending approval requests" in result.output
    assert "approved" in _invoke("ls", "--db", str(db)).output


def test_ac10_ui_show_surfaces_an_ambiguous_outcome_and_its_recovery(
    db: Path, policy: ConfigAuthority
) -> None:
    digest = _request().request_digest
    with EffectStore(db, authority=policy) as store:
        store.decide(
            request_digest=digest,
            deciding_principal_id=FOUNDER,
            decision=DecisionKind.APPROVED,
        )
        store.prepare(
            request_digest=digest,
            idempotency_key="key-1",
            adapter=AdapterCapability(adapter_id="smtp", adapter_version="1"),
        )
        store.settle(idempotency_key="key-1", state=EffectState.UNKNOWN)

    result = _invoke("show", digest, "--db", str(db))
    assert result.exit_code == 0, result.output
    assert "key-1: unknown" in result.output
    assert "ambiguous" in result.output
    assert "reconcile" in result.output.lower()


def test_ac10_ui_show_never_reports_an_effect_as_delivered(db: Path, policy: ConfigAuthority) -> None:
    from fno.approvals import AdapterCapability

    digest = _request().request_digest
    with EffectStore(db, authority=policy) as store:
        store.decide(
            request_digest=digest,
            deciding_principal_id=FOUNDER,
            decision=DecisionKind.APPROVED,
        )
        store.prepare(
            request_digest=digest,
            idempotency_key="key-1",
            adapter=AdapterCapability(adapter_id="smtp", adapter_version="1"),
        )
        store.settle(
            idempotency_key="key-1", state=EffectState.ACKNOWLEDGED, external_ref="msg-9"
        )

    result = _invoke("show", digest, "--db", str(db))
    assert "key-1: acknowledged" in result.output
    assert "delivered" not in result.output.lower()


def test_config_authority_fails_closed_when_unconfigured() -> None:
    authority = ConfigAuthority()
    assert authority.is_configured is False
    assert (
        authority.may_approve(
            principal_id=FOUNDER, effect_class="communication.external", destination="d"
        )
        is False
    )


def test_config_authority_scopes_a_principal_to_its_effect_class() -> None:
    authority = ConfigAuthority({"communication.external": [INTERN]})
    assert authority.may_approve(
        principal_id=INTERN, effect_class="communication.external", destination="d"
    )
    assert not authority.may_approve(
        principal_id=INTERN, effect_class="publication.public", destination="d"
    )
