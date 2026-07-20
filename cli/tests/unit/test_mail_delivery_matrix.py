"""Delivery-matrix conformance test (x-e864) -- the recurrence guard.

Mail delivery has now been reworked four times (x-39a4 push-first, x-d899 a2a
injection, x-1f23 live-inject-first, and this node). Every round shipped one
lane and left another walled off, because nothing pinned the WHOLE matrix. This
file is that pin: the normative delivery matrix from the design doc encoded as
fixtures, one assertion per cell.

A refactor that reintroduces a wall -- demoting to the durable queue while a
live rung was never attempted, or refusing a token some store could still
resolve -- fails a cell here instead of shipping round five.

| # | Recipient state                      | Expected delivery      |
|---|--------------------------------------|------------------------|
| 1 | Live bg thread, discovery MISSES it   | socket inject          |
| 2 | Asleep session (resolvable on disk)   | wake-and-deliver       |
| 3 | Live foreground, fno owns the pane    | owned-PTY send         |
| 3'| Live foreground, pane NOT owned       | durable + named reason |
| 4 | Every live rung attempted and failed  | durable + lane receipt |
| 5 | Unknown token (resolves nowhere)      | exit 16, nothing sent  |
| 6a| Retired handle TYPED by a caller      | refuse, suggest bare   |
| 6b| Retired handle READ off a record      | migrate + deliver      |
| 6c| Retired-addressed mail stranded       | reported, not vanished |

Cells 6a-6c shipped with PR #491; they are pinned here so this node cannot
regress them. The daemon is faked at the ``_mail_inject_claude`` boundary and
the wake at the ``wake_and_deliver`` boundary: these tests pin what the CLI does
GIVEN an answer, never the daemon's own correctness (the known false-confirm
repro is a separate node -- see the design doc's Domain Pitfalls).
"""
from __future__ import annotations

import json
import os
import time

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.paths_testing import use_tmpdir

LIVE_SID = "9a063cd3-69d4-415a-ada5-649b0164189c"
ASLEEP_SID = "5b17e2f0-1c44-4d9a-8e3b-2f6a7c081d55"


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    """Co-isolate the md render, the bus log, and every discovery source."""
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    _blank_discovery(monkeypatch, tmp_path)
    return tmp_path


def _blank_discovery(monkeypatch, tmp_path):
    """Point every discovery source at an empty dir.

    This is the fixture that makes cell 1 meaningful: with no roster and no
    transcripts, ``resolve_or_suggest`` MISSES, which is exactly the state that
    used to exit 16 before the socket was ever consulted.
    """
    from fno.agents import discover

    empty = tmp_path / "empty-discovery"
    empty.mkdir(exist_ok=True)
    for env in (
        discover.SESSIONS_DIR_ENV,
        discover.PROJECTS_DIR_ENV,
        discover.CODEX_SESSIONS_DIR_ENV,
    ):
        monkeypatch.setenv(env, str(empty))
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))


def _seed_asleep_transcript(monkeypatch, tmp_path, *, session_id=ASLEEP_SID, age_s=7200):
    """Seed a transcript whose mtime is well outside the liveness recency window.

    That combination -- the transcript exists on disk, but the session is not
    live -- IS the asleep state. Discovery correctly refuses to list it (it is a
    liveness-gated LISTING); the ladder must still reach it, because asleep is a
    resumable state rather than voicemail.
    """
    from fno.agents import discover

    projects = tmp_path / "projects"
    proj = projects / "-Users-x-proj"
    proj.mkdir(parents=True, exist_ok=True)
    transcript = proj / f"{session_id}.jsonl"
    transcript.write_text(
        json.dumps({"type": "assistant", "isSidechain": False,
                    "message": {"model": "claude-opus-4-8"}}) + "\n",
        encoding="utf-8",
    )
    old = time.time() - age_s
    os.utime(transcript, (old, old))
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    return transcript


