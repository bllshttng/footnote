"""Schema walker + generators for the unified config.

The Pydantic ``SettingsModel`` is the single source of truth for what a config
key is, its type, and its default. This module walks that model (plus the
presentation ``registry``) and emits three derived artifacts:

  * ``--json-schema``  : Pydantic's JSON Schema for the whole model.
  * ``--markdown``     : the COMPLETE settings reference (every key, its type,
                         default, doc blurb, and wizard disposition) used to
                         regenerate ``docs/configuration-guide.md``.
  * ``--wizard-plan``  : a JSON view filtered to the wizard-asked fields, for
                         ``/fno:setup`` to consume instead of hardcoding its
                         question list.

Determinism: leaves are emitted in ``model_fields`` declaration order (stable),
so regenerating twice produces byte-identical output (AC1-HP / AC5-FR).
"""
from __future__ import annotations

import json
import typing
from dataclasses import dataclass
from typing import Any, Iterator, Optional, TypeGuard

from pydantic import BaseModel

from fno.config import SettingsModel
from fno.config import registry as _registry


@dataclass(frozen=True)
class Leaf:
    """One terminal config key (a scalar, list, or map field)."""

    path: str
    type_str: str
    default: Any


def _is_model(t: object) -> TypeGuard[type[BaseModel]]:
    return isinstance(t, type) and issubclass(t, BaseModel)


def _model_arg(annotation: object) -> Optional[type[BaseModel]]:
    """Return the BaseModel class iff ``annotation`` is a *direct* model or
    Optional[model]. A ``list[Model]`` / ``dict[str, Model]`` is NOT descended:
    its container is a leaf (the value structure is dynamic).
    """
    if _is_model(annotation):
        return annotation
    origin = typing.get_origin(annotation)
    # Only unwrap Optional / Union, never list/dict/etc.
    if origin is typing.Union:
        args = [a for a in typing.get_args(annotation) if a is not type(None)]
        if len(args) == 1:
            arg = args[0]
            if _is_model(arg):
                return arg
    return None


def _type_str(annotation: object) -> str:
    """Render a field annotation as a short, human-readable type string."""
    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    if origin is typing.Union:
        non_none = [a for a in args if a is not type(None)]
        rendered = " | ".join(_type_str(a) for a in non_none)
        return f"{rendered} (optional)" if type(None) in args else rendered
    if origin in (list, typing.List):
        inner = _type_str(args[0]) if args else "Any"
        return f"list[{inner}]"
    if origin in (dict, typing.Dict):
        if len(args) == 2:
            return f"dict[{_type_str(args[0])}, {_type_str(args[1])}]"
        return "dict"
    if isinstance(annotation, type):
        return annotation.__name__
    return str(annotation)


def iter_leaves(
    model: type[BaseModel] = SettingsModel,
    *,
    _prefix: str = "",
    _instance: Optional[BaseModel] = None,
) -> Iterator[Leaf]:
    """Yield every terminal leaf of ``model`` in declaration order.

    Descends only into nested BaseModel fields (config blocks); collections of
    models (``list[Model]`` / ``dict[str, Model]``) are emitted as a single leaf
    for their container. Defaults are read off a default-constructed instance so
    the reported default is exactly what the model would produce.
    """
    if _instance is None:
        _instance = model()  # all blocks have defaults / default_factory
    for name, field in model.model_fields.items():
        path = f"{_prefix}.{name}" if _prefix else name
        inner = _model_arg(field.annotation)
        value = getattr(_instance, name, None)
        if inner is not None and isinstance(value, BaseModel):
            yield from iter_leaves(inner, _prefix=path, _instance=value)
        else:
            yield Leaf(path=path, type_str=_type_str(field.annotation), default=value)


def all_leaf_paths(model: type[BaseModel] = SettingsModel) -> list[str]:
    """All leaf dotted paths, in declaration order (used by the CI checks)."""
    return [leaf.path for leaf in iter_leaves(model)]


# ---------------------------------------------------------------------------
# Generators
# ---------------------------------------------------------------------------


def json_schema() -> str:
    """Pydantic JSON Schema for the whole SettingsModel (deterministic)."""
    return json.dumps(SettingsModel.model_json_schema(), indent=2, sort_keys=True)


def _fmt_default(value: Any) -> str:
    if value is None:
        return "_(none)_"
    if isinstance(value, bool):
        return f"`{str(value).lower()}`"
    if isinstance(value, (list, dict)):
        return f"`{json.dumps(value)}`"
    return f"`{value}`"


