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
    # THIS session's identity: the reply must stamp its canonical handle as `from`,
    # not a project name, so the sender can reply back and drain-self finds it.
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "11111111-2222-3333-4444-555566667777")

    msg = _seed_name_lane_inbound(
        to="claude-meeeeeee", from_="9a063cd3", body="ping"
    )
    r = runner.invoke(app, ["mail", "reply", "--to", msg, "--body", "ack"])
    assert r.exit_code == 0, r.output
    # I never typed the sender handle on the command line; the reply still names it.
    assert "9a063cd3" in r.output
    assert msg in r.output  # the correlated msg-id (re:<id>)

    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 1
    assert replies[0].to == "9a063cd3"  # sender resolved to its canonical handle
    assert replies[0].from_ == "11111111"  # my canonical handle, not a project
    assert f'reply_to="{msg}"' in replies[0].body  # wire attr rides in the body


def test_ac1err_unknown_msg_id_is_rejected_sending_nothing(runner, mailbox):
    # AC1-ERR: a --to id absent from BOTH the bus and the transcript is a hard
    # error; nothing is sent.
    r = runner.invoke(
        app, ["mail", "reply", "--to", "msg-nope", "--from", "web", "--body", "hi"]
    )
    assert r.exit_code != 0
    assert _bus_msgs() == []


def _seed_transcript_envelope(
    monkeypatch, tmp_path, *, session_id: str, from_: str, msg_id: str
) -> None:
    """Write a claude transcript for <session_id> containing a live-injected
    <fno_mail from=<from_> id=<msg_id>> envelope (JSON-escaped, as it lands), and
    seam CLAUDE_CODE_SESSION_ID + the projects dir so resolve_live_sender finds
    it. No bus record is written -- that is the whole point (LD11a)."""
    import json

    from fno.agents import discover

    projects = tmp_path / "projects"
    enc = projects / "-Users-x-proj"
    enc.mkdir(parents=True, exist_ok=True)
    envelope = (
        f'<fno_mail from="{from_}" harness="claude-code" model="opus" '
        f'id="{msg_id}">\nping\n</fno_mail>'
    )
    line = json.dumps({"type": "user", "message": {"role": "user", "content": envelope}})
    (enc / f"{session_id}.jsonl").write_text(line + "\n", encoding="utf-8")
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", session_id)


def test_us3_reply_to_live_injected_id_resolves_sender_from_transcript(
    runner, mailbox, monkeypatch, tmp_path
):
    # US3 / AC2-HP: a live-injected message wrote NO durable thread, so its id is
    # not on the bus. `fno mail reply --to <id>` recovers the sender off this
    # session's transcript and threads the correlation, addressed to that handle.
    sender = "9a063cd3"
    msg = "msg-live1"
    # Isolate discovery FIRST (sender offline -> durable floor), then seed the
    # transcript, which re-points the projects dir so resolve_live_sender reads
    # my seeded envelope rather than the empty isolation dir.
    _isolate_empty_discovery(monkeypatch, tmp_path)
    _seed_transcript_envelope(
        monkeypatch, tmp_path,
        session_id="11111111-2222-3333-4444-555566667777",
        from_=sender, msg_id=msg,
    )

    r = runner.invoke(app, ["mail", "reply", "--to", msg, "--body", "pong"])
    assert r.exit_code == 0, r.output

    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 1
    rep = replies[0]
    assert rep.to == sender  # addressed to the origin handle by identity
    assert rep.in_reply_to == msg  # bus correlation off an id with no durable thread
    assert f'reply_to="{msg}"' in rep.body  # wire attr equals the same id


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
        to="claude-meeeeeee", from_="9a063cd3", body="ping"
    )
    for body in ("first reply", "second reply"):
        r = runner.invoke(
            app, ["mail", "reply", "--to", msg, "--body", body]
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
        to="claude-meeeeeee", from_="deadbeef", body="ping"
    )
    r = runner.invoke(
        app, ["mail", "reply", "--to", msg, "--body", "ack"]
    )
    assert r.exit_code == 0, r.output
    assert "queued (durable)" in r.output

    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 1
    rep = replies[0]
    assert rep.to == "deadbeef"  # sender's canonical handle
    assert rep.in_reply_to == msg  # bus correlation
    assert f'reply_to="{msg}"' in rep.body  # wrapped-body wire attr (never split)


