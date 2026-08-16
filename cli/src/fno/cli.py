"""fno CLI entry point (formerly `fno`, kept as deprecated alias).

This module is the top-level Typer app.  Sub-apps and individual commands
that live in other ``fno.*`` modules are registered lazily via the
``LAZY_SUBCOMMANDS`` map below: their modules are not imported at startup,
only when the corresponding command is actually invoked.

Why: ``fno --help`` runs ~30 times per target phase.  Eager imports of every
sub-app paid for every call dominated startup time (~225ms p50).  The lazy
loader brings that under 160ms (>=30% drop) by deferring imports to the
moment the sub-app is invoked.  See ``fno._lazy_group`` for the
mechanism and ``cli/benchmarks/measure_cli_help.py`` for measurement.

Commands defined inline below (``help``, ``review``) stay eager
because they live in this file and have no module body to defer.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any, Optional

import click
import typer

from fno._lazy_group import make_lazy_group_cls


# ---------------------------------------------------------------------------
# Lazy subcommand map
# ---------------------------------------------------------------------------
# Each entry is ``name: (import_path, short_help)`` or
# ``name: (import_path, short_help, options)``.
#
# - ``import_path``: ``"module.path:attr"``
# - ``short_help``: the line shown next to the command in ``fno --help``
# - ``options``: dict; supports ``{"hidden": True}`` to hide from listings
#
# Short-help strings are kept in sync with the underlying sub-app's own
# help text.  They are stored here so ``fno --help`` can render them
# without importing the module.  When you change a sub-app's help, update
# the matching entry here too.

# Tier discipline (x-71b6, "In-N-Out menu"): `fno --help` advertises a small
# curated menu; everything else is hidden (invocable, just not listed). The
# advertised set is `help`, `setup`, `backlog`, `agents`, `whoami`, `doctor`,
# `test`, `config`, `update` (9). `help` / `cost` / `review` are eager inline
# commands above; `cost` and `review` carry `hidden=True` on their decorators.
# Everything hidden here is one `fno help --all` away. New verbs default to
# hidden; promotion to the menu is a deliberate, lint-gated act (see fno lint).
LAZY_SUBCOMMANDS: dict[str, tuple[str, str] | tuple[str, str, dict[str, Any]]] = {
    # Sub-apps (Typer instances) -----------------------------------------
    "state": ("fno.state.cli:cli", "manage fno state files", {"hidden": True}),
    "target": (
        "fno.target_cli:target_app",
        "Target session bootstrap (fno target init)",
        {"hidden": True},
    ),
    "backlog": ("fno.graph.cli:cli", "Feature graph management"),
    "autonomy": (
        "fno.autonomy_cli:autonomy_app",
        "Inspect every path that can start a session without an operator asking.",
        {"hidden": True},
    ),
    "runtime": ("fno.runtime.cli:cli", "manage runtime workers and worktrees", {"hidden": True}),
    "worker": ("fno.worker.cli:cli", "manage delivery worker phases", {"hidden": True}),
    "event": ("fno.events.cli:cli", "emit and audit events", {"hidden": True}),
    "approvals": (
        "fno.approvals.cli:approvals_app",
        "Inspect and decide pending approvals for consequential effects.",
        {"hidden": True},
    ),
    "delivery": (
        "fno.delivery.cli:delivery_app",
        "Evaluate generic delivery evidence.",
        {"hidden": True},
    ),
    "plugins": (
        "fno.plugins.cli:plugins_app",
        "Install, verify, activate, and inspect function packs.",
        {"hidden": True},
    ),
    "mail": (
        "fno.mail.cli:mail_app",
        "Durable polled mailbox: send/unread/ack/reply/drain/status.",
        {"hidden": True},
    ),
    "mcp": (
        "fno.mcp.cli:mcp_app",
        "MCP sidecar client verbs (send an envelope to a channel).",
        {"hidden": True},
    ),
    "agents": ("fno.agents.cli:agents_app", "Cross-CLI agent dispatch (claude / codex / gemini)."),
    "plan": ("fno.plan:plan_app", "Plan frontmatter stamping (in-package)", {"hidden": True}),
    "pr": ("fno.pr:pr_app", "PR utilities (wraps scripts/lib/pr-*.sh)", {"hidden": True}),
    "stub-manifest": (
        "fno.stub_manifest:stub_manifest_app",
        "Stub-manifest for contract-tier dependents (emit/validate/check-pr).",
        {"hidden": True},
    ),
    "status-fanout": (
        "fno.status_fanout:status_fanout_app",
        "Sweep events.jsonl and route status events to configured sinks (tick).",
        {"hidden": True},
    ),
    "bundle": ("fno.bundle:bundle_app", "Skill bundle build + lint.", {"hidden": True}),
    "lint": ("fno.lint_cli:lint", "Repository lint checks", {"hidden": True}),
    "claim": ("fno.claims.cli:cli", "Work-claim coordination primitive", {"hidden": True}),
    "decide": (
        "fno.decide.cli:decide_app",
        "Record and recover durable operator decisions.",
        {"hidden": True},
    ),
    "resume": (
        "fno.resume.cli:cli",
        "Durable typed resume receipts (evidence, never write authority)",
        {"hidden": True},
    ),
    "carveout": (
        "fno.carveout:carveout_app",
        "Capture left-out work (deferred decisions, out-of-scope bugs) for retro-triage.",
        {"hidden": True},
    ),
    "outstanding": (
        "fno.outstanding:outstanding_app",
        "What is waiting on a human: unharvested carve-outs and open questions.",
        {"hidden": True},
    ),
    "annotate": (
        "fno.annotate:annotate_app",
        "Record an operator review finding against a node (add/list/resolve); gates loop-check.",
        {"hidden": True},
    ),
    "retro": (
        "fno.retro.cli:retro_app",
        "Consume retro-triage triggers; file left-out work as backlog nodes.",
        {"hidden": True},
    ),
    "think": (
        "fno.provenance.cli:think_app",
        "Think inspection and explicit conversational dispatch.",
        {"hidden": True},
    ),
    "phase": (
        "fno.phase:phase_app",
        "Phase utilities (kill-check via the fno-agents binary)",
        {"hidden": True},
    ),
    "executor": (
        "fno.executor:executor_app",
        "Executor resolution (locked-decision parser + surface inference)",
        {"hidden": True},
    ),
    "roles": (
        "fno.roles.cli:roles_app",
        "Inspect bounded business-role definitions and resolutions.",
        {"hidden": True},
    ),
    "config": ("fno.config_cli:app", "Configuration management"),
    "notify": (
        "fno.notify:notify_app",
        "OS notification helper (in-package; macOS osascript / Linux notify-send)",
        {"hidden": True},
    ),
    "paths": ("fno.paths_cli:app", "Path resolution helpers", {"hidden": True}),
    "setup": ("fno.setup_cli:app", "Interactive settings.yaml wizard"),
    "tokens": ("fno.tokens:app", "Token usage tracking", {"hidden": True}),
    "codemap": ("fno.codemap_cli:app", "Codebase map management", {"hidden": True}),
    "worktree": ("fno.worktree_cli:app", "Worktree management", {"hidden": True}),
    "route": (
        "fno.route_cli:route_app",
        "Provider route lanes: ls / set / unset / env (GLM build lane).",
        {"hidden": True},
    ),
    "posture": (
        "fno.posture_cli:posture_app",
        "Apply attended/autonomous stance as ordinary config keys (generator, not a layer).",
        {"hidden": True},
    ),
    "evals": (
        "fno.evals.cli:evals_app",
        "Golden-task efficacy evals (run / report / diff)",
        {"hidden": True},
    ),
    "observer": (
        "fno.observer.cli:observer_app",
        "Skill eval over a recorded corpus (sweep / replay).",
        {"hidden": True},
    ),
    "pr-watch": (
        "fno.pr_watch.cli:cli",
        "PR-state watcher: auto-fire /pr check + /pr merged for open-PR backlog nodes",
        {"hidden": True},
    ),
    "loops": (
        "fno.loops:loops_app",
        "Loop level config + pause-all kill switch (pause-all/resume-all/status/ls)",
        {"hidden": True},
    ),
    "skill-diff": (
        "fno.skill_diff.cli:skill_diff_app",
        "Skill-diff proposer: observer failure patterns -> cited SKILL.md diff -> PR (tick/reconcile).",
        {"hidden": True},
    ),
    # Individual commands (plain functions wrapped as single-command apps) -
    "whoami": (
        "fno.agent.cli:whoami_command",
        "Operating-stack summary: project + fleet + walker + session + harness.",
    ),
    "status": (
        "fno.agent.cli:status_command",
        "Session gate satisfaction + bounded events tail + inconsistencies.",
        {"hidden": True},
    ),
    "context": (
        "fno.context_probe:context_command",
        "Context-window usage for this session or a given transcript (hook door).",
        {"hidden": True},
    ),
    "doctor": (
        "fno.doctor:doctor_command",
        "Diagnose installed-vs-source fno skew (network-free).",
    ),
    "done": ("fno.done.cli:done_command", "Mark a backlog node as done.", {"hidden": True}),
    "research": (
        "fno.research:research_command",
        "Retrieve + store: ddgs backbone -> self-fetch -> sources.jsonl.",
        {"hidden": True},
    ),
    "scoreboard": (
        "fno.scoreboard.cli:scoreboard_command",
        "Read-only telemetry: stop-cause, spend, autonomy, survival, coverage.",
        {"hidden": True},
    ),
    "test": (
        "fno.test_cmd:test_command",
        "Run pytest honestly: worktree-pinned PYTHONPATH, rtk-bypassed, real exit code.",
    ),
    "update": ("fno.update:update_command", "Reinstall fno from its source directory."),
    "upgrade": (
        "fno.update:update_command",
        "Reinstall fno from its source directory.",
        {"hidden": True},
    ),
    "restart": (
        "fno.restart:restart_command",
        "Restart running fno processes (agents daemon; mux with --mux) onto fresh builds.",
        {"hidden": True},
    ),
    "dispatch": (
        "fno.dispatch:dispatch_app",
        "Grab one ready node into a mux pane (mux leader+g shells `dispatch one`).",
        {"hidden": True},
    ),
}

# T1 actions are parsed by the group dispatcher and keep their existing typing.
# These selected actions remain registered leaves as deliberate readability
# slack; the checked-in allocation and its reference costs live in
# scripts/ci/verb-collapse-map.tsv.
COLLAPSE_KEEP: dict[str, set[str]] = {
    "agents": {"ask", "list", "logs", "loop", "loop-check", "needs", "resume", "rm", "spawn"},
    "annotate": {"list"},
    "approvals": set(),
    "backlog": {"advance", "done", "get", "next", "queued", "reconcile", "update"},
    "bundle": set(),
    "carveout": set(),
    "claim": {"release"},
    "config": {"accounts", "get", "set"},
    "dispatch": set(),
    "evals": set(),
    "event": {"emit"},
    "loops": {"resume-all"},
    "mail": {"send"},
    "observer": set(),
    "paths": set(),
    "plan": set(),
    "plugins": set(),
    "pr": {"merge"},
    "pr-watch": set(),
    "resume": set(),
    "retro": set(),
    "roles": set(),
    "route": set(),
    "runtime": set(),
    "setup": set(),
    "skill-diff": set(),
    "state": set(),
    "stub-manifest": set(),
    "target": {"init"},
    "think": set(),
    "worker": set(),
    "worktree": set(),
}

for _group_name, _keep in COLLAPSE_KEEP.items():
    _entry = LAZY_SUBCOMMANDS[_group_name]
    _import_path, _short_help = _entry[:2]
    _options = dict(_entry[2]) if len(_entry) == 3 else {}
    _options["collapse_keep"] = sorted(_keep)
    LAZY_SUBCOMMANDS[_group_name] = (_import_path, _short_help, _options)


# ---------------------------------------------------------------------------
# Helpers that defer their own imports until needed
# ---------------------------------------------------------------------------


def _load_v2_config_flag(repo_root: Path) -> bool:
    """Read ``v2_enabled`` from the project's config (config.toml, else legacy
    settings.yaml).

    Returns False on any read/parse failure - v2 is strictly opt-in.
    Local settings win over the per-user global file.

    Imports are deferred so the cli module body stays cheap to load.
    """
    from fno.config import config_read_candidates, read_config_flat

    def _flag_set(path: Path) -> bool:
        try:
            return path.is_file() and read_config_flat(path).get("v2_enabled") is True
        except Exception:
            return False

    # Project-local candidates first - resolving them never raises - so a local
    # config.toml wins even when the global fallback is malformed. config_file()
    # is resolved only after, and guarded, since it can raise on a bad global
    # config; that must fail open (return False), never shadow the local file.
    if any(_flag_set(p) for p in config_read_candidates([repo_root / ".fno" / "settings.yaml"])):
        return True
    try:
        from fno import paths as _paths

        fallback = config_read_candidates([_paths.config_file()])
    except Exception:
        return False
    return any(_flag_set(p) for p in fallback)


def _warn_deprecated_alias_if_needed() -> None:
    # Exactly one command post-rename (`fno`); no deprecated alias to warn about.
    return


def _check_migration() -> None:
    """Fast-path stat check; runs migration on first invocation post-upgrade.

    The try/except Exception: pass is intentional - this migration is a
    best-effort startup convenience, not load-bearing.  Hard-fail path
    remains ``fno setup migrate-paths`` (called explicitly).

    Guarded by FNO_SKIP_MIGRATION=1 (explicit opt-out) or PYTEST_CURRENT_TEST
    (pytest sets this automatically in all test processes) for isolation.
    """
    if os.environ.get("FNO_SKIP_MIGRATION") == "1":
        return
    if os.environ.get("PYTEST_CURRENT_TEST"):
        return
    try:
        from fno import paths as _paths

        sentinel = _paths.state_dir() / ".path-migration-done"
        if sentinel.exists():
            return
        # Stale .tmp cleanup is intentionally NOT done here (Finding F /
        # round 5+6 on the original migration PR). Deleting .tmp files
        # outside the lock creates a race: process A cleans stale tmps,
        # process B deletes A's in-flight tmp, then A's os.replace() fails.
        # run_migration() performs the cleanup INSIDE its FileLock, which
        # serializes it.
        from fno.setup.migrate_paths import run_migration

        run_migration(settings_root=_paths.state_dir())
    except Exception:  # noqa: BLE001
        # Never block normal fno commands on migration failure - silent fallback.
        pass


# ---------------------------------------------------------------------------
# App definition
# ---------------------------------------------------------------------------


class _HonestMenuGroup(make_lazy_group_cls(LAZY_SUBCOMMANDS)):  # type: ignore[misc]
    """Root group; appends the honest verb-count line to `fno --help`.

    The curated menu is small on purpose (menu-caps). Without a count it reads
    as the whole surface, so a reader (human or agent) concludes a missing verb
    does not exist and works around its absence. This line states how many
    top-level verbs really exist and where the rest are. The count is read from
    the registry at render time - lazy entries resolve to a hidden-aware stub
    with no module import, so `fno --help` stays fast and works from a bare
    install. The leaf count lives in scripts/ci/verb-baseline.txt (and the
    `fno lint verb-ratchet` output) for anyone who wants the full surface.
    """

    def format_help(self, ctx: click.Context, formatter: click.HelpFormatter) -> None:
        super().format_help(ctx, formatter)
        names = self.list_commands(ctx)
        visible = 0
        for n in names:
            cmd = self.get_command(ctx, n)
            if cmd is not None and not getattr(cmd, "hidden", False):
                visible += 1
        formatter.write_paragraph()
        formatter.write_text(
            f"Showing {visible} of {len(names)} top-level verbs, plus the Rust "
            f"front's `mux` and `version`. Hidden verbs are invocable: run "
            f"`fno help --all` for every command, `fno mux` for pane control."
        )


app = typer.Typer(
    name="fno",
    help="CLI for the footnote autonomous delivery pipeline",
    no_args_is_help=True,
    invoke_without_command=True,
    add_completion=False,
    cls=_HonestMenuGroup,
    # Both of these keep `typer.rich_utils` off the error path. typer defers that
    # import to exception time in TWO places -- `typer.main.except_hook` (gated by
    # pretty_exceptions_enable) and the ClickException arm of `typer.core._main`
    # (gated by rich_markup_mode) -- so the error-REPORTING path is itself a
    # first-time import. When what it is reporting is "a lazy subcommand import
    # failed because uv was reinstalling this package underneath us", that import
    # fails too and the operator gets `cannot import name 'rich_utils' from
    # 'typer'` instead of the cause. Setting only one of these leaves the other
    # path live.
    # Not free of consequence, and both consequences are wanted: help and errors
    # render as plain click rather than rich boxes. Every lazily-loaded subgroup
    # ALREADY passes rich_markup_mode=None (see _lazy_group._load_real), so this
    # makes the top level consistent with the rest of the CLI instead of an
    # exception to it, and it stops `fno --help` paying the 237ms rich_utils
    # import that the lazy group exists to avoid.
    pretty_exceptions_enable=False,
    rich_markup_mode=None,
)


@app.callback()
def callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(False, "--json", "-J", help="output JSON where supported"),
    version: bool = typer.Option(False, "--version", help="show version and exit", is_eager=True),
) -> None:
    _check_migration()
    _warn_deprecated_alias_if_needed()
    ctx.ensure_object(dict)
    ctx.obj["json"] = json_output
    if version:
        # Read version directly without triggering ``fno`` package
        # attribute lookup -- the package is already imported (we are inside
        # ``fno.cli``), so this is free.
        from fno import __version__

        typer.echo(f"fno {__version__}")
        raise typer.Exit(code=0)


# Shorthand legend (short-flags Phase 3, ab-a04f3f1a). The convention's
# source of truth is cli/tests/test_short_flag_convention.py; keep this
# text in sync when the register or the per-command maps change.
SHORTHAND_LEGEND = """\
fno shorthand legend (short-flag convention)

