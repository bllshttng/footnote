"""fno.config.writer — the `fno config set` write path (ab-098967b4, US7).

A small write verb alongside the read-only `get` / `doctor`, so toggles like
``config.agents.a2a.auto`` are settable without hand-editing the config file. The
value is coerced to the schema field's type and validated by constructing the
changed *block* in isolation (so a field validator like
``A2aBlock.ceiling_is_positive`` fires, while unrelated top-level keys such as
``work:`` are never re-validated). The write is atomic (temp file +
``os.replace``) under a file lock, so a concurrent set / first-use-confirm write
serializes and a mid-write failure leaves the original config file intact
(AC7-EDGE / AC7-FR).

Emits flat ``config.toml`` via ``tomli_w`` (stage 3 hard cut). TOML has no null,
so None-valued keys are dropped on write (the loader reads an absent key as its
default); comments are not preserved (machine-managed file). An unmigrated
``settings.yaml`` at the target location is converted to ``config.toml`` before
the write (the shared one-shot migrate), so a legacy install is never lost.
"""
from __future__ import annotations

import copy
import os
import tempfile
import tomllib
import typing
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional, cast, get_args, get_origin

import tomli_w
import yaml
from pydantic import BaseModel, ValidationError

from fno.config import (
    SettingsModel,
    _global_settings_path,
    _migrate_yaml_to_toml,
    _strip_none,
)


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


