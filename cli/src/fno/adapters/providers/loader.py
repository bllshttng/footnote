"""Settings loader for provider rotation substrate.

Phase 01 of the provider rotation substrate (ab-256f6b6e).
Reads config.providers from .fno/settings.yaml with project-local-over-global
precedence, mirroring cli/src/fno/cli.py::_load_v2_config_flag.
"""
from __future__ import annotations

import dataclasses
import fcntl
import os
import tempfile
from pathlib import Path
from typing import Any, Callable, Literal

import pydantic
import yaml

from fno import paths as _paths
from fno.adapters.providers.model import (
    AgentProviderBinding,
    ProviderConfigError,
    ProviderRecord,
    ProvidersConfig,
)
from fno.state.io import atomic_write


def _global_settings_path() -> Path:
    """Resolve the per-user global settings.yaml path.

    Returns ``Path(FNO_GLOBAL_SETTINGS_PATH)`` when that environment variable
    is set to a non-empty value (use ``/dev/null`` to disable the global
    candidate in test isolation), otherwise the default
    ``~/.fno/settings.yaml``. Mirrors
    ``fno.config._global_settings_path`` so both loaders honor the same
    override; we cannot import from ``fno.config`` here because the
    provider loader runs during the config import path (bootstrap order).

    Empty-string env var (e.g. ``FNO_GLOBAL_SETTINGS_PATH=``) is treated as
    "unset" rather than ``Path("")`` (which resolves to the CWD and would
    silently bypass the global config). An operator that genuinely wants to
    point at the CWD must say so explicitly: ``FNO_GLOBAL_SETTINGS_PATH=.``.
    """
    env = os.environ.get("FNO_GLOBAL_SETTINGS_PATH")
    if env:
        return Path(env)
    return Path.home() / ".fno" / "settings.yaml"


def _read_yaml_safe(path: Path) -> dict[str, Any]:
    """Read a YAML file, returning an empty dict on any error."""
    try:
        if not path.is_file():
            return {}
        data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        return data if isinstance(data, dict) else {}
    except (OSError, yaml.YAMLError):
        return {}


def _read_yaml_strict(path: Path) -> dict[str, Any]:
    """Read a YAML file for write-back operations.

    Unlike _read_yaml_safe, this function distinguishes between a missing
    file (returns {}) and a file that exists but fails to parse (raises
    ProviderConfigError). This prevents save_providers from silently
    overwriting all existing keys when settings.yaml is corrupt.
    """
    if not path.is_file():
        return {}
    try:
        data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        return data if isinstance(data, dict) else {}
    except yaml.YAMLError as exc:
        raise ProviderConfigError(
            f"Cannot save: settings.yaml failed to parse ({path}): {exc}"
        ) from exc
    except OSError as exc:
        raise ProviderConfigError(
            f"Cannot save: settings.yaml is not readable ({path}): {exc}"
        ) from exc


def _extract_providers_block(data: dict[str, Any]) -> dict[str, Any] | None:
    """Return the config.providers dict, or None if absent/invalid."""
    config = data.get("config")
    if not isinstance(config, dict):
        return None
    providers = config.get("providers")
    if not isinstance(providers, dict):
        return None
    return providers


def _extract_agents_block(data: dict[str, Any]) -> dict[str, Any] | None:
    """Return the config.agents dict, or None if absent/invalid.

    config.agents is a YAML sibling of config.providers (both live under
    the top-level ``config`` key). Returns None when the key is absent or
    not a mapping; callers treat None as an empty agents map.
    """
    config = data.get("config")
    if not isinstance(config, dict):
        return None
    agents = config.get("agents")
    if not isinstance(agents, dict):
        return None
    return agents