UPPERCASE = global register. One fixed meaning on every command:

  -J  --json     structured JSON output
  -A  --all      include everything / widen scope
  -F  --force    skip confirmation / overwrite guard
  -N  --dry-run  show what would happen without doing it
  -R  --reason   required rationale string
  -Y  --yolo     confirmation / danger-mode bypass

lowercase = per-command value flags. -p is "the primary thing this
command is about" and differs by family:

  fno agents ask                        -p provider   (-c cwd, -t timeout)
  fno backlog add/idea/update/intake    -p priority   (-c cwd, -d details, -t type/title)
  fno backlog next/ready/find           -p project    (find: -s status, -d domain)
  fno backlog capture add               -p priority   (-s source, -w where)
  fno config accounts add               -p priority   (-H harness, -a auth, -s scope)
  fno gate verify                       -p phase      (-s state, -x strict)
  fno event emit                        -t type       (-d data, -s source)
  fno mail send                         -k kind       (-b body; --to-project long-only)
  fno done                              -p pr-number  (-l link, -m note)
  fno carveout add                      -k kind       (-p priority)

Unix-entrenched lowercase stays put: -h help, -n tail / -f follow
(agents logs), -m note (done), -o output (codemap), -b blocked
(backlog pick), -I ideas (backlog next/ready).