def render_markdown() -> str:
    """Render the COMPLETE settings reference as Markdown.

    Every modeled key appears with its dotted path, type, default, wizard
    disposition (always/advanced/never), and doc blurb from the registry.
    """
    lines: list[str] = []
    lines.append("# Configuration reference")
    lines.append("")
    lines.append(
        "> Generated by `fno config schema --markdown` from the Pydantic "
        "`SettingsModel` (the single source of truth). Do not edit by hand; "
        "edit the model + `cli/src/fno/config/registry.py` and regenerate."
    )
    lines.append("")
    lines.append(
        "Keys live in a flat `config.toml` (`.fno/config.toml` project-local, "
        "`~/.fno/config.toml` global). A dotted key like `branch.prefix` is "
        "`prefix` under `[branch]`. For a copy-paste template with every key at "
        "its default, see [settings.example.toml](settings.example.toml); read "
        "or set individual keys with `fno config get <key>` / `fno config set "
        "<key> <value>`."
    )
    lines.append("")
    lines.append("| Key | Type | Default | Wizard | Description |")
    lines.append("|-----|------|---------|--------|-------------|")
    for leaf in iter_leaves():
        meta = _registry.meta_for(leaf.path)
        wizard = meta.wizard if meta else "never"
        doc = meta.doc if meta else ""
        lines.append(
            f"| `{leaf.path}` | {leaf.type_str} | {_fmt_default(leaf.default)} "
            f"| {wizard} | {doc} |"
        )
    lines.append("")
    return "\n".join(lines)


def _toml_value(value: Any) -> str:
    """Render a leaf default as a valid TOML value.

    Callers handle ``None`` (TOML has no null; optional keys are emitted
    commented-out). Strings use ``json.dumps`` (a valid TOML basic string for
    these config defaults); lists/dicts use JSON flow, which is valid TOML for
    scalar-element arrays and the empty inline table ``{}``.
    """
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value)
    return json.dumps(value)


class _TomlNode:
    """One table in the emit tree: direct leaves first, then child tables.

    TOML requires every key/value of a table to precede its sub-table headers,
    but the model yields top-level scalars like ``loops``/``schema_version``
    *after* nested blocks. Splitting leaves from tables here restores a valid
    emission order regardless of declaration order.
    """

    def __init__(self) -> None:
        self.leaves: list[Leaf] = []
        self.tables: dict[str, "_TomlNode"] = {}


def _build_tree() -> _TomlNode:
    root = _TomlNode()
    for leaf in iter_leaves():
        segs = leaf.path.split(".")
        node = root
        for seg in segs[:-1]:
            node = node.tables.setdefault(seg, _TomlNode())
        node.leaves.append(leaf)
    return root


def _emit_table(node: _TomlNode, prefix: list[str], lines: list[str]) -> None:
    # Header for this table iff it holds direct keys (a pure container is left
    # implicit; TOML creates it from the dotted sub-table header).
    if prefix and node.leaves:
        lines.append("")
        lines.append(f"[{'.'.join(prefix)}]")
    for leaf in node.leaves:
        meta = _registry.meta_for(leaf.path)
        if meta and meta.doc:
            lines.append(f"# {' '.join(meta.doc.split())}")
        key = leaf.path.split(".")[-1]
        if leaf.default is None:
            lines.append(f"# {key} = <unset>   # optional; no default")
        else:
            lines.append(f"{key} = {_toml_value(leaf.default)}")
    for seg, child in node.tables.items():
        _emit_table(child, prefix + [seg], lines)


def render_example_toml() -> str:
    """Render a complete, valid example config: every modeled key set to its
    default, each preceded by its one-line doc blurb as a comment.

    The output is a real (parseable) ``config.toml`` representing the built-in
    defaults, usable as a copy-paste-and-edit reference. Flat, top-level blocks
    with no wrapper (mirrors the on-disk file). Optional keys with no default
    are shown commented out (TOML has no null). Regenerating twice is
    byte-identical (same determinism guarantee as render_markdown).
    """
    lines: list[str] = [
        "# footnote settings reference (settings.example.toml)",
        "#",
        "# Generated by `fno config schema --toml` from the Pydantic SettingsModel",
        "# (the single source of truth). Do not edit by hand; edit the model +",
        "# cli/src/fno/config/registry.py and regenerate.",
        "#",
        "# Every key is shown set to its DEFAULT. Copy the keys you want to change",
        "# into .fno/config.toml (project-local) or ~/.fno/config.toml (global);",
        "# anything you omit falls back to these defaults. config.toml is flat:",
        "# top-level scalars and [blocks], no wrapper. Optional keys with no",
        "# default are shown commented out (TOML has no null).",
        "#",
        "# For opinionated starters (recommended values, safe opt-ins on), see",
        "# docs/settings.global.example.toml and docs/settings.local.example.toml.",
        "",
    ]
    _emit_table(_build_tree(), [], lines)
    lines.append("")
    return "\n".join(lines)


def wizard_plan() -> str:
    """JSON the /fno:setup skill consumes: the always/advanced fields only."""
    asked: list[dict[str, Any]] = []
    for leaf in iter_leaves():
        meta = _registry.meta_for(leaf.path)
        if meta is None or meta.wizard == "never":
            continue
        asked.append(
            {
                "path": leaf.path,
                "type": leaf.type_str,
                "default": leaf.default,
                "tier": meta.wizard,
                "question": meta.question,
                "default_source": meta.default_source,
                "doc": meta.doc,
            }
        )
    return json.dumps({"fields": asked}, indent=2)
