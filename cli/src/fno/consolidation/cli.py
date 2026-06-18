"""fno consolidation audit - thin wrapper over the bash audit gate.

The bash script at scripts/ci/check-no-stale-skill-refs.sh is the source of
truth. This subcommand exists so the same audit runs from any cwd inside the
repo via the polished CLI surface.
"""
import subprocess
import sys
from pathlib import Path

import typer

from fno._subprocess_util import propagate_returncode
from fno.paths import resolve_repo_root

app = typer.Typer(
    name="consolidation",
    help="Consolidation gates (audit stale skill references, etc.)",
    no_args_is_help=True,
)


@app.command()
def audit() -> None:
    """Audit for stale references to cut, demoted, or merged skills.

    Mirrors scripts/ci/check-no-stale-skill-refs.sh. Exit code matches the
    bash script: 0 on clean, 1 on stale references found, 2 on script error.
    """
    repo_root = Path(resolve_repo_root())
    script = repo_root / "scripts" / "ci" / "check-no-stale-skill-refs.sh"
    if not script.exists():
        typer.echo(f"audit script not found at {script}", err=True)
        raise typer.Exit(code=2)
    try:
        result = subprocess.run(["bash", str(script)], cwd=repo_root)
    except FileNotFoundError as exc:
        typer.echo(f"failed to run audit script: {exc}", err=True)
        raise typer.Exit(code=2)
    raise typer.Exit(code=propagate_returncode(result.returncode))
