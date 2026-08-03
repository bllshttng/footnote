"""Hidden ``fno plugins`` administrative surface.

``ls``, ``inspect``, and ``verify`` are read-only; ``activate`` and ``deactivate``
are the only writers. The group registers hidden in ``LAZY_SUBCOMMANDS`` so it
costs no advertised top-level slot (``fno lint menu-caps`` caps the menu at 10).

Every command emits a versioned ``--json`` envelope so the skill layer and CI
parse rather than scrape, matching the ``approvals``, ``delivery``, ``roles``,
and ``company`` precedent.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import typer

from fno.handoff.output import json_mode, merge_json_flag
from fno.plugins.activate import ActivationRefusal, activate, deactivate
from fno.plugins.manifest import pack_digest
from fno.plugins.registry import PackRegistryStore, RegistryCorrupt
from fno.plugins.verify import load_manifest, verify_pack
from fno.roles.registry import default_role_root

plugins_app = typer.Typer(
    name="plugins",
    help="Install, verify, activate, and inspect function packs.",
    no_args_is_help=True,
)

_RESPONSE_VERSION = 1


def _envelope(payload: dict[str, Any]) -> dict[str, Any]:
    return {"version": _RESPONSE_VERSION, **payload}


def _emit_json(value: Any) -> None:
    typer.echo(json.dumps(value, indent=2, sort_keys=True, default=str))


@plugins_app.callback()
def _plugins_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit stable structured JSON."),
    root: Path | None = typer.Option(None, "--root", help="Role root (default: conventional)."),
) -> None:
    merge_json_flag(ctx, json_output)
    ctx.ensure_object(dict)
    ctx.obj["plugins_root"] = root if root is not None else default_role_root()


def _store(ctx: typer.Context) -> tuple[PackRegistryStore, Path]:
    root = ctx.obj.get("plugins_root") if isinstance(ctx.obj, dict) else None
    root_path = Path(root) if root is not None else default_role_root()
    return PackRegistryStore(root_path / ".pack-registry.json"), root_path


def _json_requested(ctx: typer.Context, local: bool) -> bool:
    merge_json_flag(ctx, local)
    return json_mode(ctx)


@plugins_app.command("verify")
def verify_command(
    ctx: typer.Context,
    path: Path = typer.Argument(..., help="Pack directory or plugin.yaml file."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit stable structured JSON."),
) -> None:
    """Verify a pack on two axes; exit non-zero unless every condition passed."""
    store, _ = _store(ctx)
    try:
        installed = store.installed_index()
    except RegistryCorrupt as exc:
        typer.echo(f"registry corrupt; cannot verify dependencies: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    report = verify_pack(path, installed=installed)
    if _json_requested(ctx, json_output):
        _emit_json(_envelope(report.as_dict()))
    else:
        typer.echo(f"pack: {report.pack_path}")
        for condition in report.conditions:
            checked = "checked" if condition.checked else "unchecked"
            typer.echo(
                f"  {condition.family.value:<13} {condition.name:<28} "
                f"{checked:<9} {condition.result.value}"
            )
            if condition.detail and condition.result.value != "passed":
                typer.echo(f"      {condition.detail}")
        typer.echo(f"ok: {report.ok}")
    if not report.ok:
        raise typer.Exit(code=1)


@plugins_app.command("activate")
def activate_command(
    ctx: typer.Context,
    path: Path = typer.Argument(..., help="Pack directory or plugin.yaml file."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit stable structured JSON."),
) -> None:
    """Activate a verified pack into the plugin role layer. Grants nothing."""
    store, root = _store(ctx)
    try:
        outcome = activate(path, registry_store=store, role_root=root)
    except ActivationRefusal as exc:
        typer.echo(f"refused ({exc.reason.value}): {exc.detail}", err=True)
        raise typer.Exit(code=1) from exc
    receipt = outcome.receipt
    payload = {
        "pack_id": receipt.pack_id,
        "pack_digest": receipt.pack_digest,
        "resolved_version": receipt.resolved_version,
        "written_paths": list(receipt.written_paths),
        "already_active": outcome.already_active,
    }
    if _json_requested(ctx, json_output):
        _emit_json(_envelope(payload))
    else:
        label = "already active" if outcome.already_active else "activated"
        typer.echo(f"{label} {receipt.pack_id} {receipt.resolved_version} digest={receipt.pack_digest[:12]}")
        for written in receipt.written_paths:
            typer.echo(f"  wrote {written}")


@plugins_app.command("deactivate")
def deactivate_command(
    ctx: typer.Context,
    pack_id: str = typer.Argument(..., help="Installed pack id."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit stable structured JSON."),
) -> None:
    """Remove only the definition paths this pack's receipt recorded."""
    store, root = _store(ctx)
    try:
        outcome = deactivate(pack_id, registry_store=store, role_root=root)
    except RegistryCorrupt as exc:
        typer.echo(f"registry corrupt; cannot deactivate safely: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    payload = {
        "pack_id": pack_id,
        "removed": list(outcome.removed),
        "left_alone": list(outcome.left_alone),
    }
    if _json_requested(ctx, json_output):
        _emit_json(_envelope(payload))
    else:
        typer.echo(f"deactivated {pack_id}: removed {len(outcome.removed)} definition(s)")
        for left in outcome.left_alone:
            typer.echo(f"  left alone (not receipted as current): {left}")


