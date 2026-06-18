"""CLI surface for setup commands (`fno setup ...`).

Lives next to paths_cli.py for consistency; implementation lives in
the setup/ package (setup/migrate_paths.py etc.).
"""
from __future__ import annotations

from pathlib import Path
from typing import Callable, Optional

import typer

import fno.paths as _paths

app = typer.Typer(help="Setup commands (migration, doctor, etc.)")


@app.callback(invoke_without_command=True)
def setup_main(ctx: typer.Context) -> None:
    """Setup helpers. Bare ``fno setup`` lists the available subcommands.

    (With more than one subcommand Typer no longer auto-runs a lone command,
    so show help instead of erroring on a bare invocation.)
    """
    if ctx.invoked_subcommand is None:
        typer.echo(ctx.get_help())
        raise typer.Exit(0)


@app.command("plan")
def plan_cmd(
    advanced: bool = typer.Option(
        False, "--advanced", help="Include the 'advanced' tier, not just 'always'."
    ),
) -> None:
    """Emit the wizard question plan as JSON.

    The headless / scriptable path over the SAME schema walker the /fno:setup
    skill consumes, so the question set is derived from the model + registry,
    never hardcoded. By default emits only the ``always`` fields (the ~4-6 real
    per-project decisions); ``--advanced`` also includes ``advanced`` fields.
    """
    import json

    from fno.config import schema_gen

    raw = json.loads(schema_gen.wizard_plan())
    if not advanced:
        raw["fields"] = [f for f in raw["fields"] if f.get("tier") == "always"]
    typer.echo(json.dumps(raw, indent=2))


@app.command("migrate-paths")
def migrate_paths_cmd(
    force: bool = typer.Option(False, "--force", "-F", help="Re-run even if sentinel exists"),
) -> None:
    """Run path migration. Idempotent via ~/.fno/.path-migration-done sentinel."""
    from fno.setup.migrate_paths import run_migration

    raise typer.Exit(run_migration(force=force, settings_root=_paths.state_dir()))


def scaffold_post_merge(
    repo_root: Path,
    *,
    prompt_fn: Callable[[str, str], Optional[str]],
    confirm_fn: Callable[[str], bool],
    echo_fn: Callable[[str], None] = lambda _m: None,
    suggested: Optional[str] = None,
) -> dict:
    """Interactive-agnostic scaffold for ``config.post_merge.parking_lot_path``
    (and a ``config.project.id`` confirm) in this repo's ``.fno/settings.yaml``.

    The only writer of these keys (the oracle is read-only). Atomic + schema
    validated via ``set_config_value``; an invalid path (absolute / ``..``) is
    rejected and re-prompted (AC2-ERR). Suggested-but-editable, NEVER silently
    derived (AC2-UI). Idempotent for an already-ready repo (AC2-EDGE). A
    cancelled prompt writes nothing partial (AC2-FR).

    ``prompt_fn(message, default) -> str | None`` (None signals cancel);
    ``confirm_fn(message) -> bool``.
    """
    from fno.config.writer import ConfigSetError, set_config_value
    from fno.config_cli import post_merge_readiness

    repo_root = Path(repo_root)
    verdict = post_merge_readiness(repo_root)

    if verdict.status == "error":
        # The existing settings.yaml cannot be read; re-prompting would loop
        # forever (set_config_value fails on the same malformed file every
        # time). Refuse up front with the real cause.
        echo_fn(
            f"settings.yaml could not be read: {verdict.cause}. "
            "Fix it before scaffolding post-merge config."
        )
        return {"changed": False, "reason": "settings-error"}

    if verdict.status == "ready":
        echo_fn(
            f"post-merge already configured: parking_lot_path={verdict.parking_lot_path}"
        )
        if verdict.project_id:
            echo_fn(f"project.id={verdict.project_id}")
        if not confirm_fn("Update the existing parking_lot_path?"):
            return {
                "changed": False,
                "reason": "already-ready",
                "parking_lot_path": verdict.parking_lot_path,
            }

    project_id = verdict.project_id or repo_root.name
    if suggested is None:
        suggested = f"internal/{project_id}/backlog/parking-lot.md"
    echo_fn(
        "config.post_merge.parking_lot_path is repo-relative and is NOT derived "
        "from the project name - the vault AREA often differs from the project "
        "(e.g. example-pipeline -> internal/etl/backlog/parking-lot.md). The "
        "suggested value below is editable; only what you confirm is written."
    )

    written_path: Optional[str] = None
    while True:
        value = prompt_fn("config.post_merge.parking_lot_path", suggested)
        if value is None or not value.strip():
            return {"changed": False, "reason": "cancelled"}
        value = value.strip()
        try:
            set_config_value(
                "config.post_merge.parking_lot_path",
                value,
                scope="project",
                repo_root=repo_root,
            )
        except ConfigSetError as exc:
            if exc.exit_code == 2:
                # AC2-ERR: an invalid *value* (absolute / '..' path) -> re-prompt.
                echo_fn(f"rejected: {exc}")
                continue
            # exit_code 1 (unknown key / malformed existing file): structural,
            # not fixable by a different value. Abort instead of looping.
            echo_fn(f"cannot write config: {exc}")
            return {"changed": False, "reason": "config-error"}
        written_path = value
        break

    # project.id is scaffold-and-note only (never a warn trigger); confirm a
    # value if unset, defaulting to the repo basename.
    if not verdict.project_id:
        pid_val = prompt_fn(
            "config.project.id (optional; for clean provenance)", repo_root.name
        )
        if pid_val and pid_val.strip():
            try:
                set_config_value(
                    "config.project.id",
                    pid_val.strip(),
                    scope="project",
                    repo_root=repo_root,
                )
            except ConfigSetError as exc:
                echo_fn(f"project.id not set: {exc}")

    new_verdict = post_merge_readiness(repo_root)
    echo_fn(new_verdict.summary_line())
    return {
        "changed": True,
        "parking_lot_path": written_path,
        "status": new_verdict.status,
    }


@app.command("post-merge")
def post_merge_cmd() -> None:
    """Scaffold config.post_merge.parking_lot_path (+ project.id) for this repo.

    Prompts for the repo-relative parking-lot path the /fno:pr merged ritual
    writes to. Suggested-but-editable; the value is never silently derived.
    """
    from fno.config_cli import _repo_root

    def prompt_fn(message: str, default: str) -> Optional[str]:
        try:
            return typer.prompt(message, default=default)
        except typer.Abort:
            return None

    def confirm_fn(message: str) -> bool:
        try:
            return typer.confirm(message, default=False)
        except typer.Abort:
            return False

    result = scaffold_post_merge(
        _repo_root(),
        prompt_fn=prompt_fn,
        confirm_fn=confirm_fn,
        echo_fn=typer.echo,
    )
    if not result.get("changed") and result.get("reason") == "cancelled":
        typer.echo("setup post-merge: cancelled; nothing written.")
    raise typer.Exit(0)
