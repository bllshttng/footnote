"""fno.config.writer — the `fno config set` write path (ab-098967b4, US7).

A small write verb alongside the read-only `get` / `doctor`, so toggles like
``config.agents.a2a.auto`` are settable without hand-editing YAML. The value is
coerced to the schema field's type and validated by constructing the changed
*block* in isolation (so a field validator like ``A2aBlock.ceiling_is_positive``
fires, while unrelated top-level keys such as ``work:`` are never re-validated).
The write is atomic (temp file + ``os.replace``) under a file lock, so a
concurrent set / first-use-confirm write serializes and a mid-write failure
leaves the original settings file intact (AC7-EDGE / AC7-FR).

Limitation: PyYAML ``safe_dump`` does not preserve comments, so a `set` rewrites
the target file without its comments. Acceptable for the machine-managed
settings file; ruamel.yaml is not a dependency.
"""
from __future__ import annotations

import copy
import os
import tempfile
import typing
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional, get_args, get_origin

import yaml
from pydantic import BaseModel, ValidationError

from fno.config import SettingsModel, _global_settings_path


class ConfigSetError(Exception):
    """A user-facing config-set failure carrying a CLI exit code."""

    def __init__(self, message: str, exit_code: int = 1):
        super().__init__(message)
        self.exit_code = exit_code


@dataclass
class SetResult:
    key: str
    value: Any
    path: Path
    scope: str  # "global" | "project"


def _unwrap_optional(ann: Any) -> Any:
    """``Optional[X]`` / ``Union[X, None]`` / ``X | None`` -> ``X`` (best-effort).

    Handles BOTH ``typing.Union`` and the PEP 604 ``types.UnionType`` (``X |
    None``), so a schema field declared with either syntax coerces correctly.
    """
    import types as _types

    origin = get_origin(ann)
    if origin is typing.Union or origin is getattr(_types, "UnionType", None):
        non_none = [a for a in get_args(ann) if a is not type(None)]
        if len(non_none) == 1:
            return non_none[0]
    return ann


def _as_model(ann: Any) -> Optional[type[BaseModel]]:
    base = _unwrap_optional(ann)
    if isinstance(base, type) and issubclass(base, BaseModel):
        return base
    return None


def _resolve_parent_block(
    parts: list[str],
) -> Optional[tuple[type[BaseModel], str, Any]]:
    """Return ``(parent_block_cls, leaf_field, leaf_annotation)`` for a dotted
    key, or None if the path is unknown or a non-leaf segment is not a model.
    """
    cls: type[BaseModel] = SettingsModel
    for part in parts[:-1]:
        fields = getattr(cls, "model_fields", {})
        if part not in fields:
            return None
        model = _as_model(fields[part].annotation)
        if model is None:
            return None
        cls = model
    leaf = parts[-1]
    fields = getattr(cls, "model_fields", {})
    if leaf not in fields:
        return None
    return cls, leaf, fields[leaf].annotation


def _coerce(value: str, ann: Any) -> Any:
    """Coerce a string CLI value to the schema field's type."""
    base = _unwrap_optional(ann)
    is_optional = base is not ann
    if is_optional and value.strip().lower() in ("null", "none", ""):
        return None
    if base is bool:
        v = value.strip().lower()
        if v in ("true", "1", "yes", "on"):
            return True
        if v in ("false", "0", "no", "off"):
            return False
        raise ConfigSetError(f"expected a boolean (true/false); got {value!r}", 2)
    if base is int:
        try:
            return int(value)
        except ValueError as exc:
            raise ConfigSetError(f"expected an integer; got {value!r}", 2) from exc
    if base is float:
        try:
            return float(value)
        except ValueError as exc:
            raise ConfigSetError(f"expected a number; got {value!r}", 2) from exc
    if base is list or get_origin(base) is list:
        # Accept a JSON array (`["a","b"]`) or a comma-separated string
        # (`a,b`). Items are rendered as strings (every modeled list is a
        # list[str]). Empty value -> empty list. Without this branch the raw
        # string was stored verbatim, so the wizard could not set
        # config.review.external_reviewers.
        s = value.strip()
        if not s:
            return []
        if s.startswith("["):
            import json as _json

            try:
                parsed = _json.loads(s)
            except _json.JSONDecodeError as exc:
                raise ConfigSetError(
                    f"expected a list (JSON array or comma-separated); got {value!r}",
                    2,
                ) from exc
            if not isinstance(parsed, list):
                raise ConfigSetError(f"expected a list; got {value!r}", 2)
            return [str(x) for x in parsed]
        return [item.strip() for item in s.split(",") if item.strip()]
    return value


def _target_path(scope: str, repo_root: Optional[Path]) -> Path:
    if scope == "project":
        if repo_root is None:
            from fno.paths import resolve_repo_root

            repo_root = resolve_repo_root()
        return repo_root / ".fno" / "settings.yaml"
    return _global_settings_path()


def _get_nested(d: dict[str, Any], parts: list[str]) -> Optional[dict[str, Any]]:
    node: Any = d
    for part in parts:
        if not isinstance(node, dict) or part not in node:
            return None
        node = node[part]
    return node if isinstance(node, dict) else None


