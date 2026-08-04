"""Read-only role definition and resolution inspection commands."""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import typer
from pydantic import TypeAdapter, ValidationError

from fno.company.contracts import RoleRef, WorkOrderRef
from fno.config import load_settings
from fno.handoff.output import json_mode, merge_json_flag
from fno.roles.context import build_artifact_catalog, catalog_revision
from fno.roles.models import (
    CapabilityFact,
    ContextBundleBounds,
    ContextReference,
    RoleResolutionBlocked,
)
from fno.roles.registry import (
    DiscoveredRoleDefinition,
    DiscoveryInputError,
    discover_role_definitions,
)
from fno.roles.resolver import resolve_role


roles_app = typer.Typer(
    name="roles",
    help="Inspect bounded business-role definitions and resolutions.",
    no_args_is_help=True,
)


@roles_app.callback()
def _roles_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit stable structured JSON.",
    ),
    root: Path | None = typer.Option(
        None,
        "--root",
        help="Read layered JSON definitions from this root.",
    ),
    source: list[str] | None = typer.Option(
        None,
        "--source",
        help="Also inspect a source using layer=path (repeatable).",
    ),
) -> None:
    merge_json_flag(ctx, json_output)
    ctx.ensure_object(dict)
    if root is not None:
        ctx.obj["roles_root"] = root
    if source:
        ctx.obj["role_sources"] = tuple(source)


def _json_requested(ctx: typer.Context, local: bool) -> bool:
    merge_json_flag(ctx, local)
    return json_mode(ctx)


def _emit_json(value: Any) -> None:
    typer.echo(json.dumps(value, indent=2, sort_keys=True))


def _discover(ctx: typer.Context) -> tuple[DiscoveredRoleDefinition, ...]:
    obj = ctx.obj if isinstance(ctx.obj, dict) else {}
    try:
        return discover_role_definitions(
            root=obj.get("roles_root"),
            source_specs=obj.get("role_sources", ()),
        )
    except DiscoveryInputError as exc:
        raise typer.BadParameter(str(exc), param_hint="--source") from exc


def _row(record: DiscoveredRoleDefinition) -> dict[str, Any]:
    return {
        "error": record.error,
        "function_id": record.role.function_id if record.role is not None else None,
        "layer": record.layer.value,
        "role_id": record.role.id if record.role is not None else None,
        "source": record.source_id,
        "status": record.status.value,
    }


def _token_parts(token: str) -> tuple[str | None, str]:
    function_id, separator, role_id = token.partition("/")
    if not separator:
        return None, token
    if not function_id or not role_id or "/" in role_id:
        raise typer.BadParameter(
            "role must be ROLE or FUNCTION/ROLE",
            param_hint="role",
        )
    return function_id, role_id


def _matches(
    records: tuple[DiscoveredRoleDefinition, ...],
    token: str,
) -> tuple[DiscoveredRoleDefinition, ...]:
    function_id, role_id = _token_parts(token)
    return tuple(
        record
        for record in records
        if (record.role is None and record.status.value in {"invalid", "unreadable"})
        or (
            record.role is not None
            and record.role.id == role_id
            and (function_id is None or record.role.function_id == function_id)
        )
    )


@roles_app.command("ls")
def list_roles(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit stable structured JSON.",
    ),
) -> None:
    """List source-attributed definitions without resolving context."""
    records = _discover(ctx)
    rows = [_row(record) for record in records]
    if _json_requested(ctx, json_output):
        _emit_json(rows)
        return
    for row in rows:
        role_id = row["role_id"] or "<unavailable>"
        function_id = row["function_id"] or "<unavailable>"
        typer.echo(
            f"{role_id} function={function_id} layer={row['layer']} "
            f"source={row['source']} status={row['status']}"
        )


def _show_row(
    record: DiscoveredRoleDefinition,
    *,
    highest_valid: DiscoveredRoleDefinition | None,
) -> dict[str, Any]:
    return {
        "disposition": (
            "unchecked"
            if record.status.value != "valid" or record.definition is None
            else "contributing"
            if record is highest_valid
            else "shadowed"
        ),
        "error": record.error,
        "function_id": record.role.function_id if record.role is not None else None,
        "layer": record.layer.value,
        "raw_definition": record.raw_definition,
        "role_id": record.role.id if record.role is not None else None,
        "source": record.source_id,
        "status": record.status.value,
    }


@roles_app.command("show")
def show_role(
    ctx: typer.Context,
    role: str = typer.Argument(..., help="ROLE or FUNCTION/ROLE."),
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit stable structured JSON.",
    ),
) -> None:
    """Show ordered raw definitions, validity, and shadowing."""
    records = _matches(_discover(ctx), role)
    valid_records = tuple(
        record
        for record in records
        if record.status.value == "valid" and record.definition is not None
    )
    highest_valid = valid_records[-1] if valid_records else None
    rows = [_show_row(record, highest_valid=highest_valid) for record in records]
    if _json_requested(ctx, json_output):
        _emit_json(rows)
        return
    for row in rows:
        typer.echo(
            f"{row['role_id']} function={row['function_id']} layer={row['layer']} "
            f"source={row['source']} status={row['status']} "
            f"disposition={row['disposition']}"
        )
        if row["error"]:
            typer.echo(f"  error: {row['error']}")
        typer.echo(
            "  raw: " + json.dumps(row["raw_definition"], sort_keys=True, separators=(",", ":"))
        )


