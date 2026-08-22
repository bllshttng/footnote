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


@pytest.mark.parametrize("status", ["spawning", "ready", "idle", "busy", "restarting"])
def test_force_accepts_every_non_terminal_status(_tmp_state, monkeypatch, status):
    """`live` is one of SIX non-terminal statuses, and the wrong one to test.

    `dispatch_spawn_pane` writes `spawning` until the SessionStart restamp and
    `register_agent` defaults to `idle`, so an equality test refused panes that
    were alive and blamed a recycled pane id for it. The registry publishes
    `TERMINAL_STATUSES` precisely so callers stop hand-rolling the vocabulary.
    """
    entry, sent = _install(monkeypatch)
    entry.status = status

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "status?", "--force"],
    )

    assert result.exit_code == 0, (status, result.output, result.stderr)
    assert sent, f"a {status} pane is alive and must be typed into"


@pytest.mark.parametrize("status", ["exited", "orphaned", "failed", "permanent_dead"])
def test_force_refuses_a_row_that_is_not_live(_tmp_state, monkeypatch, status):
    """A stale row's pane id may belong to somebody else by now.

    The resolved lane already gates the identical call on `status == "live"`,
    because an exited row keeps its mux ref and pane ids are reused across a mux
    restart. `--force` needs that MORE, not less: it is reached after the ladder
    missed, which is exactly the shape a dead recipient makes. Forcing overrides
    the transport, never the fact that nobody is there.
    """
    entry, sent = _install(monkeypatch)
    entry.status = status

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "status?", "--force"],
    )

    assert result.exit_code == 1
    assert status in (result.stderr or "")
    assert not sent, "nothing may be typed into a pane a dead row still names"


def test_force_refuses_a_send_to_this_session(_tmp_state, monkeypatch):
    """The ladder parks a self-send durable and `--force` returned before it.

    Typing a wrapped envelope into this session's own composer, with the turn
    interlock skipped, is what that bar exists to stop. Self-injection has its
    own lane and it is the raw one.
    """
    entry, sent = _install(monkeypatch)
    monkeypatch.setattr(
        "fno.mail.cli._self_recipient",
        lambda token, **_k: "0199aaaa",
    )

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "note to self", "--force"],
    )

    assert result.exit_code == 2
    assert "--to-self --raw" in (result.stderr or "")
    assert not sent


@pytest.mark.parametrize(
    "argv",
    [
        ["send", "node:x-1234", "ship it", "--force"],
        ["send", "--to-project", "footnote", "ship it", "--force"],
        ["send", "worker", "/fno:review", "--raw", "--force"],
        ["send", "--to-project", "footnote", "heads up", "--kind", "heads-up", "--force"],
    ],
    ids=["job-address", "project", "raw", "kind"],
)
def test_force_refuses_the_lanes_that_have_no_pane(_tmp_state, monkeypatch, argv):
    """A dropped transport flag is worse than a refused one.

    None of these lanes names one pane, and each ENDS the command itself. So a
    guard placed after any of them leaves the flag dropped there, which is how
    the first version of this guard missed `--raw` and `--kind`: it sat below
    both. The parametrize is the point, not the individual cases.
    """
    _entry_row, sent = _install(monkeypatch)

    result = runner.invoke(mail_app, argv)

    assert result.exit_code == 2
    assert "--force" in (result.stderr or "")
    assert not sent
    assert "queued (durable)" not in result.output


def test_force_reaches_a_registered_agent_by_name(_tmp_state, monkeypatch):
    """The address the refusal text tells you to use has to work.

    Discovery is liveness-gated, so a registered worker whose listing misses
    lands on the token rung, and that IS the situation `--force` exists for. The
    token rung refused any non-session-shaped token, because a name cannot be a
    durable recipient when the drain is handle-keyed. Forcing writes no such
    row: it types at a pane the registry names, and the registry resolves the
    name to the session behind it.
    """
    entry, sent = _install(monkeypatch)
    entry.name = "blueprint-x-ce6e"

    result = runner.invoke(mail_app, ["send", "blueprint-x-ce6e", "status?", "--force"])

    assert result.exit_code == 0, (result.output, result.stderr)
    assert "typed (pane 45)" in result.output
    assert sent, "a live row with a pane must be typed into, not refused"
    # Addressed by the resolved session's handle, never by the friendly name:
    # the name is not a mail address and a row keyed on it strands the message.
    assert "0199aaaa" in result.output


def test_force_refuses_a_bus_only_recipient_up_front(_tmp_state, monkeypatch):
    """Bus-only is a POLICY on the row, so it is knowable before typing.

    `_mux_pane_send` returns False for it and prints nothing, which every other
    caller reads correctly as "demote to durable". `--force` is the one caller
    that does not demote, so that silent False came out as an exit blaming a
    refusal never printed and a composer never typed into, with no durable row
    written. The same send without `--force` queues durable and gets drained.
    """
    entry, sent = _install(monkeypatch)
    entry.delivery_policy = "bus-only"

    result = runner.invoke(
        mail_app,
        ["send", "0199aaaa-1111-7000-8000-aaaaaaaaaaaa", "status?", "--force"],
    )

    assert result.exit_code == 1
    err = result.stderr or ""
    assert "bus-only" in err
    # It must name the working route, not just refuse.
    assert "without --force" in err
    assert not sent
