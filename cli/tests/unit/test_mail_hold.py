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
from fno.harness_identity import session_identity_key
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


def _stub_gate(monkeypatch, verdict: str):
    """Stub the Rust gate subprocess to answer ``verdict``; returns the calls."""
    import json

    calls: list = []

    def _fake_run(argv, input="", capture_output=True, text=True, timeout=10, **_kw):
        calls.append((argv, input))
        # The Rust gate's control pass, mirrored: a body whose first line
        # opens with the directive delivers with pass=control.
        effective = verdict
        if input.strip().lower().startswith("control:"):
            effective = "deliver"
        out = json.dumps({"verdict": effective, "pass": None, "receipt": None, "until": None})
        return SimpleNamespace(stdout=out + "\n", returncode=0)

    monkeypatch.setattr(dispatch.subprocess, "run", _fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/bin/true")
    return calls


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

    # The conversation-sourced clock is the machine's own hold; the column
    # must not read it as a hold the user set on purpose.
    auto = hold_mod.Hold(
        handle=HANDLE,
        until=armed.until,
        window_s=armed.window_s,
        clock_kind=armed.clock_kind,
        ceiling=armed.ceiling,
        source=hold_mod.CONVERSATION_SOURCE,
    )
    hold_mod._write(auto)
    assert hold_mod.read(HANDLE).source == hold_mod.CONVERSATION_SOURCE
    assert hold_mod.dnd_label(HANDLE).endswith(" (auto)")
    bare = hold_mod.Hold(
        handle=HANDLE,
        until=armed.until,
        window_s=armed.window_s,
        clock_kind=armed.clock_kind,
        ceiling=armed.ceiling,
    )
    hold_mod._write(bare)
    assert hold_mod.dnd_label(HANDLE) == hold_mod.remaining_label(HANDLE)
    assert not hold_mod.dnd_label(HANDLE).endswith(" (auto)")

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


def test_gate_maps_a_stubbed_hold_verdict_to_the_refusal(monkeypatch):
    """The gate body lives in Rust (mail_hold.rs ``gate``); Python maps the
    verdict. The entry branch returns before ``load_registry``; the body rides
    the gate so the control pass can see it.
    """
    calls = _stub_gate(monkeypatch, "hold")
    assert dispatch._delivery_policy_refusal(_entry()) == dispatch.BUS_ONLY_POLICY
    assert dispatch._delivery_policy_refusal(_entry(), "control: stop") is None
    assert calls[-1][1] == "control: stop"


def test_gate_maps_deliver_to_none_and_fails_closed_on_a_broken_gate(monkeypatch):
    """A failed or unreadable gate never lifts a hold it could not read: a
    stamped row fails closed to BUS_ONLY_POLICY, while an unstamped token
    keeps failing open toward live delivery.
    """
    monkeypatch.setattr(dispatch, "load_registry", lambda: [_entry()])
    _stub_gate(monkeypatch, "deliver")
    assert dispatch._delivery_policy_refusal(_entry()) is None
    assert dispatch._delivery_policy_refusal(HANDLE) is None

    def _boom(*_a, **_kw):
        raise OSError("gate down")

    monkeypatch.setattr(dispatch.subprocess, "run", _boom)
    assert dispatch._delivery_policy_refusal(_entry()) == dispatch.BUS_ONLY_POLICY


def test_the_raw_door_parks_through_the_gate_and_relays_the_receipt(monkeypatch):
    """With ``park=True`` the raw door's gate call carries ``--park-on-hold``
    and the parked receipt rides back in place of the refusal; without it the
    same held row answers the plain refusal.
    """
    import json

    calls: list = []

    def _fake_run(argv, input="", capture_output=True, text=True, timeout=10, **_kw):
        calls.append((list(argv), input))
        if "--park-on-hold" in argv:
            out = {
                "verdict": "parked",
                "pass": None,
                "receipt": f"held: {input} runs on {HANDLE} when the hold ends",
                "until": None,
            }
        else:
            out = {"verdict": "hold", "pass": None, "receipt": None, "until": None}
        return SimpleNamespace(stdout=json.dumps(out) + "\n", returncode=0)

    monkeypatch.setattr(dispatch.subprocess, "run", _fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/bin/true")
    assert (
        dispatch._delivery_policy_refusal(_entry(), "/compact", park=True)
        == f"held: /compact runs on {HANDLE} when the hold ends"
    )
    assert any("--park-on-hold" in argv for argv, _ in calls)
    assert dispatch._delivery_policy_refusal(_entry(), "/compact") == dispatch.BUS_ONLY_POLICY
    monkeypatch.setattr(dispatch, "load_registry", lambda: [])
    assert dispatch._delivery_policy_refusal(HANDLE) is None


def test_gate_fails_closed_when_the_binary_is_missing(monkeypatch):
    """No gate binary on a stamped row: the refusal stands (fail closed)."""
    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: None)
    assert dispatch._delivery_policy_refusal(_entry()) == dispatch.BUS_ONLY_POLICY
    monkeypatch.setattr(dispatch, "load_registry", lambda: [])
    assert dispatch._delivery_policy_refusal(HANDLE) is None


def test_two_same_window_codex_rows_never_share_one_clock(monkeypatch):
    """Writers key the full session identity key.

    Codex UUIDv7 ids opened in one 65.536-second window share their first
    eight, so two same-window sessions used to share one clock file: one
    release deleted the shared clock while only the first matching row's
    policy cleared, leaving the sibling stamped bus-only with no clock, which
    never lapses.
    """
    sid_a = "0198a3f2-77e3-7000-8000-000000000001"
    sid_b = "0198a3f2-77e3-7000-8000-000000000002"
    row_a = SimpleNamespace(
        name="alpha", short_id="", harness_session_id=sid_a, delivery_policy="bus-only"
    )
    row_b = SimpleNamespace(
        name="beta", short_id="", harness_session_id=sid_b, delivery_policy="bus-only"
    )

    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: None)
    hold_mod.arm(session_identity_key(sid_a), 5)

    assert hold_mod.read_any(row_a) is not None
    assert hold_mod.read_any(row_b) is None
    # The sibling's clock cannot satisfy its row either way: B is stamped
    # with no clock of its own, and that must read as refusing, not as held
    # on A's clock.
    assert dispatch._delivery_policy_refusal(row_b) == dispatch.BUS_ONLY_POLICY


