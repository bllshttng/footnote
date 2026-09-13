"""The landed check (x-0c0b): proof a hosted row reached its recipient's
transcript, not just that injection reported success.

``fno agents mail sent`` used to report ``claimed: true`` on every hosted send,
because ``is_deliverable`` excluded hosted rows from the unclaimed scan
entirely -- there was no unclaimed set they could ever appear in. One message
reached its recipient only because the operator pressed ESC to interrupt a
busy loop; the row had already printed `claimed: true`.

Verify by marker, never by absence: a message is proven landed only when its
bare id appears in the recipient's own transcript. A read failure or a
self-send resolution must read as ``None``, never as a false ``False``
(AC3-ERR, AC9-ERR) -- an absence has three explanations (the real outcome,
"the instrument never ran", or a pipeline loss), and a caller that turns a
`None` into a `False` picks the wrong one.
"""
from __future__ import annotations

import json

from fno.agents import discover
from fno.bus.log import iter_messages, landed_ids, new_msg_id, record_hosted_delivery
from fno.mail.landed import _sent_unclaimed, landed_states, nag_line
from fno.paths_testing import use_tmpdir

SENDER_SESSION = "sender0001"
RECIPIENT_SESSION = "recipient01"


def _write_transcript(tmp_path, monkeypatch, session_id: str, body: str = "") -> None:
    """A fake claude transcript at the path `_transcript_path` resolves for
    ``session_id``, mirroring the existing `present_mail_ids` test fixture."""
    projects = tmp_path / "projects"
    proj = projects / "-Users-x-proj"
    proj.mkdir(parents=True, exist_ok=True)
    (proj / f"{session_id}.jsonl").write_text(body, encoding="utf-8")
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))


def _envelope_line(msg_id: str) -> str:
    record = {
        "type": "user",
        "message": {
            "content": f'<fno_mail from="alice" harness="claude" id="{msg_id}"> hi </fno_mail>',
        },
    }
    return json.dumps(record) + "\n"


def _send_hosted(
    *, to_session: str = RECIPIENT_SESSION, to_harness: str = "claude",
    from_session: str = SENDER_SESSION, recipient: str = "bob", ts: str | None = None,
) -> str:
    msg_id = new_msg_id()
    record_hosted_delivery(
        msg_id=msg_id, sender="alice", recipient=recipient, body="hi",
        from_session=from_session, to_session=to_session, to_harness=to_harness,
    )
    if ts is not None:
        # Envelopes are immutable once appended; rewrite the just-written line's
        # timestamp in place so age-bound tests can backdate a send without a
        # second bus writer.
        from fno.bus.log import bus_log_path

        path = bus_log_path()
        lines = path.read_text(encoding="utf-8").splitlines()
        obj = json.loads(lines[-1])
        obj["ts"] = ts
        lines[-1] = json.dumps(obj)
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return msg_id


def test_ac1_landed_true_when_id_is_in_recipient_transcript(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    mid = _send_hosted()
    _write_transcript(tmp_path, monkeypatch, RECIPIENT_SESSION, _envelope_line(mid))

    all_msgs = list(iter_messages())
    states = landed_states(all_msgs, all_msgs)

    assert states[mid] is True


def test_ac2_landed_false_then_flips_true_once_the_id_appears(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    mid = _send_hosted()
    _write_transcript(tmp_path, monkeypatch, RECIPIENT_SESSION, "")

    all_msgs = list(iter_messages())
    assert landed_states(all_msgs, all_msgs)[mid] is False

    _write_transcript(tmp_path, monkeypatch, RECIPIENT_SESSION, _envelope_line(mid))
    all_msgs = list(iter_messages())
    assert landed_states(all_msgs, all_msgs)[mid] is True


def test_ac3_unresolvable_transcript_reads_null_never_false(tmp_path, monkeypatch):
    """A read failure is not evidence of absence: no transcript file at all
    (a session with no store record) must never masquerade as a proven miss."""
    use_tmpdir(monkeypatch, tmp_path)
    mid = _send_hosted()
    projects = tmp_path / "projects"
    projects.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))

    all_msgs = list(iter_messages())

    assert landed_states(all_msgs, all_msgs)[mid] is None


def test_ac5_landed_proof_survives_transcript_rotation(tmp_path, monkeypatch):
    """Once landed, the proof is a durable bus row, not a re-grep: a transcript
    that later rotates away must not un-flip an already-proven message."""
    from datetime import datetime, timedelta, timezone

    use_tmpdir(monkeypatch, tmp_path)
    old_ts = (datetime.now(tz=timezone.utc) - timedelta(minutes=5)).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    mid = _send_hosted(ts=old_ts)
    _write_transcript(tmp_path, monkeypatch, RECIPIENT_SESSION, _envelope_line(mid))

    outstanding = _sent_unclaimed("alice", ttl_seconds=0)
    assert outstanding == []  # landed at first sighting; recorded, not returned

    all_msgs = list(iter_messages())
    assert mid in landed_ids(all_msgs)

    # The transcript is gone now (rotated away, session dead); the durable
    # proof must still hold.
    _write_transcript(tmp_path, monkeypatch, RECIPIENT_SESSION, "")
    all_msgs = list(iter_messages())
    assert landed_states(all_msgs, all_msgs)[mid] is True