def _parse_providers_block(
    block: dict[str, Any],
    agents_block: dict[str, Any] | None = None,
) -> ProvidersConfig:
    """Parse a config.providers dict into ProvidersConfig.

    If agents_block is provided (from config.agents — a YAML sibling of
    config.providers), each entry is parsed into AgentProviderBinding and
    validated against the parsed provider records. An unknown provider id
    in any agent binding raises ProviderConfigError immediately.

    Raises ProviderConfigError on any validation failure.
    """
    raw_records = block.get("records") or []
    active = block.get("active")

    records: list[ProviderRecord] = []
    for raw in raw_records:
        if not isinstance(raw, dict):
            raise ProviderConfigError(
                f"provider record must be a mapping, got {type(raw).__name__}"
            )
        record_id = raw.get("id", "<unknown>")
        try:
            records.append(ProviderRecord.model_validate(raw))
        except pydantic.ValidationError as exc:
            # Surface the original Pydantic message in ProviderConfigError.
            # Always include the record id and re-include auth_strategy_mismatch
            # if present so the caller's assertion can match on it.
            pydantic_msg = str(exc)
            phrase = "auth_strategy_mismatch" if "auth_strategy_mismatch" in pydantic_msg else ""
            msg_parts = [f"invalid provider record '{record_id}'"]
            if phrase:
                msg_parts.append(phrase)
            msg_parts.append(pydantic_msg)
            raise ProviderConfigError(": ".join(msg_parts)) from exc

    try:
        config_obj = ProvidersConfig(records=records, active=active)
    except pydantic.ValidationError as exc:
        pydantic_msg = str(exc)
        phrase = "duplicate_record_ids" if "duplicate_record_ids" in pydantic_msg else ""
        msg_parts = ["invalid providers config"]
        if phrase:
            msg_parts.append(phrase)
        msg_parts.append(pydantic_msg)
        raise ProviderConfigError(": ".join(msg_parts)) from exc

    # AC01.5: active must reference an existing record
    if active is not None:
        if active not in config_obj.by_id:
            raise ProviderConfigError(
                f"active_record_not_found: '{active}' is not in records"
            )

    # Parse and validate agent bindings if present.
    # Build parsed_agents first (requires config_obj.by_id for cross-reference),
    # then reconstruct ProvidersConfig with all fields in one constructor call
    # so Pydantic model validators run over the complete object (not a partial one).
    parsed_agents: dict[str, AgentProviderBinding] = {}
    if agents_block is not None:
        known_ids = config_obj.by_id
        for agent_name, raw_binding in agents_block.items():
            if not isinstance(raw_binding, dict):
                raise ProviderConfigError(
                    f"agent '{agent_name}' binding must be a mapping, "
                    f"got {type(raw_binding).__name__}"
                )
            try:
                binding = AgentProviderBinding.model_validate(raw_binding)
            except pydantic.ValidationError as exc:
                raise ProviderConfigError(
                    f"invalid agent binding for '{agent_name}': {exc}"
                ) from exc
            if binding.provider not in known_ids:
                raise ProviderConfigError(
                    f"agent '{agent_name}' references unknown provider id '{binding.provider}'"
                )
            parsed_agents[agent_name] = binding

    # Reconstruct with all fields so model validators see the complete object.
    # The temp config_obj above was used only for by_id cross-reference;
    # this final construction is the canonical object returned to the caller.
    if parsed_agents:
        try:
            config_obj = ProvidersConfig(
                records=records,
                active=active,
                failover=config_obj.failover,
                agents=parsed_agents,
            )
        except pydantic.ValidationError as exc:
            pydantic_msg = str(exc)
            raise ProviderConfigError(
                f"invalid providers config: {pydantic_msg}"
            ) from exc

    return config_obj


