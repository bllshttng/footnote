"""Tests for fno.projects.resolve: canonical project-name resolver.

Five ACs:
  AC1-HP  : canonical name resolves to itself
  AC1-EDGE: short_name resolves to canonical
  AC1-ERR : unknown name raises ProjectNotFound with input + known names
  AC1-FR-e: settings.yaml missing raises a clear error
  AC1-FR-d: duplicate short_name across workspaces raises DuplicateShortName at load time
"""
from __future__ import annotations

import textwrap
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _write_settings(tmp_path: Path, content: str) -> Path:
    p = tmp_path / "settings.yaml"
    p.write_text(textwrap.dedent(content).lstrip(), encoding="utf-8")
    return p


def _patch_settings(monkeypatch, tmp_path, content):
    """Write a tmp settings.yaml, patch SETTINGS_PATH, clear cache."""
    from fno.projects import resolve as resolve_mod

    path = _write_settings(tmp_path, content)
    monkeypatch.setattr(resolve_mod, "SETTINGS_PATH", path)
    resolve_mod._clear_cache()
    return path


# ---------------------------------------------------------------------------
# AC1-HP: canonical name resolves to itself
# ---------------------------------------------------------------------------

def test_canonical_name_resolves_to_itself(tmp_path, monkeypatch):
    _patch_settings(
        monkeypatch,
        tmp_path,
        """
        work:
          workspaces:
            ws1:
              projects:
                - name: example-pipeline
                  short_name: etl
        """,
    )
    from fno.projects.resolve import resolve_project_name

    result = resolve_project_name("example-pipeline")
    assert result == "example-pipeline"


# ---------------------------------------------------------------------------
# config.work nesting: the canonical ~/.fno/settings.yaml shape (cross-project-spawn)
# ---------------------------------------------------------------------------

def test_config_work_nesting_resolves(tmp_path, monkeypatch):
    """The shipped settings.yaml nests the registry under `config.work.*`, not
    top-level `work.*`. resolve_project_name must read it there too, matching
    graph._intake.project_root_from_settings so the two resolvers cannot drift."""
    _patch_settings(
        monkeypatch,
        tmp_path,
        """
        config:
          work:
            workspaces:
              main:
                projects:
                  - name: example-pipeline
                    short_name: etl
        """,
    )
    from fno.projects.resolve import resolve_project_name

    assert resolve_project_name("example-pipeline") == "example-pipeline"
    assert resolve_project_name("etl") == "example-pipeline"


# ---------------------------------------------------------------------------
# AC1-EDGE: short_name resolves to canonical name
# ---------------------------------------------------------------------------

def test_short_name_resolves_to_canonical(tmp_path, monkeypatch):
    _patch_settings(
        monkeypatch,
        tmp_path,
        """
        work:
          workspaces:
            ws1:
              projects:
                - name: example-pipeline
                  short_name: etl
        """,
    )
    from fno.projects.resolve import resolve_project_name

    result = resolve_project_name("etl")
    assert result == "example-pipeline"


# ---------------------------------------------------------------------------
# AC1-ERR: unknown raises ProjectNotFound with input + known canonical names
# ---------------------------------------------------------------------------

def test_unknown_raises_project_not_found(tmp_path, monkeypatch):
    _patch_settings(
        monkeypatch,
        tmp_path,
        """
        work:
          workspaces:
            ws1:
              projects:
                - name: example-pipeline
                  short_name: etl
                - name: acme-web
                  short_name: web
        """,
    )
    from fno.projects.resolve import ProjectNotFound, resolve_project_name

    with pytest.raises(ProjectNotFound) as exc_info:
        resolve_project_name("nonexistent-repo")

    msg = str(exc_info.value)
    assert "nonexistent-repo" in msg
    assert "example-pipeline" in msg
    assert "acme-web" in msg


# ---------------------------------------------------------------------------
# AC1-FR-e: settings.yaml absent raises a clear error
# ---------------------------------------------------------------------------

def test_missing_settings_yaml_raises_clear_error(tmp_path, monkeypatch):
    from fno.projects import resolve as resolve_mod

    absent = tmp_path / "does_not_exist" / "settings.yaml"
    monkeypatch.setattr(resolve_mod, "SETTINGS_PATH", absent)
    resolve_mod._clear_cache()

    from fno.projects.resolve import resolve_project_name

    with pytest.raises(Exception) as exc_info:
        resolve_project_name("anything")

    # Message must hint that settings.yaml is the problem
    msg = str(exc_info.value)
    assert "settings.yaml" in msg.lower() or "settings" in msg.lower()


# ---------------------------------------------------------------------------
# AC1-FR-d: duplicate short_name across workspaces raises DuplicateShortName
# ---------------------------------------------------------------------------

def test_duplicate_short_name_raises_at_load_time(tmp_path, monkeypatch):
    _patch_settings(
        monkeypatch,
        tmp_path,
        """
        work:
          workspaces:
            ws1:
              projects:
                - name: project-alpha
                  short_name: shared-alias
            ws2:
              projects:
                - name: project-beta
                  short_name: shared-alias
        """,
    )
    from fno.projects.resolve import DuplicateShortName, resolve_project_name

    with pytest.raises(DuplicateShortName) as exc_info:
        resolve_project_name("anything")

    msg = str(exc_info.value)
    assert "project-alpha" in msg
    assert "project-beta" in msg
