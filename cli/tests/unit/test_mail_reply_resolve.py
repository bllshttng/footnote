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


def test_does_not_bind_resolve_harness_identity_at_module_scope():
    # Regression: this module is imported lazily (from inside cmd_drain_self),
    # so a module-level `from fno.harness_identity import resolve_harness_identity`
    # binds whatever that name pointed to at THIS module's first import --
    # permanently, since Python caches modules in sys.modules. A caller that
    # monkeypatches harness_identity.resolve_harness_identity around that first
    # import (e.g. cmd_drain_self via test_mail_drain_trailer.py) then poisons
    # every later caller in the same worker process: monkeypatch's teardown
    # only reverts the attribute on fno.harness_identity, never a separate name
    # binding this module captured from it. Import it locally per call instead.
    import fno.mail.reply_resolve as reply_resolve

    assert not hasattr(reply_resolve, "resolve_harness_identity")
