"""fno mail sent / fno mail withdraw (x-0548).

A sender could see that mail was stranded and could do nothing about it: the
count in `mail status` and the every-prompt nudge named a number and no ids, and
no verb retracted anything. `mail ack` only advances an INBOUND read cursor.

The withdrawal is a tombstone because the bus log is append-only and read state
is a per-consumer cursor. Deleting the line is impossible and advancing the
recipient's cursor would mark every OTHER message queued for them as seen.

The tests below are grouped by the thing that actually broke in the field: a
tombstone honored by only one reader. `scan_unread` is the choke point for seven
readers, but the sender's own nag and the relay router read `iter_messages`
directly and bypass it. Each of the three is asserted independently, because an
assertion that two paths agree pins the tag, not the destination.
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.paths_testing import use_tmpdir

MARKERS = (
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_SESSION_ID",
    "GEMINI_SESSION_ID",
)
MY_SID = "ffffabcd1234"
MY_HANDLE = "ffffabcd"
PEER = "aaaabbbb"


def _ts_ago(seconds: int) -> str:
    dt = datetime.now(tz=timezone.utc) - timedelta(seconds=seconds)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


@pytest.fixture
def env(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    for m in MARKERS:
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", MY_SID)
    return tmp_path


def _send(from_, to, body, *, ts=None, to_kind="session") -> str:
    from fno.bus.log import Envelope, append

    env_ = Envelope.new(
        from_=from_, to=to, kind="send", body=body, ts=ts, to_kind=to_kind
    )
    append(env_)
    return env_.id


def _run(*args):
    return CliRunner().invoke(app, list(args))


# ---------------------------------------------------------------------------
# fno mail sent
# ---------------------------------------------------------------------------


def test_sent_lists_the_ids_the_nag_only_counted(env):
    """The whole point: the nag says "1 unclaimed" and this says WHICH."""
    mid = _send(MY_HANDLE, PEER, "hello", ts=_ts_ago(3600))

    res = _run("mail", "sent", "--unclaimed", "--json")

    assert res.exit_code == 0
    rows = json.loads(res.stdout)
    assert [r["id"] for r in rows] == [mid]
    assert rows[0]["to"] == PEER
    assert rows[0]["claimed"] is False


def test_sent_unclaimed_applies_the_same_ttl_as_the_nag(env):
    """A message younger than the TTL is not what the nag is counting, so it
    must not appear under --unclaimed either. Two differently-scoped answers to
    one question is how a sender stops trusting the report."""
    _send(MY_HANDLE, PEER, "fresh", ts=_ts_ago(5))

    res = _run("mail", "sent", "--unclaimed", "--json")

    assert res.exit_code == 0
    assert json.loads(res.stdout) == []


def test_sent_without_filter_marks_claimed_mail(env):
    from fno.bus.cursor import advance_cursor

    claimed = _send(MY_HANDLE, PEER, "read me", ts=_ts_ago(3600))
    advance_cursor(PEER, claimed)
    stranded = _send(MY_HANDLE, "cccc1111", "not read", ts=_ts_ago(3600))

    rows = json.loads(_run("mail", "sent", "--json").stdout)

    by_id = {r["id"]: r for r in rows}
    assert by_id[claimed]["claimed"] is True
    assert by_id[stranded]["claimed"] is False


def test_sent_shows_only_my_own_outbound(env):
    """Scoped to this session's handle, so it never lists mail this session
    could not withdraw anyway."""
    mine = _send(MY_HANDLE, PEER, "mine", ts=_ts_ago(3600))
    _send(PEER, "cccc1111", "someone else's", ts=_ts_ago(3600))

    rows = json.loads(_run("mail", "sent", "--json").stdout)

    assert [r["id"] for r in rows] == [mine]


# ---------------------------------------------------------------------------
# fno mail withdraw: refusals
# ---------------------------------------------------------------------------


def test_withdraw_refuses_a_message_i_did_not_send(env):
    mid = _send(PEER, "cccc1111", "not mine", ts=_ts_ago(3600))

    res = _run("mail", "withdraw", mid)

    assert res.exit_code == 1
    assert "only its sender can withdraw it" in res.stderr


def test_withdraw_refuses_an_already_claimed_message(env):
    """Once they have read it, hiding it hides it from the sender only."""
    from fno.bus.cursor import advance_cursor

    mid = _send(MY_HANDLE, PEER, "already read", ts=_ts_ago(3600))
    advance_cursor(PEER, mid)

    res = _run("mail", "withdraw", mid)

    assert res.exit_code == 1
    assert "already claimed" in res.stderr


def test_withdraw_refuses_an_unknown_id(env):
    res = _run("mail", "withdraw", "msg-nope99")

    assert res.exit_code == 1
    assert "no such message" in res.stderr


def test_withdraw_appends_nothing_when_it_refuses(env):
    """A refusal that still wrote a tombstone would leave the log claiming a
    withdrawal that never took effect."""
    from fno.bus.log import iter_messages

    mid = _send(PEER, "cccc1111", "not mine", ts=_ts_ago(3600))
    before = len(list(iter_messages()))

    _run("mail", "withdraw", mid)

    assert len(list(iter_messages())) == before


def test_withdraw_is_idempotent(env):
    mid = _send(MY_HANDLE, PEER, "twice", ts=_ts_ago(3600))
    assert _run("mail", "withdraw", mid).exit_code == 0

    res = _run("mail", "withdraw", mid)

    assert res.exit_code == 0
    assert "already withdrawn" in res.stdout


# ---------------------------------------------------------------------------
# The three readers, asserted independently
# ---------------------------------------------------------------------------


def test_reader_1_scan_unread_omits_a_withdrawn_message(env):
    """The recipient side: seven readers reach the bus through here."""
    from fno.bus.cursor import scan_unread

    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))
    kept = _send(MY_HANDLE, PEER, "keep me", ts=_ts_ago(3600))

    _run("mail", "withdraw", mid)

    assert [m.id for m in scan_unread(PEER)] == [kept]


def test_reader_1_does_not_deliver_the_tombstone_itself(env):
    """Hiding the target but delivering the tombstone would hand the recipient
    a bare "this was withdrawn" line for a message they never saw."""
    from fno.bus.cursor import scan_unread

    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))
    _run("mail", "withdraw", mid)

    assert scan_unread(PEER) == []


def test_reader_2_sent_unclaimed_stops_counting_it(env):
    """The nag reads iter_messages directly and bypasses scan_unread. A
    withdrawal that did not reach here would leave the every-prompt line firing
    forever on a message the sender already retracted -- the exact symptom this
    verb exists to end."""
    from fno.config import load_settings
    from fno.mail.cli import _sent_unclaimed

    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))
    ttl = load_settings().inbox.unclaimed_ttl
    assert len(_sent_unclaimed(MY_HANDLE, ttl)) == 1

    _run("mail", "withdraw", mid)

    assert _sent_unclaimed(MY_HANDLE, ttl) == []


def test_reader_2_status_line_goes_quiet(env):
    """End to end through the surface a sender actually reads."""
    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))
    assert "sent unclaimed: 1" in _run("mail", "status", "--from", "proj").stdout

    _run("mail", "withdraw", mid)

    assert "sent unclaimed: 0" in _run("mail", "status", "--from", "proj").stdout


def test_reader_3_relay_router_skips_it(env):
    """The router reads iter_messages directly too, and routes by each
    message's `to` rather than filtering to one inbox."""
    from fno.relay.daemon import _tail_after_cursor

    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))
    kept = _send(MY_HANDLE, PEER, "keep me", ts=_ts_ago(3600))

    _run("mail", "withdraw", mid)

    routed, _ = _tail_after_cursor()
    assert [m.id for m in routed] == [kept]


