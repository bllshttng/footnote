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
    assert armed.clock_kind == "idle"
    assert armed.ceiling == armed.until + timedelta(seconds=300)
    read_back = hold_mod.read(HANDLE)
    assert read_back is not None
    assert read_back.until is not None
    assert hold_mod.remaining_label(HANDLE).endswith("m")

    hold_mod.clear(HANDLE)
    assert hold_mod.read(HANDLE) is None


def test_wall_clock_arm_has_fixed_deadline_and_no_idle_ceiling(monkeypatch):
    start = datetime(2026, 8, 25, 20, 0, tzinfo=timezone.utc)
    monkeypatch.setattr(hold_mod, "_now", lambda: start)
    armed = hold_mod.arm_wall(HANDLE, 8)

    assert armed.clock_kind == "wall"
    assert armed.until == start + timedelta(minutes=8)
    assert armed.ceiling is None
    assert armed.window_s == 480


def test_wall_clock_activity_preserves_the_original_deadline(monkeypatch):
    start = datetime(2026, 8, 25, 20, 0, tzinfo=timezone.utc)
    monkeypatch.setattr(hold_mod, "_now", lambda: start)
    armed = hold_mod.arm_wall(HANDLE, 8)
    later = start + timedelta(minutes=1)
    monkeypatch.setattr(hold_mod, "_now", lambda: later)

    active = hold_mod.extend(HANDLE)

    assert active is not None
    assert active.clock_kind == "wall"
    assert active.until == armed.until


def test_idle_activity_clamps_at_the_absolute_ceiling(monkeypatch):
    start = datetime(2026, 8, 25, 20, 0, tzinfo=timezone.utc)
    hold_mod._write(
        hold_mod.Hold(
            handle=HANDLE,
            until=start + timedelta(minutes=1),
            window_s=480,
            clock_kind="idle",
            ceiling=start + timedelta(minutes=2),
        )
    )
    monkeypatch.setattr(hold_mod, "_now", lambda: start)

    active = hold_mod.extend(HANDLE)

    assert active is not None
    assert active.until == start + timedelta(minutes=2)
    assert active.ceiling == start + timedelta(minutes=2)


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
    tidies it) or on `fno agents mail hold --off`. A stall, and a bounded one.
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


def test_the_gate_finds_a_clock_filed_under_the_canonical_handle(monkeypatch):
    """The key every WRITER uses, on a row where it is none of the other three.

    A codex row's short_id is a daemon worker key and its harness_session_id is
    the full id, so neither is the first-eight the clock sits under. Reading
    only those three looked correct on claude, where the handle IS the short_id,
    and left a codex hold unexpirable.
    """
    codex = SimpleNamespace(
        name="worker-migration",
        short_id="daemon-worker-key",
        harness_session_id="abcd1234-0b3f-4c5a-9e88-2ad4f0c81b97",
        delivery_policy="bus-only",
    )
    monkeypatch.setattr(dispatch, "load_registry", lambda: [codex])

    hold_mod.arm(HANDLE, 5)  # HANDLE is that session id's first eight
    assert dispatch._delivery_policy_refusal(codex) == dispatch.BUS_ONLY_POLICY

    _expire(HANDLE)
    assert dispatch._delivery_policy_refusal(codex) is None
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
        hold_mod,
        "set_policy",
        # Returns True: this stub stands in for a write that SUCCEEDED, and
        # tidy_lapsed now reports the write rather than the attempt.
        lambda handle, policy: (cleared.append((handle, policy)), True)[1],
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
    monkeypatch.setattr(hold_mod, "resolve_entry", lambda handle: _entry())
    monkeypatch.setattr(
        "fno.agents.dispatch._deliver_live", lambda *a, **k: delivered
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
                "miss_reason": None,
                "policy_cleared": True,
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


def test_a_failed_policy_write_keeps_the_clock_so_the_hold_stays_recoverable(monkeypatch):
    """A partial failure must not land somewhere worse than the failure.

    Clearing the clock regardless left a bus-only row with no clock, and that
    never lapses, so a hold that failed to lift became permanent with no
    automatic path back: `tidy_lapsed` needs a clock it no longer has.
    """
    _expire(HANDLE)
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: False)
    monkeypatch.setattr("fno.bus.cursor.scan_unread", lambda *a, **k: [])
    monkeypatch.setattr("fno.agents.events.emit", lambda *a, **k: None)

    hold_mod.release(HANDLE)

    clock = hold_mod.read(HANDLE)
    assert clock is not None, "a failed policy write must keep the clock"
    # Still lapsed, so the gate lets mail through and the next turn boundary
    # retries the tidy.
    assert hold_mod.lapsed(HANDLE) is True


