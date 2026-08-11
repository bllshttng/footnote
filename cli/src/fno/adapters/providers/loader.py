"""Settings loader for provider rotation substrate.

Phase 01 of the provider rotation substrate (ab-256f6b6e).
Reads config.providers from .fno/settings.yaml with project-local-over-global
precedence, mirroring cli/src/fno/cli.py::_load_v2_config_flag.
"""
from __future__ import annotations

import dataclasses
import fcntl
import logging
import os
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Literal

import tomli_w
import pydantic
import tomllib
import yaml

from fno.adapters.providers.model import (
    AgentProviderBinding,
    ProviderConfigError,
    ProviderRecord,
    ProvidersConfig,
    QuotaConfig,
)
from fno.state.io import atomic_write

if TYPE_CHECKING:
    from fno.adapters.providers.rotation import Combo

logger = logging.getLogger(__name__)


def _global_settings_path() -> Path:
    """Resolve the per-user global config.toml path.

    Returns the config.toml sibling of ``$FNO_GLOBAL_SETTINGS_PATH`` when that
    env var is set to a non-empty value (mirrors ``fno.config._prefer_toml``),
    otherwise the default ``~/.fno/config.toml``. We cannot import from
    ``fno.config`` here because the provider loader runs during the config
    import path (bootstrap order).

    Empty-string env var is treated as "unset" rather than ``Path("")``.
    """
    env = os.environ.get("FNO_GLOBAL_SETTINGS_PATH")
    if env:
        return Path(env).with_name("config.toml")
    return Path.home() / ".fno" / "config.toml"


def _read_parsed(path: Path) -> dict[str, Any]:
    """Parse a config file by suffix (config.toml -> TOML, else YAML).

    config.toml-first with a read-only settings.yaml fallback for an unmigrated
    install (the provider loader runs at bootstrap and cannot trigger the main
    loader's auto-migrate). Returns {} on a missing/unparseable file.
    """
    for cand in _read_candidates(path):
        if not cand.is_file():
            continue
        try:
            text = cand.read_text(encoding="utf-8")
            if cand.suffix == ".toml":
                data = tomllib.loads(text)
            else:
                data = yaml.safe_load(text) or {}
            return data if isinstance(data, dict) else {}
        except (OSError, yaml.YAMLError, tomllib.TOMLDecodeError):
            return {}
    return {}


def _read_parsed_strict(path: Path) -> dict[str, Any]:
    """Read for write-back: a missing file returns {}, an unparseable one raises
    (prevents save_providers from clobbering all keys on a corrupt file)."""
    cand = next((c for c in _read_candidates(path) if c.is_file()), None)
    if cand is None:
        return {}
    try:
        text = cand.read_text(encoding="utf-8")
        data = tomllib.loads(text) if cand.suffix == ".toml" else (yaml.safe_load(text) or {})
        return data if isinstance(data, dict) else {}
    except (yaml.YAMLError, tomllib.TOMLDecodeError) as exc:
        raise ProviderConfigError(
            f"Cannot save: config file failed to parse ({cand}): {exc}"
        ) from exc
    except OSError as exc:
        raise ProviderConfigError(
            f"Cannot save: config file is not readable ({cand}): {exc}"
        ) from exc


def _read_candidates(path: Path) -> list[Path]:
    """config.toml (canonical) then its settings.yaml sibling (legacy fallback)."""
    if path.name == "config.toml":
        return [path, path.with_name("settings.yaml")]
    if path.name == "settings.yaml":
        return [path.with_name("config.toml"), path]
    return [path]


#: Set once per process when a config was read under the pre-rename
#: ``providers`` key, so the notice is a one-liner rather than per-load noise.
_LEGACY_BLOCK_WARNED = False


