"""Push leg for the a2a status-breakpoint family.

blocked notifies the parent handle when spawn lineage exists; the push rides
`fno agents mail send` (durable-first), fires AFTER the durable events.jsonl
append, and silently skips when there is no lineage. run_summary pushes only
from Rust finalize, never from this emit path.
"""
from __future__ import annotations

import json

import pytest

from fno.events.store_client import read_committed_lines
from typer.testing import CliRunner

from fno.events.cli import _resolve_parent_handle
from fno.events.cli import cli as event_cli


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


class _R:
    def __init__(self, rc=0):
        self.returncode = rc
        self.stdout = b"msg-1 queued (durable)"
        self.stderr = b""


def _emit_blocked(runner, tmp_path, monkeypatch, *, parent, run_fn):
    events = tmp_path / ".fno" / "events.jsonl"
    state = tmp_path / ".fno" / "target-state.md"
    monkeypatch.setattr("fno.events.cli._resolve_parent_handle", lambda explicit: parent)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: "/fake/fno-agents")

    # Only the parent-push leg is faked. The event store client shares the
    # subprocess.run module attribute, so its native commit passes through to
    # the real implementation; a blanket fake would swallow the receipt.
    import subprocess as _subprocess

    original_run = _subprocess.run

    def _routed(argv, **kw):
        if argv[:3] == ["fno", "agents", "mail"] or argv[1:2] == ["machine-mail-send"]:
            return run_fn(argv, **kw)
        return original_run(argv, **kw)

    monkeypatch.setattr("fno.events.cli.subprocess.run", _routed)
    result = runner.invoke(
        event_cli,
        ["emit", "--events", str(events), "--state", str(state),
         "--type", "blocked", "--source", "test", "--run", "R1",
         "--data", json.dumps({"reason": "stuck on x"})],
    )
    return result, events


# -- resolution --

def test_resolve_parent_explicit_wins() -> None:
    assert _resolve_parent_handle("claude-parent99") == "claude-parent99"


# -- AC2-HP: blocked with lineage pushes to the parent, referencing the run --

def test_blocked_pushes_to_parent(runner, tmp_path, monkeypatch) -> None:
    sent: dict = {}

    def fake_run(argv, **kw):
        sent["argv"] = argv
        return _R(0)

    result, events = _emit_blocked(runner, tmp_path, monkeypatch, parent="claude-parent99", run_fn=fake_run)
    assert result.exit_code == 0, result.output
    ev = json.loads(read_committed_lines(events)[-1])
    assert ev["type"] == "blocked"
    # P2: parent is resolved into the durable envelope, not only the push path
    assert ev["parent"] == "claude-parent99"
    assert sent["argv"][:2] == ["/fake/fno-agents", "machine-mail-send"]
    assert sent["argv"][sent["argv"].index("--arm") + 1] == "events-push"
    assert sent["argv"][sent["argv"].index("--to") + 1] == "claude-parent99"
    # The event body references the run so the parent can correlate.
    body = sent["argv"][sent["argv"].index("--body") + 1]
    assert "R1" in body
    assert "[fno:blocked]" in body


# -- AC5-EDGE: a CLI-emitted run_summary does not push (Rust finalize owns it) --

def test_run_summary_emit_does_not_push(runner, tmp_path, monkeypatch) -> None:
    calls: list = []

    # Route like _emit_blocked: the store client shares this subprocess.run
    # module attribute, so a blanket fake would swallow the native commit's
    # receipt and fail the emit itself. Only the mail leg is faked.
    import subprocess as _subprocess

    original_run = _subprocess.run

    def fake_run(argv, **kw):
        calls.append(argv)
        if argv[:3] == ["fno", "agents", "mail"] or argv[1:2] == ["machine-mail-send"]:
            return _R(0)
        return original_run(argv, **kw)

    events = tmp_path / ".fno" / "events.jsonl"
    state = tmp_path / ".fno" / "target-state.md"
    monkeypatch.setattr("fno.events.cli._resolve_parent_handle", lambda explicit: "claude-parent99")
    monkeypatch.setattr("fno.events.cli.subprocess.run", fake_run)
    result = runner.invoke(
        event_cli,
        ["emit", "--events", str(events), "--state", str(state),
         "--type", "run_summary", "--source", "test", "--run", "R1",
         "--outcome", "FAILED",
         "--data", json.dumps({
             "termination_reason": "DoneAwaitingMerge",
             "tasks_started": 2, "tasks_done": 2, "tasks_failed": 0,
         })],
    )
    assert result.exit_code == 0, result.output
    # The store is the record; the cutover stopped journal appends.
    from fno.events.store_client import store_db_path

    assert store_db_path(events).exists()
    assert not any(a[1:2] == ["machine-mail-send"] for a in calls)


