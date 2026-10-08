"""demotion_receipt renders the registered lane's transcript-veto reasons."""

import json

from fno.mail.receipts import demotion_receipt, json_receipt


def _status(receipt: str) -> dict:
    row = json.loads(receipt)
    assert set(row) == {"msg_id", "subject", "to", "status"}
    return row


def test_demotion_receipt_carries_the_age_suffix() -> None:
    """AC2-ERR: a transcript- reason rides the age suffix; an unreadable
    transcript reads unknown, never 0s. The bare live-miss keeps the same
    suffix behavior."""
    receipt = demotion_receipt(
        "msg-1", reason="transcript-done", owner=None, age_target="nobody-here"
    )
    row = _status(receipt)
    assert row["msg_id"] == "msg-1"
    assert "transcript-done, transcript age unknown" in row["status"]
    assert "0s" not in receipt

    receipt = demotion_receipt(
        "msg-1", reason=None, owner=None, age_target="nobody-here"
    )
    row = _status(receipt)
    assert "live-miss, transcript age unknown" in row["status"]