def test_a_successful_policy_write_drops_the_clock(monkeypatch):
    _expire(HANDLE)
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    monkeypatch.setattr("fno.bus.cursor.scan_unread", lambda *a, **k: [])
    monkeypatch.setattr("fno.agents.events.emit", lambda *a, **k: None)

    hold_mod.release(HANDLE)

    assert hold_mod.read(HANDLE) is None


def test_release_reports_whether_the_flag_actually_came_off(monkeypatch):
    """`--off` asks about the FLAG, so the result must answer about the flag.

    Both of that verb's receipts describe delivery. A registry it could not
    write leaves mail held while the line reads "hold off", which is a lie
    about the operator's own session.
    """
    monkeypatch.setattr("fno.bus.cursor.scan_unread", lambda *a, **k: [])
    monkeypatch.setattr("fno.agents.events.emit", lambda *a, **k: None)

    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: False)
    assert hold_mod.release(HANDLE)["policy_cleared"] is False

    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    assert hold_mod.release(HANDLE)["policy_cleared"] is True


def test_the_release_delivers_through_the_lane_dispatcher(monkeypatch):
    """Not through the claude injector, which is one lane of several.

    Wired to `_mail_inject_claude` this was a producer on one of N paths: a
    codex, gemini or mux-hosted operator armed a hold that lifted on time and
    delivered nothing, so their mail still waited for them to type. Pin the
    dispatcher, because the failure is invisible on a claude box.
    """
    seen = {}
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    monkeypatch.setattr(hold_mod, "resolve_entry", lambda handle: _entry())
    monkeypatch.setattr("fno.bus.cursor.scan_unread", lambda *a, **k: [_msg("m", "w", "b")])
    monkeypatch.setattr("fno.bus.cursor.advance_cursor", lambda *a, **k: True)
    monkeypatch.setattr("fno.agents.events.emit", lambda *a, **k: None)

    def _fake_deliver(entry, body, from_name, **kwargs):
        seen["entry"] = entry
        seen["from_name"] = from_name
        return True

    monkeypatch.setattr("fno.agents.dispatch._deliver_live", _fake_deliver)
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *a, **k: pytest.fail("the release must not bypass the lane dispatcher"),
    )

    assert hold_mod.release(HANDLE)["outcome"] == "delivered"
    assert seen["entry"] is not None, "the dispatcher needs the resolved row"
    assert seen["from_name"] == "fno-mail-hold"


def test_a_release_with_no_registry_row_names_that_as_the_miss(monkeypatch):
    """`inject-missed` alone cannot separate a dead lane from an absent row."""
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    monkeypatch.setattr(hold_mod, "resolve_entry", lambda handle: None)
    monkeypatch.setattr("fno.bus.cursor.scan_unread", lambda *a, **k: [_msg("m", "w", "b")])
    advanced = []
    monkeypatch.setattr(
        "fno.bus.cursor.advance_cursor", lambda name, mid: advanced.append(mid)
    )
    monkeypatch.setattr("fno.agents.events.emit", lambda *a, **k: None)

    result = hold_mod.release(HANDLE)

    assert result["outcome"] == "inject-missed"
    assert result["miss_reason"] == "no-registry-row"
    assert advanced == [], "a missed delivery must never consume the cursor"


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
    assert "quiet minutes" in reason
    assert "lifts in" in reason


def test_wall_bounce_reason_names_the_wall_clock():
    hold_mod.arm_wall(HANDLE, 5)

    reason = hold_mod.bounce_reason(HANDLE)

    assert reason is not None
    assert "wall clock" in reason


