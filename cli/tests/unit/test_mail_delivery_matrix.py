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
from fno.harness_identity import canonical_handle, legacy_suffix_handle
from fno.paths_testing import use_tmpdir

LIVE_SID = "9a063cd3-69d4-415a-ada5-649b0164189c"
ASLEEP_SID = "5b17e2f0-1c44-4d9a-8e3b-2f6a7c081d55"
LIVE_HANDLE = canonical_handle(LIVE_SID)
# The retired short form read off a pre-flip record is the last-eight
# (legacy_suffix); migration resolves it to the canonical first-eight.
LIVE_LEGACY_PREFIX = legacy_suffix_handle(LIVE_SID)
ASLEEP_HANDLE = canonical_handle(ASLEEP_SID)


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
    from fno.harness_identity import HARNESS_SESSION_MARKERS

    for marker, _harness in HARNESS_SESSION_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    marker = (
        "OPENCODE_SESSION_ID"
        if session_id.startswith("ses_")
        else "CLAUDE_CODE_SESSION_ID"
    )
    monkeypatch.setenv(marker, session_id)
    res = runner.invoke(app, ["agents", "mail", "drain-self", "--json"])
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

    def _inject(recipient, _text, **_k):
        attempts.append(recipient)
        return True

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", _inject)

    res = runner.invoke(app, ["agents", "mail", "send", LIVE_HANDLE, "hi", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert "delivered (hosted)" in res.output
    assert attempts, "the socket was never consulted on a discovery miss"
    # No durable copy: the confirmed inject has only an audit bus record.
    assert "queued (durable)" not in res.output


def test_cell1_inject_body_is_envelope_wrapped(runner, mailbox, monkeypatch, tmp_path):
    """An unwrapped inject renders user-trust-framed -- a spoofing vulnerability
    (repro claude-b9b1f809), not a style miss. Locked Decision 5."""
    bodies: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda _r, text, **_k: (bodies.append(text), True)[1],
    )

    runner.invoke(app, ["agents", "mail", "send", LIVE_HANDLE, "hi", "--from-name", "web"])

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
    # Load mail.cli before patching the store so its module-level reply-path
    # binding cannot capture this test double during lazy command registration.
    import fno.mail.cli  # noqa: F401

    _seed_asleep_transcript(monkeypatch, tmp_path)
    order: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a, **_k: (order.append("inject"), False)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver", lambda *_a, **_k: (False, "spawn-exit-1")
    )
    monkeypatch.setattr(
        "fno.inbox.store.write_new_thread",
        _recording_durable(order),
    )

    runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])

    assert "inject" in order, "the socket rung was skipped entirely"
    assert order.index("inject") < order.index("durable"), (
        f"durable write preceded the inject attempt: {order}"
    )


def _recording_durable(order):
    from fno.inbox.store import write_new_thread as real

    def _wrapped(*args, **kwargs):
        order.append("durable")
        return real(*args, **kwargs)

    return _wrapped


# ---------------------------------------------------------------------------
# Cell 2: asleep session -> wake-and-deliver. Asleep is resumable, not voicemail.
# ---------------------------------------------------------------------------


def test_cell2_asleep_session_is_woken_not_queued(
    runner, mailbox, monkeypatch, tmp_path
):
    _seed_asleep_transcript(monkeypatch, tmp_path)
    # The socket misses: the session is asleep, so it is not on the roster.
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)

    woken: list[tuple[str, str]] = []

    def _wake(session_uuid, wrapped, **_kw):
        woken.append((session_uuid, wrapped))
        return True, "bg-7f3a"

    monkeypatch.setattr("fno.agents.dispatch.wake_and_deliver", _wake)

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "wake up", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert "delivered (woken)" in res.output
    assert woken, "an asleep, disk-resolvable session was never woken"
    assert woken[0][0] == ASLEEP_SID
    assert "queued (durable)" not in res.output


def test_cell2_wake_prompt_is_envelope_wrapped(runner, mailbox, monkeypatch, tmp_path):
    """The waking prompt is the mail. It MUST arrive wrapped for the same reason
    an inject must -- an unwrapped seed prompt renders as user-trusted text."""
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)

    seeds: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda _sid, wrapped, **_kw: (seeds.append(wrapped), (True, "bg-7f3a"))[1],
    )

    runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "wake up", "--from-name", "web"])

    assert seeds, "nothing was sent as a wake prompt"
    assert "<fno_mail" in seeds[0]


def test_cell2_receipt_names_the_revived_thread(runner, mailbox, monkeypatch, tmp_path):
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver", lambda *_a, **_k: (True, "bg-7f3a")
    )

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])

    assert "bg-7f3a" in res.output, "the receipt does not name the revived thread"