def _extract_accounts_block(data: dict[str, Any]) -> dict[str, Any] | None:
    """Return the accounts dict, canonical key first, pre-rename key second.

    Looks for ``accounts`` then ``providers``, each at the top level of a flat
    config.toml and then under a legacy ``config:`` wrapper; None if absent or
    invalid.

    This function - NOT ``fno.config._alias_legacy_keys`` - is the choke point
    for the rename. ``load_providers`` reads the config file through its own
    bootstrap reader (see ``_global_settings_path``) and never passes through
    ``fno.config``, so an alias placed there would sit on a path this data never
    takes. Every reader of account records arrives here.

    ``providers`` keeps working indefinitely: the fallback is a dict lookup, not
    a shim with a schedule, and nothing promises a removal. The file itself
    migrates on the first ``save_providers`` write.
    """
    global _LEGACY_BLOCK_WARNED
    config = data.get("config")
    config = config if isinstance(config, dict) else {}
    for key in ("accounts", "providers"):
        for source in (data, config):
            block = source.get(key)
            if isinstance(block, dict):
                if key == "providers" and not _LEGACY_BLOCK_WARNED:
                    _LEGACY_BLOCK_WARNED = True
                    logger.warning(
                        "config.providers is the pre-rename name for "
                        "config.accounts; the next account write migrates it"
                    )
                return block
    return None


def mutable_accounts_block(data: dict[str, Any]) -> dict[str, Any]:
    """Return the accounts block of a raw settings dict, ready for in-place edit.

    The mutator counterpart to :func:`_extract_accounts_block`, for the callers
    that edit the parsed dict directly under ``atomic_mutate_settings`` (combo
    add/remove/use/update, failover's active-flip) rather than going through
    ``save_providers``.

    Every such caller used to do ``data.setdefault("providers", {})``. Left
    alone after the rename that would create a SECOND, empty block beside the
    real ``accounts`` one and silently split the state in two - the combo would
    land somewhere nothing reads. Routing them all through here means the
    pre-rename block is migrated (moved, not copied) on first mutation, exactly
    as ``save_providers`` does, and there is one place that knows both spellings.
    """
    config = data.get("config")
    config = config if isinstance(config, dict) else None

    # Drain the three non-canonical locations UNCONDITIONALLY, even when the
    # canonical top-level block is already present. Popping only when it is
    # absent leaves a competitor behind, and a competitor is not inert:
    #   - a surviving `config.accounts` is merged OVER the edited top-level
    #     block by `_flatten_config`, so a combo write or failover's active-flip
    #     reports success against a file that never changed;
    #   - a surviving `providers` keeps the file readable under both names and
    #     splits the state, which is exactly what this helper exists to prevent.
    # Draining is also what the READER already does in effect: it returns the
    # first match in this precedence order and ignores the rest, so the blocks
    # removed here were never being read.
    drained: list[tuple[str, dict[str, Any]]] = []
    for source, key in (
        (config, "accounts"),
        (data, "providers"),
        (config, "providers"),
    ):
        if source is None:
            continue
        found = source.pop(key, None)
        if isinstance(found, dict):
            drained.append((key, found))

    block = data.get("accounts")
    if not isinstance(block, dict):
        # Adopt (never copy) the highest-precedence survivor, so the pre-rename
        # block is MOVED onto the canonical key rather than duplicated.
        block = drained[0][1] if drained else {}

    # Everything drained but not adopted loses to a higher-precedence block and
    # is therefore already unreadable - every reader is first-match-wins in this
    # same order. Dropping it is correct; dropping it SILENTLY is not, since the
    # pre-rename shape at least sat visibly on disk. The sharp case is an EMPTY
    # canonical block shadowing a populated duplicate: it wins on precedence and
    # the records go with no receipt.
    for key, discarded in drained:
        if discarded and discarded is not block:
            logger.warning(
                "discarding shadowed account block %r (%d key(s)); a "
                "higher-precedence block already provides config.accounts",
                key,
                len(discarded),
            )
    data["accounts"] = block
    return block


def _extract_agents_block(data: dict[str, Any]) -> dict[str, Any] | None:
    """Return the agents dict (flat ``agents`` or legacy ``config.agents``); None
    if absent/invalid. Callers treat None as an empty agents map."""
    agents = data.get("agents")
    if not isinstance(agents, dict):
        config = data.get("config")
        agents = config.get("agents") if isinstance(config, dict) else None
    return agents if isinstance(agents, dict) else None