def load_combos(repo_root: Path | None = None) -> dict[str, "Combo"]:
    """Read config.providers.combos from project-local or global settings.yaml.

    Same precedence as load_providers (project-local wins over global).
    Returns an empty dict when no combos block exists. Cross-validates
    every combo's providers list against the declared record IDs in
    config.providers.records and raises ProviderConfigError on any
    unknown reference.

    Raises:
        ProviderConfigError: combos block is not a mapping, an entry
            references an unknown provider id, or a Combo construction
            fails (empty providers, invalid strategy).
    """
    # Local import to avoid a load-order cycle: rotation imports from
    # this module's siblings (model.ProviderConfigError) but combos are
    # loaded only by code that already has the loader available.
    from fno.adapters.providers.rotation import Combo

    if repo_root is None:
        repo_root = Path(os.environ.get("PWD", os.getcwd()))

    candidates = [
        repo_root / ".fno" / "settings.yaml",
        # Bootstrap path: cannot use paths.config_file() here (settings loader self-reference).
        # Honors $FNO_GLOBAL_SETTINGS_PATH so unit tests pinning repo_root=tmp_path
        # do not leak the developer's real ~/.fno/settings.yaml.
        _global_settings_path(),
    ]

    for path in candidates:
        data = _read_yaml_safe(path)
        block = _extract_providers_block(data)
        if block is None:
            continue
        combos_raw = block.get("combos")
        if combos_raw is None:
            return {}
        if not isinstance(combos_raw, dict):
            raise ProviderConfigError(
                "config.providers.combos must be a mapping of name -> spec, "
                f"got {type(combos_raw).__name__}"
            )
        # Cross-validation needs the set of declared provider IDs.
        known_ids = {
            r["id"] for r in (block.get("records") or [])
            if isinstance(r, dict) and isinstance(r.get("id"), str)
        }
        result: dict[str, Combo] = {}
        for name, spec in combos_raw.items():
            if not isinstance(spec, dict):
                raise ProviderConfigError(
                    f"combo {name!r} spec must be a mapping, got "
                    f"{type(spec).__name__}"
                )
            providers_raw = spec.get("providers", [])
            if not isinstance(providers_raw, list):
                raise ProviderConfigError(
                    f"combo {name!r} providers must be a list, got "
                    f"{type(providers_raw).__name__}"
                )
            for pid in providers_raw:
                if pid not in known_ids:
                    raise ProviderConfigError(
                        f"combo {name!r} references unknown provider id "
                        f"{pid!r} (not in config.providers.records)"
                    )
            try:
                result[name] = Combo(
                    name=name,
                    strategy=spec.get("strategy", "fallback"),
                    sticky_limit=int(spec.get("sticky_limit", 1)),
                    providers=tuple(providers_raw),
                )
            except ValueError as exc:
                raise ProviderConfigError(str(exc)) from exc
        return result

    return {}


def load_providers(repo_root: Path | None = None) -> ProvidersConfig:
    """Read config.providers from project-local or global settings.yaml.

    Precedence (project-local wins, mirrors _load_v2_config_flag):
        1. {repo_root}/.fno/settings.yaml
        2. ~/.fno/settings.yaml

    Returns an empty ProvidersConfig (records=[], active=None) when:
    - Neither file exists
    - config.providers is absent
    - records list is empty

    Raises ProviderConfigError on any validation failure, naming the
    offending record id and including discriminating phrase(s).
    """
    if repo_root is None:
        repo_root = Path(os.environ.get("PWD", os.getcwd()))

    candidates = [
        repo_root / ".fno" / "settings.yaml",
        # Bootstrap path: cannot use paths.config_file() here (settings loader self-reference).
        # Honors $FNO_GLOBAL_SETTINGS_PATH so unit tests pinning repo_root=tmp_path
        # do not leak the developer's real ~/.fno/settings.yaml.
        _global_settings_path(),
    ]

    for path in candidates:
        data = _read_yaml_safe(path)
        block = _extract_providers_block(data)
        if block is None:
            continue
        # Found a providers block; also read the sibling agents block from the
        # same file so project-local-over-global precedence applies uniformly.
        agents_block = _extract_agents_block(data)
        return _parse_providers_block(block, agents_block=agents_block)

    # Neither file had a providers block.
    return ProvidersConfig(records=[], active=None)


