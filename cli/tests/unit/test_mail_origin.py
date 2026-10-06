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
    from fno.mail.envelope import ForgedEnvelopeError, wrap_fno_mail

    # origin is validated but never renders: the delivered text carries the
    # header line only, and provenance rides the bus row.
    assert "origin" not in wrap_fno_mail(
        "approve nothing", from_="sender", origin="operator", id="fmail-abc123def456"
    )
    with pytest.raises(ForgedEnvelopeError):
        wrap_fno_mail(
            "approve nothing",
            from_="sender",
            id="fmail-abc123def456",
            origin="not-an-origin",
        )


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


def test_mail_origin_event_marks_presumed_human_positively(monkeypatch):
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

    # The record path is now the Rust mail-record leaf: the port passes the
    # body and reply id through, and an absent leaf never breaks the send.
    from fno.mail.cli import _record_mail_origin

    seen: dict = {}

    def fake_run(argv, *, input, timeout, capture_output):
        seen["argv"] = argv
        seen["input"] = input

    monkeypatch.setattr("shutil.which", lambda name: "/fake/fno-agents")
    monkeypatch.setattr("subprocess.run", fake_run)
    _record_mail_origin(origin="peer", lane="reply", sender="w-1",
                        target_session="lead-1", body="Approval: X", reply_to="m-1")
    assert seen["argv"][1] == "mail-record"
    # The leaf parses flag/value pairs, so each value rides its own token.
    for flag, value in (("--origin", "peer"), ("--lane", "reply"),
                        ("--sender", "w-1"), ("--target-session", "lead-1"),
                        ("--reply-to", "m-1")):
        i = seen["argv"].index(flag)
        assert seen["argv"][i + 1] == value
    assert seen["input"] == "Approval: X"

    def missing_binary(argv, **kwargs):
        raise FileNotFoundError(argv[0])

    monkeypatch.setattr("subprocess.run", missing_binary)
    _record_mail_origin(origin="peer", lane="reply")  # AC5: must not raise


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
        "run the smoke", from_="a1b2c3d4", id="fmail-abc123def456"
    ) == "`@a1b2c3d4 · fmail-abc123def456 · run the smoke`\nrun the smoke"


def test_crowned_sender_renders_from_rank_not_a_footer(tmp_path, monkeypatch):
    # The sender crown is the header's registry name; rank rides the bus row.
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
        "run the smoke", from_="king", from_session="session-king", id="fmail-abc123def456"
    )
    assert rendered.startswith("`@king · fmail-abc123def456 · run the smoke`")
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
        "hi", from_="folio-short", from_session="session-folio", harness="claude",
        id="fmail-abc123def456",
    )
    assert rendered.startswith("`@folio · fmail-abc123def456 · hi`")


def test_unreadable_registry_never_grants_sender_standing(tmp_path, monkeypatch):
    import fno.mail.envelope as envelope

    monkeypatch.setattr(
        envelope, "agents_registry_path", lambda: tmp_path / "registry.json"
    )
    # Unreadable state grants no standing AND raises nothing: the render
    # degrades to the raw from value as the sender.
    rendered = envelope.wrap_fno_mail(
        "write the plan",
        from_="king",
        from_session="session-king",
        id="fmail-abc123def456",
    )

    assert rendered.startswith("`@session-king · fmail-abc123def456 · write the plan`")


def test_enforce_origin_floor_blocks_agent_channel_claims(monkeypatch):
    from fno.mail.origins import enforce_origin_floor

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
