"""Per-spawn account overlay resolution (x-d012).

`fno agents spawn --account <id>` pins one worker to one claude account
*without* touching the daemon-wide active `~/.claude` slot. This module resolves
an `--account` id to an env overlay merged into the child env at the same seam
`route_env` already uses (mux_spawn `_mesh_env_wrapper` for pane,
providers/claude.py `bg_create` for bg/headless).

**The working mechanism is a per-account `CLAUDE_CONFIG_DIR`** (verified
2026-07-15): a full second login in its own dir (e.g. `~/.claude-alt`, sharing
`projects/`/`plugins/`/`settings.json` with `~/.claude` via symlinks) bills the
right account. This REVERSES the design's original Locked Decision 0: the
`claude setup-token` + `CLAUDE_CODE_OAUTH_TOKEN` env lane authenticates but
BILLS THE WRONG ACCOUNT, so it is deliberately NOT built - a managed account
that is not the active slot occupant is refused with a pointer to config-dir
registration rather than a silent wrong-billing spawn.

The lanes:

    lane 1  own-dir       auth: oauth_dir             {CLAUDE_CONFIG_DIR: <root>/<id>/.claude}
    lane 2  config-dir    record.config_dir set       {CLAUDE_CONFIG_DIR: <config_dir>}   (PRIMARY)
    lane 3  managed, active                           {}   (rides the shared slot, bills correctly)
    api_key  claude api_key record                    resolved env refs

Refusals (never a silent mis-pin/mis-bill): unknown id, non-claude record, a
managed account that is not the active slot occupant (needs its own config_dir),
and a lane-1/2 config dir that is missing or holds no login (preflight before
spawn - no zombie worker).

Explicit operator intent ONLY: this never participates in dispatch defaults,
failover, or exhaustion auto-switch (x-d6be lock).
"""
from __future__ import annotations

import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from fno.adapters.providers.dispatch import _env_for_api_key, _env_for_oauth
from fno.adapters.providers.loader import load_providers
from fno.adapters.providers.model import ProviderUnavailableError
from fno.adapters.providers.staging import _default_providers_root, verify_staged


class AccountResolutionError(ValueError):
    """`--account` could not be resolved to a safe overlay; message is a receipt."""


@dataclass(frozen=True)
class AccountOverlay:
    """The resolved env overlay for one `--account` spawn."""

    account_id: str
    env: dict[str, str]
    lane: str  # own-dir | config-dir | managed-active | api-key


def _login_present(config_dir: Path) -> bool:
    """True when ``config_dir`` holds a login specific to THIS dir.

    Checks the darwin Keychain item SCOPED to config_dir (never the unscoped
    fallback - that belongs to whatever the default ~/.claude slot holds, so
    using it would pass preflight for any dir on a machine that has any login)
    plus the on-disk ``.credentials.json`` / ``.claude.json`` under the dir. An
    expired-but-present credential passes (preflight catches *missing*, not
    *stale*).
    """
    if (config_dir / ".credentials.json").exists() or (
        config_dir / ".claude.json"
    ).exists():
        return True
    if sys.platform != "darwin":
        return False
    from fno.adapters.providers.managed import (
        _claude_keychain_account,
        _claude_scoped_service,
        _run_security,
    )

    out = _run_security(
        [
            "find-generic-password",
            "-s",
            _claude_scoped_service(config_dir),
            "-a",
            _claude_keychain_account(),
            "-w",
        ]
    )
    return out.returncode == 0 and bool(out.stdout.strip())


