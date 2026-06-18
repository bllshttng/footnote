"""fno phase CLI - thin wrapper over the `fno-agents kill-check` Rust verb.

Formerly sourced scripts/lib/kill-criteria.sh; the predicate evaluator is now
folded into the bundled fno-agents binary (US1, ab-58645f63), so the verb runs
on a bare `pip install fno` (the binary ships in the wheel) with no repo-root
script dependency. The Python wrapper still resolves the default plan_path from
.fno/target-state.md and forwards it to the binary, preserving the prior
behavior.

Note: fno phase verify (phase-verifier.sh) removed in Task 3.2
(control-plane collapse, ab-d0337fbc). Only kill-check remains.
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.agents.rust_runtime import resolve_binary
from fno.paths import resolve_repo_root


phase_app = typer.Typer(
    name="phase",
    help="Phase utilities (kill-check via the bundled fno-agents binary)",
    no_args_is_help=True,
    add_completion=False,
)


def _state_file_path() -> Path:
    """Return the canonical .fno/target-state.md path relative to repo root."""
    return resolve_repo_root() / ".fno" / "target-state.md"


def _read_state_field(field: str) -> Optional[str]:
    """Read a single frontmatter field from target-state.md."""
    from fno.state.io import read_frontmatter

    state = _state_file_path()
    try:
        fm, _ = read_frontmatter(state)
    except FileNotFoundError:
        return None
    except Exception as exc:
        typer.echo(
            f"fno phase: could not parse {state}: {exc}",
            err=True,
        )
        return None
    return fm.get(field)


@phase_app.command(
    "kill-check",
    help=(
        "Evaluate kill criteria via the bundled fno-agents binary. "
        "PLAN_PATH defaults to the plan_path field in .fno/target-state.md. "
        "Exit 0 = no kill; exit 1 = predicate fired."
    ),
)
def kill_check(
    plan_path: Optional[str] = typer.Argument(
        None,
        metavar="PLAN_PATH",
        help="Path to the plan file or folder. Defaults to plan_path from .fno/target-state.md.",
    ),
) -> None:
    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno phase kill-check: the fno-agents binary was not found. It ships "
            "in the `pip install fno` wheel and with the plugin; reinstall fno or "
            "run `fno update --rust`, or set FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=2)

    path = plan_path
    if path is None:
        path = _read_state_field("plan_path") or ""

    result = subprocess.run([str(binary), "kill-check", path], check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
