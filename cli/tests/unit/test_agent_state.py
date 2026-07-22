"""Tests for fno.agent.state - AgentContext + load_agent_context().

Covers AC1-HP through AC7-EDGE from plan 01-foundation.md task 01.2:
all three layers, malformed retry, detected_paths, dual target+session,
state_file_override, non-git project root, empty .fno/.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import yaml

from fno.agent.state import (
    AgentContext,
    FleetState,
    MalformedStateError,
    MissingStateFileOverrideError,
    SessionState,
    WalkerState,
    load_agent_context,
)

FIXTURES = Path(__file__).parent / "fixtures" / "agent"


def _build_workspace(tmp_path: Path, *, target: bool = False, walker: bool = False,
                    session: bool = False, fleet: bool = False,
                    malformed_target: bool = False) -> Path:
    """Materialize a fake project_root with selected state layers present."""
    project = tmp_path / "project"
    fno_dir = project / ".fno"
    fno_dir.mkdir(parents=True)

    if target and not malformed_target:
        (fno_dir / "target-state.md").write_text(
            (FIXTURES / "target-state.md").read_text()
        )
    if malformed_target:
        (fno_dir / "target-state.md").write_text(
            (FIXTURES / "malformed-state.md").read_text()
        )
    if walker:
        (fno_dir / "megawalk-state.md").write_text(
            (FIXTURES / "megawalk-state.md").read_text()
        )
    if session:
        (fno_dir / "session-state.md").write_text(
            (FIXTURES / "session-state-think.md").read_text()
        )
    if fleet:
        # Build fleet under a tmp HOME so we don't touch the real ~/.fno/.
        fleet_root = tmp_path / "fake_home" / ".fno" / "fleet" / "fleet-fixture-001"
        fleet_root.mkdir(parents=True)
        body = (FIXTURES / "fleet-mission.md").read_text().replace(
            "__PROJECT_ROOT__", str(project.resolve())
        )
        (fleet_root / "00-INDEX.md").write_text(body)
    return project


def test_ac1_hp_all_three_layers_detected(tmp_path, monkeypatch):
    """AC1-HP: deep stack with fleet+walker+session populates all three fields."""
    project = _build_workspace(tmp_path, target=True, walker=True, fleet=True)
    monkeypatch.setenv("HOME", str(tmp_path / "fake_home"))
    monkeypatch.chdir(project)
    ctx = load_agent_context(project_root_override=project)
    assert isinstance(ctx, AgentContext)
    assert ctx.session is not None and isinstance(ctx.session, SessionState)
    assert ctx.session.session_id == "20260512T010101Z-99999-fixaaa"
    assert ctx.session.phase == "review"
    assert ctx.walker is not None and isinstance(ctx.walker, WalkerState)
    assert ctx.walker.in_flight == 3
    assert ctx.walker.done == 1
    assert ctx.fleet is not None and isinstance(ctx.fleet, FleetState)
    assert ctx.fleet.mission_id == "fleet-fixture-001"
    assert ctx.fleet.wave_current == 2
    assert ctx.fleet.wave_total == 4
    assert ctx.fleet.status == "running"


def test_ac2_err_malformed_yaml_raises_after_retry(tmp_path):
    """AC2-ERR: malformed session YAML retried once then raises MalformedStateError."""
    project = _build_workspace(tmp_path, malformed_target=True)
    with pytest.raises(MalformedStateError) as excinfo:
        load_agent_context(project_root_override=project)
    assert "target-state.md" in str(excinfo.value.path)


def test_ac3_ui_detected_paths_only_lists_present_layers(tmp_path):
    """AC3-UI: detected_paths records walker+session, omits absent fleet."""
    project = _build_workspace(tmp_path, target=True, walker=True, fleet=False)
    ctx = load_agent_context(project_root_override=project)
    assert ctx.fleet is None
    rel = {p.name for p in ctx.detected_paths}
    assert "target-state.md" in rel
    assert "megawalk-state.md" in rel
    # No fleet mission means no fleet/00-INDEX.md path recorded.
    assert not any("00-INDEX.md" == p.name for p in ctx.detected_paths)


def test_ac4_edge_dual_target_and_session_prefers_target(tmp_path):
    """AC4-EDGE: both files present -> target wins, warning recorded."""
    project = _build_workspace(tmp_path, target=True, session=True)
    ctx = load_agent_context(project_root_override=project)
    assert ctx.session is not None
    assert ctx.session.kind == "target"
    assert ctx.session.session_id == "20260512T010101Z-99999-fixaaa"
    assert any("both" in w and "target-state.md" in w for w in ctx.warnings), ctx.warnings


def test_ac5_fr_state_file_override_bypasses_session_detection(tmp_path):
    """AC5-FR: override loads session from given path; fleet/walker still detected."""
    project = _build_workspace(tmp_path, target=True, walker=True)
    override = tmp_path / "custom-state.md"
    override.write_text((FIXTURES / "session-state-think.md").read_text())
    ctx = load_agent_context(
        state_file_override=override, project_root_override=project
    )
    # Override-loaded session, NOT the project's target-state.md
    assert ctx.session is not None
    assert ctx.session.path == override
    assert ctx.session.kind == "override"
    assert ctx.session.phase == "think"
    # Walker layer still detected normally.
    assert ctx.walker is not None
    assert ctx.walker.in_flight == 3


def test_ac6_edge_non_git_project_falls_back_to_cwd(tmp_path, monkeypatch):
    """AC6-EDGE: when git rev-parse fails, project_root=cwd, warning recorded."""
    monkeypatch.chdir(tmp_path)
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    fake_git = fake_bin / "git"
    fake_git.write_text("#!/bin/sh\nexit 128\n")
    fake_git.chmod(0o755)
    # Replace PATH so the only `git` found returns 128 (mimics not-a-git-repo).
    monkeypatch.setenv("PATH", str(fake_bin))
    ctx = load_agent_context()
    assert ctx.project_root == Path.cwd()
    assert any("not a git repo" in w for w in ctx.warnings)


def test_ac7_edge_empty_fno_dir_degrades_cleanly(tmp_path):
    """AC7-EDGE: no .fno/ at all -> all layers None, no warnings, no errors."""
    project = tmp_path / "empty_project"
    project.mkdir()
    ctx = load_agent_context(project_root_override=project)
    assert ctx.session is None
    assert ctx.walker is None
    assert ctx.fleet is None
    assert ctx.detected_paths == []
    assert ctx.warnings == []


_PROVIDER_ENVS = (
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_SESSION_ID",
    "GEMINI_SESSION_ID",
    "CODEX_PLUGIN_ROOT",
    "GEMINI_PROJECT_DIR",
    "CLAUDE_PLUGIN_ROOT",
)


def _clear_provider_env(monkeypatch):
    for key in _PROVIDER_ENVS:
        monkeypatch.delenv(key, raising=False)


def test_provider_detection_defaults_to_claude(tmp_path, monkeypatch):
    """provider defaults to claude when no provider-specific env var is set."""
    _clear_provider_env(monkeypatch)
    ctx = load_agent_context(project_root_override=tmp_path)
    assert ctx.provider == "claude"


def test_provider_detection_recognizes_codex(tmp_path, monkeypatch):
    _clear_provider_env(monkeypatch)
    monkeypatch.setenv("CODEX_PLUGIN_ROOT", "/fake/codex")
    ctx = load_agent_context(project_root_override=tmp_path)
    assert ctx.provider == "codex"


def test_provider_detection_recognizes_gemini(tmp_path, monkeypatch):
    _clear_provider_env(monkeypatch)
    monkeypatch.setenv("GEMINI_PROJECT_DIR", "/fake/gemini")
    ctx = load_agent_context(project_root_override=tmp_path)
    assert ctx.provider == "gemini"


def test_ac1_codex_session_marker_resolves_codex(tmp_path, monkeypatch):
    """AC1: a real codex session sets CODEX_THREAD_ID but no *_PLUGIN_ROOT;
    it must resolve codex, not fail open to claude."""
    _clear_provider_env(monkeypatch)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-abc")
    ctx = load_agent_context(project_root_override=tmp_path)
    assert ctx.provider == "codex"


def test_ac1_err_no_markers_defaults_claude(tmp_path, monkeypatch):
    """AC1-ERR: no marker of any kind still defaults to claude (unchanged)."""
    _clear_provider_env(monkeypatch)
    ctx = load_agent_context(project_root_override=tmp_path)
    assert ctx.provider == "claude"


def test_session_marker_beats_conflicting_plugin_root(tmp_path, monkeypatch):
    """A codex session marker wins over a stale CLAUDE_PLUGIN_ROOT hint."""
    _clear_provider_env(monkeypatch)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-abc")
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", "/fake/claude")
    ctx = load_agent_context(project_root_override=tmp_path)
    assert ctx.provider == "codex"


def test_walker_only_detected_when_file_non_empty(tmp_path):
    """An empty megawalk-state.md does not produce a WalkerState."""
    project = tmp_path / "p"
    fno = project / ".fno"
    fno.mkdir(parents=True)
    (fno / "megawalk-state.md").write_text("")
    ctx = load_agent_context(project_root_override=project)
    assert ctx.walker is None


def test_walker_malformed_yaml_logs_warning_no_raise(tmp_path):
    """Walker layer failure does NOT raise (failure-isolation)."""
    project = tmp_path / "p"
    fno = project / ".fno"
    fno.mkdir(parents=True)
    (fno / "megawalk-state.md").write_text(
        (FIXTURES / "malformed-state.md").read_text()
    )
    ctx = load_agent_context(project_root_override=project)
    assert ctx.walker is None
    assert any("walker" in w and "malformed" in w for w in ctx.warnings)


def test_session_only_present_falls_back_to_session_state(tmp_path):
    """Only session-state.md present -> SessionState with kind='session'."""
    project = tmp_path / "p"
    fno = project / ".fno"
    fno.mkdir(parents=True)
    (fno / "session-state.md").write_text(
        (FIXTURES / "session-state-think.md").read_text()
    )
    ctx = load_agent_context(project_root_override=project)
    assert ctx.session is not None
    assert ctx.session.kind == "session"
    assert ctx.session.phase == "think"


def test_fleet_status_must_be_running_or_paused(tmp_path, monkeypatch):
    """A fleet mission with status: complete should NOT match."""
    project = tmp_path / "p"
    project.mkdir()
    fleet_root = tmp_path / "fake_home" / ".fno" / "fleet" / "complete-mission"
    fleet_root.mkdir(parents=True)
    (fleet_root / "00-INDEX.md").write_text(
        f"---\nmission_id: complete-mission\nstatus: complete\nprojects:\n  p:\n    cwd: {project.resolve()}\n---\n"
    )
    monkeypatch.setenv("HOME", str(tmp_path / "fake_home"))
    ctx = load_agent_context(project_root_override=project)
    assert ctx.fleet is None


def test_state_file_path_is_a_directory_raises_malformed(tmp_path):
    """A directory at .fno/target-state.md path -> MalformedStateError,
    not an uncaught OSError. Regression for codex-bot finding on PR #248:
    `path.read_text()` previously raised OSError, escaping _load_or_exit
    and crashing the introspection surface.
    """
    project = tmp_path / "p"
    fno = project / ".fno"
    fno.mkdir(parents=True)
    # Make the state-file path a directory, not a regular file.
    (fno / "target-state.md").mkdir()
    with pytest.raises(MalformedStateError):
        load_agent_context(project_root_override=project)


def test_state_file_override_missing_raises(tmp_path):
    """--state-file pointing at a missing path raises MissingStateFileOverrideError.

    Prior behavior was to warn + return None, silently degrading to "no
    session". An explicit user typo deserves a hard error.
    """
    project = _build_workspace(tmp_path, target=True)
    bogus = tmp_path / "does_not_exist.md"
    with pytest.raises(MissingStateFileOverrideError) as excinfo:
        load_agent_context(
            state_file_override=bogus, project_root_override=project
        )
    assert excinfo.value.path == bogus


def test_fleet_path_mismatch_skipped(tmp_path, monkeypatch):
    """A running fleet mission referencing a different project should NOT match."""
    project = tmp_path / "p"
    project.mkdir()
    other = tmp_path / "other"
    other.mkdir()
    fleet_root = tmp_path / "fake_home" / ".fno" / "fleet" / "other-mission"
    fleet_root.mkdir(parents=True)
    (fleet_root / "00-INDEX.md").write_text(
        f"---\nmission_id: other-mission\nstatus: running\nprojects:\n  other:\n    cwd: {other.resolve()}\n---\n"
    )
    monkeypatch.setenv("HOME", str(tmp_path / "fake_home"))
    ctx = load_agent_context(project_root_override=project)
    assert ctx.fleet is None
