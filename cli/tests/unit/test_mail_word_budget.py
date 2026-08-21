"""Rolling sender-recipient word budget (x-3700 Wave 2, ruling d-0ac789e6).

Every assertion here is a positive marker. No criterion passes because an id or
a body is merely absent, because an absence has two explanations and one of them
is that the instrument never ran.
"""
from __future__ import annotations

import json
import time

import pytest

from fno import style
from fno.mail import budget


@pytest.fixture(autouse=True)
def isolated_bus(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_BUS_DIR", str(tmp_path / "bus"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    yield


def words(n: int) -> str:
    """A body of exactly ``n`` masked words, verified through Rule 7's counter."""
    text = " ".join(f"word{i}" for i in range(n))
    assert style.word_count(text) == n
    return text


def send(sender: str, recipient: str, n: int, msg_id: str, *, enforce: bool = True):
    return budget.reserve(
        sender=sender,
        recipient=recipient,
        words=style.word_count(words(n)),
        msg_id=msg_id,
        enforce=enforce,
    )


# --- AC2-EDGE: one count, three callers -----------------------------------

def test_rule_seven_and_budget_share_one_count():
    body = "Ship the fix. See `cli/src/fno/mail/budget.py` and --flag now."
    count = style.word_count(body)
    # Rule 7 reports the same number in its own violation detail.
    long_body = " ".join([body] * 40)
    violations = style.check(long_body, surface="mail")
    seven = [v for v in violations if v.rule == 7]
    assert seven, "rule 7 must fire on a body over the cap"
    assert str(style.word_count(long_body)) in seven[0].detail


def test_masking_holds_for_the_budget():
    # A pasted log masks to near nothing; the cap covers prose, not a dump.
    assert style.word_count("```\n" + "\n".join(str(i) for i in range(200)) + "\n```") == 0


# --- AC2-HP: the rolling pair budget blocks a burst ------------------------

def test_burst_of_three_79_word_sends_is_refused():
    first = send("a", "b", 79, "msg-001")
    assert first.running_before == 0

    with pytest.raises(budget.BudgetRefused) as exc:
        send("a", "b", 79, "msg-002")
    assert exc.value.marker() == (
        "running=79 current=79 projected=158 cap=80 window=10m"
    )

    with pytest.raises(budget.BudgetRefused) as exc2:
        send("a", "b", 79, "msg-003")
    assert exc2.value.running == 79, "a refused attempt is never charged"


def test_exactly_the_cap_is_allowed():
    send("a", "b", 80, "msg-cap")
    with pytest.raises(budget.BudgetRefused):
        send("a", "b", 1, "msg-over")


# --- AC2-PAIR: accounting does not leak across identities ------------------

def test_each_pair_keeps_an_independent_total():
    send("a", "b", 79, "msg-ab")
    send("a", "c", 79, "msg-ac")  # different recipient: own window
    send("z", "b", 79, "msg-zb")  # different sender: own window
    with pytest.raises(budget.BudgetRefused):
        send("a", "b", 79, "msg-ab2")


def test_reservation_names_the_canonical_pair():
    res = send("canon-sender", "canon-recipient", 10, "msg-pair")
    assert res.pair == "canon-sender -> canon-recipient"
    path = budget._ledger_path(res.pair)
    stored = json.loads(path.read_text())
    assert stored["pair"] == "canon-sender -> canon-recipient"


# --- AC2-RESET: an inbound message starts a new conversation budget --------

def _inbound(sender: str, recipient: str, msg_id: str) -> str:
    """Write one bus envelope FROM the recipient TO the sender."""
    from fno.bus.log import Envelope, append

    append(
        Envelope.new(
            id=msg_id,
            from_=recipient,
            to=sender,
            kind="send",
            body="ok",
            word_count=1,
        )
    )
    return msg_id


def test_inbound_reply_resets_the_running_total():
    send("a", "b", 79, "msg-out1")
    time.sleep(1.1)  # bus ts has second resolution
    reset_id = _inbound("a", "b", "msg-in1")
    time.sleep(1.1)
    second = send("a", "b", 79, "msg-out2")
    assert second.running_before == 0
    assert second.reset_by == reset_id


def test_reset_requires_the_exact_pair():
    send("a", "b", 79, "msg-out1")
    time.sleep(1.1)
    _inbound("a", "other", "msg-in-wrong")  # from "other", not from "b"
    time.sleep(1.1)
    with pytest.raises(budget.BudgetRefused):
        send("a", "b", 79, "msg-out2")


# --- AC2-CONCURRENCY: two sends cannot both pass stale history -------------

def test_concurrent_sends_serialize_on_the_pair_ledger():
    import threading

    results: list = []

    def attempt(msg_id: str) -> None:
        try:
            results.append(("ok", send("a", "b", 50, msg_id)))
        except budget.BudgetRefused as exc:
            results.append(("refused", exc))

    threads = [threading.Thread(target=attempt, args=(f"msg-c{i}",)) for i in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    kinds = sorted(k for k, _ in results)
    assert kinds == ["ok", "refused"]
    refusal = next(v for k, v in results if k == "refused")
    assert refusal.marker() == (
        "running=50 current=50 projected=100 cap=80 window=10m"
    )
    stored = json.loads(budget._ledger_path("a -> b").read_text())
    assert len(stored["entries"]) == 1, "exactly one live reservation for the pair"


# --- AC2-EXCEPTION: an explicit exception does not erase history -----------

def test_exception_permits_the_send_and_still_charges_it():
    res = send("a", "b", 100, "msg-exempt", enforce=False)
    assert res.words == 100
    with pytest.raises(budget.BudgetRefused) as exc:
        send("a", "b", 1, "msg-after")
    assert exc.value.running == 100, "the exempt send stays charged"


# --- window expiry and release --------------------------------------------

def test_entries_older_than_the_window_are_pruned():
    res = send("a", "b", 79, "msg-old")
    path = budget._ledger_path(res.pair)
    stored = json.loads(path.read_text())
    stored["entries"][0]["ts"] = time.time() - budget.WINDOW_SECONDS - 1
    path.write_text(json.dumps(stored))
    fresh = send("a", "b", 79, "msg-new")
    assert fresh.running_before == 0


def test_release_gives_back_a_proven_non_delivery():
    res = send("a", "b", 79, "msg-fail")
    budget.release(res)
    again = send("a", "b", 79, "msg-retry")
    assert again.running_before == 0


def test_an_empty_ledger_file_is_removed():
    res = send("a", "b", 10, "msg-solo")
    budget.release(res)
    assert not budget._ledger_path(res.pair).exists()


# --- fail closed ----------------------------------------------------------

def test_a_malformed_active_ledger_refuses_rather_than_resetting():
    res = send("a", "b", 79, "msg-one")
    path = budget._ledger_path(res.pair)
    path.write_text("{ not json")
    with pytest.raises(budget.BudgetUnavailable) as exc:
        send("a", "b", 1, "msg-two")
    assert "a -> b" in str(exc.value)
    assert str(path) in str(exc.value), "the refusal names the recovery path"


# --- legacy rows ----------------------------------------------------------

def test_a_legacy_row_reads_back_without_a_fabricated_count():
    from fno.bus.log import from_json_line, to_json_line, Envelope

    legacy = '{"v":1,"id":"msg-legacy","ts":"2026-01-01T00:00:00Z","thread":"msg-legacy",'\
             '"from":"a","to":"b","kind":"send","body":"hello there"}'
    env = from_json_line(legacy)
    assert env.word_count is None
    assert "word_count" not in to_json_line(env)

    counted = Envelope.new(from_="a", to="b", kind="send", body="hello there", word_count=2)
    assert json.loads(to_json_line(counted))["word_count"] == 2

    zero = Envelope.new(from_="a", to="b", kind="send", body="", word_count=0)
    assert json.loads(to_json_line(zero))["word_count"] == 0, "a real zero is not legacy"