# ---------------------------------------------------------------------------
# Cell 4: every applicable live rung attempted and failed -> durable + receipt.
# Durable is a DEMOTION, never a first answer.
# ---------------------------------------------------------------------------


def test_cell4_failed_wake_demotes_durably_with_lane_receipt(
    runner, mailbox, monkeypatch, tmp_path
):
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (False, "spawn-exit-1"),
    )

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])

    # Exit 0: the envelope is safe even though every live lane missed.
    assert res.exit_code == 0, res.output
    assert "queued (durable)" in res.output
    combined = res.output + (res.stderr or "")
    assert "spawn-exit-1" in combined, "the receipt does not name why the wake failed"
    # Addressed to the canonical handle the recipient's own drain reads.
    assert _drain_as(runner, monkeypatch, ASLEEP_SID), "durable copy is not drainable"


def test_cell3_live_pane_miss_retries_once_and_names_both_attempts(monkeypatch):
    from types import SimpleNamespace

    import fno.agents.dispatch as dispatch_mod

    entry = SimpleNamespace(
        name="worker",
        harness="claude",
        mux={"session": "main", "pane_id": 7},
        exited=False,
    )
    attempts: list[int] = []
    reasons: list[str] = []
    monkeypatch.setattr(
        dispatch_mod,
        "_mux_pane_send",
        lambda *_args, **kwargs: (
            attempts.append(1),
            kwargs["failure_out"].append("pre-submit"),
            False,
        )[-1],
    )

    assert dispatch_mod._deliver_live(
        entry,
        "hi",
        "sender",
        reason_out=reasons,
    ) is False
    assert len(attempts) == 2
    assert reasons == [
        "mux-send-failed-attempt-1:pre-submit",
        "mux-send-failed-attempt-2:pre-submit",
    ]


def test_cell3_unconfirmed_mux_failure_does_not_retry(monkeypatch):
    from types import SimpleNamespace

    import fno.agents.dispatch as dispatch_mod

    entry = SimpleNamespace(
        name="worker",
        harness="claude",
        mux={"session": "main", "pane_id": 7},
        exited=False,
    )
    attempts: list[int] = []
    reasons: list[str] = []

    def _unconfirmed(*_args, **kwargs):
        attempts.append(1)
        kwargs["failure_out"].append("unconfirmed")
        return False

    monkeypatch.setattr(dispatch_mod, "_mux_pane_send", _unconfirmed)
    assert dispatch_mod._deliver_live(
        entry, "hi", "sender", reason_out=reasons
    ) is False
    assert len(attempts) == 1
    assert reasons == ["mux-send-failed-attempt-1:unconfirmed"]


def test_cell4_receipt_names_every_failed_lane(runner, mailbox, monkeypatch, tmp_path):
    """A delivery bug must be diagnosable from the sender's terminal alone."""
    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (False, "writer-possibly-live"),
    )

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])
    combined = res.output + (res.stderr or "")

    assert "inject=" in combined, "the inject lane failure is unnamed"
    assert "wake=" in combined, "the wake lane failure is unnamed"


def test_cell4_inject_boundary_parses_the_reason_side_channel(monkeypatch):
    """x-1904 change 4: the Python boundary must not discard the verb's reason.

    The Rust mail-inject verb emits ``{"delivered": bool, "reason": str}`` with
    a precise vocabulary; the boundary used to read only the bool. The
    side-channel must carry the token on every outcome, including the
    boundary's own failure modes (no binary, unparseable stdout).
    """
    from types import SimpleNamespace

    import fno.agents.dispatch as dispatch_mod

    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: "/bin/true")

    def _verb(stdout):
        def _run(_argv, **_k):
            return SimpleNamespace(returncode=0, stdout=stdout, stderr="")

        return _run

    reason: list = []
    monkeypatch.setattr(
        dispatch_mod.subprocess, "run", _verb('{"delivered": false, "reason": "attach-failed"}')
    )
    assert dispatch_mod._mail_inject_claude("ses-1", "hi", reason_out=reason) is False
    assert reason == ["attach-failed"]

    reason.clear()
    monkeypatch.setattr(
        dispatch_mod.subprocess, "run", _verb('{"delivered": true, "reason": "delivered"}')
    )
    assert dispatch_mod._mail_inject_claude("ses-1", "hi", reason_out=reason) is True
    assert reason == ["delivered"]

    # Unparseable stdout names the boundary, never silently reverts to nothing.
    reason.clear()
    monkeypatch.setattr(dispatch_mod.subprocess, "run", _verb("not json"))
    assert dispatch_mod._mail_inject_claude("ses-1", "hi", reason_out=reason) is False
    assert reason == ["unreadable"]

    # No reason_out: the plain-bool contract is unchanged for legacy callers.
    monkeypatch.setattr(
        dispatch_mod.subprocess, "run", _verb('{"delivered": false, "reason": "not-confirmed"}')
    )
    assert dispatch_mod._mail_inject_claude("ses-1", "hi") is False


