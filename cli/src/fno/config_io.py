"""Pure config file-reader leaf, shared by ``fno.config`` and ``fno.graph``.

This module exists to break the ``fno.config`` <-> ``fno.graph`` import cycle.
``graph/_intake.py`` (and its siblings) needs the low-level config *readers* but
not the ``fno.config`` god module; importing ``fno.config`` at top level closed a
load-time loop (config's ``id_prefix`` validator imports ``graph._constants``),
so graph pushed the import into function bodies. Giving graph a dependency-free
*leaf* to import instead removes the back-edge and lets both sides import at top
level.

It MUST live outside the ``config/`` package: a module at ``config/_io.py`` would
re-run ``config/__init__.py`` on import and reintroduce the half-init cycle. This
module imports only stdlib + pydantic + yaml; it never imports ``fno.config`` or
``fno.graph``. ``fno.config`` re-exports these names for back-compat, so existing
``from fno.config import read_config_flat`` callers keep working unchanged.
"""
from __future__ import annotations

import logging
import os
import tomllib
from pathlib import Path

import yaml
from pydantic import BaseModel

_LOG = logging.getLogger(__name__)


def _deep_merge(
    base: dict[str, object], override: dict[str, object]
) -> dict[str, object]:
    """Recursively merge ``override`` onto ``base``; ``override`` wins.

    Nested dicts merge key-by-key; everything else (scalars, lists, None)
    replaces wholesale. A project-level list such as ``external_reviewers``
    fully replaces the global list rather than concatenating, which keeps the
    merge predictable (no accidental duplicate or stale entries). Returns a new
    dict; neither input is mutated.
    """
    result = dict(base)
    for key, value in override.items():
        existing = result.get(key)
        if isinstance(existing, dict) and isinstance(value, dict):
            result[key] = _deep_merge(existing, value)
        elif isinstance(existing, dict) and value is None:
            # An empty override block parses as None (e.g. a bare `config:` line
            # with nothing indented under it). Do NOT let it overwrite an existing
            # dict, which would null out a nested model block (config/project/...)
            # and fail Pydantic validation (Gemini HIGH, PR #409).
            continue
        else:
            result[key] = value
    return result


def _apply_search_ceiling(candidates: list[Path]) -> list[Path]:
    """Drop candidates that resolve outside $FNO_CONFIG_SEARCH_ROOT.

    No-op when unset (all real usage). Tests set it to their tmpdir roots so the
    git-derived canonical candidate can never reach the developer's real checkout
    (repo_root/canonical climb via ``git worktree list``, which no HOME redirect
    can bound). Value is an os.pathsep-separated list because two legitimate test
    roots exist: the pytest basetemp and the redirected HOME.
    """
    raw = os.environ.get("FNO_CONFIG_SEARCH_ROOT")
    if not raw:
        return candidates
    roots: list[Path] = []
    for r in raw.split(os.pathsep):
        if not r:
            continue
        try:
            roots.append(Path(r).resolve())
        except OSError:
            pass
    if not roots:
        return candidates
    # resolve() is filesystem I/O (symlink walk) and can raise OSError on a
    # symlink loop / permission wall; config load is on every command's startup
    # path, so resolve each candidate once and degrade (drop) rather than crash.
    kept: list[Path] = []
    for c in candidates:
        try:
            resolved = c.resolve()
        except OSError:
            continue
        if any(resolved.is_relative_to(r) for r in roots):
            kept.append(c)
    return kept


def _prefer_toml(paths: list[Path]) -> list[Path]:
    """For each ``settings.yaml`` candidate, try its ``config.toml`` sibling first.

    Adds the new flat-TOML file as a higher-priority read candidate wherever the
    legacy YAML was a candidate, so a ``config.toml`` wins per-key while an
    existing ``settings.yaml`` still loads. Env-pinned non-YAML paths (e.g.
    ``/dev/null`` for test isolation) get no sibling and pass through untouched.
    """
    out: list[Path] = []
    for p in paths:
        if p.name == "settings.yaml":
            toml = p.with_name("config.toml")
            if toml not in out:
                out.append(toml)
        if p not in out:
            out.append(p)
    # Bound the final list too: direct-file readers (e.g. _intake's
    # project<->path map) reach this via config_read_candidates without going
    # through _settings_yaml_locations, and a cwd-relative candidate resolves
    # through a worktree symlink to the canonical checkout (the leak this fixes).
    return _apply_search_ceiling(out)


