"""Round-trip ACs for the name-lane ``fno mail reply`` (x-8045).

A name-lane reply answers the original sender without re-typing their handle and
threads the correlation via the bus ``in_reply_to`` (one token, also stamped as
the wire ``reply_to`` attr). These tests assert against the bus log -- the source
of truth -- not stdout wording. Covers the cross-seam ACs no single task owns:
AC2-HP (bus queryability), AC2-EDGE (dual replies), AC1-FR (live-inject miss),
plus AC1-HP / AC1-ERR / AC2-ERR routing.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    """Co-isolate the md render (FNO_INBOX_ROOT) and the bus log under tmp."""
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


def _seed_name_lane_inbound(*, to: str, from_: str, body: str) -> str:
    """Seed one inbound name-lane bus message (as if <from_> sent it to me).
    Returns its thread_id -- the ``--to`` msg-id to reply to."""
    from fno.inbox.store import write_new_thread

    return write_new_thread(
        recipient=to, sender=from_, kind="send", body=body, to_kind="name"
    ).thread_id


def _bus_msgs():
    from fno.bus.log import iter_messages

    return list(iter_messages())


def _isolate_empty_discovery(monkeypatch, tmp_path):
    """Point every discovery source at empty dirs so a handle resolves to nothing
    (an offline sender)."""
    from fno.agents import discover

    empty = tmp_path / "empty"
    empty.mkdir(exist_ok=True)
    monkeypatch.setenv(discover.SESSIONS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(empty))
    monkeypatch.setenv(discover.CODEX_SESSIONS_DIR_ENV, str(empty))
    daemon = tmp_path / "daemon-empty"
    daemon.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))


def _isolate_claude_roster(monkeypatch, tmp_path, *, session_id):
    """Only the daemon roster resolves: empty disk sources + a fixture roster
    holding one rostered claude session at <session_id>."""
    _isolate_empty_discovery(monkeypatch, tmp_path)
    daemon = tmp_path / "daemon"
    daemon.mkdir(parents=True, exist_ok=True)
    (daemon / "roster.json").write_text(
        json.dumps(
            {
                "proto": 1,
                "workers": {
                    session_id[:8]: {
                        "sessionId": session_id,
                        "pid": 4242,
                        "cwd": "/Users/x/proj",
                    }
                },
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))


def test_ac1hp_ac2hp_name_lane_reply_reaches_sender_and_is_queryable(
    runner, mailbox, monkeypatch, tmp_path
):
    # AC1-HP: reply reaches the original sender without re-typing the handle.
    # AC2-HP: the reply is queryable on the bus by in_reply_to.
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    msg = _seed_name_lane_inbound(
        to="claude-meeeeeee", from_="claude-9a063cd3", body="ping"
    )
    r = runner.invoke(
        app, ["mail", "reply", "--to", msg, "--from", "web", "--body", "ack"]
    )
    assert r.exit_code == 0, r.output
    # I never typed the sender handle on the command line; the reply still names it.
    assert "claude-9a063cd3" in r.output
    assert msg in r.output  # the correlated msg-id (re:<id>)

    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 1
    assert replies[0].to == "claude-9a063cd3"  # addressed to the original sender
    assert f'reply_to="{msg}"' in replies[0].body  # wire attr rides in the body


def test_ac1err_unknown_msg_id_is_rejected_sending_nothing(runner, mailbox):
    # AC1-ERR: a --to id absent from the bus is a hard error; nothing is sent.
    r = runner.invoke(
        app, ["mail", "reply", "--to", "msg-nope", "--from", "web", "--body", "hi"]
    )
    assert r.exit_code != 0
    assert _bus_msgs() == []


def test_ac2err_non_name_lane_routes_to_thread_store(runner, mailbox, monkeypatch):
    # AC2-ERR: a to_kind != "name" target skips the name lane entirely (the
    # existing thread-store reply path runs; no _name_lane_send call).
    import fno.mail.cli as mailcli
    from fno.inbox.store import write_new_thread

    calls: list = []
    monkeypatch.setattr(
        mailcli, "_name_lane_send", lambda *a, **k: calls.append((a, k))
    )
    h = write_new_thread(
        recipient="web", sender="etl", kind="fyi", body="broadcast", to_kind="project"
    )
    r = runner.invoke(
        app, ["mail", "reply", "--to", h.thread_id, "--from", "api", "--body", "re"]
    )
    assert r.exit_code == 0, r.output
    assert calls == []  # routed to the thread store, not the name lane


def test_ac2edge_two_replies_to_one_message_both_thread(
    runner, mailbox, monkeypatch, tmp_path
):
    # AC2-EDGE: two replies to one msg-id are two distinct bus messages, both
    # carrying the same in_reply_to.
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    msg = _seed_name_lane_inbound(
        to="claude-meeeeeee", from_="claude-9a063cd3", body="ping"
    )
    for body in ("first reply", "second reply"):
        r = runner.invoke(
            app, ["mail", "reply", "--to", msg, "--from", "web", "--body", body]
        )
        assert r.exit_code == 0, r.output

    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 2
    assert len({m.id for m in replies}) == 2  # distinct messages


def test_ac1fr_offline_sender_queues_durably_with_correlation(
    runner, mailbox, monkeypatch, tmp_path
):
    # AC1-FR: the original sender is no longer live -> the reply queues durably
    # addressed to their canonical handle, with the correlation on BOTH surfaces.
    _isolate_empty_discovery(monkeypatch, tmp_path)  # resolves to nothing (offline)

    msg = _seed_name_lane_inbound(
        to="claude-meeeeeee", from_="claude-deadbeef", body="ping"
    )
    r = runner.invoke(
        app, ["mail", "reply", "--to", msg, "--from", "web", "--body", "ack"]
    )
    assert r.exit_code == 0, r.output
    assert "queued (durable)" in r.output

    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 1
    rep = replies[0]
    assert rep.to == "claude-deadbeef"  # sender's canonical handle
    assert rep.in_reply_to == msg  # bus correlation
    assert f'reply_to="{msg}"' in rep.body  # wrapped-body wire attr (never split)