def test_gate_leaves_a_clockless_bus_only_row_refusing_on_both_branches(monkeypatch):
    """The clockless-hold guarantee, unchanged for every row busy mode
    never touched.

    The row keeps the production shape: its registry name is its label, not
    its handle, so the token branch must reach it through the address sweep
    (the canonical handle here), not through the name.
    """
    row = _entry(name="quill")
    monkeypatch.setattr(dispatch, "load_registry", lambda: [row])
    _stub_gate(monkeypatch, "hold")

    assert dispatch._delivery_policy_refusal(row) == dispatch.BUS_ONLY_POLICY
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


# --- Release rendering -----------------------------------------------------


def test_five_identical_held_messages_keep_their_own_headers():
    messages = [_msg(f"msg-{i}", "worker", "same report") for i in range(5)]
    digest = hold_mod.render_digest(messages, held_for_s=600)
    assert digest.count("`@worker · msg-") == 5
    assert digest.count("same report") == 5
    assert "(x5 identical, deduped)" not in digest


# --- Task 3: the release ----------------------------------------------------


def _capture_release(monkeypatch, messages, delivered=True):
    emitted = []
    advanced = []
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    # release() drains every mailbox form the row owns; only the canonical
    # form carries mail in these tests.
    monkeypatch.setattr(
        "fno.bus.cursor.scan_unread",
        lambda name, **k: messages if name == HANDLE else [],
    )
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
    assert result["deduped_count"] == 0
    assert advanced == ["msg-0", "msg-1", "msg-2"]
    assert [
        (kind, data.get("msg_id") if kind == "agent_mail_drained" else None)
        for kind, data in emitted
    ] == [
        ("agent_mail_drained", "msg-0"),
        ("agent_mail_drained", "msg-1"),
        ("agent_mail_drained", "msg-2"),
        ("mail_hold_released", None),
    ]
    assert emitted[-1] == (
        "mail_hold_released",
        {
            "handle": HANDLE,
            "clock": "no expiry",
            "held_count": 3,
            "deduped_count": 0,
            "held_for_s": 300,
            "outcome": "delivered",
            "miss_reason": None,
            "policy_cleared": True,
        },
    )


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
    monkeypatch.setattr(
        "fno.bus.cursor.scan_unread",
        lambda *a, **k: [_msg("fmail-000000000001", "w", "b")],
    )
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


