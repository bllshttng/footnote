"""One audit row per prompt write (x-91ba): the mail lane declares, the floor writes.

The audit row for a pane write is written by the ``fno mux pane send`` verb
itself, so every entry point that puts text at a worker's prompt is audited
the same way. The mail lane's pane arm declares its provenance with
``--source mail:<msg-id>`` so the floor's row joins the bus record, and it
must not write a second row of its own (that half is pinned in
``cli/tests/unit/test_dispatch_mux_send.py``).
"""
from __future__ import annotations

from types import SimpleNamespace

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
    def run(argv, **kwargs):
        calls.append({"argv": list(argv), "input": kwargs.get("input")})
        if argv[1:4] == ["mux", "pane", "read"]:
            return SimpleNamespace(returncode=0, stdout=screen, stderr="")
        return SimpleNamespace(returncode=returncode, stdout="", stderr="")

    return run


def test_pane_send_declares_the_mail_source_on_the_payload_paste(monkeypatch):
    """The `--source mail:<msg-id>` label rides the payload paste's argv.

    The floor writes the row carrying this label, so the operator query joins
    the dispatch to its bus record by the mail id. The submit-key sends ride
    the same dispatch without a label: a control byte is not a dispatch, and
    the floor writes no row for one.
    """
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    assert (
        dispatch._mux_pane_send(
            _entry(),
            "status?",
            guarded=False,
            raw=True,
            source_label="mail:msg-abc123",
        )
        is True
    )

    paste_args = next(c["argv"] for c in calls if "--stdin" in c["argv"])
    assert paste_args[paste_args.index("--source") + 1] == "mail:msg-abc123"
    cr_args = [c["argv"] for c in calls if "--text" in c["argv"]]
    assert cr_args, "the submit-key sends ran"
    assert all("--source" not in argv for argv in cr_args)


def test_pane_send_without_a_declared_source_sends_no_label(monkeypatch):
    """No label, no `--source`: the floor records `unattributed:<pid>`, which
    is the honest value for a caller that declared nothing."""
    calls: list[dict] = []
    monkeypatch.setattr(dispatch.subprocess, "run", _runner(calls))
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)

    assert dispatch._mux_pane_send(_entry(), "status?", guarded=False, raw=True) is True

    paste_args = next(c["argv"] for c in calls if "--stdin" in c["argv"])
    assert "--source" not in paste_args
