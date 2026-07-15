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

from fno.adapters.providers import managed
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
        return _resolve_cwd() / ".fno" / "config.toml"
    else:
        return _resolve_home() / ".fno" / "config.toml"


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
def list_providers(
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit a JSON array of record rows (Connections UI)."
    ),
) -> None:
    """List all configured providers, marking the active one with *."""
    config = _load()
    slot_active: dict[str, Optional[str]] = {}  # per-CLI slot occupant, read once

    def _is_active(record: ProviderRecord) -> bool:
        # For managed accounts the meaningful "active" is which one is
        # materialized in that CLI's slot; fall back to routing-active otherwise.
        if record.auth == "managed":
            if record.cli not in slot_active:
                slot_active[record.cli] = managed.active_slot_id(record.cli)
            return record.id == slot_active[record.cli]
        return record.id == config.active

    if json_output:
        import json as _json

        rows = [
            {
                "id": record.id,
                "name": record.name,
                "cli": record.cli,
                "auth": record.auth,
                "priority": record.priority,
                "active": _is_active(record),
                "headroom": _headroom_label(record.id),
                "snapshot": (
                    managed.snapshot_age_label(record.id)
                    if record.auth == "managed"
                    else None
                ),
            }
            for record in config.records
        ]
        typer.echo(_json.dumps(rows))
        return

    if not config.records:
        typer.echo(
            "No providers configured. Run `fno providers add` to add one."
        )
        return
    for record in config.records:
        marker = "*" if _is_active(record) else " "
        headroom_col = _headroom_label(record.id)
        line = (
            f"{marker} {record.id}  [{record.cli}] {record.auth}  "
            f"priority={record.priority}  headroom={headroom_col}"
        )
        if record.auth == "managed":
            line += f"  snapshot={managed.snapshot_age_label(record.id)}"
        typer.echo(line)


# ---------------------------------------------------------------------------
# usage (quota-aware dispatch, x-5d3e)
# ---------------------------------------------------------------------------


def _headroom_label(provider_id: str) -> str:
    """Compact headroom string for the list column. Fail-open to 'unknown'."""
    try:
        from fno.adapters.providers.runtime_state import headroom

        return headroom(provider_id).state.name.lower()
    except Exception:  # noqa: BLE001 - a display read must never break `list`
        return "unknown"


