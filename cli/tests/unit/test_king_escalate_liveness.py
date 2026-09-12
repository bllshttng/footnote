"""escalate's closing question reads the caller's liveness instead of asserting it.

Each branch asserts a POSITIVE marker in the recorded text (the live ruling
ask vs the dead crown ask), never the absence of the other branch: the original
defect was exactly a sentence asserted without reading, so a test that only
checks "does not contain X" would re-commit the absence-lie in miniature.

Law d-59af3235 capped the text: the branches are now the closing QUESTION, and
an unknown liveness reads as dead with a named "(liveness unreadable)" marker
while the full reason moves to the caller's stderr line.
"""
from __future__ import annotations

import pytest

from fno.king.escalate import MARKER, dedupe_key, question_text

IDS = ["x-1111", "x-2222"]
KEY = dedupe_key(IDS)
REASON = "NoProgress"

_LIVE_CLOSE = "Unblock, defer, or stand it down?"
_DEAD_CLOSE = "Unblock, defer, or crown a new king?"


def test_live_king_question_asks_for_a_ruling() -> None:
    text = question_text(IDS, KEY, REASON, live=True)
    assert _LIVE_CLOSE in text
    # AC26: assert presence, not absence. The guard below only proves the
    # branch input was honored, not that the dead sentence is gone.
    assert _DEAD_CLOSE not in text


def test_dead_king_question_offers_the_crown() -> None:
    text = question_text(IDS, KEY, REASON, live=False)
    assert _DEAD_CLOSE in text
    # A measured dead is not an unreadable read: the unreadable marker is
    # reserved for the None branch, where nothing was measured.
    assert "liveness unreadable" not in text


def test_unknown_king_reads_dead_and_names_the_read_failed() -> None:
    text = question_text(
        IDS, KEY, REASON, live=None, unknown_reason="registry unreadable: disk"
    )
    # Unknown reads as dead (under-claiming is safe). The full reason is
    # deliberately NOT in the text - the ask gate caps the line, and the
    # caller (king cli) echoes it on stderr - but the text names that the
    # read failed so a dead-looking king is never read as measured-dead.
    assert _DEAD_CLOSE in text
    assert "liveness unreadable" in text
    assert "registry unreadable: disk" not in text


def test_default_live_argument_reads_unknown() -> None:
    """Callers that pass nothing (older arms) under-claim: unknown, not dead."""
    text = question_text(IDS, KEY, REASON)
    assert _DEAD_CLOSE in text
    assert "liveness unreadable" in text


def test_marker_still_leads_and_dedupe_key_ignores_liveness() -> None:
    live_text = question_text(IDS, KEY, REASON, live=True)
    dead_text = question_text(IDS, KEY, REASON, live=False)
    for text in (live_text, dead_text):
        assert text.startswith(f"[{MARKER}:{KEY}]")
    # Same stalled set, one question: the liveness branch must not fork the
    # dedupe key, or a live king and its dead successor double-file.
    assert f"[{MARKER}:{KEY}]" in live_text
    assert dedupe_key(IDS) == dedupe_key(list(reversed(IDS)))


@pytest.mark.parametrize("live", [True, False, None])
def test_every_branch_carries_the_marker_and_reason(live: bool) -> None:
    text = question_text(IDS, KEY, REASON, live=live)
    assert f"[{MARKER}:{KEY}]" in text
    assert f"Reason: {REASON}" in text
