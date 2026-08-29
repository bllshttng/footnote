"""``fno config route``: legible, on-the-fly provider route lanes.

Six verbs over the existing per-spawn model-routing machinery
(``fno.agents.model_routing``), which stays the single source of the z.ai
env-var contract:

- ``ls``        - the effective merged table (built-ins + config), one row per role.
- ``set``       - route a lane to ``provider/model`` (atomic config write).
- ``unset``     - revert a lane (to its built-in default, or unrouted).
- ``env``       - eval-able ``export`` block for an interactive session.
- ``inventory`` - the declared routing inventory read (also ``fno doctor route``).
- ``init``      - append the shipped sample, commented out, to config.

``set``/``unset`` delegate to the existing atomic, file-locked ``fno config
set``/``unset`` write path - there is no second config writer here. The roles
map is a dict-leaf (REPLACE semantics), so a per-lane change is a
read-merge-write of the SCOPE's own roles map (last-writer-wins under the config
file lock; accepted).
"""
from __future__ import annotations

import json
import os
import shlex
import sys

import typer
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from fno.config import ModelRoutingBlock

route_app = typer.Typer(
    name="route",
    help="Provider route lanes: ls (effective table) / set / unset / env.",
    no_args_is_help=True,
)


def _block() -> "ModelRoutingBlock":
    from fno.config import load_settings

    return load_settings().model_routing


