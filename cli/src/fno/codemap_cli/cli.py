"""fno doctor codemap - AST + PageRank codebase map.

Thin wrapper around ``repogram.py`` (the analysis engine) and ``db-schema.py``
(optional DB-aware companion), both of which sit next to this module. The
wrapper preserves byte-equivalent output so callers that already rely on
.fno/codemap.md (blueprint, target, operator, megawalk) keep working.

The engines live INSIDE the package, not under the repo's ``scripts/``, because
they are fno's own analysis code and must be found wherever fno is installed.
Resolving them from the analyzed repo made ``fno doctor codemap`` work only when the
analyzed repo happened to be the footnote checkout itself.
"""
import os
import subprocess
import tempfile
from pathlib import Path
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.paths import resolve_repo_root


def _system_python_env() -> dict:
    """Strip the cli's venv markers so subprocess python3 hits system Python.

    repogram.py imports networkx + tree-sitter + grep-ast + pygments, which
    we deliberately don't bundle into the cli wheel (heavy native deps).
    The user's system python typically has these installed because the old
    /codemap skill ran against system python too. Stripping VIRTUAL_ENV from
    the subprocess env (and removing venv-prefixed entries from PATH) makes
    `python3` resolve to whatever their shell normally sees.
    """
    env = os.environ.copy()
    venv = env.pop("VIRTUAL_ENV", None)
    if venv:
        # os.pathsep, not ":" - the repo idiom (config_io, test_cmd). On Windows
        # the separator is ";" and splitting on ":" tears "C:\..." in half,
        # silently mangling every entry rather than filtering one.
        env["PATH"] = os.pathsep.join(
            p for p in env.get("PATH", "").split(os.pathsep) if not p.startswith(venv)
        )
    return env


def _interpreter_names() -> tuple[str, ...]:
    """Candidate basenames for a python interpreter on this platform.

    Extensionless names never resolve on Windows, where the executables are
    `python.exe` / `python3.exe`, so probing only the bare names would report
    "no interpreter" on a machine that has a perfectly good one.
    """
    if os.name == "nt":
        return ("python3.exe", "python.exe", "python3", "python")
    return ("python3", "python")


#: repogram's heavy native deps, deliberately not bundled into the fno wheel.
#: Only the three repogram HARD-exits on (repogram.py:30-54). ``tree_sitter`` is
#: deliberately absent: repogram degrades to ``Query = None`` without it, so
#: probing for it would reject an interpreter that runs the engine fine.
ENGINE_DEPS = ("networkx", "grep_ast", "pygments")

#: Exit codes, one per distinguishable failure. All three were `2` until CI
#: proved that unusable: "your fno install is broken", "you passed incompatible
#: flags", and "no python on this machine carries the engine deps" need three
#: different responses from a human and three different branches from a script.
#: The engine-resolution regression test is itself such a caller - it has to
#: tell EXIT_NO_ENGINE from EXIT_NO_INTERPRETER, and could not while both were
#: 2, because a CI box legitimately has neither and the shared code read as a
#: resolution failure.
EXIT_USAGE = 2  # incompatible flags
EXIT_NO_ENGINE = 3  # broken/incomplete fno installation
EXIT_NO_INTERPRETER = 4  # no python on PATH can import ENGINE_DEPS


def _engine_python(env: dict) -> tuple[Optional[str], list[str]]:
    """First interpreter on PATH that can actually import the engine's deps.

    Returns ``(interpreter, tried)``; ``interpreter`` is None when none work,
    and ``tried`` lists every candidate probed so the error can name them.

    Invoking a bare ``python3`` is a guess, not a resolution: a machine with
    several pythons (pyenv, homebrew, ~/.local/bin, uv) usually has the deps in
    exactly one of them, and the first on PATH is often not it. Probing turns a
    silent "Missing dependency: pip install networkx" into either the right
    interpreter or an error that lists what was tried.
    """
    probe = "import " + ", ".join(ENGINE_DEPS)
    tried: list[str] = []
    seen: set[str] = set()
    for directory in env.get("PATH", "").split(os.pathsep):
        if not directory:
            continue
        for name in _interpreter_names():
            exe = Path(directory) / name
            if not exe.is_file() or not os.access(exe, os.X_OK):
                continue
            key = str(exe.resolve())
            if key in seen:
                continue
            seen.add(key)
            tried.append(str(exe))
            try:
                probe_run = subprocess.run(
                    [str(exe), "-c", probe], capture_output=True, env=env, timeout=30
                )
            except (OSError, subprocess.TimeoutExpired):
                # A shim that hangs or refuses to exec (a broken pyenv shim is the
                # usual one) must cost us this candidate, not the whole command.
                continue
            if probe_run.returncode == 0:
                return str(exe), tried
    return None, tried


#: The engines ship as package data beside this module, so they resolve from the
#: fno INSTALLATION (wheel, editable install, or source checkout) rather than
#: from the repo being analyzed.
ENGINE_DIR = Path(__file__).resolve().parent


app = typer.Typer(
    name="codemap",
    help="Generate AST+PageRank codebase map (writes to .fno/codemap.md by default).",
    invoke_without_command=True,
)


