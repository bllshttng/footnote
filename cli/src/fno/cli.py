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

LAZY_SUBCOMMANDS: dict[str, tuple[str, str] | tuple[str, str, dict[str, Any]]] = {
    # Sub-apps (Typer instances) -----------------------------------------
    "state":         ("fno.state.cli:cli",                 "manage fno state files"),
    "target":        ("fno.target_cli:target_app",         "Target session bootstrap (fno target init)"),
    "backlog":       ("fno.graph.cli:cli",                 "Feature graph management"),
    "graph":         ("fno.graph.cli:cli",                 "Feature graph management", {"hidden": True}),
    "runtime":       ("fno.runtime.cli:cli",               "manage runtime workers and worktrees"),
    "worker":        ("fno.worker.cli:cli",                "manage delivery worker phases"),
    "event":         ("fno.events.cli:cli",                "emit and audit events"),
    "log":           ("fno.log_cmd:app",                   "Append a progress entry to the per-worktree agent-progress.jsonl"),
    "reality-check": ("fno.reality_check.cli:cli",         "check external reality"),
    "providers":     ("fno.adapters.providers.cli:cli",    "Manage provider records and active selection."),
    "mail":          ("fno.mail.cli:mail_app",             "Durable polled mailbox: send/unread/ack/reply/drain/status."),
    "agents":        ("fno.agents.cli:agents_app",          "Cross-CLI agent dispatch (claude / codex / gemini)."),
    "wake":          ("fno.wake.cli:wake_app",             "Wake-signal admin commands"),
    "plan":          ("fno.plan:plan_app",                 "Plan frontmatter stamping (in-package)"),
    "pr":            ("fno.pr:pr_app",                     "PR utilities (wraps scripts/lib/pr-*.sh)"),
    "stub-manifest": ("fno.stub_manifest:stub_manifest_app", "Stub-manifest for contract-tier dependents (emit/validate/check-pr)."),
    "bundle":        ("fno.bundle:bundle_app",             "Skill bundle build + lint."),
    "lint":          ("fno.lint_cli:app",                  "Repository lint checks"),
    "claim":         ("fno.claims.cli:cli",                 "Work-claim coordination primitive"),
    "carveout":      ("fno.carveout:carveout_app",          "Capture left-out work (deferred decisions, out-of-scope bugs) for retro-triage."),
    "retro":         ("fno.retro.cli:retro_app",            "Consume retro-triage triggers; file left-out work as backlog nodes."),
    "think":         ("fno.provenance.cli:think_app",        "Context /think dispatch (explicit conversational verb)."),
    "phase":         ("fno.phase:phase_app",               "Phase utilities (kill-check via the fno-agents binary)"),
    "executor":      ("fno.executor:executor_app",         "Executor resolution (locked-decision parser + surface inference)"),
    "config":        ("fno.config_cli:app",                "Configuration management"),
    "notify":        ("fno.notify:notify_app",             "OS notification helper (in-package; macOS osascript / Linux notify-send)"),
    "paths":         ("fno.paths_cli:app",                 "Path resolution helpers"),
    "setup":         ("fno.setup_cli:app",                 "Interactive settings.yaml wizard"),
    "consolidation": ("fno.consolidation:app",             "Consolidation utilities"),
    "tokens":        ("fno.tokens:app",                    "Token usage tracking"),
    "codemap":       ("fno.codemap_cli:app",               "Codebase map management"),
    "worktree":      ("fno.worktree_cli:app",              "Worktree management"),
    "evals":         ("fno.evals.cli:evals_app",           "Golden-task efficacy evals (run / report / diff)"),
    "pr-watch":      ("fno.pr_watch.cli:cli",              "PR-state watcher: auto-fire /pr check + /pr merged for open-PR backlog nodes"),
    # Individual commands (plain functions wrapped as single-command apps) -
    "whoami":        ("fno.agent.cli:whoami_command",       "Operating-stack summary: project + fleet + walker + session + provider."),
    "status":        ("fno.agent.cli:status_command",       "Session gate satisfaction + bounded events tail + inconsistencies."),
    "doctor":        ("fno.doctor:doctor_command",         "Diagnose installed-vs-source fno skew (network-free)."),
    "done":          ("fno.done.cli:done_command",         "Mark a backlog node as done."),
    "find":          ("fno.graph.cli:cmd_find",            "Fuzzy search across graph entries."),
    "research":      ("fno.research:research_command",     "Retrieve + store: ddgs backbone -> self-fetch -> sources.jsonl."),
    "new":           ("fno.graph.cli:cmd_new",             "Create a new graph entry without a plan file."),
    "test":          ("fno.test_cmd:test_command",         "Run pytest honestly: worktree-pinned PYTHONPATH, rtk-bypassed, real exit code."),
    "update":        ("fno.update:update_command",         "Reinstall fno from its source directory."),
    "upgrade":       ("fno.update:update_command",         "Reinstall fno from its source directory.", {"hidden": True}),
}


# ---------------------------------------------------------------------------
# Helpers that defer their own imports until needed
# ---------------------------------------------------------------------------

