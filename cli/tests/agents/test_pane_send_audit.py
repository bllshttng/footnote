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


def test_documented_query_names_who_dispatched(tmp_path):
    """The query the doc teaches answers "who told this worker to do that"
    from the events journal alone (x-91ba AC4-HP).

    A documented command nobody executes goes stale silently, so this stages a
    journal and runs the documented command form against it: two rg stages,
    the lane marker first, the worker second. The row must name who dispatched
    (`source`) and what was sent (`payload`).
    """
    import json
    import shutil
    import subprocess

    rows = [
        {
            "ts": "2026-09-12T10:00:00Z",
            "type": "agent_raw_inject",
            "source": "daemon",
            "data": {
                "lane": "pane-send",
                "target_session": "s1",
                "target_pane": 3,
                "target_name": "other-worker",
                "source": "mail:msg-old",
                "payload": "unrelated dispatch",
            },
        },
        {
            "ts": "2026-09-12T11:00:00Z",
            "type": "agent_raw_inject",
            "source": "daemon",
            "data": {
                "lane": "pane-send",
                "target_session": "s2",
                "target_pane": 7,
                "target_name": "worker-under-test",
                "target_fno_id": "sess-4242",
                "harness": "codex",
                "source": "mail:msg-beef",
                "payload": "New work, take it now: run /fno:target x-11ec",
                "outcome": "submitted",
            },
        },
        {
            "ts": "2026-09-12T12:00:00Z",
            "type": "agent_raw_inject",
            "source": "daemon",
            "data": {
                "lane": "control.sock",
                "target_session": "sess-4242",
                "payload": "injected by another lane",
            },
        },
    ]
    events = tmp_path / "events.jsonl"
    # Compact separators: the real journal is serde_json's compact form, and
    # the documented pattern matches it with no spaces.
    events.write_text(
        "\n".join(json.dumps(row, separators=(",", ":")) for row in rows) + "\n"
    )

    # The documented query, with the doc's placeholder swapped for the staged
    # journal and a real worker name for <worker-name-or-id>. The env prefix
    # rides along because rg honors RIPGREP_CONFIG_PATH, and this machine has
    # one; without it the row comes back wearing ansi colors and the parse
    # dies. CI runners have no rg, so grep -F stands in for it: same two-stage
    # fixed-string query, same pipe.
    search = shutil.which("rg") or ""
    first = f"RIPGREP_CONFIG_PATH= {search}" if search else "grep -F"
    second = f"RIPGREP_CONFIG_PATH= rg" if search else "grep -F"
    proc = subprocess.run(
        f"{first} '\"lane\":\"pane-send\"' {events} | {second} worker-under-test",
        shell=True,
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 0, proc.stderr
    hits = [
        json.loads(line) for line in proc.stdout.splitlines() if line.strip()
    ]
    assert len(hits) == 1
    hit = hits[0]["data"]
    assert hit["source"] == "mail:msg-beef"
    assert "x-11ec" in hit["payload"]
    assert hit["target_fno_id"] == "sess-4242"
