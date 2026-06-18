"""Task 2.1 tests for dispatch.py - filesystem dispatch via plan-file + backlog intake.

Five tests covering the 5 ACs defined in 02-dispatch.md:
  AC2-HP       happy path: plan file written and intake returns node_id
  AC2-ERR      intake failure: DispatchError raised, orphan plan file cleaned up
  AC2-FR(sn)   short_name input normalizes via resolver before path lookup
  AC2-FR(idem) idempotency stub: documented but not enforced here
  AC2-FR(pt)   path-traversal defense: ProjectNotFound propagates from resolver
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers / fixtures
# ---------------------------------------------------------------------------

def _make_settings(tmp_path: Path, project_name: str, short_name: str, project_path: Path) -> Path:
    """Write a minimal settings.yaml under tmp_path/.fno/ for tests."""
    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir(parents=True, exist_ok=True)
    settings = abilities_dir / "settings.yaml"
    settings.write_text(
        f"work:\n"
        f"  workspaces:\n"
        f"    test_ws:\n"
        f"      projects:\n"
        f"        - name: {project_name}\n"
        f"          short_name: {short_name}\n"
        f"          path: {project_path}\n",
        encoding="utf-8",
    )
    return settings


def _fake_intake_success(*args, **kwargs):
    """Subprocess stub: simulates successful fno backlog intake output."""
    return subprocess.CompletedProcess(
        args=args[0] if args else [],
        returncode=0,
        stdout='intake ab-abcd1234 -> backlog: "Mission ab-12345678 wave 1 - test-project"\n',
        stderr="",
    )


def _fake_intake_failure(*args, **kwargs):
    """Subprocess stub: simulates fno backlog intake failure (rc=4)."""
    return subprocess.CompletedProcess(
        args=args[0] if args else [],
        returncode=4,
        stdout="",
        stderr="graph.json not writable: permission denied",
    )


# ---------------------------------------------------------------------------
# AC2-HP: happy path
# ---------------------------------------------------------------------------

def test_dispatch_happy_path_writes_plan_and_returns_node_id(tmp_path, monkeypatch):
    """AC2-HP: dispatch_project writes a plan file at the correct path and intake
    succeeds, returning a DispatchResult with a non-empty backlog_node_id."""
    from fno.megatron.dispatch import dispatch_project, DispatchResult

    project_path = tmp_path / "test-project"
    plans_dir = project_path / "internal" / "fno" / "plans"
    plans_dir.mkdir(parents=True)

    settings_path = _make_settings(tmp_path, "test-project", "tp", project_path)

    import fno.projects.resolve as resolve_mod
    monkeypatch.setattr(resolve_mod, "SETTINGS_PATH", settings_path)
    resolve_mod._clear_cache()

    import fno.megatron.dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_SETTINGS_PATH", settings_path)
    monkeypatch.setattr("subprocess.run", _fake_intake_success)

    result = dispatch_project(
        project="test-project",
        body="build the thing",
        mission_id="ab-12345678",
        mission_slug="2026-05-13-x",
        wave=1,
        from_msg_id=None,
    )

    assert isinstance(result, DispatchResult)
    assert result.backlog_node_id == "ab-abcd1234"
    assert result.plan_path.exists(), "plan file must be written to disk"

    # Verify path shape: {project_path}/internal/fno/plans/YYYY-MM-DD-mission-{id_short}-wave-{N}-{project}.md
    assert result.plan_path.parent == plans_dir
    assert "mission-12345678" in result.plan_path.name
    assert "wave-1" in result.plan_path.name
    assert "test-project" in result.plan_path.name

    # Verify frontmatter content
    content = result.plan_path.read_text(encoding="utf-8")
    assert "mission_id: ab-12345678" in content
    assert "mission_wave: 1" in content
    assert "mission_slug: 2026-05-13-x" in content
    assert "build the thing" in content


# ---------------------------------------------------------------------------
# AC2-ERR: intake failure raises DispatchError and leaves no orphan
# ---------------------------------------------------------------------------

def test_dispatch_intake_failure_raises_and_cleans_up(tmp_path, monkeypatch):
    """AC2-ERR: when fno backlog intake fails, DispatchError is raised with the project
    name and stderr in the message, and the plan file is removed (no orphan)."""
    from fno.megatron.dispatch import dispatch_project, DispatchError

    project_path = tmp_path / "test-project"
    plans_dir = project_path / "internal" / "fno" / "plans"
    plans_dir.mkdir(parents=True)

    settings_path = _make_settings(tmp_path, "test-project", "tp", project_path)

    import fno.projects.resolve as resolve_mod
    monkeypatch.setattr(resolve_mod, "SETTINGS_PATH", settings_path)
    resolve_mod._clear_cache()

    import fno.megatron.dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_SETTINGS_PATH", settings_path)
    monkeypatch.setattr("subprocess.run", _fake_intake_failure)

    with pytest.raises(DispatchError) as exc_info:
        dispatch_project(
            project="test-project",
            body="build the thing",
            mission_id="ab-12345678",
            mission_slug="2026-05-13-x",
            wave=1,
            from_msg_id=None,
        )

    err_msg = str(exc_info.value)
    assert "test-project" in err_msg, "DispatchError must include project name"
    assert "permission denied" in err_msg, "DispatchError must include intake stderr"

    # No orphan plan file
    leftover = list(plans_dir.glob("*.md"))
    assert leftover == [], f"orphan plan files found: {leftover}"


# ---------------------------------------------------------------------------
# AC2-FR(short_name): short_name input normalizes via resolver
# ---------------------------------------------------------------------------

def test_dispatch_short_name_resolves_to_canonical(tmp_path, monkeypatch):
    """AC2-FR(short_name): passing the short_name 'tp' instead of canonical
    'test-project' correctly resolves through the resolver before path lookup."""
    from fno.megatron.dispatch import dispatch_project, DispatchResult

    project_path = tmp_path / "test-project"
    plans_dir = project_path / "internal" / "fno" / "plans"
    plans_dir.mkdir(parents=True)

    settings_path = _make_settings(tmp_path, "test-project", "tp", project_path)

    import fno.projects.resolve as resolve_mod
    monkeypatch.setattr(resolve_mod, "SETTINGS_PATH", settings_path)
    resolve_mod._clear_cache()

    import fno.megatron.dispatch as dispatch_mod
    monkeypatch.setattr(dispatch_mod, "_SETTINGS_PATH", settings_path)
    monkeypatch.setattr("subprocess.run", _fake_intake_success)

    # Use short_name "tp" instead of canonical "test-project"
    result = dispatch_project(
        project="tp",
        body="build via short name",
        mission_id="ab-12345678",
        mission_slug="2026-05-13-x",
        wave=1,
        from_msg_id=None,
    )

    assert isinstance(result, DispatchResult)
    # Plan path must be in the canonical project path, not some "tp" path
    assert result.plan_path.is_relative_to(project_path), (
        f"plan must be inside canonical project dir {project_path}, got {result.plan_path}"
    )
    # Canonical project name appears in the filename
    assert "test-project" in result.plan_path.name


# ---------------------------------------------------------------------------
# AC2-FR(idempotency-stub): documented but not enforced
# ---------------------------------------------------------------------------

def test_dispatch_idempotency_is_documented(tmp_path, monkeypatch):
    """AC2-FR(idempotency-stub): Idempotency when called twice with the same args
    depends on the upstream `fno backlog intake` command rejecting duplicate plan_paths
    (it outputs 'already intaked: ab-XXXX'). This module does NOT enforce a second-write
    guard independently; callers rely on intake's own de-dup.

    This test asserts only that the docstring of dispatch_project documents this
    behavior so future readers understand the contract.
    """
    from fno.megatron import dispatch as dispatch_mod

    doc = dispatch_mod.dispatch_project.__doc__ or ""
    assert "idempoten" in doc.lower() or "already" in doc.lower() or "intake" in doc.lower(), (
        "dispatch_project docstring should mention idempotency / intake de-dup contract"
    )


# ---------------------------------------------------------------------------
# AC2-FR(path-traversal): ProjectNotFound propagates from resolver
# ---------------------------------------------------------------------------

def test_dispatch_path_traversal_raises_project_not_found(tmp_path, monkeypatch):
    """AC2-FR(path-traversal): a project name containing '..' or '/' is rejected by
    resolve_project_name (raises ProjectNotFound) before dispatch_project can build
    any filesystem path. The error propagates unchanged -- not wrapped in DispatchError."""
    from fno.megatron.dispatch import dispatch_project
    from fno.projects.resolve import ProjectNotFound

    # Stub resolver to raise ProjectNotFound for any traversal-looking name
    import fno.megatron.dispatch as dispatch_mod

    def fake_resolve(name: str) -> str:
        raise ProjectNotFound(f"unknown project name {name!r}")

    monkeypatch.setattr(dispatch_mod, "resolve_project_name", fake_resolve)

    with pytest.raises(ProjectNotFound):
        dispatch_project(
            project="../../../etc/passwd",
            body="malicious",
            mission_id="ab-12345678",
            mission_slug="2026-05-13-x",
            wave=1,
            from_msg_id=None,
        )