# -- P1: resolution matches a spawned row by identity, not name==handle --

def test_resolve_parent_by_identity_not_display_name(monkeypatch) -> None:
    from fno.events import cli as clim
    from fno.harness_identity import HarnessIdentity

    class FakeEntry:
        name = "tgt-prj0001-claude-g1"  # caller-provided display name, NOT the handle
        harness = "claude"
        short_id = "03401fb3"    # the stored short id
        spawned_by_session = "parentsess123"
        spawned_by_harness = "claude"

    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity",
        lambda *a, **k: HarnessIdentity(session_id="03401fb3-92b2-cafe", harness="claude"),
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda *a, **k: [FakeEntry()])
    # canonical_handle("parentsess123") -> "parentse" (first-eight)
    assert clim._resolve_parent_handle(None) == "parentse"


# -- no lineage -> silent skip, no mail send --

def test_no_parent_no_push(runner, tmp_path, monkeypatch) -> None:
    calls: list = []

    def fake_run(argv, **kw):
        calls.append(argv)
        return _R(0)

    result, events = _emit_blocked(runner, tmp_path, monkeypatch, parent=None, run_fn=fake_run)
    assert result.exit_code == 0, result.output
    assert read_committed_lines(events)  # event still written
    assert not any(a[1:2] == ["machine-mail-send"] for a in calls)


# -- AC1-FR: a failing push loses nothing (event already durable, exit 0) --

def test_push_failure_keeps_event(runner, tmp_path, monkeypatch) -> None:
    def boom(argv, **kw):
        raise OSError("mail bus down")

    result, events = _emit_blocked(runner, tmp_path, monkeypatch, parent="claude-parent99", run_fn=boom)
    assert result.exit_code == 0, result.output  # push failure is non-fatal
    ev = json.loads(read_committed_lines(events)[-1])
    assert ev["type"] == "blocked"  # events.jsonl line intact, independent of push


# -- push-parent subcommand: skip without lineage --

def test_push_parent_subcommand_skips(runner, monkeypatch) -> None:
    monkeypatch.setattr("fno.events.cli._resolve_parent_handle", lambda explicit: None)
    result = runner.invoke(event_cli, ["push-parent", "--type", "run_summary", "--run", "R1"])
    assert result.exit_code == 0
    assert "no parent lineage" in result.output


def test_push_parent_subcommand_pushes(runner, monkeypatch) -> None:
    monkeypatch.setattr("fno.events.cli._resolve_parent_handle", lambda explicit: "claude-parent99")
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: "/fake/fno-agents")
    sent: dict = {}
    monkeypatch.setattr(
        "fno.events.cli.subprocess.run",
        lambda argv, **kw: (sent.__setitem__("argv", argv), _R(0))[1],
    )
    result = runner.invoke(
        event_cli,
        ["push-parent", "--type", "run_summary", "--run", "R1", "--reason", "DonePRGreen"],
    )
    assert result.exit_code == 0
    assert sent["argv"][:2] == ["/fake/fno-agents", "machine-mail-send"]
    assert sent["argv"][sent["argv"].index("--arm") + 1] == "events-push"
    assert sent["argv"][sent["argv"].index("--to") + 1] == "claude-parent99"
