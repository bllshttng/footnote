"""fno state subcommands - show, set, validate, init, archive, list-fields."""
from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Optional

import typer
import yaml

from fno.handoff.output import merge_json_flag

cli = typer.Typer(name="state", help="manage fno state files", no_args_is_help=True)


@cli.callback()
def _state_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    merge_json_flag(ctx, json_output)


# -- Internal helpers --

def _resolve_path(path: Optional[Path], type_: Optional[str]) -> Path:
    """Resolve state file path from explicit --path or auto-detect from --type.

    Phase 07: when no --path is given, resolve relative to the v2-aware
    repo root (``FNO_REPO_ROOT`` env var, then git toplevel, then cwd)
    instead of raw cwd, so users and tests see consistent behavior
    regardless of invocation cwd.
    """
    if path is not None:
        return Path(path)
    repo_root = _v2_repo_root()
    if type_ is not None:
        return repo_root / ".fno" / f"{type_}-state.md"
    return repo_root / ".fno" / "target-state.md"


def _detect_type(path: Path) -> str:
    """Auto-detect state type from file name."""
    name = path.name
    if "megawalk" in name:
        return "megawalk"
    return "target"


def _json_mode(ctx: typer.Context) -> bool:
    return bool(ctx.obj and ctx.obj.get("json", False))


def _emit(ctx: typer.Context, data) -> None:
    """Emit output as JSON or YAML/text depending on --json flag."""
    if _json_mode(ctx):
        typer.echo(json.dumps(data, default=str))
    else:
        if isinstance(data, dict):
            typer.echo(yaml.safe_dump(data, default_flow_style=False).rstrip())
        elif isinstance(data, list):
            typer.echo(json.dumps(data))
        else:
            typer.echo(str(data) if data is not None else "")


# -- Subcommands --

@cli.command()
def show(
    ctx: typer.Context,
    path: Optional[Path] = typer.Option(None, "--path", help="path to state file"),
    type_: Optional[str] = typer.Option(None, "--type", help="state type: target|megawalk"),
    field: Optional[str] = typer.Option(None, "--field", help="return a single field value"),
    v2: bool = typer.Option(
        False,
        "--v2",
        help=(
            "Prefer v2 state layout (.fno/v2/). Falls back to v1 with a "
            "stderr note when the v2 file is missing."
        ),
    ),
) -> None:
    """Show state file contents (or a single field).

    Phase 07 of cli-v2-loop-integration added the ``--v2`` routing so
    callers running the v2 loop can inspect its state without rewriting
    every shell snippet to pass ``--path``.
    """
    from fno.state.io import read_frontmatter

    state_path = _resolve_state_path_v2_aware(path, type_, v2)
    try:
        data, _body = read_frontmatter(state_path)
    except FileNotFoundError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    if field:
        value = data.get(field)
        if _json_mode(ctx):
            typer.echo(json.dumps({field: value}, default=str))
        else:
            typer.echo(str(value) if value is not None else "")
        return

    _emit(ctx, data)


def _resolve_state_path_v2_aware(
    path: Optional[Path], type_: Optional[str], v2: bool
) -> Path:
    """Resolve state path honoring --v2 with v1 fallback note on stderr.

    Precedence: explicit --path wins. Otherwise:
    - --v2 set AND .fno/v2/target-state.md exists -> v2 path
    - --v2 set AND v2 file missing -> v1 path + stderr note
    - --v2 unset -> existing v1 logic via _resolve_path
    """
    if path is not None:
        return Path(path)
    if not v2:
        return _resolve_path(path, type_)

    from fno.state.v2_paths import v1_state_path, v2_state_path

    repo_root = _v2_repo_root()
    v2_path = v2_state_path(repo_root)
    if v2_path.exists():
        return v2_path
    print("(v2 not found, using v1)", file=sys.stderr)
    return v1_state_path(repo_root)


def _v2_repo_root() -> Path:
    """Compatibility shim for the shared repo-root resolver."""
    from fno.paths import resolve_repo_root

    return resolve_repo_root()


_IMMUTABLE_MANIFEST_MSG = (
    "target-state.md is an immutable session manifest "
    "(control-plane collapse, ab-d0337fbc); "
    "only first-fill of plan_path is allowed"
)