def test_ac9_self_send_refuses_the_resolution(tmp_path, monkeypatch):
    """A hosted row whose recipient session IS the sender's own must refuse the
    grep: the send's own output already put the id in that transcript, which
    proves nothing about cross-session delivery."""
    use_tmpdir(monkeypatch, tmp_path)
    mid = _send_hosted(to_session=SENDER_SESSION, from_session=SENDER_SESSION)
    _write_transcript(tmp_path, monkeypatch, SENDER_SESSION, _envelope_line(mid))

    all_msgs = list(iter_messages())

    assert landed_states(all_msgs, all_msgs)[mid] is None


def test_ac6_nag_is_silent_once_everything_has_landed():
    assert nag_line([]) is None


def test_ac7_nag_names_at_most_three_oldest_and_esc():
    class _Row:
        def __init__(self, to, ts):
            self.to = to
            self.ts = ts

    rows = [_Row(f"peer{i}", "2020-01-01T00:00:00Z") for i in range(4)]
    line = nag_line(rows)

    assert line is not None
    assert "peer0" in line and "peer1" in line and "peer2" in line
    assert "peer3" not in line
    assert "+1 more" in line
    assert "ESC" in line


def test_ac8_abandoned_message_drops_off_the_scan_entirely(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    _send_hosted(ts="2000-01-01T00:00:00Z")  # far past any abandon TTL

    outstanding = _sent_unclaimed("alice", ttl_seconds=0)

    assert outstanding == []


def test_is_deliverable_refuses_a_landed_ack(tmp_path, monkeypatch):
    """A landed row is a receipt about another message, not inbox content, so
    the deliverability test must refuse it (x-22ce). The send row is the
    positive control, asserted in the same test: 151 of the 2378 live send rows
    carry no `delivery` field, and a predicate that returned False for
    everything would pass without it."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.bus.log import Envelope, is_deliverable, record_landed

    send = Envelope.new(from_="worker", to="king", kind="send", body="status")
    ack = record_landed(msg_id=send.id, sender="worker", recipient="king")

    assert is_deliverable(ack) is False
    assert is_deliverable(send) is True


def _durable_send_and_ack(*, to: str = "king") -> str:
    """One durable `send` row plus the `landed` row acknowledging it: the exact
    pair the field incident rendered as a blank message (x-22ce)."""
    from fno.bus.log import Envelope, append, record_landed

    send = Envelope.new(from_="worker", to=to, kind="send", body="status report")
    append(send)
    record_landed(msg_id=send.id, sender="worker", recipient=to)
    return send.id


def test_reader_1_scan_unread_shows_the_message_never_the_ack(
    tmp_path, monkeypatch
):
    """The inbox choke point: seven readers drain through scan_unread. The
    equality names the surviving id, kind and body; a merely-shorter list would
    also pass for a reader that hid everything."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.bus.cursor import scan_unread

    send_id = _durable_send_and_ack()

    unread = scan_unread("king")

    assert [(m.id, m.kind, m.body) for m in unread] == [
        (send_id, "send", "status report")
    ]


def test_reader_1_unread_cli_reports_one_real_message(
    tmp_path, monkeypatch
):
    """End to end through the verb a king actually ran during the incident:
    `mail unread -n king --json` must show one message with a body, never the
    seven empty rows the old reader produced."""
    use_tmpdir(monkeypatch, tmp_path)
    import json as _json

    from typer.testing import CliRunner

    from fno.cli import app

    send_id = _durable_send_and_ack()

    res = CliRunner().invoke(app, ["mail", "unread", "-n", "king", "--json"])

    assert res.exit_code == 0
    rows = _json.loads(res.stdout)
    assert [(r["id"], r["kind"], r["body"]) for r in rows] == [
        (send_id, "send", "status report")
    ]


def test_reader_4_markdown_render_shows_the_message_never_the_ack(
    tmp_path, monkeypatch
):
    """The markdown inbox shares `is_deliverable` with scan_unread, so the one
    predicate change must reach it too: exactly one thread file, carrying the
    sent body."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.inbox.store import inbox_dir_for, rebuild_render

    send_id = _durable_send_and_ack()

    written = rebuild_render("king")
    inbox = inbox_dir_for("king")
    text = "\n".join(
        p.read_text(encoding="utf-8") for p in sorted(inbox.glob("*.md"))
    )

    assert written == 1
    assert len(list(inbox.glob("*.md"))) == 1
    assert send_id in text
    assert "status report" in text


def test_cmd_ack_names_a_landed_row_a_receipt(tmp_path, monkeypatch):
    """After the kind filter, acking a landed id falls into the refusal branch
    where it used to read "already delivered (hosted)": false about a row that
    is the delivery proof, not the mail. The refusal must name the receipt and
    the id it acknowledges (x-22ce)."""
    use_tmpdir(monkeypatch, tmp_path)
    import json as _json

    from typer.testing import CliRunner

    from fno.bus.cursor import read_cursor
    from fno.bus.log import Envelope, append, record_landed
    from fno.cli import app

    send = Envelope.new(from_="worker", to="king", kind="send", body="status report")
    append(send)
    ack = record_landed(msg_id=send.id, sender="worker", recipient="king")

    res = CliRunner().invoke(app, ["mail", "ack", ack.id, "--name", "king"])

    assert res.exit_code == 2
    assert "landed" in res.stderr
    assert send.id in res.stderr
    assert read_cursor("king") is None

    # Positive control: a real durable send row acks cleanly and moves the
    # cursor, proving the new branch did not swallow the ack path.
    ok = CliRunner().invoke(app, ["mail", "ack", send.id, "--name", "king"])

    assert ok.exit_code == 0
    assert read_cursor("king") == send.id
    assert _json.loads(
        CliRunner().invoke(app, ["mail", "unread", "-n", "king", "--json"]).stdout
    ) == []
