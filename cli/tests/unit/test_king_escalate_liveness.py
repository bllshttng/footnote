"""escalate's closing sentence reads the caller's liveness instead of asserting it.

Each branch asserts a POSITIVE marker in the recorded text ("still reigning" /
"It has exited"), never the absence of the other sentence: the original defect
was exactly a sentence asserted without reading, so a test that only checks
"does not contain X" would re-commit the absence-lie in miniature.
"""
from __future__ import annotations

import pytest

from fno.king.escalate import MARKER, dedupe_key, question_text

IDS = ["x-1111", "x-2222"]
KEY = dedupe_key(IDS)
REASON = "NoProgress"

_DEAD_SENTENCE = "It has exited, so nothing restarts it on its own"


def test_live_king_question_says_still_reigning() -> None:
    text = question_text(IDS, KEY, REASON, live=True)
    assert "still reigning" in text
    assert "stand down" in text
    # AC26: assert presence, not absence. The guard below only proves the
    # branch input was honored, not that the dead sentence is gone.
    assert _DEAD_SENTENCE not in text


def test_dead_king_question_keeps_todays_text_verbatim() -> None:
    text = question_text(IDS, KEY, REASON, live=False)
    assert _DEAD_SENTENCE in text
    assert "crown a new king" in text
    assert "liveness unreadable" not in text


def test_unknown_king_reads_dead_and_names_the_reason() -> None:
    text = question_text(
        IDS, KEY, REASON, live=None, unknown_reason="registry unreadable: disk"
    )
    # Unknown reads as dead (under-claiming is safe), and the reason is not
    # silently dropped.
    assert _DEAD_SENTENCE in text
    assert "liveness unreadable" in text
    assert "registry unreadable: disk" in text


def test_default_live_argument_stays_the_dead_sentence() -> None:
    """Callers that pass nothing (older arms) keep today's recorded text."""
    text = question_text(IDS, KEY, REASON)
    assert _DEAD_SENTENCE in text


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
    assert f"Reason given: {REASON}" in text
