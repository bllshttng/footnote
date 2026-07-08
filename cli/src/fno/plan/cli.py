"""fno plan CLI - plan management verbs.

Verbs:
    stamp             - mark a plan's frontmatter with ship metadata (status:shipped)
    graduate          - flip a stamped plan from status:shipped to status:done
    brief             - generate a scoped task brief from a single-doc plan
    migrate-folder    - relocate a folder plan into a single-doc plan (archive folder)
    reconcile-status  - normalize drifted plan frontmatter status in place
    folder-audit      - count folder plans owned by a non-terminal graph node

stamp and graduate forward all unknown args + propagate exit codes from the
in-package ``fno.plan._stamp`` module. brief is implemented in fno.plan.brief.

Why a CLI verb at all? It's the polished surface skills can call instead of
spawning ``python3 -m fno.plan._stamp`` directly.
"""
from __future__ import annotations

import json
import subprocess
import sys
from enum import Enum
from pathlib import Path
from typing import List, Optional

import typer

from fno.paths import resolve_repo_root


plan_app = typer.Typer(
    name="plan",
    help="Plan management: stamping, graduation, and brief generation",
    no_args_is_help=True,
    add_completion=False,
)


def _forward(verb: str, extra_args: List[str]) -> int:
    """Subprocess into the in-package stamp module with verb + extra_args.

    Runs ``python3 -m fno.plan._stamp`` under the current interpreter so the
    module is always importable in-package (no repo-root resolution, no
    script-missing degrade). Returns the module's exit code so callers chain.
    """
    cmd = [sys.executable, "-m", "fno.plan._stamp", verb] + extra_args
    result = subprocess.run(cmd, check=False)
    return result.returncode


@plan_app.command(
    "stamp",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help="Stamp plan frontmatter with ship metadata. Forwards all args to fno.plan._stamp stamp.",
)
def stamp(ctx: typer.Context) -> None:
    rc = _forward("stamp", list(ctx.args))
    raise typer.Exit(code=rc)


@plan_app.command(
    "graduate",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help="Graduate a stamped plan (shipped -> done). Forwards all args to fno.plan._stamp graduate.",
)
def graduate(ctx: typer.Context) -> None:
    rc = _forward("graduate", list(ctx.args))
    raise typer.Exit(code=rc)


@plan_app.command(
    "set-expected",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Authoritatively set a plan's expected_url_count (count-only). Forwards "
        "all args to fno.plan._stamp set-expected. Used to record the "
        "group count on a shared epic-decomposition doc."
    ),
)
def set_expected(ctx: typer.Context) -> None:
    rc = _forward("set-expected", list(ctx.args))
    raise typer.Exit(code=rc)


# Update the module docstring's verb list when adding verbs above.


# ---------------------------------------------------------------------------
# fno plan brief
# ---------------------------------------------------------------------------

class _FilterMode(str, Enum):
    all = "all"
    relevant = "relevant"
    none = "none"


class _OutputFormat(str, Enum):
    markdown = "markdown"
    json = "json"


@plan_app.command(
    "brief",
    help=(
        "Generate a scoped task brief from a single-doc plan.\n\n"
        "Exit codes: 0 success, 1 plan not found, 2 contract violation "
        "(missing section / unknown task-id), 3 malformed YAML."
    ),
)
def brief(
    plan_path: str = typer.Argument(..., help="Path to the plan markdown file"),
    task: str = typer.Option(..., "--task", help="Task id to generate a brief for (e.g. 2.1)"),
    include_failure_modes: _FilterMode = typer.Option(
        _FilterMode.relevant,
        "--include-failure-modes",
        help="Which Failure Modes entries to include: all | relevant | none",
    ),
    include_locked_decisions: _FilterMode = typer.Option(
        _FilterMode.relevant,
        "--include-locked-decisions",
        help="Which Locked Decisions entries to include: all | relevant | none",
    ),
    format: _OutputFormat = typer.Option(
        _OutputFormat.markdown,
        "--format",
        help="Output format: markdown | json",
    ),
) -> None:
    """Generate a focused task brief from a single-doc plan file."""
    from fno.plan._doc import load_plan, FrontmatterError, ParseError
    from fno.plan.brief import build_brief, BriefError, BriefParseError

    # Resolve path relative to repo root when not absolute.
    # resolve_repo_root() raises RuntimeError when not inside a git repo;
    # that's a legitimate "use the bare path" fallback, not an error to
    # surface. OSError covers filesystem failures around the existence
    # check. Anything else propagates.
    resolved = Path(plan_path)
    if not resolved.is_absolute():
        try:
            repo_root = resolve_repo_root()
            candidate = repo_root / plan_path
            if candidate.exists():
                resolved = candidate
        except (RuntimeError, OSError):
            pass

    # Exit 1 if plan not found
    if not resolved.exists():
        typer.echo(
            f"fno plan brief: plan file not found: {plan_path}",
            err=True,
        )
        raise typer.Exit(code=1)

    # Load the plan doc
    try:
        doc = load_plan(resolved)
    except FrontmatterError as exc:
        typer.echo(
            f"fno plan brief: malformed frontmatter in {resolved}: {exc}",
            err=True,
        )
        raise typer.Exit(code=3)
    except (OSError, PermissionError) as exc:
        typer.echo(
            f"fno plan brief: cannot read {resolved}: {exc}",
            err=True,
        )
        raise typer.Exit(code=1)
    except Exception as exc:
        typer.echo(
            f"fno plan brief: unexpected error reading {resolved}: {exc}",
            err=True,
        )
        raise typer.Exit(code=3)

    # Build brief
    try:
        result = build_brief(
            doc,
            task_id=task,
            include_failure_modes=include_failure_modes.value,
            include_locked_decisions=include_locked_decisions.value,
        )
    except BriefParseError as exc:
        typer.echo(
            f"fno plan brief: malformed Execution Strategy YAML: {exc}",
            err=True,
        )
        raise typer.Exit(code=3)
    except BriefError as exc:
        typer.echo(
            f"fno plan brief: {exc}",
            err=True,
        )
        raise typer.Exit(code=2)

    # Emit output
    if format == _OutputFormat.json:
        typer.echo(json.dumps(result.to_json_dict(), indent=2))
    else:
        typer.echo(result.to_markdown())