def test_the_drain_delivers_a_multi_line_digest_on_the_live_lane(monkeypatch):
    """C17 evidence (x-9008, crown ruling d-9187ccf6).

    The crown's held-mail drain lost three messages when the raw door
    refused the multi-line digest with "an unframed payload must be a
    single line". That refusal's premise died with C17: the Rust typing
    layer flattens every payload into ONE submitted line (pinned by
    ``multi_line_unwrapped_types_flattened_after_c17`` in mail_inject.rs),
    so the drain hands the digest to the live lane unchanged and it
    delivers.
    """
    seen: dict = {}

    def _fake_deliver(entry, body, from_name, **kwargs):
        seen["body"] = body
        seen["from_name"] = from_name
        return True

    messages = [_msg("fmail-000000000002", "peer", "line one\nline two\nline three")]
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    monkeypatch.setattr(hold_mod, "resolve_entry", lambda handle: _entry())
    monkeypatch.setattr("fno.bus.cursor.scan_unread", lambda *a, **k: messages)
    monkeypatch.setattr("fno.bus.cursor.advance_cursor", lambda *a, **k: True)
    monkeypatch.setattr("fno.agents.events.emit", lambda *a, **k: None)
    monkeypatch.setattr("fno.agents.dispatch._deliver_live", _fake_deliver)

    result = hold_mod.release(HANDLE, held_for_s=300)

    assert result["outcome"] == "delivered"
    body = seen["body"]
    assert "\n" in body, "the digest is genuinely multi-line"
    assert "line two" in body, "the held body rides the digest intact"
    assert seen["from_name"] == "fno-mail-hold"


def test_release_delivers_held_release_frame_without_synthetic_sender(monkeypatch, tmp_path):
    """The held-release frame keeps its real per-message headers."""
    from fno.bus.cursor import scan_unread
    from fno.bus.log import Envelope, append

    home = tmp_path / "home"
    state = tmp_path / "state"
    bus = tmp_path / "bus"
    for root in (home, state, bus):
        root.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("FNO_HOME", str(home / ".fno"))
    monkeypatch.setenv("FNO_AGENTS_HOME", str(state / "agents"))
    monkeypatch.setenv("FNO_STATE_DIR", str(state))
    monkeypatch.setenv("FNO_BUS_DIR", str(bus))
    monkeypatch.setattr("fno.paths.state_dir", lambda: state)
    monkeypatch.setattr("fno.paths.bus_dir", lambda: bus)
    monkeypatch.setattr(
        "fno.paths.agents_registry_path", lambda: state / "agents" / "registry.json"
    )

    entry = SimpleNamespace(
        name=HANDLE,
        harness="claude",
        short_id=HANDLE,
        harness_session_id=f"{HANDLE}-full",
        mcp_channel_id=None,
        delivery_policy=None,
        mux=None,
        messaging_socket_path=None,
    )
    monkeypatch.setattr(hold_mod, "set_policy", lambda *_a, **_kw: True)
    monkeypatch.setattr(hold_mod, "resolve_entry", lambda _handle: entry)
    monkeypatch.setattr(dispatch, "_delivery_policy_refusal", lambda *_a, **_kw: None)
    monkeypatch.setattr(dispatch, "_switchboard_identity", lambda *_a, **_kw: None)
    monkeypatch.setattr(dispatch, "_switchboard_exchange", lambda *_a, **_kw: False)
    monkeypatch.setattr("fno.agents.events.emit", lambda *_a, **_kw: None)

    original = Envelope.new(
        id="fmail-123456789abc",
        thread="fmail-123456789abc",
        from_="worker",
        to=HANDLE,
        kind="send",
        body=(
            '<fno_mail from="worker-session" harness="codex" id="fmail-123456789abc">'
            "the held report"
            "</fno_mail>"
        ),
        ts="2026-09-27T12:00:00Z",
    )
    append(original)

    rendered: list[dict] = []

    def fake_render(payload):
        rendered.append(payload)
        if payload["mode"] == "held-release":
            return payload["body"]
        return (
            f'<fno_mail from="{payload["from"]}" '
            f'harness="{payload["harness"]}">'
            f'{payload["body"]}</fno_mail>'
        )

    monkeypatch.setattr("fno.mail.envelope._render_in_rust", fake_render)
    injected: list[str] = []

    def fake_inject(_recipient, text, *, reason_out=None, **_kwargs):
        injected.append(text)
        is_release = (
            text.startswith("1 held messages · sent ")
            and (
                "`@worker · fmail-123456789abc · the held report`" in text
                or "`worker · fmail-123456789abc · the held report`" in text
            )
            and "<fno_mail" not in text
        )
        if not is_release and reason_out is not None:
            reason_out.append("invalid-held-release-frame")
        return is_release

    monkeypatch.setattr(dispatch, "_mail_inject_claude", fake_inject)

    result = hold_mod.release(HANDLE, held_for_s=60)

    assert result["outcome"] == "delivered", result.get("miss_reason")
    assert result["miss_reason"] is None
    assert len(rendered) == 1
    assert rendered[0]["mode"] == "held-release"
    assert "the held report" in rendered[0]["body"]
    assert "<fno_mail" not in rendered[0]["body"]
    assert len(injected) == 1
    assert injected[0].startswith("1 held messages · sent ")
    assert (
        "`@worker · fmail-123456789abc · the held report`" in injected[0]
        or "`worker · fmail-123456789abc · the held report`" in injected[0]
    )
    assert "<fno_mail" not in injected[0]
    assert scan_unread(HANDLE) == []


