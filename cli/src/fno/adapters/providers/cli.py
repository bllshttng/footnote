"""Typer sub-app for fno providers commands.

Phase 02 of the provider rotation substrate (ab-256f6b6e).
Provides: list, show, add, test, use, remove.

Phase 03 will wire in staging.stage(record) inside the `add` command.
"""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path
from typing import Optional

import typer

from fno.adapters.providers.dispatch import dispatch_env
from fno.adapters.providers.loader import load_providers, save_providers
from fno.adapters.providers.model import (
    ProviderConfigError,
    ProviderNotFoundError,
    ProviderRecord,
    ProviderUnavailableError,
)

cli = typer.Typer(name="providers", help="Manage provider records and active selection.")


def _resolve_home(env: dict[str, str] | None = None) -> Path:
    """Return HOME from env override or os.environ."""
    if env and "HOME" in env:
        return Path(env["HOME"])
    return Path(os.environ.get("HOME", str(Path.home())))


def _resolve_cwd(env: dict[str, str] | None = None) -> Path:
    """Return PWD from env override or os.getcwd()."""
    if env and "PWD" in env:
        return Path(env["PWD"])
    return Path(os.environ.get("PWD", os.getcwd()))


def _get_repo_root() -> Path:
    """Derive repo_root from environment (respects test isolation)."""
    return _resolve_cwd()


def _settings_path_for_scope(scope: str) -> Path:
    """Return the settings.yaml path for the given scope."""
    if scope == "project":
        return _resolve_cwd() / ".fno" / "settings.yaml"
    else:
        return _resolve_home() / ".fno" / "settings.yaml"


def _load(scope: str = "global") -> "fno.adapters.providers.model.ProvidersConfig":  # type: ignore[name-defined]
    """Load providers config; error on validation failure."""
    from fno.adapters.providers.loader import load_providers
    repo_root = _get_repo_root()
    try:
        return load_providers(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)


# ---------------------------------------------------------------------------
# list
# ---------------------------------------------------------------------------

@cli.command("list")
def list_providers() -> None:
    """List all configured providers, marking the active one with *."""
    config = _load()
    if not config.records:
        typer.echo(
            "No providers configured. Run `fno providers add` to add one."
        )
        return
    for record in config.records:
        marker = "*" if record.id == config.active else " "
        typer.echo(f"{marker} {record.id}  [{record.cli}] {record.auth}  priority={record.priority}")


# ---------------------------------------------------------------------------
# show
# ---------------------------------------------------------------------------

@cli.command("show")
def show_provider(provider_id: str = typer.Argument(...)) -> None:
    """Show all fields of a single provider record."""
    config = _load()
    record = config.by_id.get(provider_id)
    if record is None:
        typer.echo(f"error: provider '{provider_id}' not found", err=True)
        raise typer.Exit(1)
    typer.echo(f"id:                  {record.id}")
    typer.echo(f"name:                {record.name}")
    typer.echo(f"cli:                 {record.cli}")
    typer.echo(f"auth:                {record.auth}")
    typer.echo(f"priority:            {record.priority}")
    if record.credentials_source is not None:
        typer.echo(f"credentials_source:  {record.credentials_source}")
    if record.env:
        typer.echo(f"env:                 {record.env}")
    if record.account_id:
        typer.echo(f"account_id:          {record.account_id}")
    if record.tags:
        typer.echo(f"tags:                {record.tags}")
    if record.description:
        typer.echo(f"description:         {record.description}")
    if config.active == record.id:
        typer.echo("active:              yes")


# ---------------------------------------------------------------------------
# add
# ---------------------------------------------------------------------------

