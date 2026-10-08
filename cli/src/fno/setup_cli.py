"""CLI surface for setup commands (`fno config setup ...`).

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
    """Setup helpers. Bare ``fno config setup`` lists the available subcommands.

    (With more than one subcommand Typer no longer auto-runs a lone command,
    so show help instead of erroring on a bare invocation.)
    """
    if ctx.invoked_subcommand is None:
        typer.echo(ctx.get_help())
        raise typer.Exit(0)


@app.command("cli-hooks")
def cli_hooks_cmd(
    codex: bool = typer.Option(True, "--codex/--no-codex", help="Install the Codex hook."),
    gemini: bool = typer.Option(True, "--gemini/--no-gemini", help="Install the Gemini hook."),
    gemini_settings: Optional[Path] = typer.Option(
        None, "--gemini-settings", help="Override the Gemini settings.json path."
    ),
    codex_config: Optional[Path] = typer.Option(
        None, "--codex-config", help="Override the Codex config.toml path."
    ),
    claude: bool = typer.Option(
        True, "--claude/--no-claude", help="Install the Claude WorktreeRemove hook."
    ),
    claude_settings: Optional[Path] = typer.Option(
        None, "--claude-settings", help="Override the Claude settings.json path."
    ),
    repair_only: bool = typer.Option(
        False,
        "--repair-only",
        help="Only re-point an already-wired Claude hook whose script has moved.",
    ),
) -> None:
    """Wire footnote's user-level CLI hooks.

    Codex and Gemini read the SessionStart context hook from user-level config
    (`~/.codex/config.toml`, `~/.gemini/settings.json`) that footnote cannot ship
    as a repo file. Claude gets its SessionStart hook from the plugin manifest,
    but needs WorktreeRemove in `~/.claude/settings.json` so that `claude rm`
    (which runs with no agent session, and so never loads plugin hooks) can
    clean up hook-created worktrees.

    Each merge is idempotent, backs up the file first, and never clobbers your
    other hooks.

    Codex treats the hook as untrusted until you approve it, so after this runs
    you must trust the footnote SessionStart hook in Codex before it fires.
    """
    _install_cli_hooks(
        codex=codex,
        gemini=gemini,
        gemini_settings=gemini_settings,
        codex_config=codex_config,
        codex_hooks_json=None,
        migrate_legacy_hooks_json=False,
        claude=claude,
        claude_settings=claude_settings,
        repair_only=repair_only,
    )


@app.command("cli-hooks-codex")
def cli_hooks_codex_cmd(
    codex_config: Optional[Path] = typer.Option(
        None, "--codex-config", help="Override the Codex config.toml path."
    ),
    codex_hooks_json: Optional[Path] = typer.Option(
        None, "--codex-hooks-json", help="Override the legacy Codex hooks.json path."
    ),
    migrate_legacy_hooks_json: bool = typer.Option(
        False,
        "--migrate-legacy-hooks-json",
        help="Back up and remove only footnote-owned legacy JSON SessionStart hooks.",
    ),
) -> None:
    """Wire only the Codex SessionStart hook."""
    _install_cli_hooks(
        codex=True,
        gemini=False,
        gemini_settings=None,
        codex_config=codex_config,
        codex_hooks_json=codex_hooks_json,
        migrate_legacy_hooks_json=migrate_legacy_hooks_json,
    )


@app.command("github-cli")
def github_cli_cmd() -> None:
    """Install or refresh the `gh` proxy used by Footnote workers."""
    from fno.setup.github_cli import ensure_proxy

    try:
        result = ensure_proxy()
    except (FileNotFoundError, OSError, RuntimeError) as exc:
        typer.echo(f"github-cli error: {exc}", err=True)
        raise typer.Exit(1) from exc
    action = "installed" if result.changed else "verified"
    backup = f" backup={result.backup}" if result.backup else ""
    typer.echo(
        f"github-cli {action}: proxy={result.proxy} delegate={result.delegate}{backup}"
    )


def _install_cli_hooks(
    *,
    codex: bool,
    gemini: bool,
    gemini_settings: Optional[Path],
    codex_config: Optional[Path],
    codex_hooks_json: Optional[Path],
    migrate_legacy_hooks_json: bool,
    claude: bool = False,
    claude_settings: Optional[Path] = None,
    repair_only: bool = False,
) -> None:
    import os

    from fno.setup.cli_hooks import (
        install_claude_worktree_remove_hook,
        install_codex_hook,
        install_gemini_hook,
    )

    try:
        entry = _paths.resolve_plugin_script("hooks/session-start.sh")
        if not entry.is_file():
            raise FileNotFoundError(entry)
    except Exception as exc:  # noqa: BLE001 - surface a clear message, never trace
        typer.echo(
            f"error: could not locate the installed footnote hooks dir ({exc}). "
            "Run from a footnote-enabled session, or set CLAUDE_PLUGIN_ROOT.",
            err=True,
        )
        raise typer.Exit(code=1) from exc
    command = str(entry)

    def _safe_expand(p: Path) -> Path:
        # Expand a user-supplied override (`~` is not shell-expanded in a
        # non-interactive call); degrade to the raw path in restricted envs.
        try:
            return p.expanduser()
        except (RuntimeError, OSError, ValueError):
            return p

    any_change = False
    needs_trust = False
    failures: list[str] = []

    if gemini and not repair_only:
        gpath = _safe_expand(gemini_settings) if gemini_settings else (
            Path.home() / ".gemini" / "settings.json"
        )
        res = install_gemini_hook(command, settings_path=gpath)
        any_change = any_change or res.changed
        if res.note:
            typer.echo(f"gemini: error: {res.note} ({res.path})", err=True)
            failures.append(res.note)
        elif res.already_present:
            typer.echo(f"gemini: already wired ({res.path})")
        else:
            bak = f"; backed up {res.backup.name}" if res.backup else ""
            typer.echo(f"gemini: wired SessionStart -> {command} ({res.path}{bak})")

    if codex and not repair_only:
        chome = os.environ.get("CODEX_HOME")
        cpath = _safe_expand(codex_config) if codex_config else (
            (Path(chome).expanduser() if chome else Path.home() / ".codex")
            / "config.toml"
        )
        legacy_path = (
            _safe_expand(codex_hooks_json)
            if codex_hooks_json
            else cpath.parent / "hooks.json"
        )
        res = install_codex_hook(
            command,
            config_path=cpath,
            hooks_json_path=legacy_path,
            migrate_legacy_hooks_json=migrate_legacy_hooks_json,
        )
        if res.error:
            typer.echo(f"codex: error: {res.error} ({res.path})", err=True)
            failures.append(res.error)
        else:
            any_change = any_change or res.changed
            needs_trust = needs_trust or res.needs_trust
            if res.note:
                legacy_bak = (
                    f"; backed up {res.legacy_backup.name}" if res.legacy_backup else ""
                )
                typer.echo(f"codex: {res.note}{legacy_bak} ({res.path})")
            elif res.already_present:
                typer.echo(f"codex: already wired ({res.path})")
            else:
                bak = f"; backed up {res.backup.name}" if res.backup else ""
                typer.echo(f"codex: wired SessionStart -> {command} ({res.path}{bak})")

    if claude:
        try:
            # _durable: this path is persisted into GLOBAL settings, so it must
            # not be a worktree path that dies when the worktree is archived.
            wt_entry = _paths.resolve_plugin_script_durable("hooks/worktree-remove.sh")
            if not wt_entry.is_file():
                raise FileNotFoundError(wt_entry)
        except Exception as exc:  # noqa: BLE001 - surface a clear message, never trace
            typer.echo(f"claude: error: could not locate worktree-remove.sh ({exc})", err=True)
            failures.append(str(exc))
        else:
            cpath_claude = _safe_expand(claude_settings) if claude_settings else (
                Path.home() / ".claude" / "settings.json"
            )
            res = install_claude_worktree_remove_hook(
                str(wt_entry), settings_path=cpath_claude, repair_only=repair_only
            )
            any_change = any_change or res.changed
            if repair_only and not res.changed and res.error is None:
                pass  # nothing wired, or already healthy - stay silent
            elif res.error:
                typer.echo(f"claude: error: {res.error} ({res.path})", err=True)
                failures.append(res.error)
            elif res.already_present:
                typer.echo(f"claude: already wired ({res.path})")
            else:
                bak = f"; backed up {res.backup.name}" if res.backup else ""
                fixed = f"; {res.note}" if res.note else ""
                typer.echo(
                    f"claude: wired WorktreeRemove -> {wt_entry} ({res.path}{bak}{fixed})"
                )

    if needs_trust:
        typer.echo(
            "\ncodex: the hook is UNTRUSTED until you approve it. Start Codex and "
            "approve the footnote SessionStart hook, then confirm it fires."
        )
    if failures:
        raise typer.Exit(1)
    if not any_change:
        typer.echo("\nNothing to do (hooks already wired).")
    return None


def offer_cli_hooks(
    *,
    confirm_fn: Callable[[str], bool],
    install_fn: Callable[..., None] = _install_cli_hooks,
) -> bool:
    """Offer the combined user-level hook installer."""
    if not confirm_fn(
        "Wire footnote hooks into your Codex, Gemini, and Claude user config?"
    ):
        return False

    install_fn(
        codex=True,
        gemini=True,
        gemini_settings=None,
        codex_config=None,
        codex_hooks_json=None,
        migrate_legacy_hooks_json=False,
        claude=True,
        claude_settings=None,
    )
    return True


def report_machine_blockers(*, echo_fn: Callable[[str], None]) -> list[str]:
    """Run the machine doctor at the end of first run and name what blocks.

    The wizard configures PREFERENCES; `fno doctor` knows the MACHINE. Nothing
    connected them, so a new user met every environmental hurdle later as an
    unattributable mid-loop failure. Composition of two verbs that both exist.

    Calls `fno.doctor.build_report()` and `fno.doctor._blockers()` in process
    -- never shells out. A subprocess resolves the DEPLOYED binary on PATH,
    not this source, so on the exact stale install that is itself blocker #1
    the wizard would run the wrong bytes to report it.

    Never raises and never fails the wizard: a machine finding the user has
    just been told about must not look like setup failing.
    """
    try:
        from fno.doctor import _blockers, _emit_blockers, build_report

        result = build_report()
        blockers = _blockers(result)
    except Exception as exc:
        echo_fn(f"fno doctor: could not run ({exc}); skipping machine check.")
        return []

    if blockers:
        echo_fn("")
        _emit_blockers(blockers, echo_fn=echo_fn)
    else:
        echo_fn("\nfno doctor: this machine looks clean.")
    return blockers


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


def offer_recommended_rules(
    *,
    confirm_fn: Callable[[str], bool],
    echo_fn: Callable[[str], None] = lambda _m: None,
    source_dir: Optional[Path] = None,
    target_dir: Optional[Path] = None,
) -> dict:
    """Opt-in offer to install footnote's recommended claude-code rules.

    Interactive-agnostic core (the CLI passes typer confirm/echo). Prompts once,
    default No; installs only on explicit yes. ``source_dir`` defaults to the
    shipped plugin ``rules/`` dir and ``target_dir`` to ``~/.claude/rules/`` -
    both overridable for tests. Returns ``{"installed": bool, "results": [...]}``.
    """
    import os

    from fno.setup.recommended_rules import (
        default_rules_source,
        install_recommended_rules,
        summarize,
    )

    src = source_dir or default_rules_source()
    if src is None or not Path(src).is_dir():
        # No pack resolvable anywhere: nothing to offer, say nothing.
        return {"installed": False, "results": []}

    if not confirm_fn(
        "Install footnote's recommended claude-code rules into ~/.claude/rules/?"
    ):
        return {"installed": False, "results": []}

    # os.path.expanduser (not Path.home()) so a headless / sandboxed setup run
    # with an unset HOME degrades instead of raising RuntimeError (PR #146 gemini).
    tgt = target_dir or (Path(os.path.expanduser("~")) / ".claude" / "rules")
    results = install_recommended_rules(Path(src), Path(tgt))
    echo_fn(summarize(results))
    return {"installed": True, "results": results}


def offer_prompt_provenance(
    *,
    confirm_fn: Callable[[str], bool],
    echo_fn: Callable[[str], None] = lambda _m: None,
    snippet_path: Optional[Path] = None,
    target_toml: Optional[Path] = None,
    shell_snippet_path: Optional[Path] = None,
    shell_rc: Optional[Path] = None,
) -> dict:
    """Opt-in offer to render fno pane provenance in the user's prompt.

    Two renderers, each prompted separately (default No), so the feature never
    assumes starship: a starship custom module AND a portable bash/zsh ``$PS1``
    segment. Both append idempotently and never rewrite the user's files. On
    decline the snippet path is printed for manual use. (fno's own mux status
    row is the config-free surface - a separate follow-up, not offered here.)
    Interactive-agnostic (the CLI passes typer confirm/echo). Returns
    ``{"starship": StarshipResult | None, "shell": StarshipResult | None}``."""
    from fno.setup.starship import (
        default_shell_rc,
        default_shell_snippet_source,
        default_snippet_source,
        default_starship_config,
        install_shell_source_line,
        install_starship_module,
        summarize as starship_summarize,
        summarize_shell,
    )

    out: dict = {"starship": None, "shell": None}

    # Starship users: append the custom module.
    star_src = snippet_path or default_snippet_source()
    if star_src is not None and Path(star_src).is_file():
        if confirm_fn("Add fno provenance to your starship prompt (starship.toml)?"):
            res = install_starship_module(
                Path(star_src), Path(target_toml or default_starship_config())
            )
            echo_fn(starship_summarize(res))
            out["starship"] = res
        else:
            echo_fn(f"  starship: skipped - module at {star_src}")

    # Everyone else: the portable shell-rc segment (no prompt engine).
    sh_src = shell_snippet_path or default_shell_snippet_source()
    if sh_src is not None and Path(sh_src).is_file():
        if confirm_fn(
            "Or add a portable provenance segment to your bash/zsh prompt (no starship)?"
        ):
            res = install_shell_source_line(
                Path(sh_src), Path(shell_rc or default_shell_rc())
            )
            echo_fn(summarize_shell(res))
            out["shell"] = res
        else:
            echo_fn(f"  shell prompt: skipped - snippet at {sh_src} (source it to use)")

    return out


@app.command("wizard")
def wizard_cmd() -> None:
    """Retired: the one setup verb is ``fno config setup run``.

    Both layers (global and project) run from one step table there; the
    terminal wizard this command used to drive is deleted. ``--yes`` (or a
    non-TTY stdin, or an agent session env) takes every recommended default
    without prompting; ``/fno:setup`` asks the same steps in chat.
    """
    typer.echo(
        "fno config setup wizard is retired: run `fno config setup run` "
        "(add --yes for the no-prompt path, or use /fno:setup in chat)."
    )
    raise typer.Exit(0)


@app.command("migrate-paths")
def migrate_paths_cmd(
    force: bool = typer.Option(False, "--force", "-F", help="Re-run even if sentinel exists"),
) -> None:
    """Run path migration. Idempotent via ~/.fno/.path-migration-done sentinel."""
    from fno.setup.migrate_paths import run_migration

    raise typer.Exit(run_migration(force=force, settings_root=_paths.state_dir()))


@app.command("migrate-config")
def migrate_config_cmd() -> None:
    """Convert legacy settings.yaml -> flat config.toml (hard cut).

    Walks the settings candidate chain (worktree, canonical, global), writes an
    equivalent flat config.toml sibling for each settings.yaml, and deletes the
    yaml only after the toml is durably written (atomic temp+rename). Idempotent:
    a location that already has a config.toml is left untouched. Comments in a
    hand-tuned settings.yaml are NOT carried across the round-trip.
    """
    from fno.config import run_config_migration

    results = run_config_migration()
    migrated = [p for p, action in results if action == "migrated"]
    for p, action in results:
        if action == "migrated":
            typer.echo(f"migrated -> {p} (comments not carried over)")
        elif action == "already-migrated":
            typer.echo(f"already migrated: {p}")
    if not migrated:
        typer.echo("nothing to migrate (already on config.toml).")
    else:
        typer.echo(f"migrated {len(migrated)} config file(s).")


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

    Prompts for the repo-relative parking-lot path the /fno:ship pr merged ritual
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