def _load_raw(path: Path) -> tuple[dict[str, object], bool]:
    """Load a settings file and return (data, parse_succeeded).

    Parses TOML for a ``.toml`` suffix (config.toml), YAML otherwise
    (settings.yaml). Returns ({}, False) on any OS or parse error so callers
    can fall through to the next candidate. Logs a WARNING on parse failure so
    the user knows their config was not applied.

    Returns (data, True) when the file parsed successfully (even if the dict
    is empty, i.e. the file was blank).
    """
    try:
        text = path.read_text(encoding="utf-8")
        if path.suffix == ".toml":
            data = tomllib.loads(text)
        else:
            data = yaml.safe_load(text)
        return (data if isinstance(data, dict) else {}, True)
    except (OSError, UnicodeDecodeError, yaml.YAMLError, tomllib.TOMLDecodeError) as exc:
        _LOG.warning(
            "config file at %s failed to parse: %s; using defaults",
            path,
            exc,
        )
        return ({}, False)


def _unwrap_config_dict(raw: dict[str, object]) -> dict[str, object]:
    """Normalize a settings dict to the FLAT canonical shape.

    Config fields live at the top level now (flat config.toml). Legacy files
    nest every setting under a ``config:`` key; lift those to the top level so
    both shapes validate into the same flat model. Top-level siblings the model
    ignores (``worktree``) and ``schema_version`` are preserved; a ``config``
    block key wins over a stray top-level key of the same name (canonical beats
    legacy). No-ops on an already-flat dict.
    """
    if not isinstance(raw, dict):
        return raw
    cfg = raw.get("config")
    # Accept a ConfigBlock instance too (e.g. SettingsModel(config=ConfigBlock(...))
    # in tests), not just a parsed dict. exclude_unset preserves which fields were
    # explicitly set, so partial-override semantics (e.g. a provider entry that
    # overrides only base_url, keeping the built-in api_key_env) survive.
    if isinstance(cfg, BaseModel):
        cfg = cfg.model_dump(exclude_unset=True)
    if not isinstance(cfg, dict):
        return raw
    rest = {k: v for k, v in raw.items() if k != "config"}
    return _deep_merge(rest, cfg)


def _global_settings_path() -> Path:
    """Resolve the per-user global settings.yaml path.

    Returns ``Path(FNO_GLOBAL_SETTINGS_PATH)`` when that environment variable
    is set to a non-empty value (use ``/dev/null`` to disable the global
    candidate in test isolation), otherwise the default
    ``~/.fno/settings.yaml``.

    This hook exists so unit tests pinning ``repo_root=tmp_path`` cannot leak
    the developer's real ``~/.fno/settings.yaml`` into assertions that
    expect an empty global config.

    Empty-string env var (e.g. ``FNO_GLOBAL_SETTINGS_PATH=``) is treated as
    "unset" rather than ``Path("")`` (which resolves to the CWD and would
    silently bypass the global config). An operator that genuinely wants to
    point at the CWD must say so explicitly: ``FNO_GLOBAL_SETTINGS_PATH=.``.
    """
    env = os.environ.get("FNO_GLOBAL_SETTINGS_PATH")
    if env:
        return Path(env)
    return Path.home() / ".fno" / "settings.yaml"


def read_config_flat(path: Path) -> dict[str, object]:
    """Read a single config file (config.toml -> TOML by suffix, else YAML) and
    return its FLAT dict (a legacy ``config:`` wrapper unwrapped). Returns {} on a
    missing or unparseable file. For the handful of consumers that read the
    config file DIRECTLY - the work.workspaces topology map, project detection -
    instead of through the cached ``load_settings`` (they need global-only reads,
    no per-process cache, or run at bootstrap).
    """
    data, ok = _load_raw(path)
    return _unwrap_config_dict(data) if ok else {}


def config_read_candidates(paths: list[Path]) -> list[Path]:
    """config.toml-first read candidates for a list of settings.yaml locations
    (public alias of ``_prefer_toml`` for direct-file readers)."""
    return _prefer_toml(paths)