@plugins_app.command("ls")
def list_command(
    ctx: typer.Context,
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit stable structured JSON."),
) -> None:
    """List installed packs with version, activation state, and declared effect ceiling."""
    store, _ = _store(ctx)
    try:
        registry = store.load()
    except RegistryCorrupt as exc:
        typer.echo(f"registry corrupt; cannot list packs: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    if _json_requested(ctx, json_output):
        packs = [
            {
                "pack_id": pack.pack_id,
                "version": pack.resolved_version,
                "activated": registry.receipt_for(pack.pack_id) is not None,
                "digest": pack.pack_digest[:12],
                "declared_effects": [
                    {"effect_class": e.effect_class, "destination": e.destination}
                    for e in pack.declared_effects
                ],
            }
            for pack in registry.packs
        ]
        _emit_json(_envelope({"packs": packs}))
        return
    if not registry.packs:
        typer.echo("no installed packs")
        return
    for pack in registry.packs:
        state = "active" if registry.receipt_for(pack.pack_id) is not None else "installed"
        effects = ", ".join(f"{e.effect_class}->{e.destination}" for e in pack.declared_effects) or "none"
        typer.echo(f"{pack.pack_id} {pack.resolved_version} {state} digest={pack.pack_digest[:12]} effects=[{effects}]")


@plugins_app.command("inspect")
def inspect_command(
    ctx: typer.Context,
    pack_id: str = typer.Argument(..., help="Installed pack id."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit stable structured JSON."),
) -> None:
    """Print a pack's manifest with declarations labeled as declarations."""
    store, _ = _store(ctx)
    try:
        registry = store.load()
    except RegistryCorrupt as exc:
        typer.echo(f"registry corrupt; cannot inspect: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    pack = registry.pack_by_id(pack_id)
    if pack is None:
        typer.echo(f"no installed pack {pack_id!r}", err=True)
        raise typer.Exit(code=1)
    manifest, _ = load_manifest(Path(pack.manifest_path))
    if manifest is None:
        typer.echo(f"manifest for {pack_id} no longer readable", err=True)
        raise typer.Exit(code=1)
    declared_roles = [role.role.id for role in manifest.roles]
    digest_matches = pack_digest(manifest) == pack.pack_digest
    if _json_requested(ctx, json_output):
        _emit_json(
            _envelope(
                {
                    "pack_id": manifest.id,
                    "version": manifest.version,
                    "installed_digest": pack.pack_digest,
                    "digest_matches_install": digest_matches,
                    "declared_roles": declared_roles,
                    "declared_permissions": [
                        {"effect_class": e.effect_class, "destination": e.destination}
                        for e in manifest.permissions
                    ],
                    "declared_adapters": [
                        {
                            "id": a.id,
                            "destination": a.destination,
                            "conformance": a.conformance.model_dump(),
                        }
                        for a in manifest.adapters
                    ],
                }
            )
        )
        return
    typer.echo(f"{manifest.id} {manifest.version}")
    typer.echo(f"  declares roles: {', '.join(declared_roles) or 'none'}")
    if not digest_matches:
        typer.echo(f"  WARNING: manifest digest no longer matches installed record {pack.pack_digest[:12]}", err=True)
    for effect in manifest.permissions:
        typer.echo(f"  declares effect ceiling: {effect.effect_class} -> {effect.destination}")