def _is_target_manifest(state_path: Path, type_: Optional[str], data: dict) -> bool:
    """Return True when the file is an immutable target-type manifest.

    An immutable manifest (written by the new init-target-state.sh after
    ab-d0337fbc) has no 'status:' field. Old-style manifests (written by
    pre-wedge init) carry 'status: IN_PROGRESS' and are mutable; the
    write-once rule does not apply to them so existing workflows stay intact.

    ``data`` is the frontmatter the caller already parsed - reusing it avoids
    a redundant disk read (gemini MEDIUM on #447).
    """
    detected = type_ or _detect_type(state_path)
    if detected != "target":
        return False
    # Only enforce write-once on the NEW immutable manifests (no status field).
    return "status" not in data


@cli.command(name="set")
def set_field(
    ctx: typer.Context,
    path: Optional[Path] = typer.Option(None, "--path", help="path to state file"),
    type_: Optional[str] = typer.Option(None, "--type", help="state type: target|megawalk"),
    field: str = typer.Option(..., "--field", help="field name to update"),
    value: str = typer.Option(..., "--value", help="new value"),
) -> None:
    """Set a single field in a state file atomically.

    For target manifests (control-plane collapse, ab-d0337fbc): the file is
    immutable after init. Only first-fill of an empty plan_path is allowed.
    Any other field write is refused with exit code 5.
    """
    from fno.state.io import read_frontmatter, write_frontmatter
    from fno.schemas import load_schema
    from pydantic import ValidationError

    state_path = _resolve_path(path, type_)
    try:
        data, body = read_frontmatter(state_path)
    except FileNotFoundError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    # Write-once enforcement for target manifests.
    if _is_target_manifest(state_path, type_, data):
        if field != "plan_path":
            typer.echo(_IMMUTABLE_MANIFEST_MSG, err=True)
            raise typer.Exit(code=5)
        # plan_path: only allowed when the current value is empty / missing.
        current = data.get("plan_path")
        # Treat None, empty string, and the literal string '""' as empty.
        is_empty = current is None or str(current).strip() in ("", '""', "''")
        if not is_empty:
            typer.echo(_IMMUTABLE_MANIFEST_MSG, err=True)
            raise typer.Exit(code=5)

    # Coerce value type to match existing field (int, float, bool, str)
    existing = data.get(field)
    coerced = _coerce_value(value, existing)

    # Build candidate data for validation
    candidate = dict(data)
    candidate[field] = coerced

    # Validate via schema (use detected type if not explicit)
    detected_type = type_ or _detect_type(state_path)
    try:
        Schema = load_schema(detected_type)
        Schema.model_validate(candidate)
    except ValueError as exc:
        typer.echo(f"error: unknown schema type - {exc}", err=True)
        raise typer.Exit(code=1)
    except ValidationError as exc:
        _output_validation_error(ctx, exc)
        raise typer.Exit(code=1)

    data[field] = coerced
    write_frontmatter(state_path, data, body)

    if _json_mode(ctx):
        typer.echo(json.dumps({"field": field, "value": coerced}, default=str))
    else:
        typer.echo(f"set {field} = {coerced!r}")


def _coerce_value(value: str, existing) -> object:
    """Coerce string value to match existing field's type."""
    if isinstance(existing, bool):
        return value.lower() in ("true", "1", "yes")
    if isinstance(existing, int):
        try:
            return int(value)
        except ValueError:
            pass
    if isinstance(existing, float):
        try:
            return float(value)
        except ValueError:
            pass
    return value


def _output_validation_error(ctx: typer.Context, exc) -> None:
    """Print validation error in JSON or text mode."""
    errors = exc.errors()
    if _json_mode(ctx):
        typer.echo(json.dumps({"valid": False, "errors": errors}, default=str))
    else:
        typer.echo(f"validation error: {exc}", err=True)