def _load_v2_config_flag(repo_root: Path) -> bool:
    """Read ``config.v2_enabled`` from ``.fno/settings.yaml``.

    Returns False on any read/parse failure - v2 is strictly opt-in.
    Local settings win over the per-user global file.

    yaml + fno.paths are imported inside this function so the cli
    module body stays cheap to load.
    """
    import yaml
    from fno import paths as _paths

    candidate_builders = [
        lambda: repo_root / ".fno" / "settings.yaml",
        lambda: _paths.config_file(),
    ]
    for builder in candidate_builders:
        try:
            path = builder()
            if not path.is_file():
                continue
            data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
            config = data.get("config") if isinstance(data, dict) else None
            if isinstance(config, dict) and config.get("v2_enabled") is True:
                return True
        except Exception:
            continue
    return False


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

app = typer.Typer(
    name="fno",
    help="CLI for the footnote autonomous delivery pipeline",
    no_args_is_help=True,
    invoke_without_command=True,
    add_completion=False,
    cls=make_lazy_group_cls(LAZY_SUBCOMMANDS),
)


@app.callback()
def callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(False, "--json", "-J", help="output JSON where supported"),
    version: bool = typer.Option(False, "--version", help="show version and exit", is_eager=True),
) -> None:
    from fno._compat_env import backfill_legacy_env
    backfill_legacy_env()  # one-release legacy-env back-fill (see fno._compat_env)
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
  fno providers add                     -p priority   (-c cli, -a auth, -s scope)
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
        "  fno help shorthands       show the short-flag legend\n\n"
        "Equivalent to `fno <args> --help` but reads more naturally in "
        "canonical instructions (e.g. \"Run `fno help claim` if unsure\")."
    ),
)
def help_command(ctx: typer.Context) -> None:
    """Forward to `fno <args> --help` so subcommands' own help formatters run."""
    args = list(ctx.args)
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
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Cost + usage metrics from session transcripts and ledger.json "
        "(in-package; the former scripts/metrics/session-cost.py, US2).\n\n"
        "Usage:\n"
        "  fno cost [SESSION_ID...]   per-session token/cost summary\n"
        "  fno cost --branches       per-branch cost breakdown\n"
        "  fno cost --by-provider    per-provider cost from ledger.json\n"
        "  fno cost --backfill       recalculate tasks.json costs\n"
        "  fno cost --render         re-render tasks.md from tasks.json\n\n"
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
    help=(
        "Run the internal sigma-review panel on the current diff.\n\n"
        "Reads the diff from --diff path or `git diff HEAD~1` by default.\n"
        "Resolves session_id from --session or target-state.md.\n\n"
        "Exit codes:\n"
        "  0   Reviewed (or cached hit)\n"
        " 11   Review lock busy\n"
        "130   SIGINT (workers reaped)\n"
    )
)
def review(
    ctx: typer.Context,
    session: Optional[str] = typer.Option(
        None, "--session-id", help="session id (overrides state file)"
    ),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    state: Optional[Path] = typer.Option(
        None, "--state", help="path to target-state.md"
    ),
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
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout.",
    ),
) -> None:
    """Run the internal sigma-review panel and write a quality_check artifact."""
    import subprocess
    from fno._flag_aliases import merge_deprecated_alias
    from fno.worker.review import review as _review
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
                "degraded": rp.degraded,
                "reason": rp.reason,
            }
            for agent, rp in panel_provider_routing(sid).items()
        }
        typer.echo(json.dumps(routing))
        return

    state_path = state or Path(".fno/target-state.md")

    if diff is not None:
        diff_context = diff.read_text(encoding="utf-8")
    else:
        git_result = subprocess.run(
            ["git", "diff", "HEAD~1"],
            capture_output=True,
            text=True,
        )
        if git_result.returncode != 0:
            # Silent-empty substitution was the bug: a first-commit branch
            # (no HEAD~1) or detached HEAD would yield an empty diff that
            # the panel reviewed as "clean", producing zero findings
            # indistinguishable from a real green review. Fail loud so
            # "no findings" actually means "no findings" rather than
            # "the diff was never read". The --diff path override
            # remains as the documented escape hatch.
            typer.echo(
                f"error: git diff HEAD~1 failed (rc={git_result.returncode}): "
                f"{git_result.stderr.strip()}\n"
                "Pass --diff path/to/manual.diff to review an explicit "
                "diff (e.g. first-commit branches without a HEAD~1 parent).",
                err=True,
            )
            raise typer.Exit(code=2)
        diff_context = git_result.stdout

    try:
        result = _review(
            diff_context=diff_context,
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session,
            no_cache=no_cache,
        )
    except ReviewLockBusy as exc:
        typer.echo(f"error: review lock busy: {exc}", err=True)
        raise typer.Exit(code=11)

    use_json = bool(ctx.obj and ctx.obj.get("json", False))
    if use_json:
        typer.echo(json.dumps(result))
    else:
        typer.echo(f"action: {result['action']}")
        typer.echo(f"verdict: {result.get('verdict', 'unknown')}")
        typer.echo(f"findings: {result.get('findings', 0)}")
        if result.get("cached"):
            typer.echo("cached: true")


if __name__ == "__main__":
    app()