@plan_app.command(
    "migrate-folder",
    help=(
        "Relocate a folder plan (dir + 00-INDEX.md + NN-*.md phases) into a "
        "single-doc plan beside it; archive the folder with an -archived "
        "suffix. Idempotent. --update-node repoints a node's plan_path and "
        "refuses dispatch-armed (ready/idea/claimed) nodes.\n\n"
        "Exit codes: 0 success/no-op, 1 folder or node not found, "
        "2 dispatch-armed refusal, 3 mid-write failure."
    ),
)
def migrate_folder(
    folder: str = typer.Argument(..., help="Folder-plan directory to migrate"),
    update_node: Optional[str] = typer.Option(
        None,
        "--update-node",
        help="Repoint this node's plan_path to the new doc (refuses ready/idea/claimed).",
    ),
) -> None:
    from fno.plan._migrate import MigrateError, migrate_folder as _migrate

    _EXIT = {"not-found": 1, "node-missing": 1, "dispatch-armed": 2}
    try:
        res = _migrate(folder, update_node=update_node)
    except MigrateError as exc:
        typer.echo(f"fno plan migrate-folder: {exc}", err=True)
        raise typer.Exit(code=_EXIT.get(exc.kind, 3))

    typer.echo(res.message)


@plan_app.command(
    "folder-audit",
    help=(
        "Count folder plans (00-INDEX.md dirs) owned by a non-terminal graph "
        "node (basename-joined plan_path, not frontmatter status). "
        "--non-terminal exits nonzero when the count is > 0. Fails toward "
        "defer (nonzero) on an unreadable graph or an unscannable plans dir."
    ),
)
def folder_audit(
    non_terminal: bool = typer.Option(
        False, "--non-terminal", help="Exit nonzero when the count is > 0."
    ),
    plans_dir_opt: Optional[str] = typer.Option(
        None, "--plans-dir", help="Plans dir to scan (default: resolved plans-content dir)."
    ),
) -> None:
    from fno.graph._constants import GRAPH_JSON
    from fno.graph.statuses import recompute_statuses
    from fno.graph.store import GraphCorruptError, _apply_graph_defaults, _read_json
    from fno.paths import plans_content_dir
    from fno.plan._folder_audit import scan

    plans_root = Path(plans_dir_opt) if plans_dir_opt else plans_content_dir()

    try:
        entries = recompute_statuses(_apply_graph_defaults(_read_json(GRAPH_JSON)))
    except (GraphCorruptError, OSError) as exc:
        typer.echo(
            f"fno plan folder-audit: graph.json unreadable ({exc}) - failing toward defer",
            err=True,
        )
        raise typer.Exit(code=1)

    owners = scan(plans_root, entries)
    if owners is None:
        typer.echo(
            f"fno plan folder-audit: cannot scan plans dir {plans_root} - failing toward defer",
            err=True,
        )
        raise typer.Exit(code=1)

    for o in owners:
        typer.echo(f"  {o.node_id} ({o.status}): {o.folder}")
    typer.echo(f"non-terminal folder-plan owners: {len(owners)}")

    if non_terminal and len(owners) > 0:
        raise typer.Exit(code=1)


@plan_app.command(
    "reconcile-status",
    help=(
        "Normalize drifted plan frontmatter status to the canonical vocabulary "
        "in place. Dry-run by default; pass --apply to write. Prints "
        "'N normalized, M archived, K skipped'."
    ),
)
def reconcile_status(
    plans_dir: Optional[str] = typer.Option(
        None, "--plans-dir", help="Plans dir to sweep (default: resolved plans-content dir)."
    ),
    apply: bool = typer.Option(
        False, "--apply", help="Write the changes (default: dry-run, report only)."
    ),
) -> None:
    """Sweep the plans dir, normalizing off-vocabulary/blank statuses."""
    from fno.paths import plans_content_dir
    from fno.plan.reconcile_status import sweep

    target = Path(plans_dir) if plans_dir else plans_content_dir()
    res = sweep(target, apply=apply)

    for path, old, new in res.changes:
        arrow = "->" if apply else "would ->"
        typer.echo(f"  {Path(path).name}: {old} {arrow} {new}")
    for warn in res.warnings:
        typer.echo(f"  ! {warn}", err=True)
    prefix = "" if apply else "[dry-run] "
    typer.echo(f"{prefix}{res.summary()}")