def test_cli_rejects_minutes_and_for_together(monkeypatch):
    from typer.testing import CliRunner
    from fno.mail import cli as mail_cli

    monkeypatch.setattr(
        mail_cli,
        "_self_handle_or_exit",
        lambda: (HANDLE, SimpleNamespace(harness="claude", session_id="sid")),
    )
    result = CliRunner().invoke(
        mail_cli.mail_app,
        ["hold", "--minutes", "1", "--for", "1"],
    )

    assert result.exit_code == 2
    assert "mutually exclusive" in result.output


def test_cli_for_arms_wall_clock_and_names_it_in_the_receipt(monkeypatch, capsys):
    from fno.mail import cli as mail_cli

    start = datetime(2026, 8, 25, 20, 0, tzinfo=timezone.utc)
    armed = hold_mod.Hold(
        handle=HANDLE,
        until=start + timedelta(minutes=8),
        window_s=480,
        clock_kind="wall",
    )
    monkeypatch.setattr(
        mail_cli,
        "_self_handle_or_exit",
        lambda: (HANDLE, SimpleNamespace(harness="claude", session_id="sid")),
    )
    monkeypatch.setattr(
        "fno.agents.registry.register_existing_session", lambda **_kwargs: None
    )
    monkeypatch.setattr(hold_mod, "arm_wall", lambda handle, minutes: armed)
    monkeypatch.setattr("subprocess.Popen", lambda *args, **kwargs: None)

    mail_cli.cmd_hold(minutes=None, for_minutes=8, off=False, status=False)

    output = capsys.readouterr().out
    assert "wall clock" in output
    assert "fixed deadline 20:08:00 UTC" in output


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


def test_hold_refuses_a_contaminated_env_rather_than_stamping_a_guessed_row(monkeypatch):
    """Precedence order is not ownership, so an inherited marker from a parent
    harness makes a precedence-only resolve answer with the PARENT session.

    `--to-self` already fails closed here. A hold must too, and the stakes are
    higher: a misaddressed send delivers one message to the wrong place, while
    a misaddressed hold stamps a delivery policy on another agent's row and
    arms a timer against their handle. This was live on the machine that wrote
    the test, where claude and codex markers were both present.

    The hold now shares the one owned-identity path rather than hand-rolling a
    >1-family check, so this drives the real seam: two families the process
    tree cannot decide between refuse, one family resolves.
    """
    import typer

    from fno.harness_identity import OwnedHarnessIdentity
    from fno.mail import cli as mail_cli

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda *a, **k: OwnedHarnessIdentity(
            None,
            None,
            (
                ("CLAUDE_CODE_SESSION_ID", "claude", "x"),
                ("CODEX_THREAD_ID", "codex", "y"),
            ),
            "ambiguous",
        ),
    )

    with pytest.raises(typer.Exit) as caught:
        mail_cli._self_handle_or_exit()
    assert caught.value.exit_code == 3

    # One family present is a clean session and must still resolve.
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda *a, **k: OwnedHarnessIdentity(
            f"{HANDLE}-full", "codex", (("CODEX_THREAD_ID", "codex", "y"),), "single"
        ),
    )
    resolved_handle, resolved_ident = mail_cli._self_handle_or_exit()
    assert resolved_handle == HANDLE
    # The identity travels back with the handle, so the caller writes the row
    # this function validated rather than re-resolving and getting another.
    assert resolved_ident.session_id == f"{HANDLE}-full"


def test_an_unreadable_clock_renders_a_question_mark_not_an_empty_cell(monkeypatch):
    """An instrument that declines to answer must not answer anyway.

    None renders as "-", the same cell a row with no hold gets, so a failed
    read would report mail flowing to a session whose flag says it is held.
    """
    def _boom(_entry):
        raise RuntimeError("clock unreadable")

    monkeypatch.setattr(hold_mod, "dnd_label", _boom)

    assert fmt._dnd_label(_full_entry()) == "?"


def test_tidy_lapsed_reports_the_write_not_the_attempt(monkeypatch):
    """Returning True over a policy write that no-opped is the same defect."""
    _expire(HANDLE)
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: False)

    assert hold_mod.tidy_lapsed(HANDLE) is False

    _expire(HANDLE)
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    assert hold_mod.tidy_lapsed(HANDLE) is True


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