Canonical spellings: --session-id and --pr-number. The old --session /
--pr spellings still work as hidden deprecated aliases.

Run `fno help <command>` for any command's full flag list.\
"""

SHORTHAND_POINTER = (
    "Shorthands: UPPERCASE shorts are global (-J --json, -A --all, -F --force, "
    "-N --dry-run, -R --reason, -Y --yolo); lowercase shorts are per-command. "
    "Run `fno help shorthands` for the full legend."
)

# Short help for the eager inline commands (defined in this module, so listing
# them never imports anything). `help --all` merges these with LAZY_SUBCOMMANDS.
_EAGER_COMMAND_HELP: dict[str, str] = {
    "help": "Show help for the root command or any subcommand.",
    "cost": "Cost + usage metrics from session transcripts and ledger.json.",
    "review": "Run the internal sigma-review panel on the current diff.",
}


def _render_full_menu() -> str:
    """Render every top-level command (hidden ones included) for `fno help --all`.

    Reads short-help strings straight from LAZY_SUBCOMMANDS + the eager map -
    it never imports a command module, so a broken sub-app still lists here
    (AC3-UI). Scope is the TOP LEVEL only: a sub-app's own (hidden) verbs are
    one import away and would break the no-import guarantee, so they live behind
    the scoped door `fno help <group> --all` instead.
    """
    rows = dict(_EAGER_COMMAND_HELP)
    for name, entry in LAZY_SUBCOMMANDS.items():
        rows[name] = entry[1] if isinstance(entry, tuple) and len(entry) >= 2 else ""
    width = max(len(n) for n in rows)
    lines = [
        "fno-py full top-level surface - every fno-py command, including hidden.",
        "The curated menu is `fno --help`; hidden commands are invocable, just not advertised.",
        "For a group's own verbs (hidden included) run `fno help <group> --all` "
        "(e.g. `fno help agents --all`), or `fno <group> --help` for its menu.",
        "Also: the Rust front (`fno` binary) owns `mux` and `version`, which live "
        "outside this listing - run `fno mux` / `fno mux --help` for pane control.",
        "",
        "Commands:",
    ]
    lines.extend(f"  {name.ljust(width)}  {rows[name]}" for name in sorted(rows))
    return "\n".join(lines)


