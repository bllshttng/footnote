"""`fno test [pytest-args...]` - run the Python suite honestly (x-8b64 G).

Three tribal-knowledge footguns, in one verb:

1. A worktree's bare `pytest` imports the *canonical* `fno` (installed editable
   from the main checkout), not the worktree's own source. We pin
   `PYTHONPATH=<repo>/cli/src` so a worktree tests what it changed.
2. rtk rewrites a bare `pytest`/`cargo` into a wrapped run that has stalled for
   12+ minutes with zero output. `fno test` is not a command rtk rewrites, so
   it spawns pytest directly; we also set `RTK_DISABLED=1` in the child env so
   nothing re-wraps it.
3. `... | tail && echo OK` masks pytest's real exit code (false green). We run
   pytest with inherited stdio and propagate its *actual* return code.

The interpreter is resolved worktree-venv -> canonical-venv -> the running
interpreter, so a fresh worktree with no local `.venv` still runs.

Exposed as a `click.Command` (not a plain Typer function) so the lazy group
uses it verbatim, giving `UNPROCESSED` passthrough: every pytest flag (`-x`,
`-k expr`, `::nodeid`) flows through untouched.
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Optional, Sequence

import click


def _repo_root(start: Path) -> Optional[Path]:
    """Walk up from `start` to the checkout root (the dir with cli/src/fno)."""
    for d in (start, *start.parents):
        if (d / "cli" / "src" / "fno" / "__init__.py").exists():
            return d
    return None


def _canonical_root() -> Optional[Path]:
    """The main checkout root: parent of git's common .git dir.

    In a worktree, `git rev-parse --git-common-dir` points at the MAIN repo's
    `.git`, so its parent is the canonical checkout (whose `cli/.venv` a
    worktree shares deps with). In the main checkout it is just `.git`.
    """
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--git-common-dir"],
            capture_output=True,
            text=True,
            check=False,
        )
    except FileNotFoundError:
        return None
    if out.returncode != 0 or not out.stdout.strip():
        return None
    common = Path(out.stdout.strip())
    if not common.is_absolute():
        common = (Path.cwd() / common).resolve()
    return common.parent


def _resolve_interpreter(root: Path) -> str:
    """worktree venv -> canonical venv -> the interpreter running fno."""
    local = root / "cli" / ".venv" / "bin" / "python"
    if local.exists():
        return str(local)
    canon = _canonical_root()
    if canon is not None:
        cand = canon / "cli" / ".venv" / "bin" / "python"
        if cand.exists():
            return str(cand)
    return sys.executable


def _run(args: Sequence[str]) -> int:
    """Resolve interpreter + env, run pytest, return its real exit code."""
    root = _repo_root(Path.cwd()) or Path.cwd()
    interp = _resolve_interpreter(root)

    env = os.environ.copy()
    src = str((root / "cli" / "src").resolve())
    existing = env.get("PYTHONPATH")
    env["PYTHONPATH"] = src + (os.pathsep + existing if existing else "")
    env["RTK_DISABLED"] = "1"  # never let rtk re-wrap the child run

    cmd = [interp, "-m", "pytest", *args]
    try:
        proc = subprocess.run(cmd, env=env)  # inherit stdio; no pipe, no mask
    except FileNotFoundError:
        sys.stderr.write(f"fno test: interpreter not found: {interp}\n")
        return 127
    return proc.returncode


@click.command(
    name="test",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Run pytest against the worktree source (pinned PYTHONPATH), bypassing "
        "rtk, with the real pytest exit code. All args pass through to pytest."
    ),
)
@click.argument("pytest_args", nargs=-1, type=click.UNPROCESSED)
def test_command(pytest_args: tuple[str, ...]) -> None:
    raise SystemExit(_run(list(pytest_args)))