def _drain_as(runner, monkeypatch, session_id):
    """Read what the recipient's own drain-self would see (the durable truth)."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", session_id)
    res = runner.invoke(app, ["mail", "drain-self", "--json"])
    assert res.exit_code == 0, res.output
    return json.loads(res.stdout.strip().splitlines()[-1])


# ---------------------------------------------------------------------------
# Cell 1: live bg thread that discovery does not list -> socket inject.
# The regression this whole node exists to kill.
# ---------------------------------------------------------------------------


def test_cell1_discovery_miss_still_injects_over_the_socket(
    runner, mailbox, monkeypatch, tmp_path
):
    """The socket is its own truth: a confirmed inject IS the delivery receipt.

    Discovery misses (empty roster, no transcript). Before x-e864 this exited 16
    without ever asking the daemon -- a wall invented at a knowledge boundary.
    """
    attempts: list[str] = []

    def _inject(recipient, _text):
        attempts.append(recipient)
        return True

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", _inject)

    res = runner.invoke(app, ["mail", "send", LIVE_SID[:8], "hi", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert "delivered (hosted)" in res.output
    assert attempts, "the socket was never consulted on a discovery miss"
    # No durable copy: a confirmed inject is self-recording in the transcript.
    assert "queued (durable)" not in res.output


def test_cell1_inject_body_is_envelope_wrapped(runner, mailbox, monkeypatch, tmp_path):
    """An unwrapped inject renders user-trust-framed -- a spoofing vulnerability
    (repro claude-b9b1f809), not a style miss. Locked Decision 5."""
    bodies: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda _r, text: (bodies.append(text), True)[1],
    )

    runner.invoke(app, ["mail", "send", LIVE_SID[:8], "hi", "--from-name", "web"])

    assert bodies, "nothing was injected"
    assert "<fno_mail" in bodies[0]


def test_cell1_inject_is_attempted_before_any_durable_write(
    runner, mailbox, monkeypatch, tmp_path
):
    """Ladder ORDER, not just outcome: demotion must never precede a live rung.

    Asserting only the final state would let a refactor that writes durable
    first and injects second still pass. This pins the sequence.
    """
    # Seed a reachable-but-asleep session and fail every live lane, so the send
    # actually reaches the durable floor and there is an order to assert.
    _seed_asleep_transcript(monkeypatch, tmp_path)
    order: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a: (order.append("inject"), False)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver", lambda *_a, **_k: (False, "spawn-exit-1")
    )
    monkeypatch.setattr(
        "fno.inbox.store.write_new_thread",
        _recording_durable(order),
    )

    runner.invoke(app, ["mail", "send", ASLEEP_SID[:8], "hi", "--from-name", "web"])

    assert "inject" in order, "the socket rung was skipped entirely"
    assert order.index("inject") < order.index("durable"), (
        f"durable write preceded the inject attempt: {order}"
    )


def _recording_durable(order):
    from fno.inbox.store import write_new_thread as real

    def _wrapped(**kwargs):
        order.append("durable")
        return real(**kwargs)

    return _wrapped


# ---------------------------------------------------------------------------
# Cell 2: asleep session -> wake-and-deliver. Asleep is resumable, not voicemail.
# ---------------------------------------------------------------------------


def test_cell2_asleep_session_is_woken_not_queued(
    runner, mailbox, monkeypatch, tmp_path
):
    _seed_asleep_transcript(monkeypatch, tmp_path)
    # The socket misses: the session is asleep, so it is not on the roster.
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    woken: list[tuple[str, str]] = []

    def _wake(session_uuid, wrapped, **_kw):
        woken.append((session_uuid, wrapped))
        return True, "bg-7f3a"

    monkeypatch.setattr("fno.agents.dispatch.wake_and_deliver", _wake)

    res = runner.invoke(app, ["mail", "send", ASLEEP_SID[:8], "wake up", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert "delivered (woken)" in res.output
    assert woken, "an asleep, disk-resolvable session was never woken"
    assert woken[0][0] == ASLEEP_SID
    assert "queued (durable)" not in res.output


def test_cell2_wake_prompt_is_envelope_wrapped(runner, mailbox, monkeypatch, tmp_path):
    """The waking prompt is the mail. It MUST arrive wrapped for the same reason
    an inject must -- an unwrapped seed prompt renders as user-trusted text."""
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    seeds: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda _sid, wrapped, **_kw: (seeds.append(wrapped), (True, "bg-7f3a"))[1],
    )

    runner.invoke(app, ["mail", "send", ASLEEP_SID[:8], "wake up", "--from-name", "web"])

    assert seeds, "nothing was sent as a wake prompt"
    assert "<fno_mail" in seeds[0]


def test_cell2_receipt_names_the_revived_thread(runner, mailbox, monkeypatch, tmp_path):
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver", lambda *_a, **_k: (True, "bg-7f3a")
    )

    res = runner.invoke(app, ["mail", "send", ASLEEP_SID[:8], "hi", "--from-name", "web"])

    assert "bg-7f3a" in res.output, "the receipt does not name the revived thread"


# ---------------------------------------------------------------------------
# Cell 4: every applicable live rung attempted and failed -> durable + receipt.
# Durable is a DEMOTION, never a first answer.
# ---------------------------------------------------------------------------


def test_cell4_failed_wake_demotes_durably_with_lane_receipt(
    runner, mailbox, monkeypatch, tmp_path
):
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (False, "spawn-exit-1"),
    )

    res = runner.invoke(app, ["mail", "send", ASLEEP_SID[:8], "hi", "--from-name", "web"])

    # Exit 0: the envelope is safe even though every live lane missed.
    assert res.exit_code == 0, res.output
    assert "queued (durable)" in res.output
    combined = res.output + (res.stderr or "")
    assert "spawn-exit-1" in combined, "the receipt does not name why the wake failed"
    # Addressed to the canonical handle the recipient's own drain reads.
    assert _drain_as(runner, monkeypatch, ASLEEP_SID), "durable copy is not drainable"


def test_cell4_receipt_names_every_failed_lane(runner, mailbox, monkeypatch, tmp_path):
    """A delivery bug must be diagnosable from the sender's terminal alone."""
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (False, "writer-possibly-live"),
    )

    res = runner.invoke(app, ["mail", "send", ASLEEP_SID[:8], "hi", "--from-name", "web"])
    combined = res.output + (res.stderr or "")

    assert "inject=" in combined, "the inject lane failure is unnamed"
    assert "wake=" in combined, "the wake lane failure is unnamed"


