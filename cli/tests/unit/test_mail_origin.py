from __future__ import annotations

from dataclasses import dataclass
from types import SimpleNamespace

import pytest


@dataclass
class _Identity:
    session_id: str | None = "session-123"
    harness: str | None = "codex"


def test_classify_origin_downgrades_agent_declared_authority(monkeypatch, capsys):
    from fno.mail.cli import classify_origin

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(),
    )
    # An ambient agent identity cannot claim an origin above peer, whatever
    # the flag says; the downgrade is visible, not silent.
    assert classify_origin("operator") == "peer"
    assert classify_origin("scheduler") == "peer"
    assert classify_origin("peer") == "peer"
    assert "downgraded to 'peer'" in capsys.readouterr().err


def test_classify_origin_honors_explicit_origin_without_agent_identity(monkeypatch):
    from fno.mail.cli import classify_origin

    # A real scheduler or recovery sweep has no session identity; its honest
    # declaration still stands.
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(session_id=None, harness=None),
    )
    assert classify_origin("scheduler") == "scheduler"
    assert classify_origin("operator") == "operator"


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


def test_mail_envelope_carries_and_validates_origin(monkeypatch):
    from fno.mail.envelope import ForgedEnvelopeError, fno_mail_open, wrap_fno_mail

    assert (
        fno_mail_open(
            from_="sender",
            origin="operator",
        )
        == '<fno_mail from="sender" origin="operator">'
    )
    with pytest.raises(ForgedEnvelopeError):
        fno_mail_open(
            from_="sender",
            origin="not-an-origin",
        )
    wrapped = wrap_fno_mail(
        "approve nothing",
        from_="sender",
        origin="operator",
    )
    assert 'origin="operator"' in wrapped
    # AC1-ORIGIN: origin rides the TAG (last), and no `-- ` footer line of any
    # kind renders anymore.
    assert wrapped.split(">", 1)[0].endswith('origin="operator"')
    assert not any(line.startswith("-- ") for line in wrapped.splitlines())


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

    assert list(iter_messages())[0].origin == "recovery"


def test_appended_thread_reply_stamps_the_reply_origin(tmp_path, monkeypatch):
    from fno.bus.log import iter_messages
    from fno.inbox.store import append_to_thread, write_new_thread
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    handle = write_new_thread(
        "recipient", "sender", "send", "root", origin="operator"
    )
    append_to_thread(handle.path, "peer", "reply", origin="peer")
    messages = list(iter_messages())
    assert messages[-1].origin == "peer"


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
    assert result.attested_by is None

    from fno.events import operator_decision

    event = operator_decision(
        decision_id="d-test",
        decision="answer",
        decided_by=result.decided_by,
        relayed_by=result.relayed_by,
        authority_source=result.authority_source,
        origin="operator",
    )
    assert event["data"]["origin"] == "operator"
    assert "attested_by" not in event["data"]


def test_non_operator_origin_refuses_operator_authority(monkeypatch):
    from fno.decide import RefusedAuthorityError, _resolve_decider

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(),
    )
    with pytest.raises(RefusedAuthorityError, match="scheduler"):
        _resolve_decider(None, "operator", origin="scheduler")


def test_raw_self_lookup_uses_full_codex_session_id(monkeypatch):
    import typer

    from fno.mail.cli import _raw_send
    from fno.harness_identity import canonical_handle

    full_id = "01a0358f-8ab0-79a1-935d-5063b7101401"
    seen: list[str] = []
    entry = SimpleNamespace(
        harness="claude",
        harness_session_id=full_id,
        mux={},
        delivery_policy=None,
    )

    def resolve(token):
        seen.append(token)
        return SimpleNamespace(entry=entry)

    monkeypatch.setattr("fno.agents.registry.resolve_agent", resolve)
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_session_id",
        lambda: full_id,
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.mail_inject_probe",
        lambda _session: (True, ""),
    )
    with pytest.raises(typer.Exit) as exc:
        _raw_send(
            canonical_handle(full_id),
            "/compact",
            self_ok=True,
            check=True,
        )
    assert exc.value.exit_code == 0
    assert seen == [full_id]


