"""``fno mail send --force``: receipt vocabulary and the pane provenance row.

Node x-3a64. ``--force`` changes only the TRANSPORT. Before it existed a
live-miss made the sender switch verbs, and switching verbs is what lost the
envelope, the message id, the reply handle, and the outbox row.

The receipt is the part that must not drift. `typed` is not `delivered`: bytes
written to a PTY is not delivery and is certainly not action, since a full
payload can arrive, render, and be discarded while the return selects a prompt's
default.
"""
from __future__ import annotations

import json
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.mail.cli import mail_app
from fno.paths_testing import use_tmpdir

runner = CliRunner()


@pytest.fixture
def _tmp_state(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


def _entry():
    return SimpleNamespace(
        name="worker",
        mux={"session": "main", "pane_id": 45},
        harness="claude",
        harness_session_id="0199aaaa-1111-7000-8000-aaaaaaaaaaaa",
        session_id="0199aaaa-1111-7000-8000-aaaaaaaaaaaa",
        cwd="/w",
        status="live",
    )


def _install(monkeypatch, *, typed=True, refusal=None):
    """Resolve the recipient to a mux row and stub the pane write.

    ``_mux_pane_send`` is stubbed rather than driven through subprocess, because
    what is under test here is the RECEIPT and the outbox row, not the paste.
    The paste itself is covered in ``test_dispatch_mux_send.py``.
    """
    import fno.agents.dispatch as dispatch
    import fno.agents.discover as discover_mod
    import fno.agents.registry as registry

    entry = _entry()
    monkeypatch.setattr(
        discover_mod,
        "resolve_or_suggest",
        lambda _token, **_k: (None, []),
    )
    monkeypatch.setattr(
        registry,
        "resolve_agent",
        lambda _token, **_k: SimpleNamespace(entry=entry),
    )

    sent: list[tuple] = []

    def _pane_send(e, text, **kwargs):
        sent.append((e, text, kwargs))
        return typed

    monkeypatch.setattr(dispatch, "_mux_pane_send", _pane_send)
    return entry, sent


def test_force_receipt_says_typed_with_the_pane_and_never_delivered(
    _tmp_state, monkeypatch
):
    _entry_row, sent = _install(monkeypatch)

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "status?", "--force"],
    )

    assert result.exit_code == 0, result.output
    assert "typed (pane 45)" in result.output
    assert "delivered" not in result.output
    assert sent, "the pane write must actually be attempted"


def test_force_types_the_wrapped_body_not_the_bare_text(_tmp_state, monkeypatch):
    """`--force` keeps the mail semantics: the recipient sees an envelope with a
    sender, a msg-id to reply to, and the authority trailer."""
    _entry_row, sent = _install(monkeypatch)

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "status?", "--force"],
    )
    assert result.exit_code == 0, result.output

    _e, text, kwargs = sent[0]
    assert text.startswith("<fno_mail from=")
    assert "status?" in text
    assert "peer mail" in text
    assert kwargs.get("raw") is not True, (
        "the forced send must cross the read-back gate, so it cannot be raw"
    )


def test_force_writes_an_outbox_row_naming_the_pane(_tmp_state, monkeypatch):
    """The mail_id -> pane_id mapping is what makes a typed message auditable.

    Without it a keystroke delivery is invisible to every mail surface: the
    recipient sees text with no id and the sender's outbox has no row.
    """
    from fno.bus.log import bus_log_path

    _entry_row, _sent = _install(monkeypatch)

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "status?", "--force"],
    )
    assert result.exit_code == 0, result.output

    rows = [
        json.loads(line)
        for line in bus_log_path().read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert len(rows) == 1, rows
    row = rows[0]
    assert row["delivery"] == "typed"
    assert row["meta"]["transport"] == "pane"
    assert row["meta"]["pane_id"] == "45"
    assert row["id"] in result.output, "the receipt names the id the row records"


def test_a_typed_row_is_not_redelivered_to_the_recipient(_tmp_state, monkeypatch):
    """A typed row is audit-only. Marking it deliverable would hand the
    recipient a second copy of text already sitting at its prompt."""
    from fno.bus.log import Envelope, is_deliverable

    typed_row = Envelope.new(
        from_="a", to="b", kind="send", body="x", delivery="typed"
    )
    assert is_deliverable(typed_row) is False


def test_a_refused_pane_write_claims_nothing(_tmp_state, monkeypatch):
    """The gate refuses a pane showing an option prompt. Nothing was typed, so
    no row may say it was."""
    from fno.bus.log import bus_log_path

    _entry_row, _sent = _install(monkeypatch, typed=False)

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "take option 3", "--force"],
    )

    assert result.exit_code != 0
    assert "typed" not in result.stdout
    path = bus_log_path()
    assert not path.exists() or path.read_text(encoding="utf-8").strip() == ""


def test_force_writes_the_full_sender_session_on_the_row(_tmp_state, monkeypatch):
    """`cmd_reply` reads the bus before any transcript. A forced row carrying
    only the head-8 handle refuses as ambiguous on the one transport that has no
    live confirmation to fall back to."""
    import json as _json

    from fno.bus.log import bus_log_path

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_session_id",
        lambda *_a, **_k: "0199bbbb-2222-7000-8000-bbbbbbbbbbbb",
    )
    _install(monkeypatch)

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "status?", "--force"],
    )
    assert result.exit_code == 0, result.output

    row = _json.loads(bus_log_path().read_text(encoding="utf-8").splitlines()[0])
    assert row["from_session"] == "0199bbbb-2222-7000-8000-bbbbbbbbbbbb"


def test_force_to_an_unknown_token_refuses_with_a_message(_tmp_state, monkeypatch):
    """Discovery is liveness-gated, so a registered worker whose listing misses
    lands on the token rung - the exact situation --force exists for. Without a
    handler the verb exited non-zero with an empty terminal and a traceback."""
    import fno.agents.discover as discover_mod
    import fno.agents.registry as registry

    monkeypatch.setattr(
        discover_mod, "resolve_or_suggest", lambda _t, **_k: (None, [])
    )
    monkeypatch.setattr(
        registry,
        "resolve_agent",
        lambda _t, **_k: (_ for _ in ()).throw(registry.AgentResolutionError("no")),
    )

    result = runner.invoke(mail_app, ["send", "totally-unknown-worker", "hi", "--force"])

    assert result.exit_code != 0
    assert "totally-unknown-worker" in result.output
    assert result.exception is None or isinstance(result.exception, SystemExit)