# ---------------------------------------------------------------------------
# Cell 5: unknown token -> exit 16. The typo guard survives the widened ladder.
# ---------------------------------------------------------------------------


def test_cell5_unknown_token_exits_16_and_queues_nothing(
    runner, mailbox, monkeypatch, tmp_path
):
    """The ladder widens what 'resolves' means; a full miss still refuses."""
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    res = runner.invoke(app, ["mail", "send", "deadbeef", "hi", "--from-name", "web"])

    assert res.exit_code == 16, res.output
    assert "queued (durable)" not in res.output


def test_cell5_every_source_is_consulted_before_the_refusal(
    runner, mailbox, monkeypatch, tmp_path
):
    """AC5-ERR: the refusal must come from exhaustion, not from a short circuit.

    Without this, a future change could quietly stop consulting the disk stores
    and the only symptom would be mail that mysteriously stopped arriving.
    """
    consulted: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a: (consulted.append("socket"), False)[1],
    )

    from fno.agents import discover

    real_reachable = discover.resolve_reachable
    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *a, **k: (consulted.append("disk-stores"), real_reachable(*a, **k))[1],
    )

    res = runner.invoke(app, ["mail", "send", "deadbeef", "hi", "--from-name", "web"])

    assert res.exit_code == 16
    assert "socket" in consulted, "the socket was never probed before refusing"
    assert "disk-stores" in consulted, "the disk stores were never consulted"