@app.callback()
def codemap(
    ctx: typer.Context,
    output: Optional[Path] = typer.Option(
        None,
        "--output",
        "-o",
        help="Output path. Defaults to .fno/codemap.md under the repo root.",
    ),
    tokens: int = typer.Option(2048, "--tokens", help="Token budget for the map."),
    repo: Optional[Path] = typer.Option(
        None, "--repo", help="Target repo path. Defaults to the current repo root."
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON instead of markdown."),
    orphans: bool = typer.Option(False, "--orphans", help="List files with no inbound references."),
    db_schema: bool = typer.Option(
        False,
        "--db-schema",
        help="Also append the DB-schema companion section (Supabase/Drizzle aware).",
    ),
) -> None:
    """Run the repogram analysis and write the codemap."""
    if ctx.invoked_subcommand is not None:
        return
    target_repo = repo or Path(resolve_repo_root())
    script = ENGINE_DIR / "repogram.py"
    if not script.exists():
        # Name the installation, not the analyzed repo: the old message pointed
        # at the target repo and sent readers hunting for a file that never
        # belonged there.
        typer.echo(
            f"fno doctor codemap: the repogram engine is missing from this fno "
            f"installation (looked for {script}). This is a broken/incomplete "
            f"fno install, not a problem with {target_repo}. Try `fno doctor update`.",
            err=True,
        )
        raise typer.Exit(code=EXIT_NO_ENGINE)
    # Mixed-format guard: --json + --db-schema appends a markdown section
    # to a JSON stream, producing an unparseable file. Reject the combo
    # rather than silently emitting invalid output (Codex review P2).
    if json_output and db_schema:
        typer.echo(
            "fno doctor codemap: --json and --db-schema are incompatible "
            "(JSON output cannot accept the markdown db-schema appendix)",
            err=True,
        )
        raise typer.Exit(code=EXIT_USAGE)
    # Default output path discrimination:
    #   * --json without --output -> .fno/codemap.json so a JSON
    #     run never overwrites the canonical markdown artifact (Codex P2).
    #   * --repo without --output -> write to the ANALYZED repo's
    #     .fno/codemap.md so downstream skills in that repo find
    #     the artifact they expect (Codex P2).
    if output is None:
        anchor_repo = target_repo
        out_name = "codemap.json" if json_output else "codemap.md"
        out_path = anchor_repo / ".fno" / out_name
    else:
        out_path = output
    out_path.parent.mkdir(parents=True, exist_ok=True)
    env = _system_python_env()
    interpreter, tried = _engine_python(env)
    if interpreter is None:
        typer.echo(
            f"fno doctor codemap: no interpreter on PATH can import the engine's "
            f"dependencies ({', '.join(ENGINE_DEPS)}). They are intentionally "
            f"not bundled in the fno wheel (heavy native deps). Install them "
            f"into any python3 on PATH, e.g. "
            f"`pip install networkx tree-sitter grep-ast pygments`.\n"
            f"Tried: {', '.join(tried) or '(no python3 found on PATH)'}",
            err=True,
        )
        raise typer.Exit(code=EXIT_NO_INTERPRETER)
    cmd = [interpreter, str(script), str(target_repo), "--tokens", str(tokens)]
    if json_output:
        cmd.append("--json")
    if orphans:
        cmd.append("--orphans")
    # Write to a sibling tmpfile and os.replace on success so a partial
    # crash (signal-killed repogram, missing dep) leaves the previous
    # codemap.md intact rather than truncating it to whatever bytes
    # repogram managed to flush before dying. Callers (blueprint, target,
    # operator, megawalk) read codemap.md unconditionally; a corrupt
    # half-file would silently misinform them.
    #
    # NamedTemporaryFile(delete=False) is used over the older
    # mkstemp + os.fdopen pattern because the latter leaks the OS file
    # descriptor when os.fdopen() itself raises before its `with` clause
    # takes ownership. NamedTemporaryFile binds the fd to the context
    # manager from the start (Gemini review MEDIUM, PR #267).
    tmp_path = out_path.parent / ".__codemap_tmp_init__"
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            prefix=out_path.name + ".",
            suffix=".tmp",
            dir=str(out_path.parent),
            delete=False,
            encoding="utf-8",
        ) as fh:
            tmp_path = Path(fh.name)
            result = subprocess.run(cmd, stdout=fh, env=env)
        if result.returncode != 0:
            raise typer.Exit(code=propagate_returncode(result.returncode))
        if db_schema:
            db_script = ENGINE_DIR / "db-schema.py"
            if db_script.exists():
                with open(tmp_path, "a", encoding="utf-8") as fh:
                    db_result = subprocess.run(
                        [interpreter, str(db_script), str(target_repo)],
                        stdout=fh,
                        env=env,
                    )
                # Don't fail the whole command if db-schema fails - the
                # primary codemap is the load-bearing artifact. Surface
                # the failure to stderr so the user can investigate.
                if db_result.returncode != 0:
                    typer.echo(
                        f"warning: fno doctor codemap --db-schema companion exited "
                        f"with code {db_result.returncode}; primary codemap is still valid.",
                        err=True,
                    )
        os.replace(tmp_path, out_path)
    finally:
        # mkstemp leaves the file on disk if we raise before os.replace.
        if tmp_path.exists():
            try:
                tmp_path.unlink()
            except OSError:
                pass
    typer.echo(str(out_path))