def test_cell4_receipt_carries_the_inject_reason_token(runner, mailbox, monkeypatch, tmp_path):
    """x-1904 change 4: a durable demotion names the live lane's own cause.

    The inject boundary already emits a precise reason vocabulary
    (not-confirmed / attach-failed / io-error / ...); Python used to discard
    it, so a miss to a LIVE recipient read as a bare live-miss -- and the
    _warn_deferred boilerplate called the recipient "not live", a receipt
    naming the wrong cause (it cost a wrong liveness hypothesis on measured
    evidence). The receipt must carry the token, and the preamble must not
    claim not-live when the lane itself says it missed a live recipient.
    """

    def _miss(recipient, text, *, sender=None, reason_out=None):
        if reason_out is not None:
            reason_out.append("attach-failed")
        return False

    _seed_asleep_transcript(monkeypatch, tmp_path)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", _miss)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (False, "spawn-exit-1"),
    )

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])
    combined = res.output + (res.stderr or "")

    assert res.exit_code == 0, combined
    assert "queued (durable)" in combined
    assert "[attach-failed]" in combined, (
        f"the receipt must name the inject's own reason, not a generic live-miss: {combined}"
    )
    assert "is not live" not in combined, (
        "the durable preamble must not claim not-live for a lane failure the "
        f"reason token names: {combined}"
    )


# ---------------------------------------------------------------------------
# Cell 5: unknown token -> exit 16. The typo guard survives the widened ladder.
# ---------------------------------------------------------------------------


def test_cell5_unknown_token_exits_16_and_queues_nothing(
    runner, mailbox, monkeypatch, tmp_path
):
    """The ladder widens what 'resolves' means; a full miss still refuses."""
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)

    res = runner.invoke(app, ["agents", "mail", "send", "deadbeef", "hi", "--from-name", "web"])

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
        lambda *_a, **_k: (consulted.append("socket"), False)[1],
    )

    from fno.agents import discover

    real_reachable = discover.resolve_reachable
    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *a, **k: (consulted.append("disk-stores"), real_reachable(*a, **k))[1],
    )

    res = runner.invoke(app, ["agents", "mail", "send", "deadbeef", "hi", "--from-name", "web"])

    assert res.exit_code == 16
    assert "socket" in consulted, "the socket was never probed before refusing"
    assert "disk-stores" in consulted, "the disk stores were never consulted"


def test_cell5_no_wake_is_attempted_for_an_unknown_token(
    runner, mailbox, monkeypatch, tmp_path
):
    """Never wake a session you could not resolve -- that is how you wake a
    stranger's session."""
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    woke = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (woke.append(1), (True, "x"))[1],
    )

    runner.invoke(app, ["agents", "mail", "send", "deadbeef", "hi", "--from-name", "web"])

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
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)

    res = runner.invoke(
        app, ["mail", "send", f"claude-{LIVE_LEGACY_PREFIX}", "hi", "--from-name", "web"]
    )

    assert res.exit_code != 0
    combined = res.output + (res.stderr or "")
    assert LIVE_LEGACY_PREFIX in combined, "the suggestion does not lead with the bare id"
    assert "queued (durable)" not in res.output


