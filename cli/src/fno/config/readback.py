"""Does the config read back the way an operator reads it?

The checks `fno config doctor` prints, plus the unknown-key walker and the one
provenance renderer they share. Rationale, specimens and the measurements
behind both caps: docs/architecture/config-readback.md.
"""
from __future__ import annotations

import logging
import os
import types
import typing
from pathlib import Path
from typing import Optional

from pydantic import BaseModel

#: An unknown table with more leaves than this reports as the table.
_UNKNOWN_LEAF_CAP = 3
#: A leaf name in more sections than this is a common word, not a near miss.
_NEAR_MISS_CAP = 4

_LOG = logging.getLogger("fno.config")


def _field_models(
    annotation: object,
) -> tuple[Optional[type[BaseModel]], Optional[type[BaseModel]], Optional[type[BaseModel]]]:
    """``(dict value model, list item model, nested model)``. Both union spellings."""
    candidates = list(typing.get_args(annotation)) or [annotation]
    if typing.get_origin(annotation) not in (typing.Union, types.UnionType):
        candidates = [annotation, *candidates]
    nested: Optional[type[BaseModel]] = None
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
    """Dotted keys not in the model's field set. Logs them under FNO_DEBUG.

    A ``dict[str, Model]`` field's keys are operator-chosen names, so each
    VALUE is walked with the map key in the prefix.
    """
    from fno.config import _flatten_leaf_paths

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
                    unknown.extend(
                        warn_unknown_keys(entry, item, prefix=f"{qualified}[{index}]")
                    )
        elif not isinstance(value, dict):
            continue
        elif mapped is not None:
            for name, entry in value.items():
                if isinstance(entry, dict):
                    unknown.extend(warn_unknown_keys(entry, mapped, prefix=f"{qualified}.{name}"))
        elif nested is not None:
            unknown.extend(warn_unknown_keys(value, nested, prefix=qualified))
    if os.environ.get("FNO_DEBUG"):
        for qualified in unknown:
            _LOG.warning("settings: unknown key %r (ignored for forward compatibility)", qualified)
    return unknown


def source_note(key: str, root: Optional[Path] = None) -> Optional[str]:
    """``"set in <file>"`` when a config file decides ``key``, else None."""
    from fno.config import resolve_source

    try:
        decided = resolve_source(key, root)
    except Exception:  # noqa: BLE001 - a receipt, not the loader
        return None
    return f"set in {decided[0]}" if decided is not None else None


def check_config_files_read() -> list[str]:
    """Settings files the loader could not read back. An EMPTY table is legal."""
    try:
        from fno.config import _candidate_paths
        from fno.config_io import _parse_settings
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []

    errors: list[str] = []
    seen: set[Path] = set()
    for path in _candidate_paths():
        if not path.is_file() or path.resolve() in seen:
            continue
        seen.add(path.resolve())
        error = _parse_settings(path)[1]
        if error is not None:
            errors.append(error)
    return errors


def _edit_distance_le_1(a: str, b: str) -> bool:
    """True if ``a`` and ``b`` differ by at most one insert/delete/substitute."""
    if a == b:
        return True
    la, lb = len(a), len(b)
    if abs(la - lb) > 1:
        return False
    if la == lb:
        return sum(1 for x, y in zip(a, b) if x != y) == 1
    short, long = (a, b) if la < lb else (b, a)
    i = j = edits = 0
    while i < len(short) and j < len(long):
        if short[i] == long[j]:
            i += 1
        else:
            edits += 1
            if edits > 1:
                return False
        j += 1
    return True


def _near_miss_keys(unknown: str) -> list[str]:
    """Modeled keys the operator plausibly meant, in schema order.

    Same leaf name in another section first, because that is the wrong-section
    case. Failing that, a leaf within one edit, which is the misspelling case.
    """
    try:
        from fno.config.registry import FIELD_META
    except Exception:
        return []
    leaf = unknown.rsplit(".", 1)[-1]
    others = [key for key in FIELD_META if key != unknown]
    hits = [key for key in others if key.rsplit(".", 1)[-1] == leaf]
    if not hits:
        hits = [key for key in others if _edit_distance_le_1(key.rsplit(".", 1)[-1], leaf)]
    return hits if len(hits) <= _NEAR_MISS_CAP else []


#: Top-level blocks the walker must not judge. `kanban` is real config that
#: another reader owns (the board renderer reads it straight out of the file).
#: `providers` is the pre-rename spelling of `accounts`; the loader's alias
#: copies it across and leaves it in place, so it works and is not unknown.
_UNMODELED_BLOCKS = frozenset({"kanban", "providers"})


def check_unknown_keys() -> list[str]:
    """Keys the model ignores, each named with the file that holds it.

    Reads `_aliased_layers`, the loader's own per-file collector, so a legacy
    spelling is never reported as a typo and each message names one file.
    """
    try:
        from fno.config import (
            SettingsModel,
            _aliased_layers,
            _candidate_paths,
        )
        from fno.config_io import _unwrap_config_dict

        layers = _aliased_layers(tuple(_candidate_paths()))
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []

    problems: list[str] = []
    for path, parsed in layers:
        try:
            flat = _unwrap_config_dict(parsed)
            unknown = warn_unknown_keys(
                {k: v for k, v in flat.items() if k not in _UNMODELED_BLOCKS}, SettingsModel
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
        from fno.config import load_settings
        from fno.review.provider_resolution import available_provider_kinds

        if not bool(load_settings().review.cross_model.enabled):
            return []
        kinds = [str(k).strip().lower() for k in available_provider_kinds()]
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []
    if any(kind != "claude" for kind in kinds):
        return []
    return [
        "review.cross_model.enabled is true and no non-claude provider is dispatchable; "
        f"available reviewer kinds: {', '.join(kinds) or 'none'}. The switch buys "
        "nothing for a claude-written change, which is most of them; a claude "
        "reviewer is a different family only when codex or gemini wrote the code."
    ]


def contributing_files() -> list[str]:
    """The files that actually contributed to the merge, highest first."""
    try:
        from fno.config import _aliased_layers, _candidate_paths

        return [str(path) for path, _ in _aliased_layers(tuple(_candidate_paths()))]
    except Exception:  # noqa: BLE001 - a receipt, not the loader
        return []