@dataclass
class UnsetResult:
    key: str
    was: Any  # the previous value (None if the key was absent)
    present: bool  # whether the key was present before the unset
    default: Any  # the model default the key reverts to once removed
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

    A leading ``config`` segment is dropped: the model is now flat (config
    fields live at the top level), but callers and settings.yaml still use the
    ``config.`` prefix. Stripping it here resolves ``config.agents.a2a.auto``
    against the flat model while file I/O keeps the original prefixed parts.
    """
    if parts and parts[0] == "config":
        parts = parts[1:]
    if not parts:
        # A bare `config` key (parts == ["config"]) strips to empty; there is no
        # leaf to resolve. Return None so the caller raises a clean unknown-key
        # error instead of an IndexError on parts[-1].
        return None
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


def _storage_parts(parts: list[str]) -> list[str]:
    """The dotted path a key is STORED at in config.toml: flat (top-level
    blocks, no ``config:`` wrapper). A ``config.``-prefixed key and its bare flat
    form both map to the same flat location, so the file stays single-shape."""
    return parts[1:] if parts and parts[0] == "config" else list(parts)


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
    """The config.toml this scope writes to, migrating a legacy settings.yaml
    sibling to config.toml first so an unmigrated install is converted (not
    lost) before the write lands."""
    if scope == "project":
        if repo_root is None:
            from fno.paths import resolve_repo_root

            repo_root = resolve_repo_root()
        fno_dir = repo_root / ".fno"
    else:
        fno_dir = _global_settings_path().parent
    _migrate_yaml_to_toml(fno_dir / "settings.yaml")
    return fno_dir / "config.toml"


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
                    loaded = tomllib.loads(target.read_text(encoding="utf-8"))
                except tomllib.TOMLDecodeError as exc:
                    raise ConfigSetError(
                        f"existing config at {target} is malformed: {exc}", 1
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
                with os.fdopen(fd, "wb") as f:
                    clean = cast("dict[str, Any]", _strip_none(data))
                    f.write(tomli_w.dumps(clean).encode("utf-8"))
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


def _parse_structured(value: str, key: str) -> Any:
    """Parse a block/object value as JSON (then trivial YAML), for block-set.

    JSON is tried first; a flow-style YAML mapping (``{a: b}``) is accepted as a
    fallback (Claude's Discretion #2). A value that parses as neither raises
    ConfigSetError exit 2 (AC3-ERR), leaving the file untouched.
    """
    import json as _json

    s = value.strip()
    try:
        return _json.loads(s)
    except _json.JSONDecodeError:
        pass
    try:
        return yaml.safe_load(s)
    except yaml.YAMLError as exc:
        raise ConfigSetError(
            f"invalid JSON/YAML for {key}: {exc}", 2
        ) from exc


def _resolve_final_value(
    parts: list[str], key: str
) -> tuple[Any, Optional[tuple[tuple[str, ...], type[BaseModel]]]]:
    """Coerce/parse one key's annotation into its final stored shape, deferring
    parent-block validation. Returns ``(coercer, block_to_validate)`` where
    ``coercer`` turns the raw string into the value and ``block_to_validate`` is
    ``(block_parts, parent_cls)`` for a scalar/dict leaf (validated once at the
    end of the batch against the FINAL merged state) or ``None`` for a
    block-leaf (validated in place, since it is self-contained).

    Splitting coerce from validate is what makes a multi-key batch correct for
    cross-field block invariants (e.g. config.obsidian.enabled + .vault): every
    key is applied first, then each touched block is validated once on the final
    state rather than on an intermediate state after each individual key.
    """
    parent_cls, leaf, leaf_ann = _resolve_parent_block(parts)  # type: ignore[misc]
    block_model = _as_model(leaf_ann)
    base_ann = _unwrap_optional(leaf_ann)
    is_dict_leaf = get_origin(base_ann) is dict
    block_parts = tuple(parts[:-1])

    if block_model is not None or is_dict_leaf:
        # block-leaf / dict-leaf: parse a JSON/YAML mapping (US3).
        def _coercer(value: str) -> Any:
            parsed = _parse_structured(value, key)
            if not isinstance(parsed, dict):
                raise ConfigSetError(
                    f"{key!r} is a config block; expected a JSON/YAML object "
                    f"(mapping), got {type(parsed).__name__}",
                    2,
                )
            if block_model is not None:
                # REPLACE: validate the whole block in isolation (self-contained).
                try:
                    block_model.model_validate(parsed)
                except ValidationError as exc:
                    first = exc.errors()[0] if exc.errors() else {"msg": str(exc)}
                    raise ConfigSetError(
                        f"invalid value for {key}: {first.get('msg', exc)}", 2
                    ) from exc
            return parsed

        # A block-leaf is validated in its coercer; a dict-leaf defers to the
        # parent block (so its parent's other fields are seen on final state).
        block = None if block_model is not None else (block_parts, parent_cls)
        return _coercer, block

    return (lambda value: _coerce(value, leaf_ann)), (block_parts, parent_cls)


def _validate_block(
    data: dict[str, Any], block_parts: tuple[str, ...], parent_cls: type[BaseModel]
) -> None:
    """Validate one touched block against the final merged ``data``. Raises
    ConfigSetError exit 2 naming the offending field on failure."""
    block_dict = dict(_get_nested(data, list(block_parts)) or {})
    try:
        parent_cls.model_validate(block_dict)
    except ValidationError as exc:
        first = exc.errors()[0] if exc.errors() else {"msg": str(exc)}
        loc = ".".join(str(p) for p in first.get("loc", ()))
        where = ".".join(block_parts) + (f".{loc}" if loc else "")
        raise ConfigSetError(
            f"invalid value for {where}: {first.get('msg', exc)}", 2
        ) from exc


def set_config_values(
    items: list[tuple[str, str]],
    *,
    scope: str = "global",
    repo_root: Optional[Path] = None,
) -> list[SetResult]:
    """Set one or more dotted keys in one atomic, lock-serialized pass (US2).

    All-or-nothing: every value is coerced + validated under a single lock and
    the file is written only if ALL pass (a validation failure raises before any
    temp file is created, so the original is untouched). A key appearing twice
    uses the last value (AC2-EDGE). An empty batch is a usage error.

    The single-key ``set_config_value`` delegates here, so both share one
    read-modify-write path. Returns one ``SetResult`` per distinct key, in
    first-seen order.
    """
    if not items:
        raise ConfigSetError("no key=value pairs given", 2)

    # Dedup by key, last value wins (AC2-EDGE); preserve first-seen order.
    order: list[str] = []
    deduped: dict[str, str] = {}
    for key, value in items:
        if key not in deduped:
            order.append(key)
        deduped[key] = value

    # Resolve every key up front (unknown -> exit 1) before taking the lock.
    parts_by_key: dict[str, list[str]] = {}
    for key in order:
        parts = key.split(".")
        if _resolve_parent_block(parts) is None:
            raise ConfigSetError(f"unknown config key {key!r}", 1)
        parts_by_key[key] = _storage_parts(parts)

    target = _target_path(scope, repo_root)
    final_values: dict[str, Any] = {}

    def _validate_and_merge(existing: dict[str, Any]) -> dict[str, Any]:
        # Phase 1: coerce/parse + apply every key, collecting the distinct
        # parent blocks to validate. Phase 2: validate each touched block once
        # against the FINAL merged state, so a batch setting cross-field-coupled
        # keys (config.obsidian.enabled + .vault) is judged on the end result,
        # not on an intermediate state after a single key.
        data = existing
        blocks: dict[tuple[str, ...], type[BaseModel]] = {}
        for key in order:
            coercer, block = _resolve_final_value(parts_by_key[key], key)
            final = coercer(deduped[key])
            data = _deep_set(data, parts_by_key[key], final)
            final_values[key] = final
            if block is not None:
                blocks[block[0]] = block[1]
        for block_parts, parent_cls in blocks.items():
            _validate_block(data, block_parts, parent_cls)
        return data

    try:
        written = _locked_update(target, _validate_and_merge)
    except OSError as exc:
        # AC2-FR: the temp+rename already left the original intact; surface a
        # clean non-zero exit.
        raise ConfigSetError(
            f"failed to write {target}: {exc} (settings left unchanged)", 1
        ) from exc
    return [
        SetResult(key=key, value=final_values[key], path=written, scope=scope)
        for key in order
    ]


def set_config_value(
    key: str,
    value: str,
    *,
    scope: str = "global",
    repo_root: Optional[Path] = None,
) -> SetResult:
    """Set a single dotted config key (the single-key facade over
    ``set_config_values``). The key may be a scalar/list leaf (coerced) or a
    block/dict leaf (set from a JSON/YAML object, REPLACE semantics; US3).
    Raises ``ConfigSetError`` (with an exit code) on an unknown key, a
    type-mismatched / schema-invalid value, or a malformed existing file.
    """
    return set_config_values(
        [(key, value)], scope=scope, repo_root=repo_root
    )[0]


# ---------------------------------------------------------------------------
# unset
# ---------------------------------------------------------------------------


def _deep_unset(
    d: dict[str, Any], parts: list[str]
) -> tuple[dict[str, Any], Any, bool]:
    """Return ``(new_dict, was_value, present)`` removing ``parts`` from a copy.

    Prunes any parent block left empty by the removal so the file never
    accumulates dangling ``{}`` stanzas (AC1-EDGE). If the key (or any parent
    segment) is absent, returns the unchanged copy with ``present=False``.
    """
    out = copy.deepcopy(d)
    chain: list[tuple[dict[str, Any], str]] = []
    node: Any = out
    for part in parts[:-1]:
        if not isinstance(node, dict) or not isinstance(node.get(part), dict):
            return out, None, False
        chain.append((node, part))
        node = node[part]
    leaf = parts[-1]
    if not isinstance(node, dict) or leaf not in node:
        return out, None, False
    was = node.pop(leaf)
    # Walk back up, pruning each parent that the removal left empty.
    for parent, key in reversed(chain):
        if isinstance(parent.get(key), dict) and not parent[key]:
            del parent[key]
        else:
            break
    return out, was, True


def _model_default(parts: list[str]) -> Any:
    """The value ``parts`` reverts to once unset: read off a default-constructed
    ``SettingsModel`` by walking the dotted path. Returns None if not resolvable.
    """
    if parts and parts[0] == "config":
        # Flat model: a legacy `config.` prefix resolves against the top level.
        parts = parts[1:]
    node: Any = SettingsModel()
    for part in parts:
        if isinstance(node, BaseModel) and part in type(node).model_fields:
            node = getattr(node, part)
        elif isinstance(node, dict) and part in node:
            node = node[part]
        else:
            return None
    return node


def unset_config_value(
    key: str,
    *,
    scope: str = "global",
    repo_root: Optional[Path] = None,
) -> UnsetResult:
    """Remove a dotted config key, reverting it to the model default.

    Non-destructive (the value falls back to its schema default), so no
    confirmation. An unknown key exits 1 (same as ``set``); an absent key is a
    clean no-op (``present=False``, nothing written). Atomic + lock-serialized
    via the shared ``_locked_update``; a write failure leaves the file intact.
    """
    parts = key.split(".")
    if _resolve_parent_block(parts) is None:
        raise ConfigSetError(f"unknown config key {key!r}", 1)

    default = _model_default(parts)
    store_parts = _storage_parts(parts)
    target = _target_path(scope, repo_root)
    real_target = Path(os.path.realpath(target)) if target.is_symlink() else target

    # No file -> nothing to remove; do not create an empty settings file.
    if not real_target.exists():
        return UnsetResult(
            key=key, was=None, present=False, default=default,
            path=real_target, scope=scope,
        )

    captured: dict[str, Any] = {"was": None, "present": False}

    def _mutate(existing: dict[str, Any]) -> dict[str, Any]:
        new, was, present = _deep_unset(existing, store_parts)
        captured["was"] = was
        captured["present"] = present
        return new

    try:
        written = _locked_update(target, _mutate)
    except OSError as exc:
        raise ConfigSetError(
            f"failed to write {target}: {exc} (settings left unchanged)", 1
        ) from exc

    return UnsetResult(
        key=key,
        was=captured["was"],
        present=captured["present"],
        default=default,
        path=written,
        scope=scope,
    )