def _render_group_full_menu(path: list[str]) -> Optional[str]:
    """Render a sub-app's full command list (hidden included) for
    `fno help <group> [<subgroup> ...] --all`.

    Unlike the top-level door this DOES import the named group (one module), so
    it can enumerate the group's own hidden verbs - the discovery path that the
    import-free top-level `help --all` cannot provide. Returns None when the path
    does not resolve to a command group (the caller then forwards to `--help`).
    """
    import click
    import typer.main

    root = typer.main.get_command(app)
    ctx = click.Context(root, info_name="fno")
    cmd: Any = root
    cur = ctx
    trail: list[str] = []
    for seg in path:
        nxt = cmd.get_command(cur, seg) if hasattr(cmd, "get_command") else None
        if nxt is None:
            return None
        # A lazy top-level entry resolves to a stub; load its real command so we
        # can descend and enumerate.
        if hasattr(nxt, "_load_real"):
            nxt = nxt._load_real()
        cmd = nxt
        trail.append(seg)
        cur = click.Context(cmd, info_name=seg, parent=cur)

    if not hasattr(cmd, "list_commands"):
        return None  # a leaf command, not a group

    rows: dict[str, str] = {}
    list_all = getattr(cmd, "_fno_collapsed_original_list_commands", cmd.list_commands)
    get_all = getattr(cmd, "_fno_collapsed_original_get_command", cmd.get_command)
    for name in list_all(cur):
        sub = get_all(cur, name)
        if sub is None:
            continue
        rows[name] = sub.get_short_help_str() if hasattr(sub, "get_short_help_str") else ""
    if not rows:
        return None

    title = " ".join(["fno", *trail])
    width = max(len(n) for n in rows)
    lines = [
        f"{title} full command surface - every subcommand, including hidden.",
        f"The curated menu is `{title} --help`; hidden verbs are invocable, just not advertised.",
        "",
        "Commands:",
    ]
    lines.extend(f"  {name.ljust(width)}  {rows[name]}" for name in sorted(rows))
    return "\n".join(lines)


