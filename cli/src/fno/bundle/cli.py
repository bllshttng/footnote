"""fno bundle CLI - thin Typer wrappers around the canonical bundler scripts.

Surface:
    fno bundle         -> bash scripts/generate-skill-bundles.sh
    fno bundle check   -> bash scripts/lint/check-skill-bundles-fresh.sh
    fno bundle lint    -> bash scripts/lint/no-cross-skill-runtime-calls.sh

Each invocation is a thin Typer wrapper that forwards to the canonical bash
script. The bash scripts remain the single source of truth for bundling
logic, freshness comparison, and lint rules; this CLI exists for
discoverability (`fno --help`) and so contributors, the pre-commit hook,
and CI all converge on the same surface.

The no-subcommand default runs the bundler because that's the most common
action (regenerate before commit or install). The two named subcommands
(``check``, ``lint``) are gates a contributor runs less often.
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import List

import typer

from fno.paths import resolve_repo_root


bundle_app = typer.Typer(
    name="bundle",
    help=(
        "Skill bundle build + lint. Default action (no subcommand) regenerates "
        "from skill-bundles.yaml; subcommands run the freshness gate and "
        "marketplace-readiness lint."
    ),
    invoke_without_command=True,
    add_completion=False,
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)


_VERB_TO_SCRIPT = {
    "build": Path("scripts") / "generate-skill-bundles.sh",
    "check": Path("scripts") / "lint" / "check-skill-bundles-fresh.sh",
    "lint": Path("scripts") / "lint" / "no-cross-skill-runtime-calls.sh",
}


def _script_path(verb: str) -> Path:
    return resolve_repo_root() / _VERB_TO_SCRIPT[verb]


def _forward(verb: str, extra_args: List[str]) -> int:
    script = _script_path(verb)
    if not script.is_file():
        # Capability-accurate degrade (US3 / AC3-ERR): bundling operates on the
        # plugin's `skills/` source tree, which a bare `pip install fno` does
        # not ship - there is nothing to bundle. Name the missing capability and
        # the install path, never a 127 or traceback. In a clone the script is
        # present and this branch never fires (AC3-HP, in-clone unchanged).
        typer.echo(
            f"fno bundle {verb}: needs the footnote plugin (skill sources), which "
            "a bare `pip install fno` does not ship - there is no skills/ tree "
            "to bundle.\n"
            "Install the plugin and run from its checkout:\n"
            "  clone the footnote repo, then run `claude --plugin-dir "
            "/path/to/footnote`\n"
            "Or set FNO_REPO_ROOT to an existing plugin checkout. "
            f"(resolved, not on disk: {script})",
            err=True,
        )
        return 2
    # as_posix() forces forward slashes even on Windows where Path uses
    # backslashes natively. Bash on Windows (Git Bash, WSL) interprets
    # backslashes as escape characters, so str(Path("a\b")) becomes "ab"
    # in the subprocess command. fno targets Unix today but the
    # one-line defense is free and matches PEP-recommended practice for
    # passing paths to shells.
    cmd = ["bash", script.as_posix()] + extra_args
    result = subprocess.run(cmd, check=False)
    return result.returncode


@bundle_app.callback()
def _default(ctx: typer.Context) -> None:
    """When no subcommand is given, regenerate per-skill bundles from
    skill-bundles.yaml. Equivalent to ``bash scripts/generate-skill-bundles.sh``.
    Extra args after ``fno bundle`` are forwarded to the script.
    """
    if ctx.invoked_subcommand is not None:
        return
    rc = _forward("build", list(ctx.args))
    raise typer.Exit(code=rc)


@bundle_app.command(
    "check",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Verify committed bundles match the canonical sources (freshness gate). "
        "Forwards all args to scripts/lint/check-skill-bundles-fresh.sh. "
        "Exit 0 if fresh, exit 1 on drift, exit 2 on substrate failure."
    ),
)
def check(ctx: typer.Context) -> None:
    rc = _forward("check", list(ctx.args))
    raise typer.Exit(code=rc)


@bundle_app.command(
    "lint",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Run the marketplace-readiness lint: no Skill() runtime calls, no "
        "../../<sibling>/ path escapes, requires.binaries.fno declared in "
        "driver SKILL.md. Forwards all args to "
        "scripts/lint/no-cross-skill-runtime-calls.sh."
    ),
)
def lint(ctx: typer.Context) -> None:
    rc = _forward("lint", list(ctx.args))
    raise typer.Exit(code=rc)