def _deep_set(d: dict[str, Any], parts: list[str], value: Any) -> dict[str, Any]:
    out = copy.deepcopy(d)
    node = out
    for part in parts[:-1]:
        nxt = node.get(part)
        if not isinstance(nxt, dict):
            nxt = {}
            node[part] = nxt
        node = nxt
    node[parts[-1]] = value
    return out


def _locked_update(
    target: Path,
    mutate: Callable[[dict[str, Any]], dict[str, Any]],
) -> Path:
    """Read + mutate + atomic-write the settings file, all under one exclusive
    file lock (AC7-EDGE / AC7-FR). Returns the real path written.

    The lock covers the WHOLE read-modify-write cycle, not just the final
    ``os.replace``. ``mutate`` receives the freshly-read existing settings (read
    under the lock) and returns the dict to write; it raises ``ConfigSetError``
    on a validation failure, before any temp file is created. Holding the lock
    across the read closes a TOCTOU race: two concurrent ``set`` /
    first-use-confirm writers would otherwise both parse the same old YAML and
    the later ``os.replace`` would clobber the earlier writer's key (codex P2,
    PR #522). The original file is untouched until ``os.replace``; on any
    failure before that the temp file is unlinked, so the file is never left
    partial and needs no manual recovery.

    If ``target`` is a symlink, read and write THROUGH it to its real target. A
    linked worktree's ``.fno/settings.yaml`` is a symlink to the canonical
    checkout's real file (created by ``setup-worktree.sh``), and ``os.replace``
    (rename) onto a symlink replaces the link itself, not its referent -- so a
    naive atomic write from a worktree would break the link and leave a
    divergent regular file instead of updating the shared canonical config.
    Resolving the symlink first also routes the lock to the canonical path, so a
    worktree write and a canonical write serialize on the same lock.
    """
    import fcntl

    if target.is_symlink():
        target = Path(os.path.realpath(target))
    target.parent.mkdir(parents=True, exist_ok=True)
    lock_path = target.with_suffix(target.suffix + ".lock")
    with open(lock_path, "w") as lock_fh:
        fcntl.flock(lock_fh.fileno(), fcntl.LOCK_EX)
        try:
            existing: dict[str, Any] = {}
            if target.exists():
                try:
                    loaded = yaml.safe_load(target.read_text(encoding="utf-8"))
                except yaml.YAMLError as exc:
                    raise ConfigSetError(
                        f"existing settings at {target} is malformed: {exc}", 1
                    ) from exc
                if isinstance(loaded, dict):
                    existing = loaded

            data = mutate(existing)

            fd, tmp_str = tempfile.mkstemp(
                dir=str(target.parent),
                prefix=f".{target.name}.tmp.",
                suffix=".part",
            )
            tmp = Path(tmp_str)
            try:
                with os.fdopen(fd, "w", encoding="utf-8") as f:
                    yaml.safe_dump(data, f, sort_keys=False, default_flow_style=False)
                os.replace(str(tmp), str(target))
            except Exception:
                try:
                    tmp.unlink()
                except OSError:
                    pass
                raise
        finally:
            fcntl.flock(lock_fh.fileno(), fcntl.LOCK_UN)
    return target


def set_config_value(
    key: str,
    value: str,
    *,
    scope: str = "global",
    repo_root: Optional[Path] = None,
) -> SetResult:
    """Set a dotted config key in the target settings file. Raises
    ``ConfigSetError`` (with an exit code) on an unknown key, a non-scalar key,
    a type-mismatched / schema-invalid value, or a malformed existing file.
    """
    parts = key.split(".")
    resolved = _resolve_parent_block(parts)
    if resolved is None:
        raise ConfigSetError(f"unknown config key {key!r}", 1)
    parent_cls, leaf, leaf_ann = resolved
    if _as_model(leaf_ann) is not None:
        raise ConfigSetError(
            f"{key!r} is a config block, not a scalar; set a leaf key under it", 1
        )

    coerced = _coerce(value, leaf_ann)

    target = _target_path(scope, repo_root)

    def _validate_and_merge(existing: dict[str, Any]) -> dict[str, Any]:
        # Runs UNDER the lock on the freshly-read content (see _locked_update).
        # Validate only the changed block (extra='ignore' on the blocks keeps
        # unrelated keys out; field validators like ceiling_is_positive fire).
        block_dict = dict(_get_nested(existing, parts[:-1]) or {})
        block_dict[leaf] = coerced
        try:
            parent_cls.model_validate(block_dict)
        except ValidationError as exc:
            first = exc.errors()[0] if exc.errors() else {"msg": str(exc)}
            raise ConfigSetError(
                f"invalid value for {key}: {first.get('msg', exc)}", 2
            ) from exc
        return _deep_set(existing, parts, coerced)

    try:
        written = _locked_update(target, _validate_and_merge)
    except OSError as exc:
        # AC7-FR: the temp+rename in _locked_update already left the original
        # file intact and unlinked the temp; surface a clean non-zero exit.
        raise ConfigSetError(
            f"failed to write {target}: {exc} (settings left unchanged)", 1
        ) from exc
    return SetResult(key=key, value=coerced, path=written, scope=scope)
