"""fno runtime subcommand tree."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer

cli = typer.Typer(name="runtime", help="manage runtime workers and worktrees", no_args_is_help=True)


@cli.callback()
def _runtime_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag
    merge_json_flag(ctx, json_output)


# All stubs replaced - no more stub subcommands
_STUB_SUBCOMMANDS: list[str] = []


def _make_stub(name: str):
    def stub(ctx: typer.Context) -> None:
        json_output = bool(ctx.obj and ctx.obj.get("json", False))
        payload = {"command": f"runtime {name}", "status": "not-implemented"}
        if json_output:
            typer.echo(json.dumps(payload))
        else:
            typer.echo(f"runtime {name}: not-implemented")
        raise typer.Exit(code=0)

    stub.__name__ = name.replace("-", "_")
    return stub


for _sub in _STUB_SUBCOMMANDS:
    cli.command(name=_sub)(_make_stub(_sub))


# ---------------------------------------------------------------------------
# probe
# ---------------------------------------------------------------------------

@cli.command(name="probe")
def probe_cmd(
    ctx: typer.Context,
    adapter: str = typer.Option("claude-code", help="adapter name (unused in probe, reserved)"),
    plugin_path: Optional[Path] = typer.Option(
        None,
        "--plugin-path",
        help="path to plugin.json (default: .claude-plugin/plugin.json)",
    ),
    json_flag: bool = typer.Option(False, "--json", "-J", help="output JSON"),
) -> None:
    """Run preflight checks and report environment health."""
    from fno.runtime.probe import run_probe

    all_passed, checks = run_probe(plugin_path=plugin_path)

    payload = {
        "ok": all_passed,
        "checks": [c.to_dict() for c in checks],
    }

    typer.echo(json.dumps(payload))

    exit_code = 0 if all_passed else 4
    raise typer.Exit(code=exit_code)


# ---------------------------------------------------------------------------
# worktree
# ---------------------------------------------------------------------------

@cli.command(name="worktree")
def worktree_cmd(
    ctx: typer.Context,
    action: str = typer.Option(..., "--action", help="create | list | remove"),
    name: Optional[str] = typer.Option(None, "--name", help="worktree name"),
    base: str = typer.Option("main", "--base", help="base ref for create (default: main)"),
    prune_branch: bool = typer.Option(False, "--prune-branch", help="delete branch on remove"),
    json_flag: bool = typer.Option(False, "--json", "-J", help="output JSON"),
) -> None:
    """Manage git worktrees under ~/.fno/worktrees/{proj}-{name}/."""
    from fno.runtime.worktree import create_worktree, list_worktrees, remove_worktree

    try:
        if action == "create":
            if not name:
                typer.echo(json.dumps({"error": "--name required for create"}))
                raise typer.Exit(code=1)
            result = create_worktree(name=name, base=base)
        elif action == "list":
            result = {"worktrees": list_worktrees()}  # type: ignore[assignment]
        elif action == "remove":
            if not name:
                typer.echo(json.dumps({"error": "--name required for remove"}))
                raise typer.Exit(code=1)
            result = remove_worktree(name=name, prune_branch=prune_branch)
        else:
            typer.echo(json.dumps({"error": f"unknown action {action!r}; use create|list|remove"}))
            raise typer.Exit(code=1)
    except RuntimeError as exc:
        typer.echo(json.dumps({"error": str(exc)}))
        raise typer.Exit(code=1) from exc

    typer.echo(json.dumps(result))
    raise typer.Exit(code=0)


# ---------------------------------------------------------------------------
# register-worker
# ---------------------------------------------------------------------------

@cli.command(name="register-worker")
def register_worker_cmd(
    ctx: typer.Context,
    worker_id: str = typer.Option(..., "--id", help="unique worker ID"),
    task: str = typer.Option("", "--task", help="task description"),
    campaign: str = typer.Option("", "--campaign", help="campaign/plan identifier"),
    workers_file: Optional[Path] = typer.Option(
        None,
        "--workers-file",
        help="path to workers.jsonl (default: .fno/workers.jsonl)",
    ),
    json_flag: bool = typer.Option(False, "--json", "-J", help="output JSON"),
) -> None:
    """Register a worker manually in the workers registry (used after in-session skill dispatch)."""
    from fno.runtime.registry import register_worker

    entry = register_worker(
        worker_id=worker_id,
        task=task,
        campaign=campaign,
        workers_file=workers_file,
    )

    result = {"status": "registered", "worker_id": worker_id, "entry": entry}
    typer.echo(json.dumps(result))
    raise typer.Exit(code=0)


# ---------------------------------------------------------------------------
# reap-dead-workers
# ---------------------------------------------------------------------------

@cli.command(name="reap-dead-workers")
def reap_dead_workers_cmd(
    ctx: typer.Context,
    workers_file: Optional[Path] = typer.Option(
        None,
        "--workers-file",
        help="path to workers.jsonl (default: .fno/workers.jsonl)",
    ),
    artifacts_dir: Optional[Path] = typer.Option(
        None,
        "--artifacts-dir",
        help="directory containing ship-{session_id}.md artifacts",
    ),
    dry_run: bool = typer.Option(False, "--dry-run", "-N", help="report without mutating"),
    threshold: int = typer.Option(30, "--threshold", help="abandonment threshold in minutes"),
    json_flag: bool = typer.Option(False, "--json", "-J", help="output JSON"),
) -> None:
    """Reap abandoned workers (status started + stale heartbeat + no ship artifact)."""
    from fno.runtime.reap import reap_dead_workers

    report = reap_dead_workers(
        workers_file=workers_file,
        artifacts_dir=artifacts_dir,
        dry_run=dry_run,
        threshold_minutes=threshold,
    )

    typer.echo(json.dumps(report))
    raise typer.Exit(code=0)
