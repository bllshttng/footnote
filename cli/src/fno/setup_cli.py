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


# ---------------------------------------------------------------------------
# CLI-native interactive setup wizard (x-50f9, US4)
# ---------------------------------------------------------------------------

# Keys whose natural home is the project file, not the per-user global one. The
# wizard asks "global or this project?" for these and routes the write
# accordingly; everything else is global (the config model is global-first).
PROJECT_SCOPED_KEYS = (
    "config.post_merge.parking_lot_path",
    "config.project.id",
)


def _wizard_default_str(default: object) -> str:
    """Render a model default as the string shown in (and accepted from) the
    prompt. Lists become comma-separated (the ``set`` coercer re-parses them);
    None becomes empty (an Enter on a None-default field skips it).
    """
    if default is None:
        return ""
    if isinstance(default, bool):
        return "true" if default else "false"
    if isinstance(default, list):
        return ",".join(str(x) for x in default)
    if isinstance(default, dict):
        import json

        return json.dumps(default)
    return str(default)


def run_wizard(
    repo_root: Path,
    fields: list,
    *,
    prompt_fn: Callable[[str, str], Optional[str]],
    scope_fn: Callable[[str], str],
    echo_fn: Callable[[str], None] = lambda _m: None,
) -> dict:
    """Interactive-agnostic core of ``fno setup wizard``.

    For each field (from ``schema_gen.wizard_plan``) prompt for a value and
    write it through the validated ``set_config_value`` path. Defaults come from
    the model (the wizard never reads ``load_settings``, so the ``@lru_cache`` on
    it is irrelevant - no stale-cache pitfall). A rejected value (exit 2)
    re-prompts rather than aborting (AC4-ERR). Project-scoped keys ask scope via
    ``scope_fn`` and route to the project file (AC4-EDGE). Each write echoes the
    scope + path actually written (AC4-UI). Returning None from ``prompt_fn``
    (a Ctrl-C) stops the wizard, leaving every already-written key persisted and
    nothing partial for the in-flight key (AC4-FR).

    ``prompt_fn(question, default) -> str | None`` (None == cancel the wizard);
    ``scope_fn(key) -> "global" | "project"`` (asked only for project keys);
    returns ``{"written": [keys], "cancelled": bool}``.
    """
    from fno.config.writer import ConfigSetError, set_config_value

    written: list[str] = []
    for field in fields:
        key = field["path"]
        default = field.get("default")
        question = field.get("question") or key
        default_str = _wizard_default_str(default)
        scope = scope_fn(key) if key in PROJECT_SCOPED_KEYS else "global"
        repo = repo_root if scope == "project" else None

        while True:
            value = prompt_fn(question, default_str)
            if value is None:
                echo_fn("wizard cancelled; keys written so far are saved.")
                return {"written": written, "cancelled": True}
            value = value.strip()
            # An optional (None-default) field left blank is skipped, not written.
            if value == "" and default is None:
                break
            try:
                res = set_config_value(key, value, scope=scope, repo_root=repo)
            except ConfigSetError as exc:
                if exc.exit_code == 2:
                    echo_fn(f"  rejected: {exc}")
                    continue  # AC4-ERR: re-prompt, never abort
                echo_fn(f"  cannot write {key}: {exc}")
                break
            echo_fn(f"  set {key} = {res.value} ({res.scope}: {res.path})")
            written.append(key)
            break

    return {"written": written, "cancelled": False}


@app.command("wizard")
def wizard_cmd(
    advanced: bool = typer.Option(
        False, "--advanced", help="Also surface the 'advanced' tier, not just 'always'."
    ),
) -> None:
    """Interactive terminal setup wizard (the CLI twin of /fno:setup).

    Walks the schema-derived question plan (``always`` fields by default,
    ``--advanced`` adds the rest), prompting for each and writing it through the
    validated config writer - so setup works headless / CLI-only without an
    agent. Project-scoped keys ask whether to write global or just this project.
    """
    import json

    from fno.config import schema_gen
    from fno.config_cli import _repo_root

    fields = json.loads(schema_gen.wizard_plan())["fields"]
    if not advanced:
        fields = [f for f in fields if f.get("tier") == "always"]

    typer.echo("fno setup wizard - press Enter to accept each default.\n")

    def prompt_fn(message: str, default: str) -> Optional[str]:
        try:
            return typer.prompt(message, default=default)
        except typer.Abort:
            return None

    def scope_fn(key: str) -> str:
        try:
            local = typer.confirm(
                f"Write {key} to THIS project only? (No = global)", default=False
            )
        except typer.Abort:
            return "global"
        return "project" if local else "global"

    result = run_wizard(
        _repo_root(),
        fields,
        prompt_fn=prompt_fn,
        scope_fn=scope_fn,
        echo_fn=typer.echo,
    )
    n = len(result["written"])
    if result.get("cancelled"):
        typer.echo(f"\nwizard cancelled after writing {n} key(s).")
    else:
        typer.echo(
            f"\nwizard complete: {n} key(s) written. "
            "Run `fno config doctor` to verify."
        )
    raise typer.Exit(0)


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