# ---------------------------------------------------------------------------
# What a withdrawal must NOT do
# ---------------------------------------------------------------------------


def test_withdraw_does_not_advance_the_recipient_cursor(env):
    """The failure this shape was chosen to avoid. A cursor is a last-seen
    POSITION, so advancing it past the withdrawn message would mark every other
    message queued for that recipient as seen -- trading one strand for many."""
    from fno.bus.cursor import read_cursor, scan_unread

    from_other = _send(PEER, PEER, "unrelated, still unread", ts=_ts_ago(3600))
    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))

    _run("mail", "withdraw", mid)

    assert read_cursor(PEER) is None
    assert [m.id for m in scan_unread(PEER)] == [from_other]


def test_withdraw_does_not_delete_the_line(env):
    """Append-only is an invariant of the log, not an implementation detail:
    `mail view` is the audit projection of the source of record, and a
    withdrawal that erased history would make it lie."""
    from fno.bus.log import iter_messages

    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))

    _run("mail", "withdraw", mid)

    assert any(m.id == mid for m in iter_messages())


def test_a_forged_tombstone_cannot_retract_someone_elses_mail(env):
    """The sender check lives at the READ side as well as in the verb. The verb
    is one reachable path; a hand-appended or replayed line is another, and a
    tombstone that only the verb validated would let one appear."""
    from fno.bus.cursor import scan_unread
    from fno.bus.log import WITHDRAW_KIND, Envelope, append

    mid = _send(PEER, "cccc1111", "theirs", ts=_ts_ago(3600))
    append(
        Envelope.new(
            from_=MY_HANDLE,
            to="cccc1111",
            kind=WITHDRAW_KIND,
            body=f"withdrawn: {mid}",
            meta={"withdraws": mid},
        )
    )

    assert [m.id for m in scan_unread("cccc1111")] == [mid]