@route_app.command("ls")
def ls_cmd(
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the table as JSON instead of text."
    ),
) -> None:
    """Render the effective routing table.

    One row per role: role -> target (provider/model) -> protocol -> key status
    (which env var / file satisfied it, or MISSING) -> auto-assigned-by. Built-in
    roles, config overrides, the known ``build`` lane, and the protected roles all
    appear. Degrades an unreadable key source to MISSING rather than erroring.
    """
    from fno.agents.model_routing import build_route_table

    rows = build_route_table()
    if json_output:
        typer.echo(json.dumps(rows, indent=2))
        return

    cols = ("role", "provider_model", "protocol", "key", "assigned_by")
    header = {
        "role": "ROLE",
        "provider_model": "PROVIDER/MODEL",
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
    provider_model: str = typer.Argument(
        ...,
        help="provider/model - e.g. zai/glm-5.3 or zai/glm-5.3[1m] "
        "(legacy comma form zai,glm-5.3 is also accepted).",
    ),
    local: bool = typer.Option(
        False,
        "--local/--global",
        "-l/-g",
        help="Write the project-local config.toml instead of the per-user "
        "global one (default global; routing is operator-level).",
    ),
) -> None:
    """Route a lane to ``provider/model`` (atomic, schema-validated).

    Refuses a protected role name and a provider absent from the effective
    providers map BEFORE any write. A protocol mismatch (anthropic lane pointing
    at an openai provider) warns but writes (resolve_route degrades safely).
    """
    from fno.agents.model_routing import (
        PROTECTED_ROLE_HINT,
        PROTECTED_ROLES,
        _parse_target,
        effective_providers,
    )
    from fno.config.writer import ConfigSetError, read_scope_value, set_config_values

    scope = "project" if local else "global"
    name = role.strip().lower()

    if name in PROTECTED_ROLES:
        # Name the surface that actually owns the decision: refusing without it
        # sends the operator looking for a knob this table does not have.
        owner = PROTECTED_ROLE_HINT.get(name, "")
        typer.echo(
            f"error: {name!r} is a protected role (never routable via the roles "
            "table); refusing. Config unchanged."
            + (f"\nhint: {owner}." if owner else ""),
            err=True,
        )
        raise typer.Exit(2)

    parsed = _parse_target(provider_model)
    if parsed is None:
        typer.echo(
            f"error: provider/model must be 'provider/model' with a non-empty "
            f"model token; got {provider_model!r}. Config unchanged.",
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
        merged = {**existing, name: f"{pname}/{model}"}
        set_config_values(
            [("model_routing.roles", json.dumps(merged))], scope=scope
        )
    except ConfigSetError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(exc.exit_code) from exc

    typer.echo(f"route set {name} = {pname}/{model} ({scope})")


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


@route_app.command("init")
def routing_init_cmd(
    local: bool = typer.Option(
        False, "--local/--global", "-l/-g",
        help="Append to the project-local config.toml instead of the per-user "
        "global one (default global; routing is operator-level).",
    ),
) -> None:
    """Append the shipped routing inventory sample, commented out, to config.

    The sample documents the ``[routing]`` block shape; nothing is enabled until
    you uncomment and edit rows. Idempotent: a config already carrying the
    sample marker is left untouched. The sample lives INSIDE the package
    (``fno/routing_sample.toml``) so an installed wheel finds it exactly like a
    checkout does - the events-schema precedent.
    """
    import fcntl
    from pathlib import Path

    from fno.config.writer import _target_path

    sample = Path(__file__).resolve().parent / "routing_sample.toml"
    if not sample.is_file():
        typer.echo(f"error: sample not found at {sample}", err=True)
        raise typer.Exit(1)
    target = _target_path("project" if local else "global", None)
    if target.is_symlink():
        target = Path(os.path.realpath(target))
    marker = "# SAMPLE routing inventory"
    commented = "\n".join(
        ("# " + line.rstrip()) if line.strip() else "#" for line in
        sample.read_text(encoding="utf-8").splitlines()
    )
    # The SAME exclusive lock discipline config.writer._locked_update uses
    # (sidecar <config>.lock + flock), held across the read and the append, so
    # a concurrent `fno config set` rename cannot drop the appended block and
    # this append cannot land on a replaced inode.
    target.parent.mkdir(parents=True, exist_ok=True)
    lock_path = target.with_suffix(target.suffix + ".lock")
    try:
        with open(lock_path, "w") as lock_fh:
            fcntl.flock(lock_fh.fileno(), fcntl.LOCK_EX)
            try:
                existing = (
                    target.read_text(encoding="utf-8") if target.is_file() else ""
                )
                if marker in existing:
                    typer.echo(
                        f"routing sample already present in {target}; nothing to do."
                    )
                    raise typer.Exit(0)
                with open(target, "a", encoding="utf-8") as fh:
                    fh.write("\n\n" + commented + "\n")
            finally:
                fcntl.flock(lock_fh.fileno(), fcntl.LOCK_UN)
    except OSError as exc:
        typer.echo(f"error: cannot update {target}: {exc}", err=True)
        raise typer.Exit(1) from exc
    typer.echo(f"routing sample appended (commented out) to {target}")


@route_app.command("inventory")
def inventory_cmd(
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the inventory as JSON instead of text."
    ),
) -> None:
    """What can this installation reach: every declared routing row.

    One row per declared model with its resolved band (row band, else a
    snapshot-derived percentile, else unbanded) and a reachability verdict:
    ``ok`` (a known harness can invoke it), ``not-installed`` (the named
    harness is not one fno can drive; the row refuses BY NAME on stderr rather
    than silently vanishing from routing), ``unbanded`` (never a grid
    candidate), or ``incomplete`` (no --model value). An empty inventory says
    so: a virgin install routes nothing from the grid.
    """
    from fno.agents.harnesses import READABLE_PROVIDERS
    from fno.route_resolve import resolve_inventory

    inv = resolve_inventory()
    rows: list[dict[str, str]] = []
    refusals: list[str] = []
    if not inv.rows:
        note = "no inventory declared (config.routing.models is empty); the grid records no-inventory-declared"
        if json_output:
            typer.echo(json.dumps({"objective": inv.objective, "models": [], "note": note}, indent=2))
        else:
            typer.echo(note)
        return
    for name in sorted(inv.rows):
        row = inv.rows[name]
        if not row.harness or not row.model:
            verdict = "incomplete"
        elif row.harness not in READABLE_PROVIDERS:
            verdict = "not-installed"
            refusals.append(f"{name}: harness {row.harness!r} is not installed (known: {', '.join(READABLE_PROVIDERS)})")
        elif not row.band:
            verdict = "unbanded"
        else:
            verdict = "ok"
        pct = "" if row.percentile is None else f"{row.percentile:g}"
        rows.append({
            "name": name,
            "harness": row.harness,
            "model": row.model,
            "band": row.band or "unbanded",
            "percentile": pct,
            "effort": row.effort,
            "verdict": verdict,
        })
    if json_output:
        typer.echo(json.dumps({
            "objective": inv.objective,
            "prefer_harness": inv.prefer_harness,
            "models": rows,
        }, indent=2))
    else:
        typer.echo(f"objective={inv.objective}"
                   + (f" prefer_harness={inv.prefer_harness}" if inv.prefer_harness else ""))
        cols = ("name", "harness", "model", "band", "percentile", "effort", "verdict")
        widths = {
            c: max(len(c), *(len(r[c]) for r in rows)) if rows else len(c)
            for c in cols
        }

        def _fmt(r: dict[str, str]) -> str:
            return "  ".join(r[c].ljust(widths[c]) for c in cols).rstrip()

        typer.echo(_fmt({c: c.upper() for c in cols}))
        for r in rows:
            typer.echo(_fmt(r))
    for line in refusals:
        typer.echo(f"refused: {line}", err=True)


