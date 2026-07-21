"""US3: recover a live-injected message's sender from the transcript envelope."""
from __future__ import annotations

from fno.mail.reply_resolve import sender_from_transcript_text


def test_extracts_from_of_matching_id_json_escaped():
    # The envelope lives inside a JSONL record, so its quotes arrive escaped.
    line = (
        '{"type":"user","message":{"role":"user","content":'
        '"<fno_mail from=\\"9a063cd3\\" harness=\\"claude-code\\" model=\\"opus\\" '
        'id=\\"msg-live1\\">\\nping\\n</fno_mail>"}}'
    )
    assert sender_from_transcript_text(line, "msg-live1") == "9a063cd3"


def test_extracts_from_of_matching_id_unescaped():
    text = '<fno_mail from="deadbeef" harness="codex" model="gpt" id="msg-xyz"> hi'
    assert sender_from_transcript_text(text, "msg-xyz") == "deadbeef"


def test_absent_id_returns_none():
    text = '<fno_mail from="deadbeef" id="msg-other"> hi'
    assert sender_from_transcript_text(text, "msg-live1") is None


def test_empty_text_returns_none():
    assert sender_from_transcript_text("", "msg-live1") is None


def test_picks_the_envelope_carrying_the_id_not_a_neighbor():
    # Two envelopes in the stream; only the one with the target id is answered.
    text = (
        '<fno_mail from="aaaa1111" id="msg-a"> first\n'
        '<fno_mail from="bbbb2222" id="msg-b"> second'
    )
    assert sender_from_transcript_text(text, "msg-b") == "bbbb2222"
