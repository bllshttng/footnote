"""Tests for inbox store.py - resolve_project() (Task 2.2)."""
from __future__ import annotations

from pathlib import Path

import pytest
import yaml


def _write_settings(path: Path, project: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(yaml.dump({"project": project}))


# ---------------------------------------------------------------------------
# AC1-HP: settings.yaml with project:
# ---------------------------------------------------------------------------

def test_ac1_hp_from_settings(tmp_path):
    """AC1-HP: project: in .fno/settings.yaml is returned."""
    from fno.inbox.store import resolve_project

    settings = tmp_path / ".fno" / "settings.yaml"
    _write_settings(settings, "acme-web")

    result = resolve_project(cwd=tmp_path)
    assert result == "acme-web"


def test_ac1_hp_override_wins(tmp_path):
    """AC1-HP: override= takes precedence over any settings file."""
    from fno.inbox.store import resolve_project

    # Even with a settings file, override wins
    settings = tmp_path / ".fno" / "settings.yaml"
    _write_settings(settings, "acme-web")

    result = resolve_project(cwd=tmp_path, override="fno")
    assert result == "fno"


def test_ac1_hp_override_no_settings():
    """AC1-HP: override= works with no settings file at all."""
    from fno.inbox.store import resolve_project

    result = resolve_project(cwd=Path("/tmp"), override="my-project")
    assert result == "my-project"


def test_ac1_hp_walk_up(tmp_path):
    """AC1-HP: walks up directory tree to find settings.yaml."""
    from fno.inbox.store import resolve_project

    # settings.yaml at top level; cwd at nested subdir
    settings = tmp_path / ".fno" / "settings.yaml"
    _write_settings(settings, "top-level-project")

    nested = tmp_path / "src" / "module"
    nested.mkdir(parents=True)

    result = resolve_project(cwd=nested)
    assert result == "top-level-project"


# ---------------------------------------------------------------------------
# AC2-ERR: No settings.yaml -> ProjectIdentificationError
# ---------------------------------------------------------------------------

def test_ac2_err_no_settings():
    """AC2-ERR: no settings.yaml anywhere raises ProjectIdentificationError."""
    from fno.inbox.store import ProjectIdentificationError, resolve_project

    with pytest.raises(ProjectIdentificationError) as exc_info:
        resolve_project(cwd=Path("/tmp"))

    assert str(exc_info.value) == "set 'project' in .fno/config.toml or pass --from"


def test_ac2_err_exact_message(tmp_path):
    """AC2-ERR: error message is exactly the canonical string."""
    from fno.inbox.store import ProjectIdentificationError, resolve_project

    with pytest.raises(ProjectIdentificationError) as exc_info:
        resolve_project(cwd=tmp_path)

    assert str(exc_info.value) == "set 'project' in .fno/config.toml or pass --from"


# ---------------------------------------------------------------------------
# AC4-EDGE: cwd in /tmp with no project -> same error (no basename fallback)
# ---------------------------------------------------------------------------

def test_ac4_edge_tmp_dir_no_fallback():
    """AC4-EDGE: /tmp has no settings, no fallback to cwd basename."""
    from fno.inbox.store import ProjectIdentificationError, resolve_project

    with pytest.raises(ProjectIdentificationError):
        resolve_project(cwd=Path("/tmp"))


# ---------------------------------------------------------------------------
# AC4-EDGE: settings.yaml exists but no project: field
# ---------------------------------------------------------------------------

def test_ac4_edge_settings_missing_project_field(tmp_path):
    """AC4-EDGE: settings.yaml present but no project: field -> walk up and fail."""
    from fno.inbox.store import ProjectIdentificationError, resolve_project

    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(yaml.dump({"config": {"v2_enabled": True}}))

    with pytest.raises(ProjectIdentificationError):
        resolve_project(cwd=tmp_path)


# ---------------------------------------------------------------------------
# Regression ab-6e5c5da0: top-level project: is now a mapping, not a scalar
# ---------------------------------------------------------------------------

def test_top_level_project_mapping_uses_id(tmp_path):
    """Regression for ab-6e5c5da0.

    settings.yaml now carries a top-level `project:` *mapping*
    (`{id, vision, goals, constraints}`). resolve_project previously did
    `str(data["project"])`, stringifying the whole dict into an invalid
    project name with path separators (raising ValueError downstream and
    a Traceback every session). It must extract `.id`.
    """
    from fno.inbox.store import resolve_project

    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(yaml.dump({"project": {"id": "fno", "vision": "x", "goals": []}}))

    result = resolve_project(cwd=tmp_path)
    assert result == "fno"


def test_config_project_id_preferred_over_deprecated_top_level(tmp_path):
    """config.project.id is canonical and wins over deprecated top-level project.id."""
    from fno.inbox.store import resolve_project

    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        yaml.dump({
            "project": {"id": "old-name"},
            "config": {"project": {"id": "canonical-name"}},
        })
    )

    result = resolve_project(cwd=tmp_path)
    assert result == "canonical-name"


def test_project_mapping_without_id_walks_up(tmp_path):
    """A project: mapping lacking an id is treated as absent -> keep searching."""
    from fno.inbox.store import ProjectIdentificationError, resolve_project

    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(yaml.dump({"project": {"vision": "x"}}))

    with pytest.raises(ProjectIdentificationError):
        resolve_project(cwd=tmp_path)