@app.command(
    name="help",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Show help for the root command or any subcommand. The git-style "
        "counterpart to `--help`.\n\n"
        "Usage:\n"
        "  fno help                  show top-level help (same as `fno --help`)\n"
        "  fno help claim            show `fno claim` help\n"
        "  fno help claim acquire    show `fno claim acquire` help\n"
        "  fno help shorthands       show the short-flag legend\n"
        "  fno help --all            show every command, including hidden\n\n"
        "Equivalent to `fno <args> --help` but reads more naturally in "
        'canonical instructions (e.g. "Run `fno help claim` if unsure").'
    ),
)
def help_command(ctx: typer.Context) -> None:
    """Forward to `fno <args> --help` so subcommands' own help formatters run."""
    args = list(ctx.args)
    if "--all" in args or "-A" in args:
        rest = [a for a in args if a not in ("--all", "-A")]
        if rest:
            # Scoped door: `fno help <group> [<subgroup>] --all` lists that
            # group's own verbs, hidden included (imports the one named group).
            group_menu = _render_group_full_menu(rest)
            if group_menu is not None:
                typer.echo(group_menu)
                return
            # Not a command group - fall through to the group's normal --help.
            args = rest
        else:
            # The top-level full-surface door: every top-level command including
            # hidden ones, rendered from the registry with no module import.
            typer.echo(_render_full_menu())
            return
    if args == ["shorthands"]:
        # Help topic, not a command - print the legend instead of forwarding
        # to a (nonexistent) `fno shorthands --help`.
        typer.echo(SHORTHAND_LEGEND)
        return
    if not args:
        # No subcommand named - print the root help via the parent context
        # so we don't shell out for the no-arg case.
        typer.echo(ctx.parent.get_help() if ctx.parent else ctx.get_help())
        typer.echo("")
        typer.echo(SHORTHAND_POINTER)
        return
    # Forward to `<binary> <args> --help`. Pick the invocation form based
    # on how this process was launched:
    #   - Installed console script (`fno help ...`): sys.argv[0] resolves
    #     to the binary path and is directly executable.
    #   - Module mode (`python -m fno.cli help ...`): sys.argv[0] is
    #     the module file path (cli.py), which is typically not chmod +x
    #     and has no shebang, so spawning it as a subprocess raises
    #     PermissionError / FileNotFoundError. Fall back to the canonical
    #     `python -m fno.cli` form, which works in both modes.
    import subprocess

    binary = sys.argv[0] if sys.argv else ""
    if binary and os.path.isfile(binary) and os.access(binary, os.X_OK):
        cmd = [binary, *args, "--help"]
    else:
        cmd = [sys.executable, "-m", "fno.cli", *args, "--help"]
    result = subprocess.run(cmd, check=False)
    from fno._subprocess_util import propagate_returncode

    raise typer.Exit(code=propagate_returncode(result.returncode))


