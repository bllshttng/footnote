"""demotion_receipt renders the registered lane's transcript-veto reasons."""

from fno.mail.receipts import demotion_receipt


def test_transcript_reason_carries_the_age_suffix() -> None:
    """AC2-ERR: a transcript- reason rides the age suffix; an unreadable
    transcript reads unknown, never 0s."""
    receipt = demotion_receipt(
        "msg-1", reason="transcript-done", owner=None, age_target="nobody-here"
    )
    assert receipt == "msg-1 queued (durable) [transcript-done, transcript age unknown]"
    assert "0s" not in receipt


def test_bare_live_miss_keeps_its_suffix() -> None:
    """The original live-miss suffix behavior is unchanged."""
    receipt = demotion_receipt(
        "msg-1", reason=None, owner=None, age_target="nobody-here"
    )
    assert receipt == "msg-1 queued (durable) [live-miss, transcript age unknown]"