def _flatten_config(data: dict[str, Any]) -> dict[str, Any]:
    """Lift a legacy ``config:`` wrapper's keys to the top level so a write-back
    produces a single-shape flat config.toml. No-op on an already-flat dict."""
    cfg = data.get("config")
    if not isinstance(cfg, dict):
        return data
    merged = {k: v for k, v in data.items() if k != "config"}
    merged.update(cfg)
    return merged


def _strip_none(data: Any) -> Any:
    """Recursively drop None-valued keys. TOML has no null; the loader reads an
    absent key as its default, so stripping None is lossless and keeps tomli_w
    from choking on an unserializable value."""
    if isinstance(data, dict):
        return {k: _strip_none(v) for k, v in data.items() if v is not None}
    if isinstance(data, list):
        return [_strip_none(v) for v in data]
    return data


# The reserved key names AgentsBlock declares under config.agents. They share the
# namespace with per-agent provider pins, and a reserved block may carry its own
# `provider` field (agents.defaults does, meaning the HARNESS axis), so a
# shape test alone reads one as the other and raises on a valid setting.
#
# A literal rather than `set(AgentsBlock.model_fields)` because this loader runs
# inside the config bootstrap path and must not import fno.config at module scope
# (the same reason _global_settings_path above is reimplemented here). Drift is
# caught instead by a test asserting set equality against that schema; the literal
# without that test would be the drift bug.
_AGENTS_RESERVED_KEYS = frozenset(
    {
        "a2a",
        "auto_register_sessions",
        "codex",
        "confirm",
        "dead_row_grace",
        "defaults",
        "gemini",
        "happy_routed_panes",
        "max_live",
        "min_free_gb",
        "profiles",
        "spawn_permission_mode",
        "worker_qos",
    }
)


def _parse_providers_block(
    block: dict[str, Any],
    agents_block: dict[str, Any] | None = None,
) -> ProvidersConfig:
    """Parse a config.providers dict into ProvidersConfig.

    If agents_block is provided (from config.agents — a YAML sibling of
    config.providers), each binding-shaped entry (a dict with a 'provider'
    key) is parsed into AgentProviderBinding and validated against the parsed
    provider records; non-binding entries (other agent settings sharing the
    namespace) are skipped. An unknown provider id in any agent binding
    raises ProviderConfigError immediately.

    Raises ProviderConfigError on any validation failure.
    """
    raw_records = block.get("records") or []
    active = block.get("active")
    # Pass the raw value to the pydantic bool field, which coerces "false"/"0"/"no"
    # correctly and REJECTS garbage. A local bool() would read a quoted "false" as
    # True and silently arm the credential-slot mutation (peer review, PR#366).
    auto_switch = block.get("auto_switch", False)

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
        config_obj = ProvidersConfig(records=records, active=active, auto_switch=auto_switch)
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
            # config.agents is a shared namespace: provider pins sit beside the
            # reserved settings blocks. Exclude by NAME first - shape cannot tell
            # the two apart, because a reserved block is allowed any field it
            # likes and agents.defaults uses `provider` for the harness axis.
            if agent_name in _AGENTS_RESERVED_KEYS:
                continue
            # Shape stays as the second filter: it skips scalars (max_live = 15)
            # and any other non-binding entry a future key adds.
            if not isinstance(raw_binding, dict) or "provider" not in raw_binding:
                continue
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
                auto_switch=config_obj.auto_switch,  # already-coerced bool from the first build
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
        repo_root / ".fno" / "config.toml",
        # Bootstrap path: cannot use paths.config_file() here (settings loader self-reference).
        # Honors $FNO_GLOBAL_SETTINGS_PATH so unit tests pinning repo_root=tmp_path
        # do not leak the developer's real ~/.fno/settings.yaml.
        _global_settings_path(),
    ]

    for path in candidates:
        data = _read_parsed(path)
        block = _extract_accounts_block(data)
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


def load_active_combo(repo_root: Path | None = None) -> str | None:
    """Read config.providers.active_combo (project-local wins over global).

    Same candidate walk + precedence as load_combos, so the value returned
    here matches what combos_use/combos_remove write. Returns None when no
    active_combo is set anywhere.
    """
    if repo_root is None:
        repo_root = Path(os.environ.get("PWD", os.getcwd()))

    for path in (repo_root / ".fno" / "config.toml", _global_settings_path()):
        block = _extract_accounts_block(_read_parsed(path))
        if block is None:
            continue
        ac = block.get("active_combo")
        if ac:
            return ac  # project-local wins; do not consult global
    return None


