"""``fno route``: legible, on-the-fly provider route lanes.

Four verbs over the existing per-spawn model-routing machinery
(``fno.agents.model_routing``), which stays the single source of the z.ai
env-var contract:

- ``ls``    - the effective merged table (built-ins + config), one row per role.
- ``set``   - route a lane to ``provider,model`` (atomic config write).
- ``unset`` - revert a lane (to its built-in default, or unrouted).
- ``env``   - eval-able ``export`` block for an interactive session.

``set``/``unset`` delegate to the existing atomic, file-locked ``fno config
set``/``unset`` write path - there is no second config writer here. The roles
map is a dict-leaf (REPLACE semantics), so a per-lane change is a
read-merge-write of the SCOPE's own roles map (last-writer-wins under the config
file lock; accepted).
"""
from __future__ import annotations

import json
import shlex
import sys

import typer

route_app = typer.Typer(
    name="route",
    help="Provider route lanes: ls (effective table) / set / unset / env.",
    no_args_is_help=True,
)


def _block() -> object:
    from fno.config import load_settings

    return load_settings().model_routing


@route_app.command("ls")
def ls_cmd(
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the table as JSON instead of text."
    ),
) -> None:
    """Render the effective routing table.

    One row per role: role -> target (provider,model) -> protocol -> key status
    (which env var / file satisfied it, or MISSING) -> auto-assigned-by. Built-in
    roles, config overrides, the known ``build`` lane, and the protected roles all
    appear. Degrades an unreadable key source to MISSING rather than erroring.
    """
    from fno.agents.model_routing import build_route_table

    rows = build_route_table()
    if json_output:
        typer.echo(json.dumps(rows, indent=2))
        return

    cols = ("role", "target", "protocol", "key", "assigned_by")
    header = {
        "role": "ROLE",
        "target": "TARGET",
        "protocol": "PROTOCOL",
        "key": "KEY",
        "assigned_by": "ASSIGNED-BY",
    }
    widths = {
        c: max(len(header[c]), *(len(r[c]) for r in rows)) if rows else len(header[c])
        for c in cols
    }

    def _fmt(r: dict[str, str]) -> str:
        return "  ".join(r[c].ljust(widths[c]) for c in cols).rstrip()

    typer.echo(_fmt(header))
    for r in rows:
        typer.echo(_fmt(r))


@route_app.command("set")
def set_cmd(
    role: str = typer.Argument(..., help="Lane/role name, e.g. build."),
    target: str = typer.Argument(
        ..., help="provider,model - e.g. zai,glm-5.2 or zai,glm-5.2[1m]."
    ),
    local: bool = typer.Option(
        False,
        "--local/--global",
        "-l/-g",
        help="Write the project-local config.toml instead of the per-user "
        "global one (default global; routing is operator-level).",
    ),
) -> None:
    """Route a lane to ``provider,model`` (atomic, schema-validated).

    Refuses a protected role name and a provider absent from the effective
    providers map BEFORE any write. A protocol mismatch (anthropic lane pointing
    at an openai provider) warns but writes (resolve_route degrades safely).
    """
    from fno.agents.model_routing import (
        PROTECTED_ROLES,
        _parse_target,
        effective_providers,
    )
    from fno.config.writer import ConfigSetError, read_scope_value, set_config_values

    scope = "project" if local else "global"
    name = role.strip().lower()

    if name in PROTECTED_ROLES:
        typer.echo(
            f"error: {name!r} is a protected role (never routable via the roles "
            "table); refusing. Config unchanged.",
            err=True,
        )
        raise typer.Exit(2)

    parsed = _parse_target(target)
    if parsed is None:
        typer.echo(
            f"error: target must be 'provider,model' with a non-empty model "
            f"token; got {target!r}. Config unchanged.",
            err=True,
        )
        raise typer.Exit(2)
    pname, model = parsed

    providers = effective_providers(_block())
    if pname not in providers:
        typer.echo(
            f"error: unknown provider {pname!r}; known: "
            f"{', '.join(sorted(providers))}. Config unchanged.",
            err=True,
        )
        raise typer.Exit(2)

    protocol = (providers[pname].get("protocol") or "anthropic").lower()
    if protocol != "anthropic":
        typer.echo(
            f"warning: provider {pname!r} uses the {protocol!r} protocol; a claude "
            "worker needs anthropic, so resolve_route will skip this lane at spawn "
            "(fail-safe). Writing anyway.",
            err=True,
        )

    try:
        existing = read_scope_value("model_routing.roles", scope=scope)
        if not isinstance(existing, dict):
            existing = {}
        merged = {**existing, name: f"{pname},{model}"}
        set_config_values(
            [("model_routing.roles", json.dumps(merged))], scope=scope
        )
    except ConfigSetError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(exc.exit_code) from exc

    typer.echo(f"route set {name} = {pname},{model} ({scope})")


