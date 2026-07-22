"""Unit tests for ``project_root_from_settings`` in ``fno.graph._intake``.

Inverse of ``detect_project_from_settings``: maps a project name to its
work-map root path. All tests use tmp settings files (never touch real
~/.fno/) and patch ``_settings_candidate_paths`` to stay hermetic.
"""
from __future__ import annotations

import os
import textwrap
from pathlib import Path
from unittest.mock import patch

import pytest

from fno.graph._intake import project_root_from_settings


def _patch_candidates(tmp_settings: Path):
    """Return a context manager that makes _settings_candidate_paths return [tmp_settings]."""
    return patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[tmp_settings],
    )


# ---------------------------------------------------------------------------
# 1. workspaces schema: name->path lookup
# ---------------------------------------------------------------------------

def test_workspaces_schema_returns_expanded_path(tmp_path):
    """AC1: workspaces schema project name resolves to abspath(expanduser(path))."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: fno
                  path: ~/code/me/fno
    """))
    with _patch_candidates(cfg):
        result = project_root_from_settings("fno")

    expected = os.path.abspath(os.path.expanduser("~/code/me/fno"))
    assert result == expected


# ---------------------------------------------------------------------------
# 2. legacy flat schema: work.projects.{name}.path
# ---------------------------------------------------------------------------

def test_legacy_flat_schema_returns_expanded_path(tmp_path):
    """AC2: flat work.projects schema resolves project name to abspath(expanduser(path))."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(textwrap.dedent("""\
        work:
          projects:
            fno:
              path: ~/code/me/fno
    """))
    with _patch_candidates(cfg):
        result = project_root_from_settings("fno")

    expected = os.path.abspath(os.path.expanduser("~/code/me/fno"))
    assert result == expected


# ---------------------------------------------------------------------------
# 3. unmapped project -> None
# ---------------------------------------------------------------------------

def test_unmapped_project_returns_none(tmp_path):
    """AC3: a project not in the settings returns None."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: fno
                  path: ~/code/me/fno
    """))
    with _patch_candidates(cfg):
        result = project_root_from_settings("no-such-project")

    assert result is None


# ---------------------------------------------------------------------------
# 4. malformed YAML file -> None (does not raise)
# ---------------------------------------------------------------------------

def test_malformed_yaml_returns_none(tmp_path):
    """AC4: a malformed settings file returns None without raising."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text("work: [\nbad: yaml: [\n")
    with _patch_candidates(cfg):
        result = project_root_from_settings("fno")

    assert result is None


# ---------------------------------------------------------------------------
# 5. missing settings file in candidates -> None
# ---------------------------------------------------------------------------

def test_missing_settings_file_returns_none(tmp_path):
    """AC5: a candidate path that does not exist returns None."""
    missing = tmp_path / "does_not_exist.yaml"
    with _patch_candidates(missing):
        result = project_root_from_settings("fno")

    assert result is None


# ---------------------------------------------------------------------------
# 6. same name in two candidate files -> first candidate wins
# ---------------------------------------------------------------------------

def test_first_candidate_wins(tmp_path):
    """AC6: when two candidate files map the same project, the first one wins."""
    first = tmp_path / "first.yaml"
    second = tmp_path / "second.yaml"

    first.write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: fno
                  path: /first/path
    """))
    second.write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: fno
                  path: /second/path
    """))

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[first, second],
    ):
        result = project_root_from_settings("fno")

    assert result == "/first/path"


# ---------------------------------------------------------------------------
# 7. tilde + relative path values -> abspath'd absolute return
# ---------------------------------------------------------------------------

def test_tilde_path_expanded(tmp_path):
    """AC7: path values with ~ are expanded and made absolute."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(textwrap.dedent("""\
        work:
          projects:
            myproj:
              path: ~/code/myproj
    """))
    with _patch_candidates(cfg):
        result = project_root_from_settings("myproj")

    assert result == os.path.abspath(os.path.expanduser("~/code/myproj"))
    # Must be absolute, no tilde
    assert result.startswith("/")
    assert "~" not in result


# ---------------------------------------------------------------------------
# 8. project=None/empty -> None (guard, no crash)
# ---------------------------------------------------------------------------

def test_none_project_returns_none(tmp_path):
    """AC8: project=None returns None immediately (guard)."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text("work:\n  projects:\n    fno:\n      path: /some/path\n")
    with _patch_candidates(cfg):
        result = project_root_from_settings(None)  # type: ignore[arg-type]

    assert result is None


def test_empty_string_project_returns_none(tmp_path):
    """AC8b: project='' returns None immediately (guard)."""
    cfg = tmp_path / "settings.yaml"
    cfg.write_text("work:\n  projects:\n    fno:\n      path: /some/path\n")
    with _patch_candidates(cfg):
        result = project_root_from_settings("")

    assert result is None
