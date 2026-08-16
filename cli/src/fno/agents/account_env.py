"""Per-spawn account overlay resolution (x-d012).

`fno agents spawn --account <id>` pins one worker to one claude account
*without* touching the daemon-wide active `~/.claude` slot. This module resolves
an `--account` id to an env overlay merged into the child env at the same seam
`route_env` already uses (mux_spawn `_mesh_env_wrapper` for pane,
harnesses/claude.py `bg_create` for bg/headless).

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
    lane 3  managed, active                           {CLAUDE_CONFIG_DIR: ~/.claude}   (rides the shared slot)
    api_key  claude api_key record                    resolved env refs

Every lane also scrubs inherited auth vars (SCRUB_AUTH_VARS) at the substrate
apply site, so an ambient ANTHROPIC_API_KEY / CLAUDE_CODE_OAUTH_TOKEN can't
override the pinned account and bill it wrong.

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
from typing import Mapping, Optional

from fno.adapters.providers.dispatch import _env_for_api_key, _env_for_oauth
from fno.adapters.providers.loader import load_providers
from fno.adapters.providers.model import ProviderUnavailableError
from fno.adapters.providers.staging import _default_providers_root, verify_staged
from fno.agents.model_routing import MODEL_ENV_KEYS


class AccountResolutionError(ValueError):
    """`--account` could not be resolved to a safe overlay; message is a receipt."""


# Inherited auth vars that would override an account's own login and bill the
# wrong account (a parent shell or routed worker may export any of them). Every
# --account spawn scrubs these at the substrate apply site before layering the
# overlay, so the pinned account's credential is the only live one (its overlay
# re-sets any it needs). Public so the three substrate seams share one list.
#
# The tier-alias remaps belong here for the same reason the endpoint does:
# endpoint, auth, and model are ONE provider route (the invariant
# resolve_spawn_route refuses to half-compose). Scrubbing the endpoint but
# leaving ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.2[1m] behind sends the pinned
# account a foreign vendor's model id -- the worker authenticates against
# Anthropic and then asks it for a GLM model, which 404s on the first turn
# after the spawn receipt has already printed "live". The overlay is applied
# AFTER this scrub, so an account record that legitimately pins a model still
# wins.
SCRUB_AUTH_VARS = (
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    # Reused, not restated: the routed-model set is the same set a route
    # composes, so a new Claude tier lands in both at once.
    *MODEL_ENV_KEYS,
)

# The subset of a route whose VALUE is a credential. Deliberately NOT
# SCRUB_AUTH_VARS: that set answers "clear these so the route wins" and so it
# also names the endpoint and every model key, none of which is a secret. This
# set answers a different question -- "which values must never be written where
# a reader other than the child process can see them" -- and its one caller is
# the happy pane argv build, where a route key becomes a world-readable `ps`
# token. Keeping the two adjacent is the point: a future credential var has to
# be added to both, and the comments say why they differ.
SECRET_ROUTE_VARS = (
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
)


@dataclass(frozen=True)
class AccountOverlay:
    """The resolved env overlay for one `--account` spawn."""

    account_id: str
    env: dict[str, str]
    lane: str  # own-dir | config-dir | managed-active | api-key


@dataclass(frozen=True)
class CredentialDecision:
    """What fno made live for one worker, read off the composed env.

    Facts fno owns, never claims about what claude honored: composition is
    decided before the process starts, so the composed environment is a fact,
    but only claude knows which credential it sent (the x-74ea boundary).
    """

    profile: Optional[str]  # account id whose CLAUDE_CONFIG_DIR is set
    config_dir: Optional[str]
    auth: str  # "route:<vendor>" | "account:<id>" | "ambient"
    endpoint: str  # the base url, or "anthropic-default"
    model: Optional[str]
    bills: str  # vendor | account id | "unknown"


def _vendor_for_endpoint(endpoint: str) -> Optional[str]:
    """The provider name that owns ``endpoint``, or its URL host.

    Derived from the composed env, never from a flag: the receipt's honesty
    depends on the vendor label traveling with the endpoint value actually
    applied, not with the spelling the caller typed.
    """
    from urllib.parse import urlparse

    from fno.agents.model_routing import effective_providers, _routing_block

    for name, prov in effective_providers(_routing_block(None)).items():
        if prov.get("base_url") == endpoint:
            return name
    host = urlparse(endpoint).hostname if "://" in endpoint else None
    return host


def _credential_decision(
    composed: Mapping[str, str],
    *,
    account_id: Optional[str] = None,
    route_env: Optional[Mapping[str, str]] = None,
) -> CredentialDecision:
    """Derive the receipt facts from one composed environment."""
    endpoint = composed.get("ANTHROPIC_BASE_URL") or "anthropic-default"
    vendor = _vendor_for_endpoint(endpoint) if endpoint != "anthropic-default" else None
    model = composed.get("ANTHROPIC_MODEL")
    if model is None:
        model = next(
            (composed[k] for k in MODEL_ENV_KEYS if composed.get(k)), None
        )
    route_self_authed = bool(
        route_env
        and (route_env.get("ANTHROPIC_AUTH_TOKEN") or route_env.get("ANTHROPIC_API_KEY"))
    )
    if route_self_authed:
        named = vendor or "unknown"
        return CredentialDecision(
            profile=account_id,
            config_dir=composed.get("CLAUDE_CONFIG_DIR"),
            auth=f"route:{named}",
            endpoint=endpoint,
            model=model,
            bills=named,
        )
    if account_id is not None:
        # No route credential: the account's own login (Keychain OAuth for the
        # config-dir/managed lanes, the record's key for api_key) is what
        # authenticates, and an api_key record's endpoint names its vendor.
        payer = vendor or account_id
        return CredentialDecision(
            profile=account_id,
            config_dir=composed.get("CLAUDE_CONFIG_DIR"),
            auth=f"account:{account_id}",
            endpoint=endpoint,
            model=model,
            bills=payer,
        )
    return CredentialDecision(
        profile=None,
        config_dir=composed.get("CLAUDE_CONFIG_DIR"),
        auth="ambient",
        endpoint=endpoint,
        model=model,
        bills="unknown",
    )


def compose_worker_credentials(
    account_env: Optional[Mapping[str, str]],
    route_env: Optional[Mapping[str, str]],
    base: Mapping[str, str],
    *,
    account_id: Optional[str] = None,
) -> "tuple[dict[str, str], CredentialDecision]":
    """Apply the scrub/account/route precedence once and report the outcome.

    THE composition rule, stated here and nowhere else (x-8552): scrub every
    ``SCRUB_AUTH_VARS`` entry from the inherited environment, layer the account
    overlay (profile + its own login), layer the route overlay last so it wins
    ``ANTHROPIC_BASE_URL``/``ANTHROPIC_AUTH_TOKEN`` and every model tier as one
    unit. The account's login is dormant for model traffic under a route and
    still supplies the profile (measured 2026-08-15: claude prefers an env
    credential over a Keychain login).

    Every spawn seam (bg, headless, pane) consumes this one function, so a
    fourth seam cannot hand-roll a disagreeing order - the drift that let the
    settings branch drop the account's env while the env paths kept it.
    """
    composed = dict(base)
    for _var in SCRUB_AUTH_VARS:
        composed.pop(_var, None)
    if account_env:
        composed.update(account_env)
    if route_env:
        composed.update(route_env)
        # A self-authed route owns the credential slot outright: any account
        # credential the route does not name is dropped, else an api-key
        # account's vendor key rides the child env against the route's
        # endpoint - a foreign secret that may even win the auth precedence.
        if route_env.get("ANTHROPIC_AUTH_TOKEN") or route_env.get("ANTHROPIC_API_KEY"):
            for _var in SECRET_ROUTE_VARS:
                if _var not in route_env:
                    composed.pop(_var, None)
    return composed, _credential_decision(
        composed, account_id=account_id, route_env=route_env
    )


def _login_present(config_dir: Path) -> bool:
    """True when ``config_dir`` holds a login CREDENTIAL specific to THIS dir.

    Checks the darwin Keychain item SCOPED to config_dir (never the unscoped
    fallback - that belongs to whatever the default ~/.claude slot holds, so
    using it would pass preflight for any dir on a machine that has any login)
    and the on-disk ``.credentials.json`` (the actual auth secret elsewhere).
    Deliberately does NOT accept ``.claude.json``: that is settings/account
    metadata, present in a logged-OUT dir too, so treating it as a login would
    let a credentialless dir spawn an auth-prompt zombie. An expired-but-present
    credential passes (preflight catches *missing*, not *stale*).
    """
    if (config_dir / ".credentials.json").exists():
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
        claude_ids = sorted(r.id for r in config.records if r.harness == "claude")
        listing = ", ".join(claude_ids) or "(none registered)"
        raise AccountResolutionError(
            f"account {account_id!r} is not registered. "
            f"claude accounts: {listing}"
        )

    if record.harness != "claude":
        raise AccountResolutionError(
            f"account {account_id!r} is a {record.harness}/{record.auth} record; "
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
        return AccountOverlay(
            account_id, {"CLAUDE_CONFIG_DIR": str(cfg)}, "config-dir"
        )

    if record.auth == "oauth_dir":
        if not verify_staged(record, root=providers_root):
            raise AccountResolutionError(
                f"account {account_id!r} is not staged; run `fno config accounts "
                "register`/stage before spawning against it"
            )
        overlay = _env_for_oauth(record, providers_root)
        # Preflight the STAGED dir for a login too: verify_staged only checks the
        # symlink+target exist, so a staged-but-logged-out dir would otherwise
        # spawn an auth-prompt zombie (the same check the config-dir lane runs).
        staged = overlay.get("CLAUDE_CONFIG_DIR")
        if staged and not _login_present(Path(staged)):
            raise AccountResolutionError(
                f"account {account_id!r} staged dir {staged} holds no claude login "
                f"(run: CLAUDE_CONFIG_DIR={staged} claude /login)"
            )
        return AccountOverlay(account_id, overlay, "own-dir")

    if record.auth == "managed":
        from fno.adapters.providers.managed import active_slot_id

        active = active_slot_id("claude", providers_root)
        # Compare the REQUESTED id against the active slot id only. record.account_id
        # is configurable metadata (defaults to id); comparing it here would treat
        # a non-active record whose account_id happens to equal the active id as
        # active and bill the wrong account.
        if account_id == active:
            # Lane 3: the account IS the active slot occupant; the worker rides
            # the shared ~/.claude slot (correct billing) and extends the
            # live-pin. Pin CLAUDE_CONFIG_DIR to the canonical slot rather than
            # returning {} - an empty overlay would let a stale parent
            # CLAUDE_CONFIG_DIR (e.g. exported from a prior alt-account session)
            # leak through and silently bill the wrong account. Managed claude
            # accounts materialize into ~/.claude by definition.
            slot = str(Path.home() / ".claude")
            return AccountOverlay(
                account_id, {"CLAUDE_CONFIG_DIR": slot}, "managed-active"
            )

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
            f"  or make it active:  fno config accounts use {account_id}  (daemon-wide)"
        )

    # api_key claude record: resolve its env refs (e.g. a routed ANTHROPIC_*).
    # Scrub the standard auth vars too, then apply - so an inherited token of a
    # DIFFERENT kind than the record provides can't override the record's creds.
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