def _fmt_resets_in(resets_at: float, now: float) -> str:
    """Render a reset epoch as a relative 'in 40m' / 'reset' string."""
    delta = resets_at - now
    if delta <= 0:
        return "reset"
    mins = int(delta // 60)
    if mins < 60:
        return f"in {mins}m"
    hours = mins // 60
    rem = mins % 60
    return f"in {hours}h{rem:02d}m"


@cli.command("usage")
def usage_providers(
    refresh: bool = typer.Option(
        False, "--refresh", help="Force a fresh probe (bypass the TTL cache)."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit a one-line JSON object keyed by provider id."
    ),
) -> None:
    """Show per-provider rate-limit windows: used % and reset time.

    A provider with no fresh snapshot (never probed, probe failed, or CLI
    without a probe) shows ``unknown`` and the command still exits 0 - probing
    is advisory and fail-open (AC1-ERR).
    """
    import json as _json
    import time as _time

    from fno.adapters.providers.loader import load_quota_config
    from fno.adapters.providers.runtime_state import read_usage, refresh_usage

    config = _load()
    now = _time.time()
    ttl = load_quota_config(repo_root=_get_repo_root()).probe_ttl_seconds

    out: dict[str, object] = {}
    for record in config.records:
        if refresh:
            snap = refresh_usage(record.id, ttl_seconds=0, now=now)
        else:
            snap = read_usage(record.id, ttl_seconds=ttl, now=now)
        if snap is None or not snap.windows:
            out[record.id] = "unknown"
        else:
            out[record.id] = {
                "source": snap.source,
                "probed_at": snap.probed_at,
                "windows": [
                    {"label": w.label, "used_pct": w.used_pct, "resets_at": w.resets_at}
                    for w in snap.windows
                ],
            }

    if json_output:
        typer.echo(_json.dumps(out))
        return

    if not config.records:
        typer.echo("No providers configured.")
        return
    for record in config.records:
        entry = out[record.id]
        if entry == "unknown":
            typer.echo(f"{record.id}  [{record.cli}]  unknown")
            continue
        assert isinstance(entry, dict)
        for w in entry["windows"]:  # type: ignore[index]
            typer.echo(
                f"{record.id}  [{record.cli}]  {w['label']:<8} "
                f"{w['used_pct']:5.1f}%  {_fmt_resets_in(w['resets_at'], now)}"
            )


# ---------------------------------------------------------------------------
# required-bot-check (quota-aware dispatch, x-5d3e US5)
# ---------------------------------------------------------------------------

# Map a required-bot GitHub login (substring match) to the provider CLI kind
# that backs it, so we can look up that provider's headroom. Advisory only: an
# unmappable bot is silently skipped (fail-open).
_REQUIRED_BOT_CLI: dict[str, str] = {
    "codex": "codex",   # chatgpt-codex-connector
    "gemini": "gemini",  # gemini-code-assist
    "claude": "claude",
}


def _bot_provider_cli(bot: str) -> Optional[str]:
    low = bot.lower()
    for needle, cli_kind in _REQUIRED_BOT_CLI.items():
        if needle in low:
            return cli_kind
    return None


def required_bot_headroom_check() -> list[dict]:
    """Return one dict per required-bot whose provider is EXHAUSTED.

    Read-only + fail-open (x-5d3e US5): reads the CACHED snapshot only (never
    probes at promise time), unions config.review.github_apps + required_bots,
    maps each bot to a provider record via its CLI kind, and reports those whose
    headroom is EXHAUSTED. Any read failure yields an empty list - a telemetry
    read must never block a promise. Each returned dict emits one
    ``quota_required_bot_exhausted`` decision event as a side effect (AC3-HP).
    """
    try:
        from fno.adapters.providers.runtime_state import HeadroomState, headroom
        from fno.config import load_settings

        settings = load_settings()
        review = settings.review
        bots = list(dict.fromkeys([*(review.github_apps or []), *(review.required_bots or [])]))
        if not bots:
            return []
        config = load_providers(repo_root=_get_repo_root())
        by_cli: dict[str, list[str]] = {}
        for rec in config.records:
            by_cli.setdefault(rec.cli, []).append(rec.id)
    except Exception:  # noqa: BLE001 - a promise-time read must never raise
        return []

    warnings: list[dict] = []
    for bot in bots:
        cli_kind = _bot_provider_cli(bot)
        if cli_kind is None:
            continue
        provider_ids = by_cli.get(cli_kind, [])
        if not provider_ids:
            continue
        # Warn only when EVERY provider of the kind is exhausted: one healthy
        # account (multi-account) keeps the gate un-wedged, so warning on a
        # single exhausted account would be a false positive. Mirrors the
        # lane-routing rule (a kind is exhausted only if all its records are).
        exhausted: list[tuple[str, Optional[float]]] = []
        for provider_id in provider_ids:
            try:
                h = headroom(provider_id)
            except Exception:  # noqa: BLE001
                # An unreadable provider is UNKNOWN, not exhausted - it keeps
                # the kind from counting as fully-exhausted (fail-open).
                exhausted = []
                break
            if h.state is HeadroomState.EXHAUSTED:
                exhausted.append((provider_id, h.resets_at))
            else:
                exhausted = []
                break
        if exhausted and len(exhausted) == len(provider_ids):
            provider_id, retry_at = exhausted[0]
            warnings.append({"bot": bot, "provider": provider_id, "retry_at": retry_at})
            _emit_required_bot_exhausted(bot, provider_id, retry_at)
    return warnings


def _emit_required_bot_exhausted(bot: str, provider: str, retry_at: Optional[float]) -> None:
    """Emit the single decision event for a required-bot exhaustion. Non-fatal."""
    try:
        from fno.events import _build, append_event

        data: dict = {"bot": bot, "provider": provider, "headroom": "exhausted"}
        if retry_at is not None:
            data["retry_at"] = retry_at
        append_event(_build("quota_required_bot_exhausted", "target", data))
    except Exception:  # noqa: BLE001
        pass


@cli.command("required-bot-check")
def required_bot_check_cmd(
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit a JSON list of exhausted required-bot providers."
    ),
) -> None:
    """Warn (pre-promise) if a required review bot's provider is out of quota.

    Read-only, advisory, exit 0 always: names each exhausted bot/provider and
    the reset time so a coming review-gate wedge surfaces now instead of hours
    later. Prints nothing when every required bot has headroom (or none are
    configured). The attended <help> tag is raised by the target skill.
    """
    import json as _json

    warnings = required_bot_headroom_check()
    if json_output:
        typer.echo(_json.dumps(warnings))
        return
    if not warnings:
        return
    now = _resolve_time()
    for w in warnings:
        reset = _fmt_resets_in(w["retry_at"], now) if w.get("retry_at") else "unknown"
        typer.echo(
            f"required-bot {w['bot']}: provider {w['provider']} EXHAUSTED, "
            f"review gate will wedge until reset ({reset})"
        )