@app.command(
    name="cost",
    hidden=True,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Cost + usage metrics from session transcripts and ledger.json "
        "(in-package; the former scripts/metrics/session-cost.py, US2).\n\n"
        "Usage:\n"
        "  fno cost [SESSION_ID...]   per-session token/cost summary\n"
        "  fno cost --branches       per-branch cost breakdown\n"
        "  fno cost --by-provider    per-provider cost from ledger.json\n"
        "  fno cost --backfill       recalculate ledger.json costs\n"
        "  fno cost --render         re-render ledger.md from ledger.json\n\n"
        "Also accepts --json / --branch / --since / --dry-run. Runs from the "
        "installed wheel with no repo-root script."
    ),
)
def cost(ctx: typer.Context) -> None:
    """Forward all args to the in-package cost CLI (``fno.cost._session_cost``).

    Eager (like ``help``/``review``) but the heavy ``_session_cost`` import is
    deferred to call time, so ``fno --help`` never pays for it. argparse owns the
    flag parsing; we bridge via ``sys.argv`` and translate its ``SystemExit``
    into a ``typer.Exit`` so the exit code propagates cleanly.
    """
    from fno.cost import _session_cost

    argv_backup = sys.argv
    sys.argv = ["fno cost", *list(ctx.args)]
    try:
        _session_cost.main()
    except SystemExit as exc:  # argparse exits on -h / parse error / explicit exit
        code = exc.code
        if code is None:
            code = 0
        elif not isinstance(code, int):
            code = 1
        raise typer.Exit(code=code)
    finally:
        sys.argv = argv_backup


