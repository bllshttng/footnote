"""Keyed settings loader: the cache is keyed on its declaration, not on nothing.

fno.config is over the file budget and shrink-only, so the keyed pair moved
here (x-3d21 R5). The collaborators live in the package and are imported at
CALL time: a module-level import would be a cycle (the package re-exports
``load_settings`` from this module).
"""
from __future__ import annotations

import os
from functools import lru_cache
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from fno.config import SettingsModel


def _settings_key() -> tuple[Optional[str], ...]:
    """The declaration the settings resolution reads from the process.

    Everything ``_candidate_paths`` consults: the four FNO_ env overrides,
    ``HOME``, and the resolved repo root (itself keyed on cwd and
    ``FNO_REPO_ROOT``). Two calls whose key agrees read the same settings
    by construction; a test that changes any component gets a fresh load
    with no cache_clear, which retires the per-test clearer registry and
    the fixture swap (x-3d21 R5).
    """
    from fno.paths import resolve_repo_root

    env = os.environ.get
    return (
        env("FNO_CONFIG"),
        env("FNO_GLOBAL_SETTINGS_PATH"),
        env("FNO_CONFIG_SEARCH_ROOT"),
        env("FNO_NO_CANONICAL_CONFIG"),
        env("HOME"),
        str(resolve_repo_root()),
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
        _warn_unknown_keys,
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
    # so the message appears even if validation later raises.
    # The recursive walker handles nested blocks (paths, review, etc.) automatically;
    # there is no need for an additional explicit nested call (which caused duplicate emission).
    _warn_unknown_keys(raw, SettingsModel)

    raw = _revoke_unbacked_optouts(raw)
    return SettingsModel.model_validate(raw)
