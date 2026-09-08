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


def _field_models(annotation: object) -> tuple[Optional[type[BaseModel]], Optional[type[BaseModel]]]:
    """``(dict[str, Model] value model, nested model)`` for one field.

    Both union spellings resolve, so a model switching from
    ``Optional[dict[...]]`` to ``dict[...] | None`` cannot silently regress to
    walking the map's keys as field names.
    """
    candidates = list(typing.get_args(annotation)) or [annotation]
    if typing.get_origin(annotation) not in (typing.Union, types.UnionType):
        candidates = [annotation, *candidates]
    nested: Optional[type[BaseModel]] = None
    for candidate in candidates:
        if typing.get_origin(candidate) is dict:
            args = typing.get_args(candidate)
            if len(args) == 2 and isinstance(args[1], type) and issubclass(args[1], BaseModel):
                return args[1], None
        elif nested is None and isinstance(candidate, type) and issubclass(candidate, BaseModel):
            nested = candidate
    return None, nested


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
        if not isinstance(value, dict):
            continue
        mapped, nested = _field_models(model.model_fields[key].annotation)
        if mapped is not None:
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


def _read_candidates() -> tuple[list[tuple[Path, dict[str, object]]], list[str]]:
    """One walk: the layers that parsed, and the errors from those that did not.

    A file parsing to an EMPTY table is legal and reports no error.
    """
    from fno.config import _candidate_paths
    from fno.config_io import _parse_settings

    layers: list[tuple[Path, dict[str, object]]] = []
    errors: list[str] = []
    seen: set[Path] = set()
    for path in _candidate_paths():
        if not path.is_file() or path.resolve() in seen:
            continue
        seen.add(path.resolve())
        parsed, error = _parse_settings(path)
        if error is None:
            layers.append((path, parsed))
        else:
            errors.append(error)
    return layers, errors


def check_config_files_read() -> list[str]:
    """Settings files the loader could not read back."""
    try:
        return _read_candidates()[1]
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []


def _near_miss_keys(unknown: str) -> list[str]:
    """Modeled keys sharing ``unknown``'s trailing leaf name, in schema order."""
    try:
        from fno.config.registry import FIELD_META
    except Exception:
        return []
    leaf = unknown.rsplit(".", 1)[-1]
    hits = [key for key in FIELD_META if key != unknown and key.rsplit(".", 1)[-1] == leaf]
    return hits if len(hits) <= _NEAR_MISS_CAP else []


def check_unknown_keys() -> list[str]:
    """Keys the model ignores, each named with the file that holds it.

    Layers are walked separately, not merged, so every message names a file. A
    clean install reports nothing: the report is the operator's own wrong key.
    """
    try:
        from fno.config import SettingsModel
        from fno.config_io import _unwrap_config_dict

        layers = _read_candidates()[0]
    except Exception:  # noqa: BLE001 - a report, not the loader
        return []

    problems: list[str] = []
    for path, parsed in layers:
        try:
            unknown = warn_unknown_keys(_unwrap_config_dict(parsed), SettingsModel)
        except Exception:  # noqa: BLE001
            continue
        for key in unknown:
            hints = _near_miss_keys(key)
            tail = f"did you mean {' or '.join(hints)}?" if hints else "ignored"
            problems.append(f"{key} (set in {path}) is not a modeled config key; {tail}")
    return problems


def check_enabled_with_empty_population() -> list[str]:
    """A switch that is on with nothing that can satisfy it.

    Only pairs proved coupled by reading the consumer belong here; a pair added
    from the leaf name alone is how this defect was first mis-diagnosed.
    """
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
        f"available reviewer kinds: {', '.join(kinds) or 'none'}. The diversity "
        "requirement can never be met until a provider record is added."
    ]


def contributing_files() -> list[str]:
    """The files that actually contributed to the merge, highest first."""
    try:
        from fno.config import _aliased_layers, _candidate_paths

        return [str(path) for path, _ in _aliased_layers(tuple(_candidate_paths()))]
    except Exception:  # noqa: BLE001 - a receipt, not the loader
        return []
