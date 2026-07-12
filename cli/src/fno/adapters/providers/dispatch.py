"""dispatch_env and resolve_env_value for provider rotation substrate.

Phase 03 of the provider rotation substrate (ab-256f6b6e).

Pure functions: no global state, no caching, concurrency-safe by design.

Phase 02b of provider rotation failover (ab-9728b70b) adds
``spawn_with_provider_snapshot``: read the active provider snapshot once
under a shared lock and inject it into a child subprocess's env so the
subprocess and its descendants see a stable provider for their lifetime,
even if the parent's active changes mid-flight.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any

from fno.adapters.providers.loader import (
    load_providers,
    read_active_provider_atomic,
)
from fno.adapters.providers.model import (
    ProviderNotFoundError,
    ProviderRecord,
    ProviderUnavailableError,
)
from fno.adapters.providers.staging import _default_providers_root, verify_staged


def resolve_env_value(value: str) -> str:
    """Resolve ${...} references in env values.

    Supported references:
        ${ENV:VAR_NAME}        - read from os.environ
        ${KEYCHAIN:item_name}  - read from macOS keychain via `security`
        ${FILE:/path/to/file}  - read first line of file
        ${literal_value}       - escape; returns "literal_value" verbatim
                                 (detected by absence of ':' after '${')

    Plain strings (no '${' at start) are returned as-is.
    Unresolved references raise ProviderUnavailableError.
    """
    if not value.startswith("${"):
        return value

    # Strip outer ${ ... }
    if not value.endswith("}"):
        return value

    inner = value[2:-1]  # contents between ${ and }

    if ":" not in inner:
        # Escape mechanism: ${literal_value} - no recognised prefix.
        return inner

    prefix, rest = inner.split(":", 1)

    if prefix == "ENV":
        try:
            return os.environ[rest]
        except KeyError:
            raise ProviderUnavailableError(
                f"Environment variable not set: {rest!r} (from reference {value!r})"
            )

    if prefix == "KEYCHAIN":
        cmd = ["security", "find-generic-password", "-w", "-s", rest]
        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                check=True,
            )
            return result.stdout.strip()
        except subprocess.CalledProcessError as exc:
            raise ProviderUnavailableError(
                f"Keychain item not found: {rest!r} (from reference {value!r})"
            ) from exc

    if prefix == "FILE":
        file_path = Path(rest)
        try:
            first_line = file_path.read_text(encoding="utf-8").splitlines()[0].strip()
            return first_line
        except (OSError, IndexError) as exc:
            raise ProviderUnavailableError(
                f"Cannot read key file: {file_path} (from reference {value!r})"
            ) from exc

    # Unknown prefix - treat as literal (escape mechanism extended).
    return inner


def _env_for_oauth(record: ProviderRecord, root: Path) -> dict[str, str]:
    """Build the env dict for an oauth_dir provider."""
    account_dir = root / record.id
    cli = record.cli
    if cli == "claude":
        return {"CLAUDE_CONFIG_DIR": str(account_dir / ".claude")}
    else:
        # gemini, codex, openclaw, hermes: HOME override.
        return {"HOME": str(account_dir / "home")}


def _env_for_api_key(record: ProviderRecord) -> dict[str, str]:
    """Build the env dict for an api_key provider; resolve all references.

    Raises ProviderUnavailableError naming the offending key if any value
    cannot be resolved. Never returns a partial dict.
    """
    assert record.env is not None  # enforced by model validator
    resolved: dict[str, str] = {}
    for key, raw_value in record.env.items():
        try:
            resolved[key] = resolve_env_value(raw_value)
        except ProviderUnavailableError as exc:
            raise ProviderUnavailableError(
                f"Cannot resolve env value for key {key!r}: {exc}"
            ) from exc
    return resolved


def dispatch_env(
    provider_id: str,
    repo_root: Path | None = None,
    root: Path | None = None,
) -> dict[str, str]:
    """Return the subprocess env dict for invoking provider_id's CLI.

    Pure function. Does not mutate any global state. Reads settings.yaml
    via load_providers(), looks up the record, and computes the env.

    Raises:
        ProviderNotFoundError: if provider_id not in config.records
        ProviderUnavailableError: if oauth_dir not staged or api_key env
            cannot be resolved (missing keychain entry, env var unset)
    """
    if root is None:
        root = _default_providers_root()
    config = load_providers(repo_root=repo_root)
    by_id = config.by_id

    if provider_id not in by_id:
        raise ProviderNotFoundError(
            f"Provider {provider_id!r} is not configured. "
            "Check config.providers.records in settings.yaml."
        )

    record = by_id[provider_id]

    if record.auth == "oauth_dir":
        if not verify_staged(record, root=root):
            raise ProviderUnavailableError(
                f"Provider {provider_id!r} is not staged. "
                "Call stage(record) before dispatch_env()."
            )
        return _env_for_oauth(record, root)

    if record.auth == "managed":
        # A managed account materializes into the shared default slot
        # (~/.claude for claude, ~/.codex for codex), so dispatch adds no
        # CLAUDE_CONFIG_DIR/HOME override - the CLI reads the slot directly.
        return {}

    # api_key path
    return _env_for_api_key(record)


# ---------------------------------------------------------------------------
# Phase 02b of provider rotation failover (ab-9728b70b).
# ---------------------------------------------------------------------------

FNO_PROVIDER_ENV_KEYS = (
    "FNO_PROVIDER_ID",
    "FNO_PROVIDER_AUTH",
    "FNO_PROVIDER_CRED_REF",
    "FNO_PROVIDER_BASE_URL",
    "FNO_PROVIDER_PRICING",
)


def _default_settings_path() -> Path:
    """Resolve the canonical settings.yaml path for snapshot reads.

    Mirrors load_providers: project-local wins, global is fallback.
    Returns the project-local path when its file exists, otherwise the
    global path (existing or not - read_active_provider_atomic handles
    missing files by creating the lock and surfacing MissingActiveProvider
    when no provider is configured).

    Finding 5: project_local.is_file() is checked BEFORE config_file() is
    resolved, so a ValidationError from a malformed global settings.yaml
    does not prevent the project-local snapshot from being used.
    """
    project_local = Path(os.environ.get("PWD", os.getcwd())) / ".fno" / "config.toml"
    # Check project-local FIRST to avoid resolving (and potentially raising on)
    # the global config_file() when a project-local override exists.
    if project_local.is_file():
        return project_local
    from fno import paths as _paths
    return _paths.config_file()


def spawn_with_provider_snapshot(
    cmd: list[str],
    *,
    settings_path: Path | None = None,
    env: dict[str, str] | None = None,
    **popen_kwargs: Any,
) -> subprocess.Popen:
    """Spawn ``cmd`` with a frozen snapshot of the active provider in env.

    The active provider is read once under fcntl.LOCK_SH immediately
    before the spawn (task 1.3 semantics) so the snapshot can't tear
    against a concurrent ``atomic_mutate_settings`` writer. The four
    ``FNO_PROVIDER_*`` env vars are then layered on top of the
    parent's env (or ``env=`` if explicitly provided) so the subprocess
    and its descendants see a stable provider for their full lifetime,
    even if the parent flips ``active`` immediately after spawn returns.

    Args:
        cmd: command argv list passed to subprocess.Popen.
        settings_path: optional explicit settings.yaml path. When None,
            resolves via the project-local-then-global precedence rule
            used by load_providers.
        env: optional explicit env dict. When None, inherits from
            ``os.environ``. The provider snapshot is layered on top
            either way.
        **popen_kwargs: additional kwargs forwarded to subprocess.Popen
            (stdout, stderr, cwd, etc.). The ``env`` kwarg is reserved -
            pass it via the named ``env`` parameter so the spawn helper
            can layer the provider snapshot.

    Returns:
        ``subprocess.Popen`` instance, started.

    Raises:
        MissingActiveProvider: if settings.yaml has no active provider
            (raised before the spawn happens; no zombie subprocess).
    """
    if "env" in popen_kwargs:
        raise TypeError(
            "spawn_with_provider_snapshot: pass env via the named keyword "
            "argument, not via popen_kwargs - the spawn helper layers the "
            "provider snapshot on top of it"
        )

    if settings_path is None:
        settings_path = _default_settings_path()

    snapshot = read_active_provider_atomic(settings_path=settings_path)

    base_env = dict(os.environ if env is None else env)
    base_env["FNO_PROVIDER_ID"] = snapshot.id
    base_env["FNO_PROVIDER_AUTH"] = snapshot.auth
    if snapshot.credential_ref is not None:
        base_env["FNO_PROVIDER_CRED_REF"] = snapshot.credential_ref
    if snapshot.base_url is not None:
        base_env["FNO_PROVIDER_BASE_URL"] = snapshot.base_url
    if snapshot.pricing is not None:
        base_env["FNO_PROVIDER_PRICING"] = json.dumps(snapshot.pricing)

    return subprocess.Popen(cmd, env=base_env, **popen_kwargs)