@app.command(
    hidden=True,
    help=(
        "The yard identity fold: species, rarity tier, crown, and first-sighting "
        "per registry citizen.\n\n"
        "Read-only over the agent registry and the graph archive. Consumed by "
        "the mux yard overlay (fail-open shell-out); `--json` is the machine "
        "surface, the text mode is a one-line-per-citizen summary. Outcome "
        "only - never the rank, the frequency, or the boundary."
    ),
)
def yard(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the fold as JSON."),
) -> None:
    """Fold the fleet into yard citizens (the f[no]nimals)."""
    import json as _json

    from fno import paths
    from fno.agents.registry import load_registry
    from fno.graph.store import GraphUnreadableError, read_graph_strict
    from fno.yard import RARITY_TIERS, fold

    rows = load_registry()
    archive_path = paths.graph_archive_json()
    # Strict on purpose: this is a truth-bound fold, not a browse verb. The
    # lenient reader turns a corrupt archive into [], which would mark every
    # citizen a first-sighting - fabricated outcome on the machine surface the
    # mux renders. Unreadable history fails the fold so the overlay degrades.
    try:
        archive = read_graph_strict(archive_path) if archive_path.exists() else []
    except GraphUnreadableError as exc:
        typer.secho(f"yard: archive unreadable, fold refused: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    citizens = fold(rows, archive)

    if as_json:
        typer.echo(_json.dumps({"citizens": citizens}, indent=2))
        return
    if not citizens:
        typer.echo("the yard is empty (no registry rows)")
        return
    for c in citizens:
        mark = " NEW" if c["first_sighting"] else ""
        crown = f" crown {c['crown_level']}" if c["crown_level"] else ""
        typer.echo(
            f"{c['name']:<24} species {c['species']:>2}  {c['rarity']:<9}{crown}{mark}"
        )
    n_new = sum(1 for c in citizens if c["first_sighting"])
    typer.echo(f"{len(citizens)} citizens, {n_new} first sighting(s); tiers: {'/'.join(RARITY_TIERS)}")


@app.command(
    hidden=True,
    help=(
        "Run the internal sigma-review panel on the current diff.\n\n"
        "Reads the diff from --diff path or `git diff HEAD~1` by default.\n"
        "Resolves session_id from --session or target-state.md.\n\n"
        "Exit codes:\n"
        "  0   Reviewed (or cached hit)\n"
        " 11   Review lock busy\n"
        "130   SIGINT (workers reaped)\n"
    ),
)
def review(
    ctx: typer.Context,
    session: Optional[str] = typer.Option(
        None, "--session-id", help="session id (overrides state file)"
    ),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    state: Optional[Path] = typer.Option(None, "--state", help="path to target-state.md"),
    diff: Optional[Path] = typer.Option(
        None, "--diff", help="path to diff file (default: git diff HEAD~1)"
    ),
    artifacts_dir: Optional[Path] = typer.Option(
        None, "--artifacts-dir", help="artifacts directory"
    ),
    no_cache: bool = typer.Option(False, "--no-cache", help="bypass cache"),
    print_providers: bool = typer.Option(
        False,
        "--print-providers",
        help="Print the per-agent cross-model provider routing as JSON and exit "
        "(no panel run). The /review sigma skill consumes this so it dispatches "
        "the same providers as the fno review panel.",
    ),
    publish_sigma: Optional[Path] = typer.Option(
        None,
        "--publish-sigma",
        help="Publish a completed sigma report to the node-bound durable artifact.",
    ),
    inspect_sigma: bool = typer.Option(
        False,
        "--inspect-sigma",
        help="Inspect the current node-bound sigma artifact without running a panel.",
    ),
    sigma_last_head: bool = typer.Option(
        False,
        "--sigma-last-head",
        help=(
            "Print the head the node's current sigma artifact was reviewed at "
            "and exit. Empty stdout plus a non-zero exit means no prior head."
        ),
    ),
    assess_assurance: bool = typer.Option(
        False,
        "--assess-assurance",
        help="Classify the review policy from --policy-size/--risk-surface and print "
        "the assurance verdict as JSON, then exit. Exits 3 (unsatisfied) when a "
        "high-assurance change has no different-family reviewer.",
    ),
    policy_size: Optional[str] = typer.Option(
        None, "--policy-size", help="Plan size (S/M/L) for --assess-assurance."
    ),
    risk_surface: Optional[list[str]] = typer.Option(
        None,
        "--risk-surface",
        help="Named risk surface for --assess-assurance (repeatable).",
    ),
    sigma_node: Optional[str] = typer.Option(None, "--sigma-node", hidden=True),
    sigma_pr: Optional[int] = typer.Option(None, "--sigma-pr", hidden=True),
    sigma_head: Optional[str] = typer.Option(None, "--sigma-head", hidden=True),
    sigma_current_head: Optional[str] = typer.Option(None, "--sigma-current-head", hidden=True),
    sigma_round: Optional[str] = typer.Option(None, "--sigma-round", hidden=True),
    sigma_scope_base: Optional[str] = typer.Option(None, "--sigma-scope-base", hidden=True),
    sigma_scope_reason: Optional[str] = typer.Option(None, "--sigma-scope-reason", hidden=True),
    sigma_project: Optional[str] = typer.Option(None, "--sigma-project", hidden=True),
    sigma_reviews_root: Optional[Path] = typer.Option(None, "--sigma-reviews-root", hidden=True),
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Output structured JSON to stdout.",
    ),
) -> None:
    """Run the internal sigma-review panel and write a quality_check artifact."""
    from fno._flag_aliases import merge_deprecated_alias
    from fno.worker.review import (
        ReviewInputError,
        capture_review_input,
        review as _review,
    )
    from fno.review.locking import ReviewLockBusy

    session = merge_deprecated_alias(
        session, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )

    ctx.ensure_object(dict)
    if json_output:
        ctx.obj["json"] = True

    # --print-providers: resolve the per-agent routing via the SAME path the
    # panel uses (worker.review.panel_provider_routing -> resolve_panel_providers)
    # and exit before any diff/panel work. Empty {} means all-claude (cross-model
    # OFF). This is the seam that lets /review sigma honor config.review.cross_model
    # without a parallel resolver (US3, no drift).
    if print_providers:
        from fno.worker.review import panel_provider_routing, resolve_session_id

        # Resolve the session the SAME way the panel run does (explicit flag,
        # else target-state.md) so the implementer-provider read - which
        # `alternate` routing excludes - matches; otherwise the skill could
        # resolve a different provider than the panel (drift).
        sid = resolve_session_id(session, state or Path(".fno/target-state.md"))
        routing = {
            agent: {
                "provider": rp.provider,
                "harness": rp.route.harness if rp.route else rp.provider,
                "route_provider": rp.route.provider if rp.route else None,
                "model": rp.route.model if rp.route else None,
                "degraded": rp.degraded,
                "reason": rp.reason,
            }
            for agent, rp in panel_provider_routing(sid).items()
        }
        typer.echo(json.dumps(routing))
        return

    if assess_assurance:
        from fno.worker.review import resolve_session_id, review_assurance

        sid = resolve_session_id(session, state or Path(".fno/target-state.md"))
        verdict = review_assurance(
            sid, size=policy_size, risk_surfaces=list(risk_surface or [])
        )
        typer.echo(json.dumps(verdict))
        # Fail closed so a direct CLI caller gets the same block the skill does:
        # an unsatisfied high-assurance policy must not read as a clean pass.
        raise typer.Exit(0 if verdict["satisfied"] else 3)

    if publish_sigma is not None or inspect_sigma or sigma_last_head:
        from fno.config import load_settings
        from fno.paths import vault_root
        from fno.review.artifact import (
            inspect_sigma_artifact,
            publish_sigma_artifact,
            read_sigma_last_head,
        )

        project = sigma_project or load_settings().project.id
        root = sigma_reviews_root
        if root is None:
            vault = vault_root()
            root = vault / "internal" if vault is not None else None
        missing = [
            name
            for name, value in (
                ("--sigma-node", sigma_node),
                ("--sigma-pr", sigma_pr),
                ("config.project.id/--sigma-project", project),
                ("configured vault/--sigma-reviews-root", root),
            )
            if value is None
        ]
        if publish_sigma is not None or inspect_sigma:
            missing.extend(
                name for name, value in (("--sigma-head", sigma_head),) if value is None
            )
        if publish_sigma is not None:
            missing.extend(
                name
                for name, value in (
                    ("--sigma-current-head", sigma_current_head),
                    ("--sigma-round", sigma_round),
                )
                if value is None
            )
            if (sigma_scope_base is None) != (sigma_scope_reason is None):
                missing.append(
                    "--sigma-scope-base and --sigma-scope-reason (both or neither)"
                )
        if missing:
            typer.echo(f"error: sigma artifact metadata missing: {', '.join(missing)}", err=True)
            raise typer.Exit(code=2)

        if sigma_last_head and (publish_sigma is not None or inspect_sigma):
            typer.echo(
                "error: --sigma-last-head cannot combine with --publish-sigma/--inspect-sigma",
                err=True,
            )
            raise typer.Exit(code=2)

        if sigma_last_head:
            assert root is not None and project is not None
            assert sigma_node is not None and sigma_pr is not None
            last_head = read_sigma_last_head(
                reviews_root=root,
                project=project,
                node=sigma_node,
                pr_number=sigma_pr,
            )
            if last_head.head_sha is None:
                typer.echo(
                    f"error: sigma last head unavailable: {last_head.reason}", err=True
                )
                raise typer.Exit(code=1)
            # Bare SHA only: callers consume it as one token of shell input.
            typer.echo(last_head.head_sha)
            return

        assert root is not None and project is not None
        assert sigma_node is not None and sigma_pr is not None and sigma_head is not None
        if publish_sigma is not None:
            assert sigma_current_head is not None and sigma_round is not None
            publish_result = publish_sigma_artifact(
                publish_sigma,
                reviews_root=root,
                project=project,
                node=sigma_node,
                pr_number=sigma_pr,
                reviewed_head=sigma_head,
                current_head=sigma_current_head,
                round_id=sigma_round,
                scope_base=sigma_scope_base,
                scope_reason=sigma_scope_reason,
            )
            payload = {
                "published": publish_result.published,
                "reason": publish_result.reason,
                "path": str(publish_result.current_path),
                "round_path": str(publish_result.round_path),
                "head_sha": sigma_head,
                "finding_count": publish_result.finding_count,
            }
        else:
            inspected = inspect_sigma_artifact(
                reviews_root=root,
                project=project,
                node=sigma_node,
                pr_number=sigma_pr,
                head_sha=sigma_head,
            )
            payload = {
                "status": inspected.status,
                "reason": inspected.reason,
                "path": str(inspected.path),
                "head_sha": sigma_head,
                "finding_count": inspected.finding_count,
                "review_round": inspected.round_id,
                "body": inspected.body,
            }
        if json_output:
            typer.echo(json.dumps(payload))
        else:
            for key, value in payload.items():
                if key != "body":
                    typer.echo(f"{key}: {value}")
        return

    state_path = state or Path(".fno/target-state.md")

    try:
        reviewed_head, diff_context = capture_review_input(diff)
    except ReviewInputError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=2)

    try:
        review_result = _review(
            diff_context=diff_context,
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session,
            no_cache=no_cache,
            git_sha_value=reviewed_head,
        )
    except ReviewLockBusy as exc:
        typer.echo(f"error: review lock busy: {exc}", err=True)
        raise typer.Exit(code=11)

    use_json = bool(ctx.obj and ctx.obj.get("json", False))
    if use_json:
        typer.echo(json.dumps(review_result))
    else:
        typer.echo(f"action: {review_result['action']}")
        typer.echo(f"verdict: {review_result.get('verdict', 'unknown')}")
        typer.echo(f"findings: {review_result.get('findings', 0)}")
        if review_result.get("cached"):
            typer.echo("cached: true")


if __name__ == "__main__":
    app()