def test_reply_to_retired_sender_migrates_the_address_and_delivers(
    runner, mailbox, monkeypatch, tmp_path
):
    """A pre-flip record's retired `from` is migrated to the bare id and DELIVERED
    to the live sender - not refused.

    The retired form is not an address a caller may pass, but this one came off an
    old record, and the address it would carry today is a substring. Refusing would
    be a wall invented at a knowledge boundary: a human doing a translation the
    code can do, and a live peer treated as unreachable."""
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    msg = _seed_name_lane_inbound(to="meeeeeee", from_="claude-9a063cd3", body="ping")
    r = runner.invoke(app, ["mail", "reply", "--to", msg, "--body", "ack"])

    assert r.exit_code == 0, r.output
    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert len(replies) == 1
    assert replies[0].to == "9a063cd3"  # migrated, never the retired string


def test_reply_to_retired_sender_offline_still_addresses_the_bare_id(
    runner, mailbox, monkeypatch, tmp_path
):
    """Even with nothing live, the durable floor carries the MIGRATED address, so
    the record is drainable if that session ever wakes - the old string never is."""
    _isolate_empty_discovery(monkeypatch, tmp_path)
    msg = _seed_name_lane_inbound(to="meeeeeee", from_="claude-deadbeef", body="ping")

    r = runner.invoke(app, ["mail", "reply", "--to", msg, "--body", "ack"])

    assert r.exit_code == 0, r.output
    replies = [m for m in _bus_msgs() if m.in_reply_to == msg]
    assert [m.to for m in replies] == ["deadbeef"]


def test_ac1fr_offline_full_uuid_handle_wire_to_matches_durable(
    runner, mailbox, monkeypatch, tmp_path
):
    # AC2: the wire `to` attr carries the canonical handle, matching the
    # durable-bus recipient exactly -- never a divergent bare-hex form.
    _isolate_empty_discovery(monkeypatch, tmp_path)
    uuid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    msg = _seed_name_lane_inbound(
        to="claude-meeeeeee", from_=f"claude-{uuid}", body="ping"
    )
    r = runner.invoke(
        app, ["mail", "reply", "--to", msg, "--body", "ack"]
    )
    assert r.exit_code == 0, r.output
    rep = next(m for m in _bus_msgs() if m.in_reply_to == msg)
    assert rep.to == f"claude-{uuid}"  # durable floor to the full handle
    assert f'to="claude-{uuid}"' in rep.body  # wire `to` matches it exactly


# ---------------------------------------------------------------------------
# Dead-letterbox: fail loud when the message lands only on the durable floor
# (x-730d). The warning rides stderr; the durable enqueue keeps exit 0.
# ---------------------------------------------------------------------------


def test_deferred_warning_on_inject_miss(runner, mailbox, monkeypatch, tmp_path):
    # The recipient is rostered (resolves) but the live inject misses -> the
    # message hits only the durable floor, so stderr carries the deferral line.
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "11111111-2222-3333-4444-555566667777")

    msg = _seed_name_lane_inbound(
        to="claude-meeeeeee", from_="9a063cd3", body="ping"
    )
    r = runner.invoke(app, ["mail", "reply", "--to", msg, "--body", "ack"])
    assert r.exit_code == 0, r.output
    assert "has no live pane" in (r.stderr or "")
    assert "queued (durable)" in r.stdout


def test_no_deferred_warning_on_inject_hit(runner, mailbox, monkeypatch, tmp_path):
    # The inject succeeds -> hosted delivery, no deferral warning on stderr.
    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    _isolate_claude_roster(monkeypatch, tmp_path, session_id=sid)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: True)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "11111111-2222-3333-4444-555566667777")

    msg = _seed_name_lane_inbound(
        to="claude-meeeeeee", from_="9a063cd3", body="ping"
    )
    r = runner.invoke(app, ["mail", "reply", "--to", msg, "--body", "ack"])
    assert r.exit_code == 0, r.output
    assert "delivered (hosted)" in r.stdout
    assert "no live pane" not in (r.stderr or "")