def _resolve_time() -> float:
    import time as _time

    return _time.time()


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
# register (managed credential store, US1)
# ---------------------------------------------------------------------------

@cli.command("register")
def register_provider(
    provider_id: str = typer.Argument(..., help="Unique account id (lowercase alphanumeric + hyphens)"),
    cli_name: str = typer.Option("claude", "--cli", "-c", help="claude|codex"),
    priority: int = typer.Option(100, "--priority", "-p"),
    name: Optional[str] = typer.Option(None, "--name"),
    scope: str = typer.Option("global", "--scope", "-s", help="global|project"),
) -> None:
    """Snapshot the CURRENT login into a managed account store and record it.

    Sign into the account you want to register (`claude /login`), then run this.
    It captures the live credential (Keychain blob on darwin, credential file
    elsewhere; codex `auth.json`) into ``~/.fno/providers/<id>/`` and writes an
    ``auth: managed`` ProviderRecord. Idempotent: re-running refreshes the
    snapshot for an already-registered account (US1).
    """
    try:
        record = ProviderRecord(
            id=provider_id,
            name=name or provider_id,
            cli=cli_name,  # type: ignore[arg-type]
            auth="managed",
            priority=priority,
        )
    except Exception as exc:  # noqa: BLE001 - surface pydantic validation receipts
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)

    # Snapshot the current login FIRST - refuse cleanly if nothing to capture.
    try:
        adir = managed.snapshot_current(record)
    except managed.ManagedStoreError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(1)

    repo_root = _get_repo_root()
    try:
        config = load_providers(repo_root=repo_root)
    except ProviderConfigError as exc:
        typer.echo(f"error loading existing config: {exc}", err=True)
        raise typer.Exit(1)

    from fno.adapters.providers.model import ProvidersConfig

    new_records = [r for r in config.records if r.id != record.id]
    new_records.append(record)
    try:
        save_providers(ProvidersConfig(records=new_records, active=config.active), scope=scope)  # type: ignore[arg-type]
    except OSError as exc:
        typer.echo(f"error: failed to write config: {exc}", err=True)
        raise typer.Exit(1)

    # The captured login IS what currently sits in this CLI's slot: stamp it
    # active so the next switch captures-before-overwrite the right account. A
    # stamp write failure is non-fatal (the record is saved) but must be loud,
    # not a raw traceback - it degrades to no active-marker + no first capture.
    try:
        managed.stamp_active_slot(record.cli, record.id)
    except OSError as exc:
        typer.echo(f"warning: registered but could not stamp active slot: {exc}", err=True)

    typer.echo(f"Registered managed account '{record.id}' (snapshot at {adir}, scope={scope}).")


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

    record = config.by_id[provider_id]

    # Managed records materialize the account's credentials into the shared
    # slot (capture-before-overwrite + live-pin gate); oauth_dir/api_key records
    # only re-point active routing (spawns pick up the env), as before.
    if record.auth == "managed":
        from fno.agents.events import emit as _emit

        try:
            result = managed.switch(record, by_id=config.by_id, emit_fn=_emit)
        except managed.SwitchDeferred as exc:
            typer.echo(f"switch deferred: {exc}", err=True)
            raise typer.Exit(2)
        except managed.ManagedStoreError as exc:
            typer.echo(f"switch failed: {exc}", err=True)
            raise typer.Exit(1)
        except KeyboardInterrupt as exc:
            for note in getattr(exc, "__notes__", ()):
                typer.echo(f"switch interrupted: {note}", err=True)
            raise
        if record.cli == "codex":
            if not result.slot_changed:
                typer.echo(
                    f"Managed Codex account '{result.active}' is already materialized "
                    f"(structural; {result.reason})."
                )
            elif result.verification == "codex-recognized":
                typer.echo(
                    f"Materialized managed Codex account '{result.active}' into the slot; "
                    "Codex recognized the local auth."
                )
            else:
                typer.echo(
                    f"Materialized managed Codex account '{result.active}' into the slot "
                    f"(structural fallback: {result.reason})."
                )
        else:
            typer.echo(f"Materialized managed account '{result.active}' into the slot (verified).")

    from fno.adapters.providers.model import ProvidersConfig
    new_config = ProvidersConfig(records=config.records, active=provider_id)

    try:
        save_providers(new_config, scope=scope)  # type: ignore[arg-type]
    except OSError as exc:
        # For a managed record the slot + its per-CLI stamp are already switched
        # (that IS the operative state managed dispatch reads); only the routing
        # pointer failed to persist. Say so, so the user retries the save rather
        # than believing the switch did not happen.
        if record.auth == "managed":
            typer.echo(
                f"warning: slot switched to '{provider_id}' but saving the active "
                f"routing pointer failed ({exc}); re-run `fno providers use "
                f"{provider_id}` to persist it.",
                err=True,
            )
        else:
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