@route_app.command("unset")
def unset_cmd(
    role: str = typer.Argument(..., help="Lane/role name to revert, e.g. build."),
    local: bool = typer.Option(
        False,
        "--local/--global",
        "-l/-g",
        help="Remove from the project-local config.toml instead of the per-user "
        "global one (default global).",
    ),
) -> None:
    """Revert a lane. Idempotent: an unconfigured role is a no-op (exit 0).

    A role removed from config falls back to its built-in default where one
    exists (the DEFAULT_ROUTED_ROLES), otherwise it is simply unrouted (next
    spawn -> primary Anthropic model). Running workers keep their stamped env.
    """
    from fno.agents.model_routing import DEFAULT_ROUTED_ROLES, DEFAULT_SECONDARY_MODEL
    from fno.config.writer import (
        ConfigSetError,
        read_scope_value,
        set_config_values,
        unset_config_value,
    )

    scope = "project" if local else "global"
    name = role.strip().lower()
    builtin = f"zai,{DEFAULT_SECONDARY_MODEL}"

    try:
        existing = read_scope_value("model_routing.roles", scope=scope)
    except ConfigSetError as exc:
        # A malformed/unreadable scope file must surface here, not masquerade as a
        # clean "not configured" no-op (the read now raises rather than -> {}).
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(exc.exit_code) from exc
    if not isinstance(existing, dict):
        existing = {}

    if name not in existing:
        if name in DEFAULT_ROUTED_ROLES:
            typer.echo(
                f"not configured in {scope}: {name} is routed by the built-in "
                f"default ({builtin}); nothing to unset."
            )
        else:
            typer.echo(f"not configured in {scope}: {name} (no-op).")
        raise typer.Exit(0)

    merged = {k: v for k, v in existing.items() if k != name}
    try:
        if merged:
            set_config_values(
                [("model_routing.roles", json.dumps(merged))], scope=scope
            )
        else:
            # Last role removed: prune the empty roles map to a clean default.
            unset_config_value("model_routing.roles", scope=scope)
    except ConfigSetError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(exc.exit_code) from exc

    if name in DEFAULT_ROUTED_ROLES:
        typer.echo(f"route unset {name}; reverts to built-in default ({builtin}) ({scope})")
    else:
        typer.echo(
            f"route unset {name}; no longer routed (next spawn -> primary model) ({scope})"
        )


@route_app.command("env")
def env_cmd(
    spec: str = typer.Argument(
        ...,
        help="A role (build) or an explicit provider,model (zai,glm-5.2).",
    ),
) -> None:
    """Print an eval-able env block for interactive use.

        eval "$(fno route env build)" && claude

    Fails CLOSED: if the target has no resolvable key it exits non-zero, names
    the checked env var/file on stderr, and emits NO export lines on stdout (a
    half-eval'd block would point a session at z.ai with no auth).
    """
    from fno.agents.model_routing import (
        _parse_target,
        _role_target,
        effective_providers,
        key_source,
        resolve_explicit_route,
        resolve_route,
    )

    block = _block()
    notes: list[str] = []
    target_pname: str | None = None

    if "," in spec:
        parsed = _parse_target(spec)
        if parsed is None:
            typer.echo(
                f"route env: malformed target {spec!r}; expected provider,model",
                err=True,
            )
            raise typer.Exit(2)
        target_pname, model = parsed
        route = resolve_explicit_route(target_pname, model, notice=notes.append)
    else:
        tgt = _role_target(spec.strip().lower(), block)
        target_pname = tgt[0] if tgt else None
        route = resolve_route(spec, notice=notes.append)

    if not route:
        reason = "; ".join(notes)
        # Name the missing env var/file when the target maps to a real provider.
        if target_pname:
            prov = effective_providers(block).get(target_pname)
            if prov:
                satisfying, checked = key_source(prov)
                if not satisfying and checked:
                    detail = f"missing API key (checked {', '.join(checked)})"
                    reason = f"{reason}; {detail}" if reason else detail
        typer.echo(
            f"route env: {reason or f'{spec!r} is not routed (unconfigured / protected / no key)'}",
            err=True,
        )
        raise typer.Exit(1)

    # Clear any parent Anthropic credential BEFORE the routed exports, exactly as
    # bg_create pops them (claude.py): a lingering ANTHROPIC_API_KEY or
    # CLAUDE_CODE_OAUTH_TOKEN in the invoking shell would otherwise authenticate
    # the eval'd session against Anthropic instead of the routed token - the
    # silent-Anthropic path this switch exists to prevent. `unset` on an already
    # unset var is a harmless no-op. Emitted only past the fail-closed guard, so a
    # refused resolve still writes nothing (AC2-FR).
    sys.stdout.write("unset ANTHROPIC_API_KEY\n")
    sys.stdout.write("unset CLAUDE_CODE_OAUTH_TOKEN\n")
    for k in sorted(route):
        sys.stdout.write(f"export {k}={shlex.quote(route[k])}\n")
