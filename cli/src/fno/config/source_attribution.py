"""Which config file decided a key: the loader-replay source attribution."""

from __future__ import annotations

from pathlib import Path
from typing import Optional

from fno.config_io import _deep_merge
from fno.config_io import _load_raw
from fno.config_io import _unwrap_config_dict


def resolve_source(
    key: str, root: Optional[Path] = None
) -> Optional[tuple[Path, list[Path]]]:
    """Which config file decided ``key``: ``(decider, overridden)`` or None.

    Consumes the SAME aliased layers :func:`load_settings` merges (via
    :func:`_aliased_layers`), in the same order, and attributes the key by
    REPLAYING the loader's merge: the decider is the highest-precedence file
    whose own value is the value the merge serves under the loader's own
    semantics - deep-merge highest-wins-per-key plus the post-merge
    ``_unwrap_config_dict`` flatten, where a ``config:``-wrapped block beats
    flat top-level keys. Presence per layer alone would mis-attribute exactly
    there: a flat project key that the wrapped global's block overrides at
    unwrap would name the project as decider while the model serves the
    global's value. Change detection alone had the mirror defect: a project
    restating the global's value verbatim left the credit with the global, and
    the source line read as the global overriding the local file.

    The overridden list names the lower setters whose value the merge
    discarded; a lower layer that set the same value is not overridden.

    The worktree-local ``config.local.toml`` enters as the highest layer
    through the same allowlist filter the loader applies, so a dropped
    non-allowlisted key can never masquerade as a source. A value that arrived
    through the legacy spelling reports the file that actually holds it (the
    alias ran per layer inside the shared collector).

    None = no file sets the key (the value is a built-in default).
    """
    from fno.config import _aliased_layers, _candidate_paths, _worktree_local_override

    candidates = _candidate_paths(root)
    layers = list(_aliased_layers(tuple(candidates)))
    if candidates:
        local_path = candidates[0].parent / "config.local.toml"
        if local_path.is_file() and not local_path.is_symlink():
            local_parsed, lok = _load_raw(local_path)
            if lok:
                override = _worktree_local_override(local_parsed)
                if override:
                    layers.insert(0, (local_path.resolve(), override))

    _MISSING = object()

    def _get(dotted: str, data: object) -> object:
        node: object = data
        for part in dotted.split("."):
            if not isinstance(node, dict) or part not in node:
                return _MISSING
            node = node[part]
        return node

    def _value(data: object) -> object:
        for v in variants:
            got = _get(v, data)
            if got is not _MISSING:
                return got
        return _MISSING

    # The same prefix tolerance get_cmd applies to lookups: a bare
    # `review.required_bots` and a legacy `config.`-prefixed spelling are one key.
    variants = [key, key[len("config.") :]] if key.startswith("config.") else [key, f"config.{key}"]

    # Replay lowest precedence first, loader order. A worktree's .fno/config.toml
    # is often a symlink to the canonical checkout's; candidate.resolve()
    # collapses both chain tiers onto one path, and the same file must not
    # replay twice and "override" itself.
    seen: set[Path] = set()
    setters: list[tuple[Path, object]] = []
    decider: Optional[Path] = None
    merged: dict[str, object] = {}
    prev: object = _MISSING
    for path, parsed in reversed(layers):
        if path in seen:
            continue
        seen.add(path)
        own = _value(_unwrap_config_dict(parsed))
        if own is not _MISSING:
            setters.append((path, own))
        merged = _deep_merge(merged, parsed)
        now = _value(_unwrap_config_dict(merged))
        if now is not _MISSING and now != prev:
            decider = path
        if own is not _MISSING and now is not _MISSING and own == now:
            # This layer's own value is the value the merge serves, so it
            # decides the key even when a lower file said the same thing.
            decider = path
        if now is not _MISSING:
            prev = now
    if not setters:
        return None
    assert decider is not None  # the first setter introduces the value: a change
    # An equal lower layer set the key but lost nothing, so it is not overridden.
    return (decider, [p for p, own in setters if p != decider and own != prev])
