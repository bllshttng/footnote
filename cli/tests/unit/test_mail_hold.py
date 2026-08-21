"""Busy mode: the hold, its clock, the release, and what each one proves (x-481e).

Every assertion here is a POSITIVE marker. None of them assert that nothing was
injected during a hold, because a working hold and a dead bus produce the same
absence and a test that cannot separate them is not an instrument.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone
from types import SimpleNamespace

import pytest

from fno.agents import dispatch, format as fmt
from fno.mail import hold as hold_mod

HANDLE = "abcd1234"


@pytest.fixture(autouse=True)
def _isolated_state(tmp_path, monkeypatch):
    """Point the clock directory at a tmp state root, not the real ~/.fno."""
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    return tmp_path


def _entry(**over):
    base = dict(
        name=HANDLE,
        short_id="",
        harness_session_id=f"{HANDLE}-full",
        delivery_policy="bus-only",
    )
    base.update(over)
    return SimpleNamespace(**base)


def _msg(msg_id, sender, body, ts="2026-08-20T10:00:00Z"):
    return SimpleNamespace(id=msg_id, from_=sender, body=body, ts=ts)


def _expire(handle):
    """Age a live timed hold past its deadline without deleting its clock."""
    return hold_mod._write(
        hold_mod.Hold(
            handle=handle,
            until=datetime.now(timezone.utc) - timedelta(seconds=1),
            window_s=300,
        )
    )


# --- Task 1: the hold and its clock -----------------------------------------


def test_arm_writes_a_readable_clock_and_clear_removes_it():
    armed = hold_mod.arm(HANDLE, 5)
    assert armed.window_s == 300
    read_back = hold_mod.read(HANDLE)
    assert read_back is not None
    assert read_back.until is not None
    assert hold_mod.remaining_label(HANDLE).endswith("m")

    hold_mod.clear(HANDLE)
    assert hold_mod.read(HANDLE) is None


def test_a_permanent_policy_renders_as_held_with_no_countdown():
    hold_mod.arm_permanent(HANDLE)
    assert hold_mod.read(HANDLE).until is None
    assert hold_mod.remaining_label(HANDLE) == "held"


# --- Task 2: auto-expire, on BOTH branches of the gate ----------------------


def test_a_flag_with_no_clock_is_not_a_hold_and_never_lapses():
    """A bus-only row with no clock predates busy mode, so it keeps its policy.

    Lifting it would silently revoke the no-paste guarantee of every row
    stamped by `fno agents register --delivery-policy bus-only`, which has no
    clock by construction. The hold-forever fear that argues for the opposite
    does not apply here: held mail is durable on the bus and surfaces at the
    next turn boundary, so a lost clock costs a stall, never a message.
    """
    assert hold_mod.lapsed(HANDLE) is False


def test_a_permanent_policy_never_lapses():
    hold_mod.arm_permanent(HANDLE)
    assert hold_mod.lapsed(HANDLE) is False


def test_a_live_hold_does_not_lapse_and_an_expired_one_does():
    hold_mod.arm(HANDLE, 5)
    assert hold_mod.lapsed(HANDLE) is False

    hold_mod._write(
        hold_mod.Hold(
            handle=HANDLE,
            until=datetime.now(timezone.utc) - timedelta(seconds=1),
            window_s=300,
        )
    )
    assert hold_mod.lapsed(HANDLE) is True


def test_a_corrupt_clock_reads_as_no_clock_and_keeps_holding():
    """An unreadable clock is not evidence the hold ended, so the flag stands.

    The hold then lifts at the recipient's next turn boundary (notify-self
    tidies it) or on `fno mail hold --off`. A stall, and a bounded one.
    """
    hold_mod.arm(HANDLE, 5)
    hold_mod.hold_path(HANDLE).write_text("{not json", encoding="utf-8")
    assert hold_mod.read(HANDLE) is None
    assert hold_mod.lapsed(HANDLE) is False


def test_gate_entry_branch_refuses_a_live_hold_and_lifts_an_expired_one():
    """The branch a caller holding an AgentEntry takes (dispatch.py:576, :5741, :6773).

    It returns before ``load_registry`` is ever called, so a self-heal wired
    only into the registry loop below would be decorative here.
    """
    hold_mod.arm(HANDLE, 5)
    assert dispatch._delivery_policy_refusal(_entry()) == dispatch.BUS_ONLY_POLICY

    _expire(HANDLE)
    assert dispatch._delivery_policy_refusal(_entry()) is None


def test_gate_token_branch_refuses_a_live_hold_and_lifts_an_expired_one(monkeypatch):
    """The other branch: a caller holding only an id/handle token."""
    monkeypatch.setattr(dispatch, "load_registry", lambda: [_entry()])

    hold_mod.arm(HANDLE, 5)
    assert dispatch._delivery_policy_refusal(HANDLE) == dispatch.BUS_ONLY_POLICY

    _expire(HANDLE)
    assert dispatch._delivery_policy_refusal(HANDLE) is None


def test_gate_leaves_a_clockless_bus_only_row_refusing_on_both_branches(monkeypatch):
    """The x-e21e guarantee, unchanged for every row busy mode never touched."""
    monkeypatch.setattr(dispatch, "load_registry", lambda: [_entry()])

    assert dispatch._delivery_policy_refusal(_entry()) == dispatch.BUS_ONLY_POLICY
    assert dispatch._delivery_policy_refusal(HANDLE) == dispatch.BUS_ONLY_POLICY


def test_extend_pushes_a_live_hold_out_and_refuses_everything_else():
    hold_mod.arm(HANDLE, 5)
    first = hold_mod.read(HANDLE).until
    extended = hold_mod.extend(HANDLE)
    assert extended is not None and extended.until >= first

    hold_mod.arm_permanent(HANDLE)
    assert hold_mod.extend(HANDLE) is None

    hold_mod.clear(HANDLE)
    assert hold_mod.extend(HANDLE) is None


def test_tidy_lapsed_clears_a_timed_hold_but_never_a_permanent_policy(monkeypatch):
    cleared = []
    monkeypatch.setattr(
        hold_mod, "set_policy", lambda handle, policy: cleared.append((handle, policy))
    )

    hold_mod.arm_permanent(HANDLE)
    assert hold_mod.tidy_lapsed(HANDLE) is False
    assert hold_mod.read(HANDLE) is not None

    hold_mod._write(
        hold_mod.Hold(
            handle=HANDLE,
            until=datetime.now(timezone.utc) - timedelta(seconds=1),
            window_s=60,
        )
    )
    assert hold_mod.tidy_lapsed(HANDLE) is True
    assert hold_mod.read(HANDLE) is None
    assert cleared == [(HANDLE, None)]


# --- Task 4: dedupe ---------------------------------------------------------


def test_five_identical_bodies_render_once_and_still_carry_all_five_ids():
    messages = [_msg(f"msg-{i}", "worker", "same report") for i in range(5)]
    survivors = hold_mod.dedupe(messages)

    assert len(survivors) == 1
    _representative, count, ids = survivors[0]
    assert count == 5
    assert ids == [f"msg-{i}" for i in range(5)]

    digest = hold_mod.render_digest(HANDLE, survivors, held_for_s=600)
    assert "(x5 identical, deduped)" in digest
    assert digest.count("same report") == 1


def test_same_body_from_two_senders_is_not_deduped():
    survivors = hold_mod.dedupe(
        [_msg("a", "one", "ping"), _msg("b", "two", "ping")]
    )
    assert len(survivors) == 2


# --- Task 3: the release ----------------------------------------------------


def _capture_release(monkeypatch, messages, delivered=True):
    emitted = []
    advanced = []
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    monkeypatch.setattr("fno.bus.cursor.scan_unread", lambda *a, **k: messages)
    monkeypatch.setattr(
        "fno.bus.cursor.advance_cursor", lambda name, mid: advanced.append(mid)
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude", lambda *a, **k: delivered
    )
    monkeypatch.setattr(
        "fno.agents.events.emit",
        lambda kind, **data: emitted.append((kind, data)),
    )
    return emitted, advanced


def test_release_delivers_the_digest_and_consumes_every_held_id(monkeypatch):
    messages = [_msg(f"msg-{i}", "worker", "same report") for i in range(3)]
    emitted, advanced = _capture_release(monkeypatch, messages)

    result = hold_mod.release(HANDLE, held_for_s=300)

    assert result["outcome"] == "delivered"
    assert result["held_count"] == 3
    assert result["deduped_count"] == 2
    assert advanced == ["msg-0", "msg-1", "msg-2"]
    assert emitted == [
        (
            "mail_hold_released",
            {
                "handle": HANDLE,
                "held_count": 3,
                "deduped_count": 2,
                "held_for_s": 300,
                "outcome": "delivered",
            },
        )
    ]


def test_release_fires_its_marker_even_when_nothing_was_held(monkeypatch):
    """A release event that fires only on a non-empty digest cannot tell a
    working expiry from a dead timer."""
    emitted, advanced = _capture_release(monkeypatch, [])

    result = hold_mod.release(HANDLE, held_for_s=300)

    assert result["outcome"] == "empty"
    assert emitted[0][0] == "mail_hold_released"
    assert emitted[0][1]["held_count"] == 0
    assert advanced == []


def test_a_missed_inject_leaves_the_mail_on_the_bus(monkeypatch):
    messages = [_msg("msg-0", "worker", "report")]
    emitted, advanced = _capture_release(monkeypatch, messages, delivered=False)

    result = hold_mod.release(HANDLE, held_for_s=60)

    assert result["outcome"] == "inject-missed"
    assert advanced == [], "a missed delivery must never consume the cursor"
    assert emitted[0][1]["outcome"] == "inject-missed"


# --- Task 5: the bounce -----------------------------------------------------


def test_bounce_reason_names_the_recipient_and_when_it_lands():
    hold_mod.arm(HANDLE, 5)
    reason = hold_mod.bounce_reason(HANDLE)

    assert reason is not None
    assert HANDLE in reason
    assert "do-not-disturb" in reason
    assert "lifts in" in reason


def test_bounce_reason_is_silent_for_a_permanent_policy_and_for_no_hold():
    assert hold_mod.bounce_reason(HANDLE) is None
    hold_mod.arm_permanent(HANDLE)
    assert hold_mod.bounce_reason(HANDLE) is None


# --- Task 6: the render -----------------------------------------------------


def test_serialized_row_carries_the_policy_and_the_remaining_hold():
    hold_mod.arm(HANDLE, 5)
    row = fmt.serialize_entry(_full_entry(), live_status=None)

    assert row["delivery_policy"] == "bus-only"
    assert row["dnd"].endswith("m")


def test_a_row_with_no_hold_renders_a_null_dnd():
    row = fmt.serialize_entry(_full_entry(delivery_policy=None), live_status=None)
    assert row["delivery_policy"] is None
    assert row["dnd"] is None


def test_a_bus_only_row_with_no_clock_reads_held_not_blank():
    """The pre-busy-mode row: flag set by hand, no clock, mail genuinely held.

    A blank cell here would be the column lying about the one row it exists to
    describe, and every row stamped before this file existed is this shape.
    """
    row = fmt.serialize_entry(_full_entry(), live_status=None)

    assert row["delivery_policy"] == "bus-only"
    assert row["dnd"] == "held"


def test_the_dnd_column_and_the_delivery_gate_never_disagree():
    """Whatever the column says, the gate must agree mail is or is not moving."""
    cases = [
        ("no clock", lambda: None),
        ("permanent", lambda: hold_mod.arm_permanent(HANDLE)),
        ("live timed", lambda: hold_mod.arm(HANDLE, 5)),
        ("lapsed timed", lambda: _expire(HANDLE)),
    ]
    for name, arrange in cases:
        hold_mod.clear(HANDLE)
        arrange()
        held_per_column = fmt.serialize_entry(_full_entry(), live_status=None)["dnd"]
        held_per_gate = (
            dispatch._delivery_policy_refusal(_entry()) == dispatch.BUS_ONLY_POLICY
        )
        assert (held_per_column is not None) is held_per_gate, name


def test_a_lapsed_hold_renders_no_dnd_because_mail_flows_again():
    hold_mod._write(
        hold_mod.Hold(
            handle=HANDLE,
            until=datetime.now(timezone.utc) - timedelta(seconds=1),
            window_s=60,
        )
    )
    row = fmt.serialize_entry(_full_entry(), live_status=None)
    assert row["dnd"] is None


def test_the_table_renders_a_dnd_column():
    rows = [
        {
            "name": HANDLE,
            "address": HANDLE,
            "harness": "claude",
            "status": "live",
            "dnd": "~4m",
            "cwd": "/tmp",
        }
    ]
    table = fmt.render_table(rows, terminal_width=200)

    assert "DND" in table.splitlines()[0]
    assert "~4m" in table


def _full_entry(**over):
    from fno.agents.registry import AgentEntry

    fields = dict(
        name=HANDLE,
        harness="claude",
        cwd="/tmp",
        harness_session_id=f"{HANDLE}-full",
        log_path="",
        delivery_policy="bus-only",
    )
    fields.update(over)
    return AgentEntry(**fields)
