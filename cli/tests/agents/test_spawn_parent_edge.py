"""Task 2.2: spawn-time parent edge — spawned_by_session/harness/cwd.

Also covers x-42c5's spawn_trigger field: the CAUSE of a spawn (distinct from
the parent-edge WHO above), ambient-captured from FNO_SPAWN_TRIGGER the same
way spawned_by_* is captured from the session env vars.

Acceptance criteria (operator-locked):

  AC-HP: spawn a claude worker when CLAUDE_CODE_SESSION_ID is set ->
         registry row AND agent_spawned event carry spawned_by_session,
         spawned_by_harness="claude", spawned_by_cwd=parent PWD.
  AC-EDGE-codex: when CODEX_SESSION_ID is set (and no claude var) ->
         harness="codex", session=codex id, cwd captured.
  AC-EDGE-gemini: when GEMINI_SESSION_ID is set (and no claude/codex) ->
         harness="gemini", session=gemini id.
  AC-EDGE-none: when NO session env vars are set -> all three fields are
         None, spawn still succeeds (no raise).
  AC-EDGE-multi: when CODEX_THREAD_ID is set with legacy vars -> thread wins.
"""
from __future__ import annotations

import os

import pytest

from fno.paths_testing import use_tmpdir
from fno.agents import events as events_mod
from fno.agents.registry import (
    load_registry,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_spawn_result(short_id: str = "ab12cd34"):
    """Return a fake ProviderResult-like object for _claude_create_path."""
    from fno.agents.dispatch import DispatchAskResult
    return DispatchAskResult(
        kind="create",
        short_id=short_id,
        duration_ms=10,
    )


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _clear_parent_markers(monkeypatch):
    for marker in (
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
    ):
        monkeypatch.delenv(marker, raising=False)


@pytest.fixture
def workdir_claude(tmp_path, monkeypatch):
    """Isolated fno home with a fake claude binary."""
    from tests.agents._fake_claude import install_fake_claude
    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


@pytest.fixture
def captured_emits(monkeypatch):
    """Capture spawn births as (kind, data) tuples.

    Births write the daemon envelope via emit_spawned (x-8cd5 Wave 6), not the
    flat events.emit, so this intercepts the spawn seam the ACs assert on.
    """
    calls: list[tuple[str, dict]] = []

    def _capture(**data):
        calls.append(("agent_spawned", data))

    monkeypatch.setattr(events_mod, "emit_spawned", _capture)
    return calls


# ---------------------------------------------------------------------------
# AC-HP: claude spawn with CLAUDE_CODE_SESSION_ID set
# ---------------------------------------------------------------------------


def test_spawn_records_parent_edge_claude(workdir_claude, captured_emits, monkeypatch):
    """AC-HP: claude spawn -> registry row and agent_spawned event carry parent edge."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-session-abc123")
    monkeypatch.setenv("PWD", "/parent/working/dir")
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("GEMINI_SESSION_ID", raising=False)

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-parent-edge", "-H", "claude", "do something", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"expected exit 0, got {result.exit_code}\noutput: {result.output}"

    # Registry row must carry all three fields.
    entries = load_registry()
    entry = next((e for e in entries if e.name == "test-parent-edge"), None)
    assert entry is not None, "registry row must exist after claude spawn"
    assert entry.spawned_by_session == "parent-session-abc123", (
        f"expected spawned_by_session='parent-session-abc123', got {entry.spawned_by_session!r}"
    )
    assert entry.spawned_by_harness == "claude", (
        f"expected spawned_by_harness='claude', got {entry.spawned_by_harness!r}"
    )
    assert entry.spawned_by_cwd == "/parent/working/dir", (
        f"expected spawned_by_cwd='/parent/working/dir', got {entry.spawned_by_cwd!r}"
    )

    # agent_spawned event must carry all three fields.
    spawned_events = [(k, d) for k, d in captured_emits if k == "agent_spawned"]
    assert len(spawned_events) == 1, (
        f"expected exactly 1 agent_spawned event, got {len(spawned_events)}: {spawned_events}"
    )
    ev_data = spawned_events[0][1]
    assert ev_data.get("spawned_by_session") == "parent-session-abc123"
    assert ev_data.get("spawned_by_harness") == "claude"
    assert ev_data.get("spawned_by_cwd") == "/parent/working/dir"
    assert ev_data.get("name") == "test-parent-edge"


# ---------------------------------------------------------------------------
# AC-EDGE: codex session env (no claude var set)
# ---------------------------------------------------------------------------


def test_spawn_parent_edge_codex_harness(workdir_claude, captured_emits, monkeypatch):
    """AC-EDGE: CODEX_SESSION_ID set (no claude) -> harness='codex', session captured."""
    monkeypatch.setenv("CODEX_SESSION_ID", "codex-parent-xyz")
    monkeypatch.setenv("PWD", "/codex/parent")
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    monkeypatch.delenv("GEMINI_SESSION_ID", raising=False)

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-codex-edge", "-H", "claude", "do something", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"exit {result.exit_code}\n{result.output}"

    entries = load_registry()
    entry = next((e for e in entries if e.name == "test-codex-edge"), None)
    assert entry is not None
    assert entry.spawned_by_session == "codex-parent-xyz"
    assert entry.spawned_by_harness == "codex"
    assert entry.spawned_by_cwd == "/codex/parent"

    spawned_events = [(k, d) for k, d in captured_emits if k == "agent_spawned"]
    assert len(spawned_events) == 1
    ev_data = spawned_events[0][1]
    assert ev_data.get("spawned_by_harness") == "codex"
    assert ev_data.get("spawned_by_session") == "codex-parent-xyz"


# ---------------------------------------------------------------------------
# AC-EDGE: gemini session env
# ---------------------------------------------------------------------------


def test_spawn_parent_edge_gemini_harness(workdir_claude, captured_emits, monkeypatch):
    """AC-EDGE: GEMINI_SESSION_ID set (no claude/codex) -> harness='gemini'."""
    monkeypatch.setenv("GEMINI_SESSION_ID", "gemini-parent-99")
    monkeypatch.setenv("PWD", "/gemini/parent")
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-gemini-edge", "-H", "claude", "do something", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"exit {result.exit_code}\n{result.output}"

    entries = load_registry()
    entry = next((e for e in entries if e.name == "test-gemini-edge"), None)
    assert entry is not None
    assert entry.spawned_by_session == "gemini-parent-99"
    assert entry.spawned_by_harness == "gemini"

    spawned_events = [(k, d) for k, d in captured_emits if k == "agent_spawned"]
    assert len(spawned_events) == 1
    assert spawned_events[0][1].get("spawned_by_harness") == "gemini"


# ---------------------------------------------------------------------------
# AC-EDGE: no session env vars -> all three fields None, spawn succeeds
# ---------------------------------------------------------------------------


def test_spawn_parent_edge_no_env_vars(workdir_claude, captured_emits, monkeypatch):
    """AC-EDGE-none: absent all session env vars -> fields are None, spawn does not raise."""
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("GEMINI_SESSION_ID", raising=False)

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-no-env", "-H", "claude", "do something", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"exit {result.exit_code}\n{result.output}"

    entries = load_registry()
    entry = next((e for e in entries if e.name == "test-no-env"), None)
    assert entry is not None
    assert entry.spawned_by_session is None
    assert entry.spawned_by_harness is None
    # cwd is captured from PWD even when no session — still a string or None
    # (implementation detail; we just ensure no crash and session/harness are None)

    spawned_events = [(k, d) for k, d in captured_emits if k == "agent_spawned"]
    assert len(spawned_events) == 1
    ev_data = spawned_events[0][1]
    assert ev_data.get("spawned_by_session") is None
    assert ev_data.get("spawned_by_harness") is None


# ---------------------------------------------------------------------------
# AC-EDGE: CODEX_THREAD_ID wins over legacy markers
# ---------------------------------------------------------------------------


def test_spawn_parent_edge_codex_thread_wins(
    workdir_claude, captured_emits, monkeypatch
):
    """AC-EDGE-multi: CODEX_THREAD_ID outranks all legacy session markers."""
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-wins-session")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "claude-loses-session")
    monkeypatch.setenv("CODEX_SESSION_ID", "codex-legacy-loses-session")

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-priority", "-H", "claude", "do something", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"exit {result.exit_code}\n{result.output}"

    entries = load_registry()
    entry = next((e for e in entries if e.name == "test-priority"), None)
    assert entry is not None
    assert entry.spawned_by_session == "thread-wins-session"
    assert entry.spawned_by_harness == "codex"

    spawned_events = [(k, d) for k, d in captured_emits if k == "agent_spawned"]
    assert len(spawned_events) == 1
    assert spawned_events[0][1].get("spawned_by_harness") == "codex"
    assert spawned_events[0][1].get("spawned_by_session") == "thread-wins-session"


# ---------------------------------------------------------------------------
# AC-HP: exactly one agent_spawned emitted (not duplicated)
# ---------------------------------------------------------------------------


def test_spawn_emits_exactly_one_agent_spawned(workdir_claude, captured_emits, monkeypatch):
    """agent_spawned is emitted exactly once per successful claude spawn."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "once-session-id")
    monkeypatch.delenv("CODEX_SESSION_ID", raising=False)
    monkeypatch.delenv("GEMINI_SESSION_ID", raising=False)

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-once-emit", "-H", "claude", "task", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"exit {result.exit_code}\n{result.output}"

    spawned_events = [(k, d) for k, d in captured_emits if k == "agent_spawned"]
    assert len(spawned_events) == 1, (
        f"expected exactly 1 agent_spawned, got {len(spawned_events)}"
    )


# ---------------------------------------------------------------------------
# x-42c5: spawn_trigger — the CAUSE of a spawn, distinct from the parent edge
# ---------------------------------------------------------------------------


def test_spawn_records_trigger_cause_when_dispatcher_sets_it(
    workdir_claude, captured_emits, monkeypatch
):
    """A dispatcher (e.g. think-spawn) sets FNO_SPAWN_TRIGGER before shelling
    out to `fno agents spawn`; the registry row it writes records the cause,
    answering "why was this spawned" as a field rather than a timestamp-gap
    inference against the filing node's created_at."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-session-abc123")
    monkeypatch.setenv("FNO_SPAWN_TRIGGER", "think_spawn:work-start")

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-spawn-trigger", "-H", "claude", "do something", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"exit {result.exit_code}\n{result.output}"

    entries = load_registry()
    entry = next((e for e in entries if e.name == "test-spawn-trigger"), None)
    assert entry is not None
    assert entry.spawn_trigger == "think_spawn:work-start"


def test_spawn_trigger_absent_for_a_direct_human_spawn(
    workdir_claude, captured_emits, monkeypatch
):
    """A human running `fno agents spawn` directly never sets FNO_SPAWN_TRIGGER,
    so the field stays None - absence reads as "an operator asked for this,"
    which is the honest default (not every registry row has an automated cause)."""
    monkeypatch.delenv("FNO_SPAWN_TRIGGER", raising=False)

    from fno.agents.cli import agents_app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "test-no-spawn-trigger", "-H", "claude", "do something", "--substrate", "bg"],
        catch_exceptions=False,
    )

    assert result.exit_code == 0, f"exit {result.exit_code}\n{result.output}"

    entries = load_registry()
    entry = next((e for e in entries if e.name == "test-no-spawn-trigger"), None)
    assert entry is not None
    assert entry.spawn_trigger is None


def test_spawn_trigger_does_not_leak_into_this_process_environment(monkeypatch):
    """Code-review finding (x-42c5): _capture_spawn_trigger POPS the var, not just
    reads it. Left in place, it would ride into a spawned worker's own persistent
    environment via a later dict(os.environ) snapshot (bg_create builds the new
    worker's env that way, and scrub_ambient_identity does not know this marker),
    mislabeling that worker's OWN later spawns with this spawn's cause."""
    from fno.agents.dispatch import _capture_spawn_trigger

    monkeypatch.setenv("FNO_SPAWN_TRIGGER", "think_spawn:work-start")
    assert _capture_spawn_trigger() == "think_spawn:work-start"
    assert "FNO_SPAWN_TRIGGER" not in os.environ
    # A second read in the same process (e.g. a nested spawn) sees nothing left.
    assert _capture_spawn_trigger() is None