def test_cell6a_retired_handle_triggers_no_wake_or_inject(
    runner, mailbox, monkeypatch, tmp_path
):
    """A refusal means nothing happened -- no side effects on any lane."""
    touched: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a, **_k: (touched.append("inject"), False)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (touched.append("wake"), (False, "x"))[1],
    )

    runner.invoke(
        app, ["mail", "send", f"claude-{LIVE_LEGACY_PREFIX}", "hi", "--from-name", "web"]
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
    from fno.bus.log import iter_messages
    from fno.inbox.store import write_new_thread

    _seed_asleep_transcript(monkeypatch, tmp_path, session_id=LIVE_SID)
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (False, "wake-refused"),
    )
    inbound = write_new_thread(
        recipient="meeeeeee",
        sender=LIVE_LEGACY_PREFIX,
        kind="send",
        body="ping",
        to_kind="name",
    )

    result = runner.invoke(
        app,
        ["mail", "reply", "--to", inbound.thread_id, "--body", "ack"],
    )

    assert result.exit_code == 0, result.output
    replies = [m for m in iter_messages() if m.in_reply_to == inbound.thread_id]
    assert len(replies) == 1
    assert replies[0].to == LIVE_HANDLE
    drained = _drain_as(runner, monkeypatch, LIVE_SID)
    assert any(message["id"] == replies[0].id for message in drained)


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
        lambda *_a, **_k: (touched.append("inject"), True)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (touched.append("wake"), (True, "x"))[1],
    )
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", LIVE_SID)

    res = runner.invoke(app, ["agents", "mail", "send", LIVE_HANDLE, "note to self", "--from-name", "web"])

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
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)

    woke = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (woke.append(1), (True, "x"))[1],
    )

    res = runner.invoke(app, ["agents", "mail", "send", "c0ffee11", "hi", "--from-name", "web"])

    assert res.exit_code != 0, res.output
    assert not woke, "an ambiguous short id woke a session"
    combined = res.output + (res.stderr or "")
    assert twin_a in combined and twin_b in combined, "both candidates must be named"


# ---------------------------------------------------------------------------
# AC6-UI: exactly one receipt line on stdout, whatever the outcome.
# ---------------------------------------------------------------------------


def test_cell1_codex_is_probed_too_when_nothing_resolved(
    runner, mailbox, monkeypatch, tmp_path
):
    """The discovery-miss probe must not be claude-only.

    A live codex session that live-discovery misses is the same blind spot this
    node fixes for claude: with no store record there is no harness to read off,
    so both injectors are tried. Both are cheap and side-effect-free on a miss.
    """
    tried: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a, **_k: (tried.append("claude"), False)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_codex",
        lambda *_a, **_k: (tried.append("codex"), True)[1],
    )

    res = runner.invoke(app, ["agents", "mail", "send", LIVE_HANDLE, "hi", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert tried == ["claude", "codex"], f"probe order/coverage wrong: {tried}"
    assert "delivered (hosted)" in res.output
    assert "queued (durable)" not in res.output


def test_a_resolved_codex_session_is_probed_on_its_own_harness(
    runner, mailbox, monkeypatch, tmp_path
):
    """When a store DID record the harness, probe that one rather than both."""
    from fno.agents import discover

    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *_a, **_k: (
            discover.ReachableSession(
                session_id=ASLEEP_SID, source="registry", agent="codex"
            ),
            [],
        ),
    )
    tried: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda *_a, **_k: (tried.append("claude"), False)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_codex",
        lambda *_a, **_k: (tried.append("codex"), True)[1],
    )

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert tried == ["codex"], f"a resolved codex session was probed as claude: {tried}"


def test_an_unreadable_store_refuses_short_token_without_durable_write(
    runner, mailbox, monkeypatch, tmp_path
):
    """Unreadable evidence cannot authorize any short-token side effect."""
    from fno.agents import discover

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *_a, **_k: (_ for _ in ()).throw(discover.StoreReadError(["transcript"])),
    )

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])

    assert res.exit_code == 2, res.output
    assert "queued (durable)" not in res.output
    assert "unreadable stores: transcript" in (res.output + (res.stderr or ""))
    assert "full session id" in (res.output + (res.stderr or ""))
    assert not _drain_as(runner, monkeypatch, ASLEEP_SID)


def test_an_unreadable_store_never_injects_into_an_unproven_candidate(
    runner, mailbox, monkeypatch, tmp_path
):
    """A lone visible candidate is not unique while any store is unreadable."""
    from fno.agents import discover

    candidate = discover.ReachableSession(
        session_id=ASLEEP_SID,
        source="transcript",
        agent="claude",
    )
    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *_a, **_k: (_ for _ in ()).throw(
            discover.StoreReadError(["registry"], resolved=candidate)
        ),
    )
    attempted: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda session_id, _message, **_k: (attempted.append(session_id), True)[1],
    )
    monkeypatch.setattr(
        "fno.mail.cli._wake_rung",
        lambda *_a, **_k: pytest.fail("unproven candidate was woken"),
    )

    res = runner.invoke(
        app,
        ["mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"],
    )

    assert res.exit_code == 2, res.output
    assert attempted == []
    assert "queued (durable)" not in res.output
    assert "unreadable stores: registry" in (res.output + (res.stderr or ""))
    assert ASLEEP_SID in (res.output + (res.stderr or ""))
    assert "full session id" in (res.output + (res.stderr or ""))
    assert not _drain_as(runner, monkeypatch, ASLEEP_SID)