@cli.command("add")
def add_provider(
    provider_id: str = typer.Argument(..., help="Unique provider id (lowercase alphanumeric + hyphens)"),
    cli_name: str = typer.Option(..., "--cli", "-c", help="claude|gemini|codex|openclaw|hermes"),
    auth: str = typer.Option(..., "--auth", "-a", help="oauth_dir|api_key"),
    credentials_source: Optional[Path] = typer.Option(None, "--credentials-source"),
    env: list[str] = typer.Option([], "--env", help="KEY=VALUE pairs for api_key auth"),
    priority: int = typer.Option(100, "--priority", "-p"),
    name: Optional[str] = typer.Option(None, "--name"),
    account_id: Optional[str] = typer.Option(None, "--account-id"),
    scope: str = typer.Option("global", "--scope", "-s", help="global|project"),
    force: bool = typer.Option(False, "--force", "-F", help="Overwrite existing record"),
) -> None:
    """Add a new provider record. Refuses to overwrite without --force.

    Phase 02 writes the settings.yaml record.
    # TODO(phase-03): wire up staging.stage(record) here
    (fno.adapters.providers.staging does not exist yet)
    """
    # Parse --env KEY=VALUE pairs
    env_dict: dict[str, str] | None = None
    if env:
        env_dict = {}
        for pair in env:
            if "=" not in pair:
                typer.echo(
                    f"error: malformed --env entry '{pair}' (expected KEY=VALUE format)",
                    err=True,
                )
                raise typer.Exit(1)
            k, _, v = pair.partition("=")
            env_dict[k] = v
        if not env_dict:
            env_dict = None

    # Default name to id
    record_name = name or provider_id

    # Build + validate the record via Pydantic
    try:
        record = ProviderRecord(
            id=provider_id,
            name=record_name,
            cli=cli_name,  # type: ignore[arg-type]
            auth=auth,  # type: ignore[arg-type]
            credentials_source=credentials_source,
            env=env_dict,
            priority=priority,
            account_id=account_id,
        )
    except Exception as exc:
        # Surface Pydantic validation errors including auth_strategy_mismatch
        msg = str(exc)
        typer.echo(f"error: {msg}", err=True)
        raise typer.Exit(1)

    # Load existing config
    repo_root = _get_repo_root()
    try:
        config = load_providers(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error loading existing config: {exc}", err=True)
        raise typer.Exit(1)

    # Check for duplicate
    if record.id in config.by_id and not force:
        typer.echo(
            f"error: provider '{record.id}' already exists. Use --force to overwrite.",
            err=True,
        )
        raise typer.Exit(1)

    # Replace or append
    new_records = [r for r in config.records if r.id != record.id]
    new_records.append(record)

    from fno.adapters.providers.model import ProvidersConfig
    new_config = ProvidersConfig(records=new_records, active=config.active)

    # Write
    try:
        save_providers(new_config, scope=scope)  # type: ignore[arg-type]
    except OSError as exc:
        typer.echo(f"error: failed to write settings.yaml: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(f"Provider '{record.id}' added (scope={scope}).")


# ---------------------------------------------------------------------------
# test
# ---------------------------------------------------------------------------

@cli.command("test")
def test_provider(
    provider_id: str = typer.Argument(...),
    smoke: bool = typer.Option(False, "--smoke", help="Attempt a real CLI invocation (costs quota)"),
) -> None:
    """Validate provider config: record lookup + binary on PATH + credentials_source exists.

    With --smoke, also attempt a real CLI invocation (not fully implemented in Phase 02).
    """
    config = _load()
    record = config.by_id.get(provider_id)
    if record is None:
        typer.echo(f"error: provider '{provider_id}' not found", err=True)
        raise typer.Exit(1)

    # Check CLI binary on PATH
    binary = shutil.which(record.cli)
    if binary is None:
        typer.echo(
            f"error: CLI binary '{record.cli}' is not on PATH",
            err=True,
        )
        raise typer.Exit(1)

    # Check credentials_source for oauth_dir
    if record.auth == "oauth_dir":
        if record.credentials_source is None:
            typer.echo(
                f"error: auth=oauth_dir but credentials_source is not set for '{provider_id}'",
                err=True,
            )
            raise typer.Exit(1)
        if not record.credentials_source.exists():
            typer.echo(
                f"error: credentials_source '{record.credentials_source}' does not exist",
                err=True,
            )
            raise typer.Exit(1)

    if smoke:
        # Resolve dispatch env vars so the subprocess runs under the right
        # credentials, matching how real target invocations work.
        try:
            env_vars = dispatch_env(provider_id, repo_root=_get_repo_root())
        except (ProviderNotFoundError, ProviderUnavailableError) as exc:
            typer.echo(f"smoke: dispatch_env failed for '{provider_id}': {exc}", err=True)
            raise typer.Exit(1)

        try:
            smoke_result = subprocess.run(
                [record.cli, "--help"],
                capture_output=True,
                text=True,
                timeout=10,
                env={**os.environ, **env_vars},
            )
            if smoke_result.returncode != 0:
                typer.echo(
                    f"smoke test: '{record.cli} --help' exited {smoke_result.returncode}",
                    err=True,
                )
                raise typer.Exit(1)
        except (subprocess.TimeoutExpired, FileNotFoundError) as exc:
            typer.echo(f"smoke test error: {exc}", err=True)
            raise typer.Exit(1)

    typer.echo(f"Provider '{provider_id}' looks OK.")


# ---------------------------------------------------------------------------
# use
# ---------------------------------------------------------------------------

@cli.command("use")
def use_provider(
    provider_id: str = typer.Argument(...),
    scope: str = typer.Option("project", "--scope", help="project|global"),
) -> None:
    """Set the active provider (default scope: project)."""
    repo_root = _get_repo_root()
    try:
        config = load_providers(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)

    if provider_id not in config.by_id:
        typer.echo(f"error: provider '{provider_id}' not found", err=True)
        raise typer.Exit(1)

    from fno.adapters.providers.model import ProvidersConfig
    new_config = ProvidersConfig(records=config.records, active=provider_id)

    try:
        save_providers(new_config, scope=scope)  # type: ignore[arg-type]
    except OSError as exc:
        typer.echo(f"error: failed to write settings.yaml: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(f"Active provider set to '{provider_id}' (scope={scope}).")


# ---------------------------------------------------------------------------
# remove
# ---------------------------------------------------------------------------

@cli.command("remove")
def remove_provider(
    provider_id: str = typer.Argument(...),
    force: bool = typer.Option(False, "--force", "-F", help="Allow removing the active provider"),
    scope: str = typer.Option("global", "--scope", help="global|project"),
) -> None:
    """Remove a provider record. Refuses to remove the active provider without --force."""
    repo_root = _get_repo_root()
    try:
        config = load_providers(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)

    if provider_id not in config.by_id:
        typer.echo(f"error: provider '{provider_id}' not found", err=True)
        raise typer.Exit(1)

    if config.active == provider_id and not force:
        typer.echo(
            f"error: '{provider_id}' is the active provider. Use --force to remove it.",
            err=True,
        )
        raise typer.Exit(1)

    new_records = [r for r in config.records if r.id != provider_id]
    new_active = config.active if config.active != provider_id else None

    from fno.adapters.providers.model import ProvidersConfig
    new_config = ProvidersConfig(records=new_records, active=new_active)

    try:
        save_providers(new_config, scope=scope)  # type: ignore[arg-type]
    except OSError as exc:
        typer.echo(f"error: failed to write settings.yaml: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(f"Provider '{provider_id}' removed (scope={scope}).")


# ---------------------------------------------------------------------------
# combos sub-app: add / list / remove / test / use
# Plan B (Spec 4, ab-0e5a921e). Combos are named ordered provider lists
# with a rotation strategy (fallback | round_robin). Storage lives under
# ``config.providers.combos.<name>`` in settings.yaml.
# ---------------------------------------------------------------------------

combos_cli = typer.Typer(
    name="combos",
    help="Manage provider combos (named ordered lists with rotation strategies).",
)
cli.add_typer(combos_cli, name="combos")


def _combos_settings_path(scope: str) -> Path:
    """Resolve settings path for combos commands (mirrors providers scope)."""
    return _settings_path_for_scope(scope)


@combos_cli.command("add")
def combos_add(
    name: str = typer.Argument(..., help="Combo name (must be unique)"),
    strategy: str = typer.Option(
        "fallback",
        "--strategy",
        help="Rotation strategy: fallback | round_robin",
    ),
    sticky_limit: int = typer.Option(
        1,
        "--sticky",
        help="round_robin only: calls per provider before advancing (>=1; clamped)",
    ),
    providers_csv: str = typer.Option(
        ...,
        "--providers",
        help="Comma-separated provider IDs (must exist in config.providers.records)",
    ),
    scope: str = typer.Option("project", "--scope", help="project | global"),
) -> None:
    """Add a new combo. Validates each provider exists; refuses if combo already exists."""
    from fno.adapters.providers.loader import (
        atomic_mutate_settings,
        load_providers,
    )
    from fno.adapters.providers.rotation import Combo

    providers_list = [p.strip() for p in providers_csv.split(",") if p.strip()]
    if not providers_list:
        typer.echo("error: --providers must list at least one provider id", err=True)
        raise typer.Exit(1)

    # Cross-validate provider IDs against records BEFORE mutation.
    config = _load()
    unknown = [p for p in providers_list if p not in config.by_id]
    if unknown:
        typer.echo(
            f"error: combo {name!r} references unknown provider id(s) "
            f"{unknown!r} (not in config.providers.records)",
            err=True,
        )
        raise typer.Exit(1)

    # Validate strategy/sticky via Combo construction (raises ValueError).
    try:
        Combo(
            name=name,
            strategy=strategy,  # type: ignore[arg-type]
            sticky_limit=sticky_limit,
            providers=tuple(providers_list),
        )
    except ValueError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)

    target = _combos_settings_path(scope)

    def mutator(data: dict) -> dict:
        config_section = data.setdefault("config", {})
        providers_section = config_section.setdefault("providers", {})
        combos_section = providers_section.setdefault("combos", {})
        if name in combos_section:
            raise ValueError(
                f"combo {name!r} already exists in {scope} settings; "
                "remove it first or use a different name"
            )
        combos_section[name] = {
            "strategy": strategy,
            "sticky_limit": sticky_limit,
            "providers": providers_list,
        }
        return data

    try:
        atomic_mutate_settings(mutator, settings_path=target)
    except ValueError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)
    except OSError as exc:
        typer.echo(f"error: failed to write settings.yaml: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(
        f"Combo {name!r} added (strategy={strategy}, sticky={sticky_limit}, "
        f"providers={providers_list}, scope={scope})."
    )


@combos_cli.command("list")
def combos_list(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit JSON instead of a table"),
) -> None:
    """List all configured combos with cursor + member-count info."""
    import json as json_mod

    from fno.adapters.providers.loader import load_combos
    from fno.adapters.providers.rotation import compute_providers_hash
    from fno.adapters.providers.runtime_state import read_cursor

    repo_root = _get_repo_root()
    try:
        combos = load_combos(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)
    if not combos:
        if as_json:
            typer.echo("[]")
        else:
            typer.echo(
                "No combos configured. Run `fno providers combos add` to add one."
            )
        return

    rows = []
    for name, combo in combos.items():
        providers_hash = compute_providers_hash(combo.providers)
        cursor = read_cursor(name, providers_hash)
        rows.append({
            "name": name,
            "strategy": combo.strategy,
            "sticky_limit": combo.sticky_limit,
            "members": list(combo.providers),
            "cursor_index": cursor.cursor_index if cursor else None,
            "consecutive_use_count": cursor.consecutive_use_count if cursor else None,
        })

    if as_json:
        typer.echo(json_mod.dumps(rows, indent=2))
        return
    for row in rows:
        cursor_str = (
            f"idx={row['cursor_index']}/cnt={row['consecutive_use_count']}"
            if row["cursor_index"] is not None
            else "no-cursor"
        )
        typer.echo(
            f"{row['name']}  [{row['strategy']}, sticky={row['sticky_limit']}]  "
            f"members={','.join(row['members'])}  {cursor_str}"
        )


@combos_cli.command("remove")
def combos_remove(
    name: str = typer.Argument(...),
    scope: str = typer.Option("project", "--scope", help="project | global"),
) -> None:
    """Remove a combo. If the combo is the active_combo, also clears that field."""
    from fno.adapters.providers.loader import atomic_mutate_settings

    target = _combos_settings_path(scope)
    cleared_active = False

    def mutator(data: dict) -> dict:
        nonlocal cleared_active
        providers_section = data.get("config", {}).get("providers", {})
        combos_section = providers_section.get("combos", {})
        if name not in combos_section:
            raise ValueError(f"combo {name!r} not found in {scope} settings")
        del combos_section[name]
        if providers_section.get("active_combo") == name:
            providers_section["active_combo"] = None
            cleared_active = True
        return data

    try:
        atomic_mutate_settings(mutator, settings_path=target)
    except ValueError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)
    except OSError as exc:
        typer.echo(f"error: failed to write settings.yaml: {exc}", err=True)
        raise typer.Exit(1)

    msg = f"Combo {name!r} removed (scope={scope})."
    if cleared_active:
        msg += " active_combo cleared."
    typer.echo(msg)


@combos_cli.command("test")
def combos_test(
    name: str = typer.Argument(...),
) -> None:
    """Validate a combo: shape, member existence, per-member cooldown state.

    Config-only by design: does NOT issue API calls (smoke-testing every
    member multiplies cost). For an active liveness probe, run
    `fno providers test <id> --smoke` per member.
    """
    from fno.adapters.providers.loader import load_combos
    from fno.adapters.providers.runtime_state import read_state

    repo_root = _get_repo_root()
    try:
        combos = load_combos(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)

    if name not in combos:
        typer.echo(f"error: combo {name!r} not found", err=True)
        raise typer.Exit(1)

    combo = combos[name]
    state = read_state()
    # Per-member health.
    cooldowned: list[str] = []
    healthy: list[str] = []
    for pid in combo.providers:
        h = state.provider_health.get(pid)
        if h is not None and h.rate_limited_until is not None and h.rate_limited_until > 0:
            import time as _t
            if h.rate_limited_until > _t.time():
                cooldowned.append(pid)
                continue
        healthy.append(pid)

    typer.echo(f"Combo {name!r}: strategy={combo.strategy}, sticky={combo.sticky_limit}")
    for pid in combo.providers:
        if pid in cooldowned:
            h = state.provider_health[pid]
            typer.echo(f"  {pid}: in_cooldown until {h.rate_limited_until}")
        else:
            typer.echo(f"  {pid}: ok")

    if not cooldowned:
        verdict = "all_healthy"
    elif len(cooldowned) == len(combo.providers):
        verdict = "all_in_cooldown"
    else:
        verdict = "partial_cooldown"
    typer.echo(f"verdict: {verdict}")


@combos_cli.command("use")
def combos_use(
    name: str = typer.Argument(...),
    scope: str = typer.Option("project", "--scope", help="project | global"),
) -> None:
    """Set the active combo (used as default when no --combo flag is passed)."""
    from fno.adapters.providers.loader import (
        atomic_mutate_settings,
        load_combos,
    )

    repo_root = _get_repo_root()
    try:
        combos = load_combos(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)

    if name not in combos:
        typer.echo(f"error: combo {name!r} not found", err=True)
        raise typer.Exit(1)

    target = _combos_settings_path(scope)

    def mutator(data: dict) -> dict:
        config_section = data.setdefault("config", {})
        providers_section = config_section.setdefault("providers", {})
        providers_section["active_combo"] = name
        return data

    try:
        atomic_mutate_settings(mutator, settings_path=target)
    except OSError as exc:
        typer.echo(f"error: failed to write settings.yaml: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(f"Active combo set to {name!r} (scope={scope}).")
