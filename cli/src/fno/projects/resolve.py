"""Canonical project-name resolver for the fno fleet orchestrator.

Maps project ``name`` or ``short_name`` values to the canonical ``name``
declared in ``~/.fno/settings.yaml`` under
``work.workspaces.{ws_name}.projects[]``.

Public surface
--------------
- ``resolve_project_name(s)`` -- resolve name-or-short_name -> canonical name.
- ``_clear_cache()``          -- test helper; resets the module-level cache.
- ``ProjectNotFound``         -- raised on unknown input.
- ``DuplicateShortName``      -- raised at cache-build time on conflicting aliases.
- ``SETTINGS_PATH``           -- module-level Path; monkeypatch in tests.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Optional


SETTINGS_PATH: Path = (
    Path(os.path.expanduser("~")) / ".fno" / "settings.yaml"
)

# Module-level cache: {name_or_short_name: canonical_name}
_CACHE: Optional[dict[str, str]] = None


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------

class SettingsNotFound(Exception):
    """Raised when settings.yaml does not exist at the expected path."""


class ProjectNotFound(Exception):
    """Raised when the input string does not match any known project name."""


class DuplicateShortName(Exception):
    """Raised when two projects in different workspaces share a short_name."""


# ---------------------------------------------------------------------------
# Cache builder
# ---------------------------------------------------------------------------

def _build_cache() -> dict[str, str]:
    """Read SETTINGS_PATH and build the {alias: canonical_name} map.

    Raises
    ------
    SettingsNotFound  -- if the file does not exist.
    ProjectNotFound   -- if the YAML is parseable but has no work.workspaces.
    DuplicateShortName -- if two projects share the same name/short_name key.
    yaml.YAMLError    -- (re-raised as cause) if YAML parsing fails.
    """
    try:
        import yaml
    except ImportError:  # pragma: no cover - PyYAML is a hard CLI dep
        raise ImportError("PyYAML is required; install it with: pip install pyyaml")

    if not SETTINGS_PATH.exists():
        raise SettingsNotFound(
            f"settings.yaml not found at {SETTINGS_PATH}; "
            "run 'fno setup' or create ~/.fno/settings.yaml"
        )

    try:
        data = yaml.safe_load(SETTINGS_PATH.read_text(encoding="utf-8")) or {}
    except yaml.YAMLError as exc:
        raise ProjectNotFound(
            f"settings.yaml at {SETTINGS_PATH} contains invalid YAML"
        ) from exc

    if not isinstance(data, dict):
        return {}

    # Accept both the nested `config.work.workspaces` schema (the shape the
    # canonical ~/.fno/settings.yaml ships) and the legacy top-level `work.*`,
    # mirroring graph._intake.project_root_from_settings so the two resolvers
    # cannot drift on where the registry lives. Guard `config` with isinstance:
    # a malformed YAML where `config` is a scalar/list would make `.get("work")`
    # raise AttributeError instead of degrading to the empty-registry path.
    config = data.get("config")
    work = (config.get("work") if isinstance(config, dict) else None) or data.get("work") or {}
    if not isinstance(work, dict):
        return {}

    workspaces = work.get("workspaces", {})
    if not isinstance(workspaces, dict):
        return {}

    mapping: dict[str, str] = {}

    for _ws_name, ws_data in workspaces.items():
        if not isinstance(ws_data, dict):
            continue
        projects = ws_data.get("projects", [])
        if not isinstance(projects, list):
            continue
        for project in projects:
            if not isinstance(project, dict):
                continue
            canonical = project.get("name")
            if not isinstance(canonical, str) or not canonical:
                continue
            short = project.get("short_name")

            # Register canonical name
            _register(mapping, canonical, canonical)

            # Register short_name alias (if distinct from canonical)
            if isinstance(short, str) and short and short != canonical:
                _register(mapping, short, canonical)

    return mapping


def _register(mapping: dict[str, str], key: str, canonical: str) -> None:
    """Insert key -> canonical; raise DuplicateShortName on collision."""
    if key in mapping and mapping[key] != canonical:
        raise DuplicateShortName(
            f"short_name/name {key!r} is claimed by both "
            f"{mapping[key]!r} and {canonical!r}; "
            "remove one of the duplicate aliases from settings.yaml"
        )
    mapping[key] = canonical


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

def _get_cache() -> dict[str, str]:
    global _CACHE
    if _CACHE is None:
        _CACHE = _build_cache()
    return _CACHE


def resolve_project_name(s: str) -> str:
    """Return the canonical project name for *s* (name or short_name).

    Parameters
    ----------
    s:
        A project ``name`` or ``short_name`` as declared in
        ``~/.fno/settings.yaml``.

    Returns
    -------
    str
        The canonical ``name`` for that project.

    Raises
    ------
    ProjectNotFound
        If *s* does not match any known name or short_name.
    DuplicateShortName
        If the settings.yaml cache cannot be built due to a conflicting alias.
    SettingsNotFound
        If settings.yaml does not exist.
    """
    cache = _get_cache()
    if s in cache:
        return cache[s]
    known = sorted(
        {v for v in cache.values()}  # deduplicate (short_names share canonicals)
    )
    raise ProjectNotFound(
        f"unknown project name {s!r}; known canonical names: {known}"
    )


def _clear_cache() -> None:
    """Test helper: reset the module-level cache so the next call re-reads disk."""
    global _CACHE
    _CACHE = None
