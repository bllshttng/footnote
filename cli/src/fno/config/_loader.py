"""Keyed settings loader: the cache is keyed on its declaration, not on nothing.

fno.config is over the file budget and shrink-only, so the keyed pair moved
here (x-3d21 R5). The collaborators live in the package and are imported at
CALL time: a module-level import would be a cycle (the package re-exports
``load_settings`` from this module).
"""
from __future__ import annotations

import os
from functools import lru_cache
from pathlib import Path
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from fno.config import SettingsModel


def _canonical_root_from_gitfile(repo_root: Path) -> Optional[Path]:
    """The canonical root read off a linked worktree's ``.git`` pointer file.

    A linked worktree's ``.git`` is a FILE (``gitdir:
    <canonical>/.git/worktrees/<name>``), so the main checkout's root is the
    prefix before ``/.git/worktrees/``. Pure file IO: the settings cache key
    is computed on every load and must never shell out to
    ``git worktree list``. A real dir (this IS canonical) or any shape this
    cannot parse returns ``None`` and the canonical candidate is skipped, so
    it contributes nothing to the fingerprint, exactly like a missing file.
    """
    git_path = repo_root / ".git"
    if not git_path.is_file():
        return None
    try:
        text = git_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("gitdir:"):
            gitdir = line[len("gitdir:") :].strip()
            marker = "/.git/worktrees/"
            idx = gitdir.find(marker)
            if idx > 0:
                return Path(gitdir[:idx])
    return None


def _settings_fingerprint(repo_root: Path) -> tuple[tuple[str, int, int], ...]:
    """``(path, mtime_ns, size)`` per settings candidate that exists - the
    same triple ``config_io._global_merged_config`` keys on. An OSError
    (missing or unreadable file) contributes nothing, so creating a candidate
    later changes the key and reparses.

    The locations are stated directly instead of calling ``_candidate_paths``:
    that walk runs ``_ensure_migrated``, and the key is computed on EVERY
    settings read, so a migration check would ride each one. The canonical
    candidate is derived from the already-resolved repo root's ``.git``
    pointer, never from a fresh subprocess.
    """
    from fno.config_io import _global_settings_path

    env_config = os.environ.get("FNO_CONFIG")
    if env_config:
        locations = [Path(env_config)]
    else:
        locations = [
            repo_root / ".fno" / "config.toml",
            repo_root / ".fno" / "settings.yaml",
            repo_root / ".fno" / "config.local.toml",
        ]
        if os.environ.get("FNO_NO_CANONICAL_CONFIG") != "1":
            canonical = _canonical_root_from_gitfile(repo_root)
            if canonical is not None and canonical != repo_root:
                locations += [
                    canonical / ".fno" / "config.toml",
                    canonical / ".fno" / "settings.yaml",
                ]
        global_path = _global_settings_path()
        if global_path.name == "settings.yaml":
            locations.append(global_path.with_name("config.toml"))
        locations.append(global_path)
    fingerprint: list[tuple[str, int, int]] = []
    for candidate in locations:
        try:
            st = candidate.stat()
        except OSError:
            continue
        fingerprint.append((str(candidate), st.st_mtime_ns, st.st_size))
    return tuple(fingerprint)


def _settings_key() -> tuple:
    """The declaration the settings resolution reads from the process, plus
    a content fingerprint of the settings candidates.

    The declaration half: the four FNO_ env overrides, ``HOME``, and the
    resolved repo root (itself keyed on cwd and ``FNO_REPO_ROOT``). Two calls
    whose key agrees read the same settings by construction; a test that
    changes any component gets a fresh load with no cache_clear, which
    retires the per-test clearer registry and the fixture swap (x-3d21 R5).

    The fingerprint half exists because the declaration alone never covered
    the file CONTENTS - the old docstring claimed it covered "everything
    ``_candidate_paths`` consults", which is true of the locations and false
    of the bytes in them, and a long-lived process filled that gap with a
    stale parse. An edit now changes the key and reparses on its own, with
    no invalidation protocol for writers to remember. Cost: keys vary with
    file mtimes, so ``_load_settings_at``'s ``maxsize=8`` holds up to eight
    recent parses and an evicted declaration is simply re-collected on
    demand.
    """
    from fno.paths import resolve_repo_root

    env = os.environ.get
    repo_root = resolve_repo_root()
    return (
        env("FNO_CONFIG"),
        env("FNO_GLOBAL_SETTINGS_PATH"),
        env("FNO_CONFIG_SEARCH_ROOT"),
        env("FNO_NO_CANONICAL_CONFIG"),
        env("HOME"),
        str(repo_root),
        _settings_fingerprint(repo_root),
    )