@route_app.command("env")
def env_cmd(
    spec: str = typer.Argument(
        ...,
        help="A role (build) or an explicit provider/model (zai/glm-5.3).",
    ),
) -> None:
    """Print an eval-able env block for interactive use.

        eval "$(fno config route env build)" && claude

    Fails CLOSED: if the target has no resolvable key it exits non-zero, names
    the checked env var/file on stderr, and emits NO export lines on stdout (a
    half-eval'd block would point a session at z.ai with no auth).
    """
    from fno.agents.model_routing import (
        RouteCompositionError,
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

    # A separator (slash or legacy comma) marks an explicit provider/model; a
    # bare token is a role name (roles never contain a separator).
    if "/" in spec or "," in spec:
        parsed = _parse_target(spec)
        if parsed is None:
            typer.echo(
                f"route env: malformed target {spec!r}; expected provider/model",
                err=True,
            )
            raise typer.Exit(2)
        target_pname, model = parsed
        route = resolve_explicit_route(target_pname, model, notice=notes.append)
    else:
        tgt = _role_target(spec.strip().lower(), block)
        target_pname = tgt[0] if tgt else None
        try:
            route = resolve_route(spec, notice=notes.append)
        except RouteCompositionError as exc:
            typer.echo(f"route env: {exc}", err=True)
            raise typer.Exit(1) from exc

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


# ---- route-settings overlay files (x-5cc5): what exists, what references it ----
#
# The spawn path writes one content-addressed overlay per distinct route under
# state_dir()/route-settings. The names are digests, so the only way to answer
# "which file is my session on / is any of them stale" is this read verb.

settings_app = typer.Typer(
    name="settings",
    help="Recorded route-settings overlays: ls what exists, what references it.",
    no_args_is_help=True,
)


@settings_app.command("ls")
def settings_ls_cmd(
    prune: bool = typer.Option(
        False,
        "--prune",
        help=(
            "Delete overlays no registry row references and older than the age "
            "threshold. Content-addressing makes regeneration free."
        ),
    ),
    age_days: int = typer.Option(
        14, "--age-days", help="Prune age threshold in days (default 14)."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the rows as JSON instead of text."
    ),
) -> None:
    """List every recorded route-settings overlay.

    One row per file: digest name, provider (stamp/base_url match against
    today's registry), model, haiku tier, a STALE marker naming old -> new
    when the tier differs from today's provider default, age, and whether a
    registry row still references it. Never prints file contents: the
    overlays carry live auth tokens.
    """
    import time

    from fno import paths
    from fno.agents.model_routing import (
        effective_providers,
        provider_name_for_route,
        read_route_settings,
    )
    from fno.config import load_settings

    settings = load_settings()
    referenced_paths: "set[str] | None"
    try:
        from fno.agents.registry import load_registry

        referenced_paths = {
            p for p in (getattr(e, "route_settings_path", None) for e in load_registry()) if p
        }
    except Exception:  # noqa: BLE001 - unreadable registry degrades the column, never the verb
        referenced_paths = None

    base_dir = paths.state_dir() / "route-settings"
    rows: list[dict[str, object]] = []
    now = time.time()
    pruned: list[str] = []
    files = sorted(base_dir.glob("*.json")) if base_dir.is_dir() else []
    for path in files:
        age = max(0.0, (now - path.stat().st_mtime) / 86400.0)
        try:
            route = read_route_settings(str(path))
        except Exception:  # noqa: BLE001 - one bad file degrades its row, not the listing
            route = None
        pname = provider_name_for_route(route or {}, settings=settings) if route else None
        stale = ""
        if route and pname:
            todays = effective_providers(settings.model_routing)[pname].get("haiku_model")
            recorded = route.get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
            if todays and recorded and str(todays) != recorded:
                stale = f"{recorded} -> {todays}"
        referenced = (
            "?" if referenced_paths is None else ("yes" if str(path) in referenced_paths else "no")
        )
        rows.append({
            "file": path.name,
            "provider": pname or "-",
            "model": (route or {}).get("ANTHROPIC_MODEL", "-"),
            "haiku": (route or {}).get("ANTHROPIC_DEFAULT_HAIKU_MODEL", "-"),
            "stale": stale or "-",
            "age_days": round(age, 1),
            "referenced": referenced,
        })
        if prune and referenced == "no" and age >= age_days:
            path.unlink()
            pruned.append(str(path))

    if json_output:
        typer.echo(json.dumps({"rows": rows, "pruned": pruned}, indent=2))
        return
    cols = ("file", "provider", "model", "haiku", "stale", "age_days", "referenced")
    header = {
        "file": "FILE",
        "provider": "PROVIDER",
        "model": "MODEL",
        "haiku": "HAIKU",
        "stale": "STALE",
        "age_days": "AGE-D",
        "referenced": "REF",
    }
    str_rows = [{c: str(r[c]) for c in cols} for r in rows]
    widths = {
        c: max(len(header[c]), *(len(r[c]) for r in str_rows)) if str_rows else len(header[c])
        for c in cols
    }
    typer.echo("  ".join(header[c].ljust(widths[c]) for c in cols).rstrip())
    for r in str_rows:
        typer.echo("  ".join(r[c].ljust(widths[c]) for c in cols).rstrip())
    if prune:
        for p in pruned:
            typer.echo(f"pruned {p}")
        typer.echo(
            f"{len(pruned)} file(s) pruned (unreferenced, older than {age_days}d); "
            f"{len(rows) - len(pruned)} kept"
        )


route_app.add_typer(settings_app)