def load_quota_config(repo_root: Path | None = None) -> QuotaConfig:
    """Read config.providers.quota from project-local or global settings.

    Same precedence as load_combos (project-local wins over global). Returns
    all-defaults when no quota block exists. Fail-safe like the autonomous
    opt-in blocks (ActiveBacklogConfig): a malformed block degrades to defaults
    rather than raising out of a dispatch decision - the dangerous direction
    for an opt-in autonomous feature is silently-enabled, and defaults are off.
    """
    if repo_root is None:
        repo_root = Path(os.environ.get("PWD", os.getcwd()))

    candidates = [
        repo_root / ".fno" / "config.toml",
        _global_settings_path(),
    ]
    for path in candidates:
        data = _read_parsed(path)
        block = _extract_accounts_block(data)
        if block is None:
            continue
        quota_raw = block.get("quota")
        if quota_raw is None:
            return QuotaConfig()
        if not isinstance(quota_raw, dict):
            return QuotaConfig()
        try:
            return QuotaConfig.model_validate(quota_raw)
        except pydantic.ValidationError as exc:
            logger.warning(
                "config.providers.quota malformed (%s); using defaults", exc
            )
            return QuotaConfig()
    return QuotaConfig()


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
        repo_root / ".fno" / "config.toml",
        # Bootstrap path: cannot use paths.config_file() here (settings loader self-reference).
        # Honors $FNO_GLOBAL_SETTINGS_PATH so unit tests pinning repo_root=tmp_path
        # do not leak the developer's real ~/.fno/settings.yaml.
        _global_settings_path(),
    ]

    for path in candidates:
        data = _read_parsed(path)
        block = _extract_accounts_block(data)
        if block is None:
            continue
        # Found a providers block; also read the sibling agents block from the
        # same file so project-local-over-global precedence applies uniformly.
        agents_block = _extract_agents_block(data)
        return _parse_providers_block(block, agents_block=agents_block)

    # Neither file had a providers block.
    return ProvidersConfig(records=[], active=None)


# ---------------------------------------------------------------------------
# "The active account" - ONE resolver.
#
# Two notions of active coexist and can disagree: `config.providers.active`
# (routing-active, what the config points at) and the id stamped into a CLI's
# shared slot (what a worker's credential actually comes from). For an
# `auth: managed` record only the slot can supply the credential, so the slot
# occupant is the truth and routing-active is a stale pointer.
#
# The display path had this right and the dispatch path did not, which meant a
# quota decision was evaluated for one account while the worker spawned on
# another. Both now route through `_active_id_for`, so there is one branch to be
# right about rather than two that drift.
# ---------------------------------------------------------------------------


def _active_id_for(
    record: ProviderRecord,
    config: ProvidersConfig,
    root: Path | None = None,
) -> str | None:
    """THE branch: the id in force on ``record``'s lane. Fail-open to None."""
    if record.auth == "managed":
        from fno.adapters.providers.managed import active_slot_id

        try:
            return active_slot_id(record.harness, root)
        except Exception:  # noqa: BLE001 - an unreadable store must not break display
            return None
    return config.active


def is_effective_active(
    record: ProviderRecord,
    config: ProvidersConfig,
    root: Path | None = None,
) -> bool:
    """True when ``record`` is the account actually in force on its lane."""
    return record.id == _active_id_for(record, config, root)


def effective_active(
    config: ProvidersConfig | None = None,
    *,
    repo_root: Path | None = None,
    root: Path | None = None,
) -> str | None:
    """The record id a default dispatch actually runs on, or None.

    Resolves the routing-active record and then asks its own lane who is in
    force, so a managed routing-active pointer that the slot has since moved
    past yields the slot occupant rather than the stale name.
    """
    if config is None:
        config = load_providers(repo_root=repo_root)
    if not config.active:
        return None
    record = config.by_id.get(config.active)
    if record is None:
        return config.active
    return _active_id_for(record, config, root)