def test_a_release_with_no_registry_row_names_that_as_the_miss(monkeypatch):
    """`inject-missed` alone cannot separate a dead lane from an absent row."""
    monkeypatch.setattr(hold_mod, "set_policy", lambda *a, **k: True)
    monkeypatch.setattr(hold_mod, "resolve_entry", lambda handle: None)
    monkeypatch.setattr(
        "fno.bus.cursor.scan_unread",
        lambda *a, **k: [_msg("fmail-000000000003", "w", "b")],
    )
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


def test_release_by_the_clock_key_still_drains_the_canonical_mailbox(monkeypatch):
    """Durable mail is filed under the canonical first-eight (the name lane's
    recipient) while the clock sits under the full identity key: the release
    the clock triggers must scan the row's mailbox forms, not the clock key.
    """
    sid = "0198a3f2-77e3-7000-8000-000000000009"
    _emitted, advanced = _capture_release(monkeypatch, [])
    monkeypatch.setattr(
        hold_mod,
        "resolve_entry",
        lambda handle: SimpleNamespace(
            name="worker", short_id="", harness_session_id=sid, delivery_policy=None
        ),
    )
    monkeypatch.setattr(
        "fno.bus.cursor.scan_unread",
        lambda name, **k: [_msg("fmail-000000000004", "w", "b")]
        if name == "0198a3f2"
        else [],
    )

    result = hold_mod.release(session_identity_key(sid), held_for_s=10)

    assert result["outcome"] == "delivered"
    assert result["held_count"] == 1
    assert advanced == ["fmail-000000000004"]


# --- Task 5: the bounce -----------------------------------------------------


def test_bounce_reason_returns_the_gate_receipt(monkeypatch):
    """The receipt body lives in the Rust gate (C16); Python returns its
    ``receipt`` field verbatim, and its None too."""
    import json

    def _fake_run(argv, **_kw):
        out = json.dumps(
            {
                "verdict": "hold",
                "pass": None,
                "receipt": "held until about 21:04: worker is in do-not-disturb; "
                "delivers itself then",
                "until": None,
            }
        )
        return SimpleNamespace(stdout=out, returncode=0)

    monkeypatch.setattr(hold_mod.subprocess, "run", _fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/bin/true")

    reason = hold_mod.bounce_reason(HANDLE)

    assert reason is not None
    assert "do-not-disturb" in reason
    assert reason.startswith("held until about 21:04")


def test_bounce_reason_none_keeps_the_callers_text(monkeypatch):
    import json

    def _fake_run(argv, **_kw):
        out = json.dumps({"verdict": "hold", "pass": None, "receipt": None, "until": None})
        return SimpleNamespace(stdout=out, returncode=0)

    monkeypatch.setattr(hold_mod.subprocess, "run", _fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/bin/true")

    assert hold_mod.bounce_reason(HANDLE) is None


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


def test_the_dnd_column_and_the_delivery_gate_never_disagree(monkeypatch):
    """Whatever the column says, the gate must agree mail is or is not moving.

    The stub answers from the SAME Python clock state the column reads, so the
    parity contract itself is what runs; the gate's own clock logic is tested
    in Rust.
    """
    import json

    def _fake_run(argv, **_kw):
        token = argv[argv.index("--session") + 1]
        clock = hold_mod.read_any(token)
        # Mirror the Rust gate: only a LAPSED timed clock delivers; no clock,
        # a permanent (until: null) clock, and a live clock all hold.
        lapsed = (
            clock is not None
            and clock.until is not None
            and clock.until <= hold_mod._now()
        )
        verdict = "deliver" if lapsed else "hold"
        out = json.dumps({"verdict": verdict, "pass": None, "receipt": None, "until": None})
        return SimpleNamespace(stdout=out, returncode=0)

    monkeypatch.setattr(dispatch.subprocess, "run", _fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/bin/true")
    # The entry branch passes the harness session id to the gate, so the
    # clocks here sit under that same key; the column's addresses() sweep
    # finds it either way.
    full = _entry().harness_session_id
    cases = [
        ("no clock", lambda: None),
        ("permanent", lambda: hold_mod.arm_permanent(full)),
        ("live timed", lambda: hold_mod.arm(full, 5)),
        ("lapsed timed", lambda: _expire(full)),
    ]
    for name, arrange in cases:
        hold_mod.clear(HANDLE)
        hold_mod.clear(full)
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