def test_cell5_no_wake_is_attempted_for_an_unknown_token(
    runner, mailbox, monkeypatch, tmp_path
):
    """Never wake a session you could not resolve -- that is how you wake a
    stranger's session."""
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)
    woke = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (woke.append(1), (True, "x"))[1],
    )

    runner.invoke(app, ["mail", "send", "deadbeef", "hi", "--from-name", "web"])

    assert not woke, "an unresolvable token triggered a wake"


# ---------------------------------------------------------------------------
# Cell 6a / 6b: the retired-handle discriminator (PR #491).
# Caller-error refuses; data-artifact migrates. The two directions never blur.
# ---------------------------------------------------------------------------


def test_cell6a_caller_typed_retired_handle_is_refused(
    runner, mailbox, monkeypatch, tmp_path
):
    """Nothing mints the retired ``<harness>-<short8>`` form any more, so a
    typed one is a caller bug worth surfacing rather than silently translating."""
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    res = runner.invoke(
        app, ["mail", "send", f"claude-{LIVE_SID[:8]}", "hi", "--from-name", "web"]
    )

    assert res.exit_code != 0
    combined = res.output + (res.stderr or "")
    assert LIVE_SID[:8] in combined, "the suggestion does not lead with the bare id"
    assert "queued (durable)" not in res.output


def test_cell6a_retired_handle_triggers_no_wake_or_inject(
    runner, mailbox, monkeypatch, tmp_path
):
    """A refusal means nothing happened -- no side effects on any lane."""
    touched: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a: (touched.append("inject"), False)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (touched.append("wake"), (False, "x"))[1],
    )

    runner.invoke(
        app, ["mail", "send", f"claude-{LIVE_SID[:8]}", "hi", "--from-name", "web"]
    )

    assert not touched, f"a refused retired handle still hit lanes: {touched}"


def test_cell6b_retired_form_read_off_a_stored_record_is_migrated(
    runner, mailbox, monkeypatch, tmp_path
):
    """A retired address READ off a stored record is a data artifact, not a
    caller error: migrate to the bare id and deliver through the normal ladder.

    The reply path first shipped this as a refusal -- the same wall class as this
    node's root cause -- and was reversed on PR #491.
    """
    from fno.harness_identity import LEGACY_HANDLE_RE, canonical_handle

    stored = f"claude-{LIVE_SID[:8]}"
    assert LEGACY_HANDLE_RE.fullmatch(stored), "fixture is not the retired form"

    # The migration target is the bare id -- what every live path addresses today.
    assert canonical_handle(LIVE_SID) == LIVE_SID[:8]


# ---------------------------------------------------------------------------
# Cell 7 (AC7-EDGE): self-send and ambiguity. Two ways to wake the wrong thing.
# ---------------------------------------------------------------------------


def test_self_send_queues_durably_without_touching_a_live_lane(
    runner, mailbox, monkeypatch, tmp_path
):
    """A session cannot inject into or wake itself; attempting it is a deadlock
    dressed as a delivery."""
    touched: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a: (touched.append("inject"), True)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (touched.append("wake"), (True, "x"))[1],
    )
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", LIVE_SID)

    res = runner.invoke(app, ["mail", "send", LIVE_SID[:8], "note to self", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert "queued (durable)" in res.output
    assert not touched, f"a self-send hit a live lane: {touched}"
    assert "self-send" in (res.output + (res.stderr or ""))


def test_ambiguous_short_id_wakes_nothing_and_names_both_candidates(
    runner, mailbox, monkeypatch, tmp_path
):
    """Guessing between two sessions that share a short8 wakes a stranger."""
    from fno.agents import discover

    twin_a = "c0ffee11-1111-2222-3333-444444444444"
    twin_b = "c0ffee11-9999-8888-7777-666666666666"
    projects = tmp_path / "projects"
    for sid in (twin_a, twin_b):
        proj = projects / f"-Users-x-{sid[-4:]}"
        proj.mkdir(parents=True, exist_ok=True)
        t = proj / f"{sid}.jsonl"
        t.write_text("{}\n", encoding="utf-8")
        old = time.time() - 7200
        os.utime(t, (old, old))
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: False)

    woke = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (woke.append(1), (True, "x"))[1],
    )

    res = runner.invoke(app, ["mail", "send", "c0ffee11", "hi", "--from-name", "web"])

    assert res.exit_code != 0, res.output
    assert not woke, "an ambiguous short id woke a session"
    combined = res.output + (res.stderr or "")
    assert twin_a in combined and twin_b in combined, "both candidates must be named"