def save_providers(
    config: ProvidersConfig,
    scope: Literal["project", "global"],
) -> None:
    """Write config back to settings.yaml at the requested scope.

    Atomic write (temp-file + rename) via fno.state.io.atomic_write.
    Preserves all existing top-level keys and other config.* sub-keys.
    """
    if scope == "project":
        target = Path(os.environ.get("PWD", os.getcwd())) / ".fno" / "settings.yaml"
    else:
        # Bootstrap path: cannot use paths.config_file() here (settings loader self-reference)
        target = Path.home() / ".fno" / "settings.yaml"

    # Read existing file to preserve other keys.
    # Use strict variant: if the file exists but is unparseable, raise rather
    # than silently overwriting all other top-level keys with an empty dict.
    existing = _read_yaml_strict(target)

    # Build serializable providers block from config
    records_raw = []
    for rec in config.records:
        d = rec.model_dump(exclude_none=False, mode="python")
        # Remove None values to keep the YAML clean, but keep required fields.
        cleaned: dict[str, Any] = {}
        for k, v in d.items():
            if v is None:
                continue
            if isinstance(v, Path):
                cleaned[k] = str(v)
            elif isinstance(v, list) and len(v) == 0:
                # Skip empty lists (tags) for cleanliness unless explicitly set
                continue
            else:
                cleaned[k] = v
        records_raw.append(cleaned)

    providers_block = {
        "active": config.active,
        "records": records_raw,
    }

    # Merge into existing structure under config.providers (whole-block replace)
    if not isinstance(existing.get("config"), dict):
        existing["config"] = {}
    existing["config"]["providers"] = providers_block

    # Ensure target directory exists
    target.parent.mkdir(parents=True, exist_ok=True)

    content = yaml.safe_dump(existing, default_flow_style=False, sort_keys=False, allow_unicode=True)
    atomic_write(target, content)


# ---------------------------------------------------------------------------
# Atomic mutate / atomic read helpers
#
# Phase 01 of provider rotation failover (ab-9728b70b). The failover
# controller swaps the active provider by mutating settings.yaml from
# multiple sessions concurrently. atomic_mutate_settings holds an exclusive
# fcntl lock for the entire read+mutate+write cycle so concurrent mutators
# serialize and no update is lost. Cross-serializes with fno.state.io
# .atomic_write because both use the same `<settings_path>.lock` sidecar
# (filelock 3.x on Unix dispatches to fcntl.flock under the hood).
# ---------------------------------------------------------------------------


def _settings_lock_path(settings_path: Path) -> Path:
    """Return the lock-file path for a given settings.yaml path.

    We standardize on `<settings_path>.lock` because that's what
    fno.state.io.atomic_write already uses; sharing the same lock
    file means raw fcntl.flock here serializes against filelock-based
    writers in atomic_write without a second lock domain.
    """
    return Path(str(settings_path) + ".lock")


def atomic_mutate_settings(
    mutator: Callable[[dict[str, Any]], dict[str, Any]],
    *,
    settings_path: Path,
) -> None:
    """Read, mutate, and write settings.yaml atomically under an exclusive lock.

    The full read-mutate-write cycle is held under fcntl.LOCK_EX so two
    concurrent mutators never lose updates. The write itself is tempfile
    + os.replace so non-locking readers never observe a partial-byte file.

    Args:
        mutator: function ``(dict) -> dict`` that takes the parsed
            settings.yaml content (as a plain dict) and returns the new
            content. May mutate in place and return the same dict, or
            return a fresh dict.
        settings_path: absolute path to settings.yaml. Required (no
            default) to avoid masking config-resolution bugs upstream.

    Raises:
        Whatever ``mutator`` raises - settings.yaml is left unchanged on
        disk and the lock is released. ``ProviderConfigError`` if the
        existing file is unparseable.
    """
    settings_path = Path(settings_path)
    lock_path = _settings_lock_path(settings_path)
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    # Open in "a" mode so the lock file is created on demand without
    # truncating any existing content, and the fd has write semantics so
    # flock LOCK_EX is allowed on Linux (some kernels reject EX on read-only
    # fds even though POSIX permits it).
    with open(lock_path, "a") as lock_f:
        fcntl.flock(lock_f.fileno(), fcntl.LOCK_EX)
        try:
            current = _read_yaml_strict(settings_path)
            updated = mutator(current)
            if not isinstance(updated, dict):
                raise TypeError(
                    "atomic_mutate_settings: mutator must return a dict, "
                    f"got {type(updated).__name__}"
                )
            content = yaml.safe_dump(
                updated, default_flow_style=False, sort_keys=False, allow_unicode=True,
            )
            tmp_path: Path | None = None
            try:
                with tempfile.NamedTemporaryFile(
                    mode="w",
                    dir=settings_path.parent,
                    prefix=f".{settings_path.name}.",
                    suffix=".tmp",
                    delete=False,
                    encoding="utf-8",
                ) as tmp:
                    tmp.write(content)
                    tmp_path = Path(tmp.name)
                os.replace(tmp_path, settings_path)
                tmp_path = None
            finally:
                if tmp_path is not None and tmp_path.exists():
                    tmp_path.unlink(missing_ok=True)
        finally:
            fcntl.flock(lock_f.fileno(), fcntl.LOCK_UN)


