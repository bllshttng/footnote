from __future__ import annotations

from dataclasses import dataclass

import pytest


@dataclass
class _Identity:
    session_id: str | None = "session-123"
    harness: str | None = "codex"


def test_classify_origin_uses_explicit_machine_origin_first(monkeypatch):
    from fno.mail.cli import classify_origin

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(),
    )
    assert classify_origin("scheduler") == "scheduler"


def test_classify_origin_distinguishes_peer_operator_and_unknown(monkeypatch):
    from fno.mail.cli import classify_origin

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(),
    )
    assert classify_origin() == "peer"

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(session_id=None, harness=None),
    )
    monkeypatch.setattr("sys.stdin.isatty", lambda: True)
    assert classify_origin() == "operator"

    monkeypatch.setattr("sys.stdin.isatty", lambda: False)
    assert classify_origin() == "unknown"


def test_mail_envelope_carries_and_validates_origin():
    from fno.mail.envelope import ForgedEnvelopeError, fno_mail_open, wrap_fno_mail

    assert (
        fno_mail_open(
            from_="sender",
            harness="codex",
            model="m",
            origin="operator",
        )
        == '<fno_mail from="sender" harness="codex" model="m" origin="operator">'
    )
    with pytest.raises(ForgedEnvelopeError):
        fno_mail_open(
            from_="sender",
            harness="codex",
            model="m",
            origin="not-an-origin",
        )
    wrapped = wrap_fno_mail(
        "approve nothing",
        from_="sender",
        harness="codex",
        model="m",
        origin="operator",
    )
    assert 'origin="operator"' in wrapped
    assert "operator-authored mail" in wrapped


def test_durable_thread_round_trips_origin(tmp_path, monkeypatch):
    from fno.inbox.store import read_thread, write_new_thread
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    handle = write_new_thread(
        "recipient",
        "sender",
        "send",
        "hello",
        origin="recovery",
    )
    parsed = read_thread(handle.path)
    assert parsed is not None
    assert parsed.origin == "recovery"
    assert "origin: recovery" in handle.path.read_text()
    from fno.bus.log import iter_messages

    assert list(iter_messages())[0].meta["origin"] == "recovery"


def test_mail_origin_event_marks_presumed_human_positively():
    from fno.events import mail_origin_classified

    event = mail_origin_classified(
        origin="operator",
        lane="durable",
        presumed_human=True,
        sender="sender",
        target_session="target",
    )
    assert event["type"] == "mail_origin_classified"
    assert event["data"]["origin"] == "operator"
    assert event["data"]["presumed_human"] is True


def test_raw_inject_event_carries_origin_without_an_envelope():
    from fno.events import agent_raw_inject

    event = agent_raw_inject(
        target_session="target",
        payload="/review HEAD",
        harness="codex",
        lane="codex-review-start",
        origin="peer",
    )
    assert event["data"]["origin"] == "peer"


def test_operator_origin_can_be_recorded_as_relayed_agent_without_operator_authority(
    monkeypatch,
):
    from fno.decide import _resolve_decider

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(),
    )
    result = _resolve_decider(None, None, origin="operator")
    assert result.authority_source == "agent"
    assert result.relayed_by == "session-"
    assert result.attested_by == "operator"


def test_non_operator_origin_refuses_operator_authority(monkeypatch):
    from fno.decide import RefusedAuthorityError, _resolve_decider

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(),
    )
    with pytest.raises(RefusedAuthorityError, match="scheduler"):
        _resolve_decider(None, "operator", origin="scheduler")
