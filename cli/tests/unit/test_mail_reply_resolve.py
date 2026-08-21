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


def test_to_requirement_rejects_a_forwarded_envelope():
    # A forward quotes the original envelope verbatim, so the id IS present in a
    # session the message was never sent to. Matching on the id alone reads that
    # quote as a receipt and answers in the wrong session.
    text = '<fno_mail from="aaaa1111" to="cccc3333" id="msg-fwd"> quoted'
    assert sender_from_transcript_text(text, "msg-fwd", to="bbbb2222") is None


def test_to_requirement_accepts_the_envelope_addressed_here():
    text = '<fno_mail from="aaaa1111" to="bbbb2222" id="msg-fwd"> mine'
    assert sender_from_transcript_text(text, "msg-fwd", to="bbbb2222") == "aaaa1111"


def test_resolve_live_sender_picks_the_store_holding_the_receipt(tmp_path, monkeypatch):
    # The defect this fixes: a claude session launched under codex carries BOTH
    # families' markers. resolve_harness_identity picks codex by precedence, so
    # the resolver read a stranger's rollout as this session's transcript and
    # every received message refused with "not in the bus log or this session's
    # transcript" -- while the envelope sat in the claude transcript all along.
    #
    # The codex candidate is searched FIRST here, matching the real marker order
    # that produced the bug, and its store carries an envelope with the SAME id.
    # Only `to=` separates them: without it this returns the foreign sender.
    import fno.harness_identity as hi
    import fno.mail.reply_resolve as rr

    codex_id = "01a02125-4eb4-7bf1-b74e-d238887eb092"
    claude_id = "b30d467b-ef4c-4780-8500-7084c9087f07"

    foreign = tmp_path / "codex.jsonl"
    foreign.write_text('<fno_mail from="ffff9999" to="cafebabe" id="msg-live"> x')
    own = tmp_path / "claude.jsonl"
    own.write_text('<fno_mail from="0ae85ed8" to="b30d467b" id="msg-live"> x')

    monkeypatch.setattr(
        hi,
        "present_harness_markers",
        lambda *a, **k: (
            ("CODEX_THREAD_ID", "codex", codex_id),
            ("CLAUDE_CODE_SESSION_ID", "claude", claude_id),
            ("CODEX_SESSION_ID", "codex", codex_id),
        ),
    )
    monkeypatch.setattr(
        rr,
        "_transcript_path",
        lambda harness, session_id: foreign if harness == "codex" else own,
    )

    assert rr.resolve_live_sender("msg-live") == "0ae85ed8"


def test_resolve_live_sender_skips_an_unreadable_store(tmp_path, monkeypatch):
    # An unreadable candidate must not end the search: the store that matters
    # can be the one after it. Aborting on the first OSError reproduces the
    # original symptom for a different reason.
    import fno.harness_identity as hi
    import fno.mail.reply_resolve as rr

    own = tmp_path / "claude.jsonl"
    own.write_text('<fno_mail from="0ae85ed8" to="b30d467b" id="msg-live"> x')

    monkeypatch.setattr(
        hi,
        "present_harness_markers",
        lambda *a, **k: (
            ("CODEX_SESSION_ID", "codex", "01a02125-4eb4-7bf1-b74e-d238887eb092"),
            ("CLAUDE_CODE_SESSION_ID", "claude", "b30d467b-ef4c-4780-8500-7084c9087f07"),
        ),
    )
    monkeypatch.setattr(
        rr,
        "_transcript_path",
        lambda harness, session_id: (tmp_path / "gone.jsonl") if harness == "codex" else own,
    )

    assert rr.resolve_live_sender("msg-live") == "0ae85ed8"
