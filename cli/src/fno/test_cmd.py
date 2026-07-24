"""`fno test [pytest-args...]` / `fno test rust [cargo-args...]` - run a suite
honestly AND tersely.

Footguns this verb encodes (tribal knowledge made runtime behavior):

1. A worktree's bare `pytest` imports the *canonical* `fno` (installed editable
   from the main checkout), not the worktree's own source. We pin
   `PYTHONPATH=<repo>/cli/src` so a worktree tests what it changed.
2. rtk rewrites a bare `pytest`/`cargo` into a wrapped run that has stalled for
   12+ minutes with zero output. `fno test` spawns the runner directly and sets
   `RTK_DISABLED=1` in the child env so nothing re-wraps it.
3. `... | tail && echo OK` masks the real exit code (false green). We propagate
   the child's *actual* return code.
4. Full test output in an agent transcript is re-read by every later request in
   the session. Default mode therefore captures ALL output to
   `<repo>/.fno/last-test.log` and prints only a summary: on failure, the TAIL
   of the log (errors live at the end - read from the end, expand upward via
   the log path). `--stream` restores inherited stdio for interactive runs.

The interpreter is resolved worktree-venv -> canonical-venv -> the running
interpreter, so a fresh worktree with no local `.venv` still runs.

Exposed as a `click.Command` (not a plain Typer function) so the lazy group
uses it verbatim, giving `UNPROCESSED` passthrough: every pytest flag (`-x`,
`-k expr`, `::nodeid`) flows through untouched.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from collections import deque
from pathlib import Path
from typing import Optional, Sequence

import click

_TAIL_LINES = 40


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


def _log_path(root: Path) -> Path:
    d = root / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    # ponytail: one shared file per checkout, last-writer-wins; per-run files
    # if parallel same-worktree runs ever matter.
    return d / "last-test.log"


def _tail(path: Path, n: int) -> list[str]:
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            return list(deque(fh, maxlen=n))
    except OSError:
        return []


def _child_env(root: Path) -> dict:
    env = os.environ.copy()
    src = str((root / "cli" / "src").resolve())
    existing = env.get("PYTHONPATH")
    env["PYTHONPATH"] = src + (os.pathsep + existing if existing else "")
    env["RTK_DISABLED"] = "1"  # never let rtk re-wrap the child run
    return env


def _run_captured(cmds: Sequence[Sequence[str]], env: dict, log: Path) -> int:
    """Run each command with output captured to `log`; print the terse verdict.

    The header (command + log path) prints BEFORE the run so a long suite never
    looks stalled - a watcher can `tail -f` the log. Returns the first non-zero
    child exit code, else 0.
    """
    rc = 0
    with open(log, "w", encoding="utf-8") as fh:
        for cmd in cmds:
            print(f"running: {' '.join(map(str, cmd))} | log: {log}", flush=True)
            fh.write(f"$ {' '.join(map(str, cmd))}\n")
            fh.flush()
            try:
                proc = subprocess.run(cmd, env=env, stdout=fh, stderr=subprocess.STDOUT)
            except OSError as exc:
                sys.stderr.write(f"fno test: failed to run {cmd[0]}: {exc}\n")
                return 127
            if proc.returncode != 0:
                rc = proc.returncode
                break  # first failure wins; its output is the log tail
    if rc == 0:
        lines = [ln.rstrip() for ln in _tail(log, 5) if ln.strip()]
        summary = lines[-1] if lines else "(no output)"
        print(f"PASS | {summary}")
    else:
        print(
            f"FAIL (rc={rc}) - last {_TAIL_LINES} lines below; read from the end, "
            f"expand upward if needed: tail -100 {log}"
        )
        sys.stdout.write("".join(_tail(log, _TAIL_LINES)))
    return rc


def _run(args: Sequence[str], stream: bool = False) -> int:
    """Resolve interpreter + env, run pytest, return its real exit code."""
    root = _repo_root(Path.cwd()) or Path.cwd()
    interp = _resolve_interpreter(root)

    # Default to THE Python suite (cli/tests) when no collection target was
    # given. A bare `pytest` collects from cwd, which from the repo root pulls
    # in script-style tests/ files that `raise SystemExit` at import (pytest
    # INTERNALERROR, not a real run). A collection target is a non-flag arg
    # that names a path or nodeid; a flag value like `-k expr` does not count.
    pytest_args = list(args)
    has_target = any(
        (not a.startswith("-")) and ("::" in a or "/" in a or Path(a).exists())
        for a in pytest_args
    )
    if not has_target:
        pytest_args.append(str((root / "cli" / "tests").resolve()))

    env = _child_env(root)
    cmd = [interp, "-m", "pytest", *pytest_args]
    if stream:
        try:
            proc = subprocess.run(cmd, env=env)  # inherit stdio; no pipe, no mask
        except OSError as exc:
            # FileNotFoundError (missing) AND PermissionError (present but not
            # executable) are both OSError; either means we could not run it.
            sys.stderr.write(f"fno test: failed to run interpreter {interp}: {exc}\n")
            return 127
        return proc.returncode
    return _run_captured([cmd], env, _log_path(root))


def _run_rust(args: Sequence[str], stream: bool = False) -> int:
    """Run the Rust suites: nextest when installed, else `cargo test -q`.

    No workspace root exists, so without an explicit `--manifest-path` we sweep
    every `crates/*/Cargo.toml` (the two-test-trees lesson: a green subset is
    not proof).
    """
    root = _repo_root(Path.cwd()) or Path.cwd()
    cargo_args = list(args)
    if shutil.which("cargo-nextest"):
        base = ["cargo", "nextest", "run"]
    else:
        base = ["cargo", "test", "-q"]

    if "--manifest-path" in cargo_args:
        cmds = [[*base, *cargo_args]]
    else:
        manifests = sorted((root / "crates").glob("*/Cargo.toml"))
        if not manifests:
            sys.stderr.write(f"fno test rust: no crates/*/Cargo.toml under {root}\n")
            return 2
        cmds = [[*base, "--manifest-path", str(m), *cargo_args] for m in manifests]

    env = _child_env(root)
    if stream:
        rc = 0
        for cmd in cmds:
            try:
                proc = subprocess.run(cmd, env=env)
            except OSError as exc:
                sys.stderr.write(f"fno test: failed to run {cmd[0]}: {exc}\n")
                return 127
            if proc.returncode != 0:
                return proc.returncode
        return rc
    return _run_captured(cmds, env, _log_path(root))


@click.command(
    name="test",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    help=(
        "Run the Python suite (or `fno test rust ...` for the crates) with the "
        "real exit code, rtk bypassed, and PYTHONPATH pinned to the worktree. "
        "Use this, never a bare `pytest` in a worktree: that imports the "
        "canonical fno, lets rtk re-wrap the run, and masks the exit code. "
        "Full output goes to .fno/last-test.log; the transcript gets PASS or "
        "the failing tail. --stream restores full inherited-stdio output."
    ),
)
@click.option("--stream", is_flag=True, help="Stream full output (no capture/log).")
@click.argument("runner_args", nargs=-1, type=click.UNPROCESSED)
def test_command(stream: bool, runner_args: tuple[str, ...]) -> None:
    args = list(runner_args)
    if args and args[0] == "rust":
        raise SystemExit(_run_rust(args[1:], stream=stream))
    raise SystemExit(_run(args, stream=stream))
