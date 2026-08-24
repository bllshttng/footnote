"""The enveloped pane lane: wrap by default, ``raw`` opt-out, prompt refusal.

Node x-3a64. A pane drive types at a worker's prompt, and unwrapped it is
indistinguishable from the operator typing. These tests pin the three decisions
that changed: what a default send puts on the wire, that ``raw=True`` is
byte-identical to the old behavior, and that a pane showing an option prompt is
refused rather than typed into.

Deliberately NO module-level "idle pane" fixture (the two legacy pane modules
have one so their transport assertions still run). Here the gate is the subject.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

import fno.agents.dispatch as dispatch


def _entry():
    return SimpleNamespace(
        mux={"session": "main", "pane_id": 7},
        harness="claude",
        harness_session_id="worker-session",
        session_id=None,
        cwd="/w",
        status="live",
    )


def _runner(calls, *, screen="", returncode=0):
    """A fake ``subprocess.run`` covering the pane verbs this lane issues."""

    def run(argv, **kwargs):
        calls.append({"argv": list(argv), "input": kwargs.get("input")})
        if argv[1:4] == ["mux", "pane", "read"]:
            return SimpleNamespace(returncode=0, stdout=screen, stderr="")
        return SimpleNamespace(returncode=returncode, stdout="", stderr="")

    return run


def _detector(monkeypatch, asked, verdict=None):
    """Stub the manifest engine and record the harness it was asked about.

    Patched at :func:`fno.agents.mux_spawn._evaluate_manifest_screen` rather
    than through ``subprocess.run``, because that function resolves an INSTALLED
    ``fno-agents`` binary and returns ``{"error": "manifest-eval binary
    unavailable"}`` without ever shelling out when there is none. CI has none, so
    a subprocess-level stub silently never ran there and every gate assertion
    read the unavailable branch instead of the one it named.

    ``verdict=None`` is the default no-match. Pass ``{"error": ...}`` to model a
    detector that could not run at all, which is a different answer from "no
    prompt showing".
    """
    answer = verdict if verdict is not None else {"matched": False}

    def _eval(harness, screen, runner=None, **_kwargs):
        asked.append({"harness": harness, "screen": screen})
        return answer

    monkeypatch.setattr("fno.agents.mux_spawn._evaluate_manifest_screen", _eval)
    return asked


def _pasted(calls):
    for call in calls:
        if "--stdin" in call["argv"]:
            return call["input"] or ""
    return ""


def _verbs(calls):
    return [
        c["argv"][3] for c in calls if c["argv"][1:3] == ["mux", "pane"] and len(c["argv"]) > 3
    ]


def test_default_send_wraps_the_body_in_an_fno_mail_envelope(monkeypatch):
    """The paste opens with `<fno_mail from=` and closes with the trailer.

    This is the whole node in one assertion: a worker reading the paste can tell
    it came from a peer, whichever transport typed it.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(monkeypatch, [])

    assert dispatch._mux_pane_send(_entry(), "status?", guarded=False) is True

    paste = _pasted(calls)
    assert paste.startswith("<fno_mail from=")
    assert paste.rstrip().endswith("</fno_mail>")
    assert "status?" in paste
    assert "peer mail" in paste, "the authority trailer is the point, not decoration"


def test_read_receipt_identity_mismatch_refuses_before_typing(monkeypatch, capsys):
    calls: list[dict] = []
    entry = _entry()
    entry.name = "worker"
    monkeypatch.setattr(
        "fno.mail.pane_transport._pane_entry",
        lambda _session, _pane: entry,
    )
    _detector(monkeypatch, [])

    def run(argv, **kwargs):
        calls.append({"argv": list(argv), "input": kwargs.get("input")})
        if argv[1:4] == ["mux", "pane", "read"]:
            return SimpleNamespace(
                returncode=0,
                stdout='{"pane_id":7,"text":"$ ","pane_name":"caller",'
                '"registry_fno_id":"caller-session"}',
                stderr="",
            )
        if argv[1:4] == ["mux", "pane", "ls"]:
            return SimpleNamespace(
                returncode=0,
                stdout='[{"pane_id":7,"name":"worker","fno_id":"worker-session"}]',
                stderr="",
            )
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(dispatch.subprocess, "run", run)
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    assert dispatch._mux_pane_send(entry, "status?", guarded=False) is False
    assert not any(call["argv"][1:4] == ["mux", "pane", "send"] for call in calls)
    assert "identity mismatch" in capsys.readouterr().err


def test_read_receipt_without_text_refuses_before_typing(monkeypatch, capsys):
    calls: list[dict] = []
    entry = _entry()
    entry.name = "worker"
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    def run(argv, **kwargs):
        calls.append({"argv": list(argv), "input": kwargs.get("input")})
        if argv[1:4] == ["mux", "pane", "ls"]:
            return SimpleNamespace(
                returncode=0,
                stdout='[{"pane_id":7,"name":"worker","fno_id":"worker-session"}]',
                stderr="",
            )
        if argv[1:4] == ["mux", "pane", "read"]:
            return SimpleNamespace(
                returncode=0,
                stdout='{"pane_id":7,"pane_name":"worker",'
                '"registry_fno_id":"worker-session"}',
                stderr="",
            )
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(dispatch.subprocess, "run", run)
    _detector(monkeypatch, [])

    failure: list[str] = []
    assert (
        dispatch._mux_pane_send(
            entry, "status?", guarded=False, failure_out=failure
        )
        is False
    )
    assert failure == ["pre-submit"]
    assert not any(call["argv"][1:4] == ["mux", "pane", "send"] for call in calls)
    assert "text" in capsys.readouterr().err


def test_raw_send_is_byte_identical_and_still_audits(monkeypatch):
    """`raw=True` types exactly the caller's bytes and keeps the audit row.

    The `agent_raw_inject` ledger row exists to mark a send that reaches a
    prompt with no attribution. Since the default is enveloped, `raw` is the
    only way to produce one - so the row keeps its meaning instead of firing on
    every routine peer message.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    seen: list = []
    monkeypatch.setattr(
        dispatch.events, "daemon_lifecycle_log", lambda: "/dev/null", raising=False
    )
    import fno.events as events_mod

    monkeypatch.setattr(
        events_mod, "append_event", lambda event, path, **_k: seen.append(event)
    )

    assert dispatch._mux_pane_send(_entry(), "1", guarded=False, raw=True) is True
    assert _pasted(calls) == "1"
    assert [e["type"] for e in seen] == ["agent_raw_inject"]


def test_raw_send_skips_the_read_back(monkeypatch):
    """A raw send never reads the pane.

    Not an optimization: `_send_permission_response` types a digit to ANSWER a
    showing prompt, so gating raw would break the one caller that requires the
    prompt to be there.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    dispatch._mux_pane_send(_entry(), "2", guarded=False, raw=True)
    assert "read" not in _verbs(calls)


def test_a_showing_option_prompt_refuses_and_types_nothing(monkeypatch, capsys):
    """A submit against a showing prompt dismisses the payload and selects the
    highlighted default. Verified specimen: a king's option-3 ruling was typed,
    discarded, and the worker took option 1."""
    calls: list[dict] = []
    monkeypatch.setattr(
        dispatch.subprocess,
        "run",
        _runner(calls, screen="Do you want to proceed?\n 1. Yes\n 2. No"),
    )
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(
        monkeypatch,
        [],
        {
            "matched": True,
            "rule_id": "claude-permission-prompt",
            "answerable": {"options": ["1", "2"], "fingerprint": "abc"},
        },
    )

    assert dispatch._mux_pane_send(_entry(), "take option 3", guarded=False) is False
    assert "send" not in _verbs(calls), "nothing may be typed at a showing prompt"
    assert "claude-permission-prompt" in capsys.readouterr().err


def test_an_unrunnable_detector_refuses_rather_than_typing_blind(monkeypatch, capsys):
    """An instrument that never ran is not an idle pane.

    The absence of a detected prompt has two explanations and a send built on
    one cannot tell them apart, so the unmeasurable case refuses.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(monkeypatch, [], {"matched": False, "error": "manifest-eval unavailable"})

    assert dispatch._mux_pane_send(_entry(), "hello", guarded=False) is False
    assert "send" not in _verbs(calls)
    err = capsys.readouterr().err
    assert "manifest-eval unavailable" in err
    assert "refusing to type blind" in err


def test_an_oserror_reading_the_pane_refuses_instead_of_raising(monkeypatch, capsys):
    """A frame read can fail with an OSError the mux helper does not translate.

    `_run_mux` converts FileNotFoundError and TimeoutExpired into a
    DispatchAskError and nothing else, so a PermissionError on a bad FNO_BIN
    (or ENOEXEC, or EMFILE) came back raw. The only caller catches
    PaneSendRefused, so that killed the send with a traceback where an
    unreadable frame demotes to the durable bus. Both facts are identical:
    nothing looked at the pane.
    """
    calls: list[dict] = []

    def run(argv, **kwargs):
        calls.append({"argv": list(argv), "input": kwargs.get("input")})
        if argv[1:4] == ["mux", "pane", "read"]:
            raise PermissionError(13, "Permission denied")
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(dispatch.subprocess, "run", run)
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(monkeypatch, [])

    # Returns False rather than propagating: the assertion is the absence of a
    # raised PermissionError as much as the value.
    assert dispatch._mux_pane_send(_entry(), "hello", guarded=False) is False
    assert "send" not in _verbs(calls)
    err = capsys.readouterr().err
    assert "Permission denied" in err
    assert "refusing to type blind" in err


def test_an_already_wrapped_body_is_not_wrapped_twice(monkeypatch):
    """The mail lane hands this function an already-enveloped body. Re-wrapping
    would nest one attribution inside another, and the renderer refuses a body
    holding an `<fno_mail>` tag anyway."""
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(monkeypatch, [])

    body = '<fno_mail from="peer" harness="claude-code" model="m">hi</fno_mail>'
    assert dispatch._mux_pane_send(_entry(), body, guarded=False) is True
    assert _pasted(calls) == body


def test_every_pane_verb_this_lane_issues_is_raw(monkeypatch):
    """The lane prepares its own bytes, so the Rust verb must type them
    verbatim. A second preparation pass would re-read the pane and re-decide a
    question this one already answered."""
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(monkeypatch, [])

    dispatch._mux_pane_send(_entry(), "status?", guarded=False)

    sends = [c["argv"] for c in calls if c["argv"][1:4] == ["mux", "pane", "send"]]
    assert sends, "the lane must actually send"
    for argv in sends:
        assert "--raw" in argv, argv


@pytest.mark.parametrize("harness", ["claude", "codex"])
def test_the_gate_asks_about_the_recipients_own_harness(monkeypatch, harness):
    """A prompt looks different per harness, so the detector is asked with the
    RECIPIENT's harness, never the sender's."""
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    asked: list[dict] = []
    _detector(monkeypatch, asked)

    entry = _entry()
    entry.harness = harness
    dispatch._mux_pane_send(entry, "status?", guarded=False)

    assert asked, "the gate must run"
    assert asked[0]["harness"] == harness


def test_the_pane_drive_envelope_carries_an_id_to_reply_to(monkeypatch):
    """A sender with no reply handle closes three of the four defects, not four.

    A bare pane drive writes no bus row, and it does not need one:
    `resolve_live_sender` recovers a sender off the transcript for an id the bus
    never saw, and it reads `from_session` first, so what comes back is the
    collision-safe address rather than the head-8.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(monkeypatch, [])

    dispatch._mux_pane_send(_entry(), "status?", guarded=False)

    pasted = _pasted(calls)
    assert 'id="msg-' in pasted, pasted
    # The id has to be quotable, so it must survive into what is actually typed.
    import re

    msg_id = re.search(r'id="(msg-[^"]+)"', pasted).group(1)
    assert msg_id in pasted


def test_a_blocked_pane_with_no_answer_grammar_is_still_refused(monkeypatch, capsys):
    """`answerable` is a SUBSET of blocked, and the wrong half to gate on.

    `evaluate_answerable` returns nothing for a matched blocked rule carrying no
    answer grammar, or one whose options failed to parse: a codex auth wall, a
    trust prompt whose menu did not render as a list. Those are the panes most
    in need of the refusal, and gating on `answerable` alone sent every one of
    them a paste and a CR. Spawn readiness already tests `state == "blocked"`,
    and the transport doc states that same contract.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(
        monkeypatch,
        [],
        {"matched": True, "state": "blocked", "rule_id": "auth_wall"},
    )

    assert dispatch._mux_pane_send(_entry(), "hello", guarded=False) is False
    assert "send" not in _verbs(calls)
    err = capsys.readouterr().err
    assert "auth_wall" in err
    assert "blocking prompt" in err


def test_a_non_mail_payload_reaches_the_pane_verbatim(monkeypatch):
    """`mail=None` means this is NOT a2a mail, so it must land unwrapped.

    Three separate failures came out of enveloping it. A busy-hold digest
    EMBEDS `<fno_mail>` bodies and `wrap_fno_mail` refuses a body holding one,
    so every hold release to a pane raised and the cursor never advanced. A
    post-merge ritual arrived dressed as chat, and the caller's True suppressed
    the cold-dispatch fallback, losing it silently. And the auto-wrap stamped
    the dispatching process's handle instead of the declared `from_name`, so the
    envelope named the wrong peer.

    Driven through `_deliver_live` rather than `_mux_pane_send`, because the
    defect was in what that caller passes, and a test that calls the inner
    function directly cannot see it.
    """
    # A digest-shaped body: it CONTAINS an envelope rather than being one.
    body = 'held for you:\n<fno_mail from="peer" harness="claude" model="m">hi</fno_mail>'
    calls: list[dict] = []
    # The read-back that CONFIRMS the paste has to see the payload on screen.
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls, screen=body))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(monkeypatch, [])
    monkeypatch.setattr(
        dispatch, "_delivery_policy_refusal", lambda _e: None, raising=False
    )

    # The return value does NOT discriminate: the enveloped bug also reported
    # success, which is precisely how a lost ritual went unnoticed. What the
    # pane actually received is the evidence.
    dispatch._deliver_live(_entry(), body, "fno-mail-hold")

    pasted = _pasted(calls)
    assert body in pasted, pasted
    # No SECOND envelope wrapped around the digest, and nothing stamped with
    # this process's handle in place of the declared sender.
    assert not pasted.lstrip().startswith("<fno_mail from="), pasted


def test_a_non_mail_payload_is_verbatim_but_still_gated(monkeypatch, capsys):
    """Verbatim and ungated are DIFFERENT requests, and only one was asked for.

    `prepare` does two jobs: it wraps, and it refuses a pane showing a prompt.
    Skipping it wholesale to keep a ritual or a digest byte-exact also skipped
    the gate. On a codex auth wall the CR then takes the wall's default, the
    payload is discarded, and the bytes-written verdict still reads True, so a
    hold release advances the cursor and retires every held message unread.

    Only a keystroke ANSWERING a prompt wants the gate off.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    _detector(
        monkeypatch, [], {"matched": True, "state": "blocked", "rule_id": "auth_wall"}
    )
    monkeypatch.setattr(
        dispatch, "_delivery_policy_refusal", lambda _e: None, raising=False
    )

    assert dispatch._deliver_live(_entry(), "run the ritual", "fno") is False
    assert "send" not in _verbs(calls), "a blocked pane must receive nothing"
    assert "auth_wall" in capsys.readouterr().err