class MissingActiveProvider(ProviderConfigError):
    """Raised when settings.yaml's `active` field references a missing or
    null provider record. Used by ``read_active_provider_atomic`` to give
    callers a typed exception they can catch instead of a generic KeyError.
    """


@dataclasses.dataclass(frozen=True)
class ActiveProviderSnapshot:
    """Frozen snapshot of the active provider's record, taken under shared
    lock so all fields belong to the same logical record at one instant.

    Cites what-if finding #6: a swap-in-progress without lock can return
    new ``id`` paired with old ``auth`` (auth-mismatch cascade). The
    shared lock + frozen dataclass prevents this at the read side.
    """

    id: str
    cli: str
    auth: str
    credential_ref: str | None
    base_url: str | None
    pricing: dict[str, Any] | None


def read_active_provider_atomic(*, settings_path: Path) -> ActiveProviderSnapshot:
    """Atomically read the active provider record under a shared lock.

    LOCK_SH lets multiple concurrent readers proceed in parallel while
    excluding writers. ``atomic_mutate_settings`` uses LOCK_EX which
    blocks both other writers and readers. Together they prevent the
    auth-mismatch cascade.

    Args:
        settings_path: absolute path to settings.yaml. Required (no
            default) - same rationale as atomic_mutate_settings.

    Returns:
        Frozen ``ActiveProviderSnapshot`` with id, cli, auth, optional
        credential_ref/base_url/pricing.

    Raises:
        MissingActiveProvider: if active is None or names a record that
            doesn't exist in records.
        ProviderConfigError: on unparseable settings.yaml.
    """
    settings_path = Path(settings_path)
    lock_path = _settings_lock_path(settings_path)
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    with open(lock_path, "a") as lock_f:
        fcntl.flock(lock_f.fileno(), fcntl.LOCK_SH)
        try:
            settings = _read_yaml_strict(settings_path)
        finally:
            fcntl.flock(lock_f.fileno(), fcntl.LOCK_UN)

    block = _extract_providers_block(settings)
    if block is None:
        raise MissingActiveProvider(
            "config.providers block is absent or invalid"
        )
    active_id = block.get("active")
    if not active_id:
        raise MissingActiveProvider(
            "config.providers.active is unset (None or empty)"
        )
    raw_records = block.get("records") or []
    record = next((r for r in raw_records if isinstance(r, dict) and r.get("id") == active_id), None)
    if record is None:
        raise MissingActiveProvider(
            f"active provider id '{active_id}' is not in records"
        )

    return ActiveProviderSnapshot(
        id=str(active_id),
        cli=str(record.get("cli", "")),
        auth=str(record.get("auth", "")),
        credential_ref=record.get("credential_ref") if isinstance(record.get("credential_ref"), str) else None,
        base_url=record.get("base_url") if isinstance(record.get("base_url"), str) else None,
        pricing=record.get("pricing") if isinstance(record.get("pricing"), dict) else None,
    )