def save_providers(
    config: ProvidersConfig,
    scope: Literal["project", "global"],
) -> None:
    """Write config back to settings.yaml at the requested scope.

    Atomic write (temp-file + rename) via fno.state.io.atomic_write.
    Preserves all existing top-level keys and other config.* sub-keys.
    """
    if scope == "project":
        target = Path(os.environ.get("PWD", os.getcwd())) / ".fno" / "config.toml"
    else:
        # Bootstrap path: cannot use paths.config_file() here (settings loader self-reference)
        target = Path.home() / ".fno" / "config.toml"

    # Read existing file to preserve other keys.
    # Use strict variant: if the file exists but is unparseable, raise rather
    # than silently overwriting all other top-level keys with an empty dict.
    existing = _read_parsed_strict(target)

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

    providers_block: dict[str, Any] = {"records": records_raw}
    if config.active is not None:
        providers_block["active"] = config.active
    # Round-trip auto_switch off the object (only when armed, to keep the default
    # case clean). Without this a ProvidersConfig(auto_switch=True) written back
    # would silently disarm; the disk-preserve below only covers a value already
    # on disk (peer review, PR#366).
    if config.auto_switch:
        providers_block["auto_switch"] = True

    # Flat config.toml: accounts lives at the top level (whole-block replace).
    # If existing was read from a legacy wrapped file, lift its config.* keys up
    # so the written config.toml is single-shape (never a mixed config: + flat).
    existing = _flatten_config(existing)
    # Preserve account subkeys this write path does not rebuild (quota, combos,
    # failover, agents, ...). Rebuilding providers_block from only records+active
    # would otherwise silently drop them, so e.g. `fno config accounts use` after
    # an operator set config.accounts.quota.defer_dispatch would turn quota
    # deferral back off (x-5d3e review). Rebuilt keys win; everything else rides.
    #
    # Read the pre-rename `providers` block too, and pop it: this write IS the
    # migration. Carrying both keys forward would leave the file readable under
    # either name forever and the subkeys duplicated in two places.
    old_accounts = existing.get("accounts")
    if not isinstance(old_accounts, dict):
        old_accounts = existing.get("providers")
    if isinstance(old_accounts, dict):
        for key, val in old_accounts.items():
            if key not in ("records", "active"):
                providers_block.setdefault(key, val)
    existing.pop("providers", None)
    existing["accounts"] = providers_block

    target.parent.mkdir(parents=True, exist_ok=True)
    atomic_write(target, tomli_w.dumps(_strip_none(existing)))


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
            current = _read_parsed_strict(settings_path)
            updated = mutator(current)
            if not isinstance(updated, dict):
                raise TypeError(
                    "atomic_mutate_settings: mutator must return a dict, "
                    f"got {type(updated).__name__}"
                )
            content = tomli_w.dumps(_strip_none(_flatten_config(updated)))
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
    harness: str
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
        Frozen ``ActiveProviderSnapshot`` with id, harness, auth, optional
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
            settings = _read_parsed_strict(settings_path)
        finally:
            fcntl.flock(lock_f.fileno(), fcntl.LOCK_UN)

    block = _extract_accounts_block(settings)
    if block is None:
        raise MissingActiveProvider(
            "config.accounts block is absent or invalid"
        )
    active_id = block.get("active")
    if not active_id:
        raise MissingActiveProvider(
            "config.accounts.active is unset (None or empty)"
        )
    raw_records = block.get("records") or []
    record = next((r for r in raw_records if isinstance(r, dict) and r.get("id") == active_id), None)
    if record is None:
        raise MissingActiveProvider(
            f"active provider id '{active_id}' is not in records"
        )

    return ActiveProviderSnapshot(
        id=str(active_id),
        # Read both spellings: `harness` is canonical, `cli` is the pre-rename
        # key that an unmigrated config.toml still carries. A plain get("cli")
        # would silently yield "" for every migrated record.
        harness=str(record.get("harness") or record.get("cli") or ""),
        auth=str(record.get("auth", "")),
        credential_ref=record.get("credential_ref") if isinstance(record.get("credential_ref"), str) else None,
        base_url=record.get("base_url") if isinstance(record.get("base_url"), str) else None,
        pricing=record.get("pricing") if isinstance(record.get("pricing"), dict) else None,
    )
