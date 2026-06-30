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


def _yaml_scalar(value: Any) -> str:
    """Render a leaf default as a valid YAML scalar/flow value.

    Lists and dicts use JSON (which is valid YAML flow syntax); strings that
    could be misread by a YAML parser (contain an indicator char, lead/trail
    whitespace, are empty, or start with a reserved indicator like ``~``) are
    double-quoted via json.dumps.
    """
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        risky = value == "" or value.strip() != value
        risky = risky or value[:1] in "~?-:!&*#|>@%\"'`[]{},"
        risky = risky or any(c in value for c in ":#")
        # A bare string YAML would read as a bool/null/number must be quoted so
        # the round-trip preserves it as a string (e.g. "true", "null", "123").
        risky = risky or value.lower() in _YAML_RESERVED_WORDS
        risky = risky or _looks_numeric(value)
        return json.dumps(value) if risky else value
    # list / dict / anything else: JSON is a valid YAML flow representation.
    return json.dumps(value)


# Bare words YAML 1.1 readers coerce to bool/null; a string default equal to one
# (case-insensitive) must be quoted to survive the round-trip.
_YAML_RESERVED_WORDS = frozenset(
    {"true", "false", "null", "yes", "no", "on", "off", "~", "none"}
)


def _looks_numeric(s: str) -> bool:
    """True when YAML would parse ``s`` as a number (int/float/inf/nan)."""
    try:
        float(s)
        return True
    except ValueError:
        return False


def render_example_yaml() -> str:
    """Render a complete, valid example settings file: every modeled key set to
    its default, each preceded by its one-line doc blurb as a comment.

    The output is a real (parseable) settings.yaml representing the built-in
    defaults, usable as a copy-paste-and-edit reference. Leaves are emitted in
    declaration order so nested blocks stay contiguous; regenerating twice is
    byte-identical (same determinism guarantee as render_markdown).
    """
    lines: list[str] = [
        "# footnote settings reference (settings.example.yaml)",
        "#",
        "# Generated by `fno config schema --yaml` from the Pydantic SettingsModel",
        "# (the single source of truth). Do not edit by hand; edit the model +",
        "# cli/src/fno/config/registry.py and regenerate.",
        "#",
        "# Every key is shown set to its DEFAULT. Copy the keys you want to change",
        "# into .fno/settings.yaml (project-local) or ~/.fno/settings.yaml (global);",
        "# anything you omit falls back to these defaults. schema_version is the only",
        "# top-level key besides `config:` - everything else lives under config:.",
        "#",
        "# For opinionated starters (recommended values, safe opt-ins on), see",
        "# docs/settings.global.example.yaml and docs/settings.local.example.yaml.",
        "",
    ]
    open_segs: list[str] = []  # container segments currently emitted, by depth
    for leaf in iter_leaves():
        segs = leaf.path.split(".")
        container, key = segs[:-1], segs[-1]
        # Reuse the longest shared prefix; reopen the rest.
        common = 0
        while (
            common < len(open_segs)
            and common < len(container)
            and open_segs[common] == container[common]
        ):
            common += 1
        open_segs = open_segs[:common]
        for depth in range(common, len(container)):
            lines.append(f"{'  ' * depth}{container[depth]}:")
            open_segs.append(container[depth])
        indent = "  " * len(container)
        meta = _registry.meta_for(leaf.path)
        if meta and meta.doc:
            # Collapse any newlines so a multi-line blurb stays a single comment
            # line (a raw 2nd line would be parsed as YAML, not a comment).
            doc_line = " ".join(meta.doc.split())
            lines.append(f"{indent}# {doc_line}")
        lines.append(f"{indent}{key}: {_yaml_scalar(leaf.default)}")
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
