"""fno agents mail sent / fno agents mail withdraw (x-0548).

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


def _send(
    from_, to, body, *, ts=None, to_kind="session", delivery=None
) -> str:
    from fno.bus.log import Envelope, append

    env_ = Envelope.new(
        from_=from_, to=to, kind="send", body=body, ts=ts,
        to_kind=to_kind, delivery=delivery,
    )
    append(env_)
    return env_.id


def _run(*args):
    return CliRunner().invoke(app, list(args))


# ---------------------------------------------------------------------------
# fno agents mail sent
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


def test_sent_separates_hosted_audit_from_durable_unclaimed(env):
    hosted = _send(
        MY_HANDLE, PEER, "already delivered", ts=_ts_ago(3600),
        delivery="hosted",
    )
    durable = _send(MY_HANDLE, PEER, "still pending", ts=_ts_ago(3600))

    rows = json.loads(_run("mail", "sent", "--json").stdout)
    assert {
        row["id"]: (row["delivery"], row["claimed"])
        for row in rows
    } == {
        hosted: ("hosted", True),
        durable: ("durable", False),
    }

    unclaimed = json.loads(
        _run("mail", "sent", "--unclaimed", "--json").stdout
    )
    assert [row["id"] for row in unclaimed] == [durable]


def test_sent_does_not_call_a_just_sent_message_claimed(env):
    """Bus timestamps are whole seconds and the age test is strict `>`, so a
    message sent in the current second has age 0.0. A zero TTL would classify it
    as picked up -- reporting the opposite of the truth this verb exists to
    tell, on exactly the message a sender is most likely to be looking at."""
    mid = _send(MY_HANDLE, PEER, "just now")

    rows = json.loads(_run("mail", "sent", "--json").stdout)

    assert {r["id"]: r["claimed"] for r in rows} == {mid: False}


def test_sent_scopes_to_the_handle_the_send_path_stamps(env):
    """`stamp_from` is what writes `from`, and it floors to "fno" with no
    ambient identity while the precedence-only resolver would pick an inherited
    marker's session. Scoping by the wrong one lists mail this session did not
    send and hides mail it did."""
    from fno.agents.self_stamp import stamp_from

    for m in MARKERS:
        monkeypatch_env_clear(m)
    mid = _send(stamp_from(None), PEER, "from a bare shell", ts=_ts_ago(3600))

    rows = json.loads(_run("mail", "sent", "--json").stdout)

    assert [r["id"] for r in rows] == [mid]


def monkeypatch_env_clear(name: str) -> None:
    import os

    os.environ.pop(name, None)


def test_sent_shows_only_my_own_outbound(env):
    """Scoped to this session's handle, so it never lists mail this session
    could not withdraw anyway."""
    mine = _send(MY_HANDLE, PEER, "mine", ts=_ts_ago(3600))
    _send(PEER, "cccc1111", "someone else's", ts=_ts_ago(3600))

    rows = json.loads(_run("mail", "sent", "--json").stdout)

    assert [r["id"] for r in rows] == [mine]


# ---------------------------------------------------------------------------
# fno agents mail withdraw: refusals
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


def test_withdraw_refuses_hosted_audit_without_tombstone(env):
    from fno.bus.log import WITHDRAW_KIND, iter_messages

    mid = _send(
        MY_HANDLE, PEER, "already delivered", ts=_ts_ago(3600),
        delivery="hosted",
    )

    res = _run("mail", "withdraw", mid)

    assert res.exit_code == 1
    assert "already delivered (hosted)" in res.stderr
    assert all(m.kind != WITHDRAW_KIND for m in iter_messages())


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


def test_reader_4_the_markdown_inbox_lane_honors_it(env):
    """The lane the three bus filters missed. `write_new_thread` renders a
    markdown file that the project drain consumes without ever reading the bus,
    so a withdrawn message could still be triaged, woken on, and persisted after
    the sender was told it would not be delivered."""
    from fno.inbox.store import read_unread_threads, write_new_thread

    th = write_new_thread(
        recipient="proj", sender=MY_HANDLE, kind="fyi", body="retract me"
    )
    from fno.bus.log import Envelope, append

    append(
        Envelope.new(
            id=th.thread_id, from_=MY_HANDLE, to="proj", kind="send", body="retract me"
        )
    )
    assert [h.thread_id for h in read_unread_threads("proj")] == [th.thread_id]

    _run("mail", "withdraw", th.thread_id)

    assert read_unread_threads("proj") == []


def test_reader_4_fails_open_when_the_bus_is_unreadable(env):
    """Every other failure posture in the inbox errs toward never losing
    unprocessed mail. A withdrawal is not important enough to invert that, so an
    unreadable bus shows the thread rather than hiding it."""
    from fno.bus.log import bus_log_path
    from fno.inbox.store import read_unread_threads, write_new_thread

    th = write_new_thread(
        recipient="proj", sender=MY_HANDLE, kind="fyi", body="keep me"
    )
    bus_log_path().parent.mkdir(parents=True, exist_ok=True)
    bus_log_path().write_text("{not json at all\n", encoding="utf-8")

    assert [h.thread_id for h in read_unread_threads("proj")] == [th.thread_id]


def test_withdraw_accepts_the_label_the_send_path_stamped(env):
    """`stamp_from` returns an explicit `--from-name` label verbatim, so mail
    sent under one is owned by that label. Without a matching option the entire
    labelled send path had no listing and no retraction."""
    mid = _send("release-bot", PEER, "labelled", ts=_ts_ago(3600))

    assert _run("mail", "withdraw", mid).exit_code == 1  # ambient handle is not the owner

    res = _run("mail", "withdraw", mid, "--from-name", "release-bot")

    assert res.exit_code == 0
    rows = json.loads(_run("mail", "sent", "--from-name", "release-bot", "--json").stdout)
    assert rows == []


def test_withdraw_receipt_does_not_promise_an_outcome_it_cannot_verify(env):
    """The cursor read and the tombstone append are not atomic, so a drain in
    flight may already have delivered the body. The receipt names the boundary
    the tombstone guarantees instead of claiming the message was never seen."""
    mid = _send(MY_HANDLE, PEER, "retract me", ts=_ts_ago(3600))

    out = _run("mail", "withdraw", mid).stdout

    assert "no drain will deliver it from now on" in out
    assert "may have delivered it" in out


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


def test_a_typed_row_is_not_reported_as_claimed(env):
    """`typed` is the one state that must never imply the recipient consumed it.

    A typed row is excluded from the unclaimed scan the way a hosted row is, so
    it fell through to "claimed" and told the sender the recipient had read it.
    Bytes written into a PTY can be discarded by the prompt they land on, which
    is the whole reason this transport's receipt says `typed`, never
    `delivered`.
    """
    _send(MY_HANDLE, PEER, "ship it", ts=_ts_ago(60), delivery="typed")

    result = _run("agents", "mail", "sent")

    assert result.exit_code == 0, result.output
    # The JSON lane is the one machines read, so it carries the same answer.
    row = json.loads(result.output)[0]
    assert row["delivery"] == "typed"
    assert row["claimed"] is False
    # And it must distinguish "not claimed" from "can never be claimed". A typed
    # row is excluded from the unclaimed scan, withdraw refuses it, and the
    # recipient cannot drain it, so `claimed: false` alone made it identical to
    # an UNCLAIMED durable row that can still clear. This renderer and
    # `--unclaimed-only` disagreed about the same row.
    assert row["claimable"] is False


def test_withdrawing_a_typed_message_refuses_instead_of_pretending(env):
    """There is nothing to retract once the bytes are at a prompt.

    Withdraw guarded on `hosted` alone, so a forced pane message wrote its
    tombstone and printed success while retracting nothing. That is worse than
    refusing: the sender walks away believing the message is gone.
    """
    msg_id = _send(MY_HANDLE, PEER, "wrong ruling", ts=_ts_ago(60), delivery="typed")

    result = _run("agents", "mail", "withdraw", msg_id)

    assert result.exit_code == 1
    assert "cannot be recalled" in (result.stderr or result.output)
