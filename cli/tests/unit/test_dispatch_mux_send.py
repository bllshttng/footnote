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

import json
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


def _runner(calls, *, screen="", verdict=None, returncode=0):
    """A fake ``subprocess.run`` covering the three verbs this lane issues.

    ``verdict`` is what the manifest engine answers for the frame; ``None``
    means the default no-match. Pass ``{"error": ...}`` to model a detector that
    could not run at all, which is a different answer from "no prompt showing".
    """
    payload = json.dumps(verdict if verdict is not None else {"matched": False})

    def run(argv, **kwargs):
        calls.append({"argv": list(argv), "input": kwargs.get("input")})
        if "manifest-eval" in argv:
            return SimpleNamespace(returncode=0, stdout=payload, stderr="")
        if argv[1:4] == ["mux", "pane", "read"]:
            return SimpleNamespace(returncode=0, stdout=screen, stderr="")
        return SimpleNamespace(returncode=returncode, stdout="", stderr="")

    return run


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

    assert dispatch._mux_pane_send(_entry(), "status?", guarded=False) is True

    paste = _pasted(calls)
    assert paste.startswith("<fno_mail from=")
    assert paste.rstrip().endswith("</fno_mail>")
    assert "status?" in paste
    assert "peer mail" in paste, "the authority trailer is the point, not decoration"


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
        _runner(
            calls,
            screen="Do you want to proceed?\n 1. Yes\n 2. No",
            verdict={
                "matched": True,
                "rule_id": "claude-permission-prompt",
                "answerable": {"options": ["1", "2"], "fingerprint": "abc"},
            },
        ),
    )
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    assert dispatch._mux_pane_send(_entry(), "take option 3", guarded=False) is False
    assert "send" not in _verbs(calls), "nothing may be typed at a showing prompt"
    assert "claude-permission-prompt" in capsys.readouterr().err


def test_an_unrunnable_detector_refuses_rather_than_typing_blind(monkeypatch, capsys):
    """An instrument that never ran is not an idle pane.

    The absence of a detected prompt has two explanations and a send built on
    one cannot tell them apart, so the unmeasurable case refuses.
    """
    calls: list[dict] = []
    monkeypatch.setattr(
        dispatch.subprocess,
        "run",
        _runner(calls, verdict={"matched": False, "error": "manifest-eval unavailable"}),
    )
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    assert dispatch._mux_pane_send(_entry(), "hello", guarded=False) is False
    assert "send" not in _verbs(calls)
    err = capsys.readouterr().err
    assert "manifest-eval unavailable" in err
    assert "refusing to type blind" in err


def test_an_already_wrapped_body_is_not_wrapped_twice(monkeypatch):
    """The mail lane hands this function an already-enveloped body. Re-wrapping
    would nest one attribution inside another, and the renderer refuses a body
    holding an `<fno_mail>` tag anyway."""
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

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

    entry = _entry()
    entry.harness = harness
    dispatch._mux_pane_send(entry, "status?", guarded=False)

    evals = [c["argv"] for c in calls if "manifest-eval" in c["argv"]]
    assert evals, "the gate must run"
    assert evals[0][evals[0].index("--harness") + 1] == harness
