"""Does the config read back the way an operator reads it?

The checks `fno config doctor` prints, plus the unknown-key walker and the one
provenance renderer they share. Rationale, specimens and measurements:
docs/architecture/config-readback.md.
"""
from __future__ import annotations

import difflib
import importlib
import logging
import os
import types
import typing
from pathlib import Path
from typing import Any, Optional

from pydantic import BaseModel

#: An unknown table with more leaves than this reports as the table.
_UNKNOWN_LEAF_CAP = 3
#: A leaf name in more sections than this is a common word, not a near miss.
_NEAR_MISS_CAP = 4
#: Top-level blocks the walker must not judge: another reader owns `kanban`,
#: and `providers` is the pre-rename spelling the loader still aliases across.
_UNMODELED_BLOCKS = frozenset({"kanban", "providers"})

_LOG = logging.getLogger("fno.config")

_Model = Optional[type[BaseModel]]


def _cfg() -> Any:
    """``fno.config``, by importlib: a static edge fails mypy. See the doc."""
    return importlib.import_module("fno.config")


def _field_models(annotation: object) -> tuple[_Model, _Model, _Model]:
    """``(dict value model, list item model, nested model)``. Both union spellings."""
    candidates = list(typing.get_args(annotation)) or [annotation]
    if typing.get_origin(annotation) not in (typing.Union, types.UnionType):
        candidates = [annotation, *candidates]
    nested: _Model = None
    for candidate in candidates:
        origin = typing.get_origin(candidate)
        args = typing.get_args(candidate)
        if origin is dict and len(args) == 2:
            if isinstance(args[1], type) and issubclass(args[1], BaseModel):
                return args[1], None, None
        elif origin is list and len(args) == 1:
            if isinstance(args[0], type) and issubclass(args[0], BaseModel):
                return None, args[0], None
        elif nested is None and isinstance(candidate, type) and issubclass(candidate, BaseModel):
            nested = candidate
    return None, None, nested


def warn_unknown_keys(
    data: dict[str, object], model: type[BaseModel], prefix: str = ""
) -> list[str]:
    """Dotted keys the model's field set does not carry. Logs under FNO_DEBUG."""
    _flatten_leaf_paths = _cfg()._flatten_leaf_paths

    unknown: list[str] = []
    known = set(model.model_fields.keys())
    for key in data:
        qualified = f"{prefix}.{key}" if prefix else key
        value = data[key]
        if key not in known:
            leaves = (
                [f"{qualified}.{leaf}" for leaf, _ in _flatten_leaf_paths(value)]
                if isinstance(value, dict) and value
                else []
            )
            unknown.extend(leaves if 0 < len(leaves) <= _UNKNOWN_LEAF_CAP else [qualified])
            continue
        mapped, item, nested = _field_models(model.model_fields[key].annotation)
        if item is not None and isinstance(value, list):
            for index, entry in enumerate(value):
                if isinstance(entry, dict):
                    unknown.extend(warn_unknown_keys(entry, item, prefix=f"{qualified}[{index}]"))
        elif not isinstance(value, dict):
            continue
        elif mapped is not None:
            for name, entry in value.items():
                if isinstance(entry, dict):
                    unknown.extend(warn_unknown_keys(entry, mapped, prefix=f"{qualified}.{name}"))
        elif nested is not None:
            unknown.extend(warn_unknown_keys(value, nested, prefix=qualified))
    # Only the outermost call logs: a recursive call hands its keys up, so
    # logging at every level repeats a nested key once per level above it.
    if not prefix and os.environ.get("FNO_DEBUG"):
        for qualified in unknown:
            _LOG.warning("settings: unknown key %r (ignored for forward compatibility)", qualified)
    return unknown


def source_note(key: str, root: Optional[Path] = None) -> Optional[str]:
    """``"set in <file>"`` when a config file decides ``key``, else None."""
    try:
        decided = _cfg().resolve_source(key, root)
    except Exception:  # noqa: BLE001 - a receipt, not the loader
        return None
    return f"set in {decided[0]}" if decided is not None else None


def check_config_files_read() -> list[str]:
    """Settings files the loader could not read back. An EMPTY table is legal."""
    try:
        from fno.config_io import _parse_settings

        seen: set[Path] = set()
        problems = []
        for path in _cfg()._candidate_paths():
            if path.is_file() and path.resolve() not in seen:
                seen.add(path.resolve())
                problems.append(_parse_settings(path)[1])
        return [p for p in problems if p is not None]
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []


def _near_miss_keys(unknown: str) -> list[str]:
    """Modeled keys the operator plausibly meant, in schema order.

    The same leaf name in another section first, then a close leaf name.
    """
    try:
        from fno.config.registry import FIELD_META
    except Exception:
        return []
    leaf = unknown.rsplit(".", 1)[-1]
    others = [key for key in FIELD_META if key != unknown]
    hits = [key for key in others if key.rsplit(".", 1)[-1] == leaf]
    if not hits:
        close = set(difflib.get_close_matches(leaf, [k.rsplit(".", 1)[-1] for k in others], 5, 0.85))
        hits = [key for key in others if key.rsplit(".", 1)[-1] in close]
    return hits if len(hits) <= _NEAR_MISS_CAP else []


def check_unknown_keys() -> list[str]:
    """Keys the model ignores, each named with the file that holds it."""
    try:
        from fno.config_io import _unwrap_config_dict

        model = _cfg().SettingsModel
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []

    problems: list[str] = []
    for path, parsed in _layers():
        try:
            flat = _unwrap_config_dict(parsed)
            unknown = warn_unknown_keys(
                {k: v for k, v in flat.items() if k not in _UNMODELED_BLOCKS}, model
            )
        except Exception:  # noqa: BLE001
            continue
        for key in unknown:
            hints = _near_miss_keys(key)
            tail = f"did you mean {' or '.join(hints)}?" if hints else "ignored"
            problems.append(f"{key} (set in {path}) is not a modeled config key; {tail}")
    return problems


def check_enabled_with_empty_population() -> list[str]:
    """A switch on with nothing that can satisfy it. Read the consumer first."""
    try:
        from fno.review.provider_resolution import available_provider_kinds

        if not bool(_cfg().load_settings().review.cross_model.enabled):
            return []
        kinds = [str(k).strip().lower() for k in available_provider_kinds()]
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []
    if any(kind != "claude" for kind in kinds):
        return []
    return [
        "review.cross_model.enabled is true and no non-claude provider is dispatchable. "
        f"available reviewer kinds: {', '.join(kinds) or 'none'}. The switch buys nothing "
        "for a claude-written change. Only where codex or gemini wrote the code is a "
        "claude reviewer a different family."
    ]


def contributing_files() -> list[str]:
    """The files that actually contributed to the merge, highest first."""
    return [str(path) for path, _ in _layers()]


def _layers() -> list[tuple[Path, dict[str, object]]]:
    """The loader's own per-file collector, or empty when it cannot run."""
    try:
        cfg = _cfg()
        return list(cfg._aliased_layers(tuple(cfg._candidate_paths())))
    except Exception:  # noqa: BLE001 - a receipt, not the loader
        return []