def test_unreadable_store_still_allows_an_exact_full_session_id(
    runner, mailbox, monkeypatch, tmp_path
):
    """A complete id identifies one session even when another store is unreadable."""
    from fno.agents import discover

    candidate = discover.ReachableSession(
        session_id=ASLEEP_SID,
        source="transcript",
        agent="claude",
    )
    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *_a, **_k: (_ for _ in ()).throw(
            discover.StoreReadError(["registry"], resolved=candidate)
        ),
    )
    attempted: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda session_id, _message, **_k: (attempted.append(session_id), True)[1],
    )

    res = runner.invoke(
        app,
        ["mail", "send", ASLEEP_SID, "hi", "--from-name", "web"],
    )

    assert res.exit_code == 0, res.output
    assert attempted == [ASLEEP_SID]
    assert "delivered (hosted)" in res.output
    assert "queued (durable)" not in res.output


@pytest.mark.parametrize(
    ("send_id", "drain_id"),
    [
        pytest.param(ASLEEP_SID, ASLEEP_SID, id="uuid"),
        pytest.param(ASLEEP_SID.upper(), ASLEEP_SID, id="uppercase-uuid"),
        pytest.param(
            "ses_AaBbCcDdEeFf001122",
            "ses_AaBbCcDdEeFf001122",
            id="opencode",
        ),
    ],
)
def test_unreadable_store_full_id_live_miss_queues_to_drainable_full_id(
    runner, mailbox, monkeypatch, tmp_path, send_id, drain_id
):
    """A full-id live miss persists under the full id (the collision escape),
    which drain-self reads via its full-id address form."""
    from fno.agents import discover
    from fno.harness_identity import session_identity_key

    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *_a, **_k: (_ for _ in ()).throw(
            discover.StoreReadError(["registry"])
        ),
    )
    attempted: list[tuple[str, str]] = []
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_claude",
        lambda target, _message, **_k: (attempted.append(("claude", target)), False)[1],
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mail_inject_codex",
        lambda target, _message, **_k: (attempted.append(("codex", target)), False)[1],
    )

    res = runner.invoke(
        app,
        ["mail", "send", send_id, "hi", "--from-name", "web"],
    )

    assert res.exit_code == 0, res.output
    assert attempted == [("claude", send_id), ("codex", send_id)]
    expected_handle = session_identity_key(drain_id)
    assert f"queued (durable) for {expected_handle}" in res.output
    drained = _drain_as(runner, monkeypatch, drain_id)
    assert len(drained) == 1
    assert drained[0]["to"] == expected_handle


def test_a_non_claude_session_is_not_woken_as_claude(
    runner, mailbox, monkeypatch, tmp_path
):
    """Wake is claude-only (a locked scope decision).

    The registry and graph sources hold rows for every provider, so a codex
    thread id can resolve here. Feeding it to a claude resume would revive the
    wrong session; the honest answer is a durable copy naming the reason.
    """
    from fno.agents import discover

    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    monkeypatch.setattr(
        discover,
        "resolve_reachable",
        lambda *_a, **_k: (
            discover.ReachableSession(
                session_id=ASLEEP_SID, source="registry", agent="codex"
            ),
            [],
        ),
    )
    woke = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_and_deliver",
        lambda *_a, **_k: (woke.append(1), (True, "x"))[1],
    )

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    assert not woke, "a codex session was handed to a claude resume"
    assert "unsupported-harness" in (res.output + (res.stderr or ""))
    assert "queued (durable)" in res.output


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
    ``fno agents mail send <bare-8-hex>`` at exit 2, while the send path's fallback to
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
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: inject_ok)
    if wake is not None:
        monkeypatch.setattr("fno.agents.dispatch.wake_and_deliver", lambda *_a, **_k: wake)

    res = runner.invoke(app, ["agents", "mail", "send", ASLEEP_HANDLE, "hi", "--from-name", "web"])

    assert res.exit_code == 0, res.output
    receipts = [
        ln for ln in res.stdout.splitlines()
        if any(m in ln for m in ("delivered (hosted)", "delivered (woken)", "queued (durable)"))
    ]
    assert len(receipts) == 1, f"expected exactly one receipt line, got {receipts}"
    assert expected in receipts[0]

    from fno.bus.cursor import scan_unread
    from fno.bus.log import iter_messages

    rows = [m for m in iter_messages() if m.from_ == "web"]
    assert len(rows) == 1
    if expected == "queued (durable)":
        assert rows[0].delivery is None
        assert [m.id for m in scan_unread(rows[0].to)] == [rows[0].id]
    else:
        assert rows[0].delivery == "hosted"
        assert scan_unread(rows[0].to) == []