def _role_ref(
    token: str,
    records: tuple[DiscoveredRoleDefinition, ...],
) -> RoleRef:
    requested_function, role_id = _token_parts(token)
    functions = sorted(
        {
            record.role.function_id
            for record in records
            if record.role is not None and record.role.id == role_id
        }
    )
    if requested_function is not None:
        return RoleRef(id=role_id, function_id=requested_function)
    if len(functions) > 1:
        raise typer.BadParameter(
            f"role {role_id!r} exists in multiple functions; use FUNCTION/ROLE",
            param_hint="role",
        )
    return RoleRef(
        id=role_id,
        function_id=functions[0] if functions else "unavailable",
    )


def _read_models(path: Path | None, adapter: TypeAdapter[Any]) -> tuple[Any, ...]:
    if path is None:
        return ()
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
        return tuple(adapter.validate_python(raw))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValidationError) as exc:
        raise typer.BadParameter(
            f"cannot validate {path}: {exc}",
            param_hint=str(path),
        ) from exc


@roles_app.command("resolve")
def resolve_role_command(
    ctx: typer.Context,
    role: str = typer.Argument(..., help="ROLE or FUNCTION/ROLE."),
    work_order: str = typer.Option(..., "--work-order", help="Authorized graph node identity."),
    attempt: str = typer.Option(..., "--attempt", help="Immutable attempt identity."),
    capabilities: Path | None = typer.Option(
        None,
        "--capabilities",
        help="Read-only JSON array of CapabilityFact observations.",
    ),
    context: Path | None = typer.Option(
        None,
        "--context",
        help="Read-only JSON array of ContextReference observations.",
    ),
    snapshot: str | None = typer.Option(
        None,
        "--snapshot",
        help="Expected input snapshot revision.",
    ),
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit stable structured JSON.",
    ),
) -> None:
    """Resolve exactly one immutable role or one typed blocked result."""
    records = _discover(ctx)
    role_ref = _role_ref(role, records)
    definitions = tuple(
        definition
        for record in records
        if record.role == role_ref and (definition := record.definition) is not None
    )
    revision = snapshot or (definitions[0].snapshot_revision if definitions else "unavailable")
    result = resolve_role(
        role=role_ref,
        definitions=definitions,
        capability_facts=_read_models(
            capabilities,
            TypeAdapter(tuple[CapabilityFact, ...]),
        ),
        context_catalog=_read_models(
            context,
            TypeAdapter(tuple[ContextReference, ...]),
        ),
        work_order=WorkOrderRef(
            node_id=work_order,
            attempt_id=attempt,
            role_id=role_ref.id,
        ),
        clock=datetime.now(UTC),
        snapshot_revision=revision,
        bundle_bounds=ContextBundleBounds(
            max_references=32,
            max_bytes=10_000_000,
        ),
        unchecked_definitions=records,
    )
    payload = result.model_dump(mode="json")
    if _json_requested(ctx, json_output):
        _emit_json(payload)
    elif isinstance(result, RoleResolutionBlocked):
        blocked_source = result.source_id or "<unavailable>"
        typer.echo(
            f"blocked role={role_ref.function_id}/{role_ref.id} "
            f"reason={result.reason.value} source={blocked_source}"
        )
        if result.detail:
            typer.echo(f"  detail: {result.detail}")
    else:
        typer.echo(
            f"resolved role={role_ref.function_id}/{role_ref.id} "
            f"manifest={result.manifest_digest} context={result.context_bundle.digest}"
        )
        for resolved_source in result.source_chain:
            typer.echo(
                f"  {resolved_source.layer.value} {resolved_source.source_id} "
                f"{resolved_source.disposition} status={resolved_source.status.value}"
            )
    if isinstance(result, RoleResolutionBlocked):
        raise typer.Exit(code=1)


@roles_app.command("context")
def context_catalog(
    ctx: typer.Context,
    snapshot: str | None = typer.Option(
        None,
        "--snapshot",
        help="Alignment revision stamped on every reference (pass to `resolve --snapshot`).",
    ),
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit a stable JSON array (pipes into `resolve --context`).",
    ),
) -> None:
    """Emit honest ContextReference observations for configured artifacts.

    Reads the generic ``[context.artifacts]`` map and produces one observation
    per entry: a real sha256 and byte size for a readable file, or
    ``readable=false`` with a naming reason for an unreadable one. Composes
    with ``resolve``: ``fno roles resolve --snapshot <rev> --context <(fno
    roles context --json --snapshot <rev>) ...``.
    """
    settings = load_settings()
    artifacts = settings.context.artifacts
    revision = snapshot or catalog_revision(artifacts)
    references = build_artifact_catalog(
        artifacts, snapshot_revision=revision, clock=datetime.now(UTC)
    )
    if _json_requested(ctx, json_output):
        _emit_json([reference.model_dump(mode="json") for reference in references])
        return
    if not references:
        typer.echo(
            "no artifacts configured ([context.artifacts] in .fno/config.toml); "
            f"revision={revision}"
        )
        return
    for reference in references:
        if reference.readable:
            state = f"ok digest={reference.content_digest[:12]}"
        else:
            state = f"unreadable: {reference.unavailable_reason}"
        typer.echo(
            f"{reference.identifier} sensitivity={reference.sensitivity.value} "
            f"bytes={reference.byte_size} {state} revision={revision}"
        )