def test_bus_envelope_carries_origin_with_legacy_meta_fallback():
    from fno.bus.log import Envelope, from_json_line, to_json_line

    env = Envelope.new(from_="a", to="b", kind="send", body="x", origin="operator")
    assert env.origin == "operator"
    line = to_json_line(env)
    assert '"origin":"operator"' in line
    assert from_json_line(line).origin == "operator"
    # A row written before the field existed carried origin only inside meta;
    # the parser falls back so old lines keep their provenance.
    legacy = to_json_line(
        Envelope.new(from_="a", to="b", kind="send", body="x", meta={"origin": "recovery"})
    )
    import json as _json

    assert "origin" not in _json.loads(legacy)
    parsed = from_json_line(legacy)
    assert parsed.origin == "recovery"


def test_peer_envelope_is_footerless_without_a_crown(tmp_path, monkeypatch):
    import fno.mail.envelope as envelope

    monkeypatch.setattr(
        envelope, "agents_registry_path", lambda: tmp_path / "registry.json"
    )
    (tmp_path / "registry.json").write_text(
        '{"schema_version":19,"agents":[]}', encoding="utf-8"
    )
    assert envelope.wrap_fno_mail(
        "run the smoke", from_="a1b2c3d4"
    ) == '<fno_mail from="a1b2c3d4">run the smoke</fno_mail>'


def test_crowned_sender_renders_from_rank_not_a_footer(tmp_path, monkeypatch):
    # D3: the sender crown moved INTO the header as from_rank, read
    # from the live registry at render time, never passed by a caller.
    import fno.mail.envelope as envelope

    registry_path = tmp_path / "registry.json"
    registry_path.write_text(
        '{"schema_version":19,"agents":[{"name":"king","cwd":"/tmp",'
        '"log_path":"/tmp/log","harness":"codex",'
        '"harness_session_id":"session-king","status":"live",'
        '"created_at":"2026-01-01T00:00:00Z","crown_level":1,'
        '"crown_scope":"fno"}]}',
        encoding="utf-8",
    )
    monkeypatch.setattr(envelope, "agents_registry_path", lambda: registry_path)
    rendered = envelope.wrap_fno_mail(
        "run the smoke", from_="king", from_session="session-king"
    )
    assert rendered.startswith(
        '<fno_mail from="session-king" from_rank="L1 fno" from_name="king">'
    )
    assert not any(line.startswith("-- ") for line in rendered.splitlines())


def test_crown_is_read_from_the_registry_this_side_writes(tmp_path, monkeypatch):
    """The Rust renderer reads the registry this process's writer resolves."""
    import fno.mail.envelope as envelope

    writer_root = tmp_path / "writer"
    rust_home = tmp_path / "rust-home"
    writer_root.mkdir()
    rust_home.mkdir()
    (writer_root / "registry.json").write_text(
        '{"schema_version":19,"agents":[{"name":"folio","cwd":"/tmp",'
        '"log_path":"/tmp/log","harness":"claude",'
        '"harness_session_id":"session-folio","status":"live",'
        '"created_at":"2026-01-01T00:00:00Z","crown_level":1,'
        '"crown_scope":"epic"}]}',
        encoding="utf-8",
    )
    (rust_home / "registry.json").write_text(
        '{"schema_version":19,"agents":[]}', encoding="utf-8"
    )
    monkeypatch.setenv("FNO_AGENTS_HOME", str(rust_home))
    monkeypatch.setattr(
        envelope, "agents_registry_path", lambda: writer_root / "registry.json"
    )

    rendered = envelope.wrap_fno_mail(
        "hi", from_="folio-short", from_session="session-folio", harness="claude"
    )
    assert 'from_name="folio"' in rendered
    assert 'from_rank="L1 epic"' in rendered