@lru_cache(maxsize=8)
def _load_settings_at(key: tuple[Optional[str], ...]) -> "SettingsModel":
    """Load, deep-merge, and cache the settings for one declaration ``key``.

    Every existing candidate is read and deep-merged, highest priority winning
    key-by-key: $FNO_CONFIG (when set, the only candidate) ->
    <worktree>/.fno/settings.yaml -> <canonical>/.fno/settings.yaml
    -> ~/.fno/settings.yaml -> built-in defaults. See _candidate_paths for
    the canonical (main worktree from `git worktree list`) step that lets a
    linked worktree read shared config. A key absent from a higher-priority file
    falls through to the next file down, so global can hold shared defaults
    while each project sets only its deltas.

    Raises ValidationError on invalid values (glob chars, PATH_MAX, etc.).
    Emits WARNING for unknown keys.
    """
    from fno.config import (
        SettingsModel,
        _aliased_layers,
        _candidate_paths,
        _deep_merge,
        _layer_worktree_local_override,
        _revoke_unbacked_optouts,
        _unwrap_config_dict,
    )

    # Collect every candidate that exists and parses, in priority order
    # (project-local highest, global lowest). Files that fail to parse are
    # skipped (a WARNING is already emitted by _load_raw) so a corrupt
    # higher-priority file still falls through to a valid lower-priority one.
    # The walk (parse + per-layer legacy alias) is the ONE shared collector:
    # resolve_source replays the same layers, so the chain, its order, and the
    # alias pass (whose deprecation warnings fire once per process per chain,
    # not once per consumer) live in exactly one place.
    candidates = _candidate_paths()
    layers = list(_aliased_layers(tuple(candidates)))

    # Deep-merge lowest priority first so the highest-priority file wins per
    # key. config.obsidian.vault can come from global while
    # config.post_merge.parking_lot_path comes from the project file.
    # Legacy keys were aliased PER LAYER inside _aliased_layers, before this
    # merge, so a higher-priority file's legacy value still wins over a
    # lower-priority file's canonical value (and vice-versa). Aliasing only the
    # merged result would let a low-priority canonical key mask a high-priority
    # legacy key.
    raw: dict[str, object] = {}
    for _path, parsed in reversed(layers):
        raw = _deep_merge(raw, parsed)

    # Per-worktree local override (x-cbce). A real, non-symlinked local file is
    # layered only for the allowlisted collision keys.
    if candidates:
        raw = _layer_worktree_local_override(raw, candidates[0].parent)

    # _loaded_from records the PRIMARY (highest-priority) file present, for
    # `fno config doctor` and paths.config_file(). With layering there is no
    # single source; the highest-priority file is the most meaningful anchor
    # (Finding 3: paths.config_file must agree with the loader, not re-derive).
    import fno.config as _config

    _config._loaded_from = layers[0][0] if layers else None

    # Flatten the legacy config:-wrapped shape to the canonical top-level shape
    # before warning/validation so unknown-key warnings key off real block names
    # (the model is flat; a residual `config` key would look "unknown").
    raw = _unwrap_config_dict(raw)

    # Warn about unknown top-level and nested keys BEFORE model construction
    # so the message appears even if validation later raises. The recursive
    # walker handles nested blocks, so no second explicit nested call.
    #
    # Loaded by importlib, never a static import: an edge from this package to
    # the walker puts fno.config in a mypy SCC where graph._constants' lazy
    # __getattr__ re-exports degrade to Optional[Path] and fail two unrelated
    # modules. Measured: three errors with the plain import, none with this.
    import importlib

    importlib.import_module("fno.config_readback").warn_unknown_keys(raw, SettingsModel)

    raw = _revoke_unbacked_optouts(raw)
    return SettingsModel.model_validate(raw)