@cli.command()
def validate(
    ctx: typer.Context,
    path: Optional[Path] = typer.Option(None, "--path", help="path to state file"),
    type_: Optional[str] = typer.Option(None, "--type", help="state type: target|megawalk"),
) -> None:
    """Validate a state file against its schema."""
    from fno.state.io import read_frontmatter
    from fno.schemas import load_schema
    from pydantic import ValidationError

    state_path = _resolve_path(path, type_)
    try:
        data, _body = read_frontmatter(state_path)
    except FileNotFoundError as exc:
        if _json_mode(ctx):
            typer.echo(json.dumps({"valid": False, "errors": [{"msg": str(exc)}]}))
        else:
            typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    detected_type = type_ or _detect_type(state_path)
    try:
        Schema = load_schema(detected_type)
    except ValueError as exc:
        if _json_mode(ctx):
            typer.echo(json.dumps({"valid": False, "errors": [{"msg": str(exc)}]}))
        else:
            typer.echo(str(exc), err=True)
        raise typer.Exit(code=1)

    try:
        Schema.model_validate(data)
    except ValidationError as exc:
        errors = exc.errors()
        if _json_mode(ctx):
            typer.echo(json.dumps({"valid": False, "errors": errors}, default=str))
        else:
            typer.echo(f"invalid: {exc}", err=True)
        raise typer.Exit(code=1)

    if _json_mode(ctx):
        typer.echo(json.dumps({"valid": True}))
    else:
        typer.echo("valid")


@cli.command()
def init(
    ctx: typer.Context,
    type_: str = typer.Option("target", "--type", help="state type: target|megawalk"),
    output: Optional[Path] = typer.Option(None, "--output", help="output path (default: .fno/<type>-state.md)"),
    force: bool = typer.Option(False, "--force", "-F", help="overwrite existing file"),
    allow_stub: bool = typer.Option(
        False, "--allow-stub", hidden=True,
        help="bypass the target-bootstrap redirect (test use only)",
    ),
) -> None:
    """Create a fresh state file with default values."""
    # Redirect target-session bootstraps to `fno target init` (Change 3).
    # A bare `fno state init` (default type=target, default output path)
    # writes an empty stub the stop hook archives - the recurring
    # substitution for the pathless init-target-state.sh. `fno target init`
    # records input/plan_path + the owner_cwd worktree binding and refuses
    # stubs. An explicit --output is a deliberate, non-bootstrap use (e.g.
    # tests) and is left alone, as is --allow-stub. Checked before any heavy
    # import so the redirect fires even where optional deps are unavailable.
    if type_ == "target" and output is None and not allow_stub:
        typer.echo(
            "target sessions must bootstrap with 'fno target init' "
            "(records input/plan_path/owner_cwd binding); 'fno state init' "
            "writes a stub the stop hook will archive.",
            err=True,
        )
        raise typer.Exit(code=2)

    from fno.schemas import load_schema
    from fno.state.io import write_frontmatter

    out_path = output or Path(f".fno/{type_}-state.md")

    if out_path.exists() and not force:
        typer.echo(f"error: {out_path} already exists (use --force to overwrite)", err=True)
        raise typer.Exit(code=1)

    try:
        Schema = load_schema(type_)
    except ValueError as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(code=1)

    instance = Schema()
    data = instance.model_dump(exclude_none=False)
    # Remove None values that aren't meaningful in a fresh file
    data = {k: v for k, v in data.items() if v is not None or k in ("status",)}

    out_path.parent.mkdir(parents=True, exist_ok=True)
    body = f"# {type_.capitalize()} Session State\n\nInitialized by fno CLI.\n"
    write_frontmatter(out_path, data, body)

    if _json_mode(ctx):
        typer.echo(json.dumps({"created": str(out_path)}))
    else:
        typer.echo(f"created: {out_path}")


@cli.command()
def archive(
    ctx: typer.Context,
    path: Optional[Path] = typer.Option(None, "--path", help="path to state file"),
    type_: Optional[str] = typer.Option(None, "--type", help="state type"),
) -> None:
    """Archive (move) a state file to a timestamped backup."""
    import time as _time

    state_path = _resolve_path(path, type_)
    if not state_path.exists():
        typer.echo(f"error: {state_path} does not exist", err=True)
        raise typer.Exit(code=1)

    ts = int(_time.time())
    archive_path = state_path.with_suffix(f".archived.{ts}.md")
    state_path.rename(archive_path)

    if _json_mode(ctx):
        typer.echo(json.dumps({"archived": str(archive_path)}))
    else:
        typer.echo(f"archived: {state_path} -> {archive_path}")


@cli.command(name="list-fields")
def list_fields(
    ctx: typer.Context,
    type_: str = typer.Option("target", "--type", help="state type: target|megawalk"),
) -> None:
    """List all known fields for a state type."""
    from fno.schemas import load_schema

    try:
        Schema = load_schema(type_)
    except ValueError as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(code=1)

    fields = list(Schema.model_fields.keys())

    if _json_mode(ctx):
        typer.echo(json.dumps(fields))
    else:
        for f in fields:
            typer.echo(f)