def resolve_account_overlay(
    account_id: str,
    *,
    repo_root: Path | None = None,
    providers_root: Path | None = None,
) -> AccountOverlay:
    """Resolve ``account_id`` to an env overlay, or raise AccountResolutionError.

    Pure with respect to the slot and the active stamp: it only READS the
    active-slot id to distinguish the ride-the-slot case; it never writes.
    """
    if providers_root is None:
        providers_root = _default_providers_root()
    config = load_providers(repo_root=repo_root)
    by_id = config.by_id

    record = by_id.get(account_id)
    if record is None:
        claude_ids = sorted(r.id for r in config.records if r.cli == "claude")
        listing = ", ".join(claude_ids) or "(none registered)"
        raise AccountResolutionError(
            f"account {account_id!r} is not a registered provider. "
            f"claude accounts: {listing}"
        )

    if record.cli != "claude":
        raise AccountResolutionError(
            f"account {account_id!r} is a {record.cli}/{record.auth} record; "
            "--account is claude-only (codex has its own CODEX_HOME slot)"
        )

    # Lane 2 (PRIMARY): an explicit config_dir - the verified-correct mechanism.
    # Wins over everything so a converged account always rides its own dir.
    if record.config_dir is not None:
        cfg = record.config_dir
        if not cfg.exists():
            raise AccountResolutionError(
                f"account {account_id!r} config_dir {cfg} does not exist; "
                "register a live login there first"
            )
        if not _login_present(cfg):
            raise AccountResolutionError(
                f"account {account_id!r} config_dir {cfg} holds no claude login "
                f"(run: CLAUDE_CONFIG_DIR={cfg} claude /login)"
            )
        return AccountOverlay(account_id, {"CLAUDE_CONFIG_DIR": str(cfg)}, "config-dir")

    if record.auth == "oauth_dir":
        if not verify_staged(record, root=providers_root):
            raise AccountResolutionError(
                f"account {account_id!r} is not staged; run `fno providers "
                "register`/stage before spawning against it"
            )
        return AccountOverlay(account_id, _env_for_oauth(record, providers_root), "own-dir")

    if record.auth == "managed":
        from fno.adapters.providers.managed import active_slot_id

        active = active_slot_id("claude", providers_root)
        if account_id == active or record.account_id == active:
            # Lane 3: the account IS the active slot occupant; the worker rides
            # the shared ~/.claude slot (correct billing) and extends the
            # live-pin. Pin CLAUDE_CONFIG_DIR to the canonical slot rather than
            # returning {} - an empty overlay would let a stale parent
            # CLAUDE_CONFIG_DIR (e.g. exported from a prior alt-account session)
            # leak through and silently bill the wrong account. Managed claude
            # accounts materialize into ~/.claude by definition.
            slot = str(Path.home() / ".claude")
            return AccountOverlay(account_id, {"CLAUDE_CONFIG_DIR": slot}, "managed-active")

        # A managed account that is NOT the active slot occupant has no correct
        # env overlay: a setup-token injected via CLAUDE_CODE_OAUTH_TOKEN
        # authenticates but BILLS THE WRONG ACCOUNT (verified 2026-07-15). The
        # correct per-spawn mechanism for a non-active account is its OWN config
        # dir. Refuse and point there rather than ship a silent wrong-billing
        # spawn.
        raise AccountResolutionError(
            f"account {account_id!r} is managed and not the active ~/.claude "
            f"account (active: {active or 'none'}). Per-spawn selection for a "
            "non-active account needs its own config dir: register it with a "
            f"config_dir (e.g. --config-dir ~/.claude-{account_id}, a full "
            "second login) so the worker gets CLAUDE_CONFIG_DIR. The setup-token "
            "env lane bills the wrong account and is deliberately not used.\n"
            f"  or make it active:  fno providers use {account_id}  (daemon-wide)"
        )

    # api_key claude record: resolve its env refs (e.g. a routed ANTHROPIC_*).
    try:
        overlay = _env_for_api_key(record)
    except ProviderUnavailableError as exc:
        raise AccountResolutionError(
            f"account {account_id!r} env is unresolvable: {exc}"
        ) from exc
    return AccountOverlay(account_id, overlay, "api-key")


def resolve_account_overlay_or_exit(
    account_id: Optional[str],
) -> Optional[AccountOverlay]:
    """CLI wrapper: None passes through; a refusal prints the receipt and exits 2.

    Mirrors the --route fail-closed posture: a refusal spawns nothing, acquires
    no gate slot, and leaves the node dispatchable.
    """
    if account_id is None:
        return None
    import typer

    try:
        return resolve_account_overlay(account_id)
    except AccountResolutionError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=2) from exc