# ---------------------------------------------------------------------------
# AC6-UI: exactly one receipt line on stdout, whatever the outcome.
# ---------------------------------------------------------------------------


CANONICAL_ADDRESS_FORMS = [
    pytest.param("9a063cd3", id="bare-8-hex"),
    pytest.param("9a063cd3-69d4-415a-ada5-649b0164189c", id="full-uuid"),
    pytest.param("footnote-9a063cd3", id="friendly-alias"),
    pytest.param("myproj-a1b2c3d4", id="canonical-handle"),
]


@pytest.mark.parametrize("address", CANONICAL_ADDRESS_FORMS)
def test_no_guard_rejects_a_canonical_address_before_resolution(address):
    """AC11-EDGE: a validator must never swallow an address that would resolve.

    The guard sweep this pins exists because of a real regression class, not a
    hypothetical one: ``_validate_inputs``' short-id-shape guard rejected
    ``fno mail send <bare-8-hex>`` at exit 2, while the send path's fallback to
    handle resolution only fires on exit 16. The canonical address therefore
    never reached resolution at all -- and it was invisible for as long as it was
    because handles used to carry a harness prefix, so nobody sent a bare id.

    Any NEW guard added ahead of resolution that rejects one of these forms
    fails here rather than silently swallowing mail again.
    """
    from fno.agents.dispatch import _validate_inputs

    _validate_inputs(
        name=address, message="hi", from_name="web", name_is_address=True
    )


def test_short_id_shape_guard_still_rejects_a_NAME(monkeypatch):
    """The counterpart: the guard is correct for its actual input class.

    Refusing to NAME an agent like an id prevents a name/id collision, so the
    fix was to scope the guard by caller intent (``name_is_address``), never to
    delete it. Without this assertion the sweep above could be 'satisfied' by
    dropping the guard entirely.
    """
    from fno.agents.dispatch import DispatchAskError, _validate_inputs

    with pytest.raises(DispatchAskError) as err:
        _validate_inputs(name="9a063cd3", message="hi", from_name="web")
    assert err.value.exit_code == 2


def test_full_uuid_fits_the_name_length_ceiling():
    """A 36-char uuid must clear ``_NAME_MAX_LEN``.

    Checked explicitly because the ceiling is a plain constant with no test
    tying it to the address forms it has to admit: lowering it below 36 would
    reject every full-uuid send at exit 2, which is the same swallow-before-
    resolution failure in a different guard.
    """
    from fno.agents.dispatch import _NAME_MAX_LEN

    assert _NAME_MAX_LEN >= 36


@pytest.mark.parametrize(
    "inject_ok,wake,expected",
    [
        (True, None, "delivered (hosted)"),
        (False, (True, "bg-7f3a"), "delivered (woken)"),
        (False, (False, "spawn-exit-1"), "queued (durable)"),
    ],
    ids=["hosted", "woken", "durable"],
)
def test_exactly_one_receipt_line_per_send(
    runner, mailbox, monkeypatch, tmp_path, inject_ok, wake, expected
):
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a: inject_ok)
    if wake is not None:
        monkeypatch.setattr("fno.agents.dispatch.wake_and_deliver", lambda *_a, **_k: wake)

    res = runner.invoke(app, ["mail", "send", ASLEEP_SID[:8], "hi", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    receipts = [
        ln for ln in res.stdout.splitlines()
        if any(m in ln for m in ("delivered (hosted)", "delivered (woken)", "queued (durable)"))
    ]
    assert len(receipts) == 1, f"expected exactly one receipt line, got {receipts}"
    assert expected in receipts[0]