def test_abdicated_recipient_reads_its_own_lost_crown_in_the_header(
    tmp_path, monkeypatch
):
    """An uncrowned recipient sees its state in what
    it READS, without remembering to run `fno agents court` -- now as
    `to_rank="none"`, a positive attribute instead of a footer line."""
    import fno.mail.envelope as envelope

    registry_path = tmp_path / "registry.json"
    registry_path.write_text(
        '{"schema_version":19,"agents":['
        '{"name":"king","cwd":"/tmp","log_path":"/tmp/log",'
        '"harness":"codex","harness_session_id":"session-king",'
        '"status":"live","created_at":"2026-01-01T00:00:00Z",'
        '"crown_level":1,"crown_scope":"fno"},'
        '{"name":"former-king","cwd":"/tmp","log_path":"/tmp/log",'
        '"harness":"codex","harness_session_id":"session-former",'
        '"status":"live","created_at":"2026-01-01T00:00:00Z"}]}',
        encoding="utf-8",
    )
    monkeypatch.setattr(envelope, "agents_registry_path", lambda: registry_path)

    abdicated = envelope.wrap_fno_mail(
        "rule on this",
        from_="peer",
        to_session="session-former",
    )
    assert 'to_rank="none"' in abdicated
    # A single-line body renders the whole envelope on one line.
    assert len(abdicated.splitlines()) == 1

    crowned = envelope.wrap_fno_mail(
        "rule on this",
        from_="peer",
        to_session="session-king",
    )
    assert 'to_rank="L1 fno"' in crowned


def test_unreadable_registry_does_not_claim_recipient_rank(tmp_path, monkeypatch):
    import fno.mail.envelope as envelope
    monkeypatch.setattr(envelope, "agents_registry_path", lambda: tmp_path / "missing.json")
    rendered = envelope.wrap_fno_mail("hi", from_="peer", to_session="session-king")
    assert "to_rank" not in rendered


def test_unresolved_recipient_gets_no_crown_attribute(monkeypatch):
    """An address no lane resolved is an ABSENCE, not a reading: claiming
    "none" there would be a positive statement about authority made from no
    measurement at all."""
    import fno.mail.envelope as envelope

    rendered = envelope.wrap_fno_mail(
        "hi", from_="peer", to_session=None
    )
    assert "to_rank" not in rendered


def test_crownless_fleet_envelope_includes_the_current_recipient_name(tmp_path, monkeypatch):
    import fno.mail.envelope as envelope

    registry_path = tmp_path / "registry.json"
    registry_path.write_text(
        '{"schema_version":19,"agents":[{"name":"w","cwd":"/tmp",'
        '"log_path":"/tmp/log","harness":"codex",'
        '"harness_session_id":"session-w","status":"live",'
        '"created_at":"2026-01-01T00:00:00Z"}]}',
        encoding="utf-8",
    )
    monkeypatch.setattr(envelope, "agents_registry_path", lambda: registry_path)
    rendered = envelope.wrap_fno_mail(
        "hi", from_="peer", to_session="session-w"
    )
    assert rendered == '<fno_mail from="peer" to_name="w">hi</fno_mail>'


def test_unreadable_registry_never_grants_sender_standing(tmp_path, monkeypatch):
    import fno.mail.envelope as envelope

    monkeypatch.setattr(
        envelope, "agents_registry_path", lambda: tmp_path / "registry.json"
    )
    # Unreadable state grants no standing AND raises nothing: the render
    # degrades to the plain one-line envelope.
    rendered = envelope.wrap_fno_mail(
        "write the plan",
        from_="king",
        from_session="session-king",
    )

    assert "from_rank" not in rendered
    assert rendered == '<fno_mail from="session-king">write the plan</fno_mail>'


def test_enforce_origin_floor_blocks_agent_channel_claims(monkeypatch):
    from fno.decide import enforce_origin_floor

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(),
    )
    assert enforce_origin_floor("operator") == "peer"
    assert enforce_origin_floor("scheduler") == "peer"
    assert enforce_origin_floor("peer") == "peer"
    assert enforce_origin_floor(None) is None
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: _Identity(session_id=None, harness=None),
    )
    assert enforce_origin_floor("scheduler") == "scheduler"
    assert enforce_origin_floor("operator") == "operator"