benchmarks_cli = typer.Typer(
    name="benchmarks",
    help="Cache + view the OpenRouter coding benchmark snapshot (routing source of truth).",
    no_args_is_help=True,
)
cli.add_typer(benchmarks_cli, name="benchmarks")


@benchmarks_cli.command("refresh")
def benchmarks_refresh() -> None:
    """Fetch the OpenRouter coding benchmarks and cache them atomically.

    Fails loud (non-zero) on any network/auth error, never leaving a truncated
    snapshot. Meant to run manually at a fortnightly cadence; dispatch-time tier
    resolution reads the cache, never the network.
    """
    from fno.adapters.providers.benchmarks import BenchmarkError, refresh
    from fno.paths import benchmarks_json

    try:
        snapshot = refresh()
    except BenchmarkError as exc:
        typer.echo(f"benchmarks refresh: {exc}", err=True)
        raise typer.Exit(code=1)
    typer.echo(
        f"refreshed {len(snapshot['models'])} models -> {benchmarks_json()}"
    )


@benchmarks_cli.command("show")
def benchmarks_show() -> None:
    """Render the cached benchmark snapshot; warn once if it is stale (>14 days)."""
    from fno.adapters.providers.benchmarks import (
        is_stale,
        load_snapshot,
        reachable,
        staleness_seconds,
    )

    snapshot = load_snapshot()
    if snapshot is None:
        typer.echo(
            "no benchmark snapshot; run `fno providers benchmarks refresh`",
            err=True,
        )
        raise typer.Exit(code=1)
    if is_stale(snapshot):
        age = staleness_seconds(snapshot) or 0
        typer.echo(
            f"WARNING: benchmark snapshot is {int(age // 86400)} days old (>14); "
            "run `fno providers benchmarks refresh`",
            err=True,
        )
    typer.echo(f"source: {snapshot.get('source')}  fetched_at: {snapshot.get('fetched_at')}")
    for row in snapshot.get("models", []):
        name = row.get("name", "?")
        pct = row.get("coding_percentile")
        mapped = reachable(name)
        route = f"{mapped[0]}:{mapped[1]}" if mapped else "unmapped (not routable)"
        typer.echo(f"  {name:<24} coding={pct!s:<6} -> {route}")


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
        providers_section = data.setdefault("providers", {})
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

    from fno.adapters.providers.loader import load_active_combo, load_combos
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

    active_combo = load_active_combo(repo_root=repo_root)
    rows = []
    for name, combo in combos.items():
        providers_hash = compute_providers_hash(combo.providers)
        cursor = read_cursor(name, providers_hash)
        rows.append({
            "name": name,
            "strategy": combo.strategy,
            "sticky_limit": combo.sticky_limit,
            "members": list(combo.providers),
            "active": name == active_combo,
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
        providers_section = data.get("providers", {})
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
        providers_section = data.setdefault("providers", {})
        providers_section["active_combo"] = name
        return data

    try:
        atomic_mutate_settings(mutator, settings_path=target)
    except OSError as exc:
        typer.echo(f"error: failed to write settings.yaml: {exc}", err=True)
        raise typer.Exit(1)

    typer.echo(f"Active combo set to {name!r} (scope={scope}).")


@combos_cli.command("update")
def combos_update(
    name: str = typer.Argument(..., help="Existing combo to update (must exist)"),
    strategy: Optional[str] = typer.Option(
        None, "--strategy", help="Rotation strategy: fallback | round_robin (unset = keep current)"
    ),
    sticky_limit: Optional[int] = typer.Option(
        None, "--sticky", help="round_robin calls per provider (unset = keep current)"
    ),
    providers_csv: str = typer.Option(
        ..., "--providers", help="Comma-separated provider IDs (the new ordered members)"
    ),
    scope: str = typer.Option("project", "--scope", help="project | global"),
) -> None:
    """Atomically replace an existing combo's members/strategy in one write.

    The atomic alternative to remove+add: a crash mid-pair could strand the
    config with no combo. Validates each member exists and refuses an unknown
    combo. Reordering members changes compute_providers_hash, so the stored
    round-robin cursor is invalidated (read_cursor returns None on the new hash)
    without any explicit reset here.

    ``--strategy``/``--sticky`` default to the combo's CURRENT values when
    omitted, so a pure reorder (the UI's common case) never silently rewrites a
    round_robin combo to fallback/1.
    """
    from fno.adapters.providers.loader import atomic_mutate_settings
    from fno.adapters.providers.rotation import Combo

    providers_list = [p.strip() for p in providers_csv.split(",") if p.strip()]
    if not providers_list:
        typer.echo("error: --providers must list at least one provider id", err=True)
        raise typer.Exit(1)

    config = _load()
    unknown = [p for p in providers_list if p not in config.by_id]
    if unknown:
        typer.echo(
            f"error: combo {name!r} references unknown provider id(s) "
            f"{unknown!r} (not in config.providers.records)",
            err=True,
        )
        raise typer.Exit(1)

    target = _combos_settings_path(scope)
    applied: dict[str, object] = {}

    def mutator(data: dict) -> dict:
        providers_section = data.setdefault("providers", {})
        combos_section = providers_section.setdefault("combos", {})
        existing = combos_section.get(name)
        if existing is None:
            raise ValueError(
                f"combo {name!r} not found in {scope} settings; "
                "use `combos add` to create it"
            )
        # Inherit the current strategy/sticky when the flag is omitted, so a
        # reorder preserves round_robin/sticky instead of defaulting them away.
        eff_strategy = strategy if strategy is not None else existing.get("strategy", "fallback")
        eff_sticky = (
            sticky_limit if sticky_limit is not None else existing.get("sticky_limit", 1)
        )
        # Validate the resolved combo (raises ValueError, aborting the write).
        Combo(
            name=name,
            strategy=eff_strategy,  # type: ignore[arg-type]
            sticky_limit=eff_sticky,
            providers=tuple(providers_list),
        )
        combos_section[name] = {
            "strategy": eff_strategy,
            "sticky_limit": eff_sticky,
            "providers": providers_list,
        }
        applied.update(strategy=eff_strategy, sticky_limit=eff_sticky)
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
        f"Combo {name!r} updated (strategy={applied['strategy']}, "
        f"sticky={applied['sticky_limit']}, providers={providers_list}, scope={scope})."
    )
