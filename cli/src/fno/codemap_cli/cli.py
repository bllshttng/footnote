"""fno codemap - AST + PageRank codebase map.

Thin wrapper around scripts/codemap/repogram.py (the analysis engine) and
scripts/codemap/db-schema.py (optional DB-aware companion). The wrapper
preserves byte-equivalent output so callers that already rely on
.fno/codemap.md (blueprint, target, operator, megawalk) keep working.
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
        env["PATH"] = ":".join(p for p in env.get("PATH", "").split(":") if not p.startswith(venv))
    return env

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
    repo_root = Path(resolve_repo_root())
    target_repo = repo or repo_root
    script = repo_root / "scripts" / "codemap" / "repogram.py"
    if not script.exists():
        typer.echo(f"repogram script not found at {script}", err=True)
        raise typer.Exit(code=2)
    # Mixed-format guard: --json + --db-schema appends a markdown section
    # to a JSON stream, producing an unparseable file. Reject the combo
    # rather than silently emitting invalid output (Codex review P2).
    if json_output and db_schema:
        typer.echo(
            "fno codemap: --json and --db-schema are incompatible "
            "(JSON output cannot accept the markdown db-schema appendix)",
            err=True,
        )
        raise typer.Exit(code=2)
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
    cmd = ["python3", str(script), str(target_repo), "--tokens", str(tokens)]
    if json_output:
        cmd.append("--json")
    if orphans:
        cmd.append("--orphans")
    env = _system_python_env()
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
            db_script = repo_root / "scripts" / "codemap" / "db-schema.py"
            if db_script.exists():
                with open(tmp_path, "a", encoding="utf-8") as fh:
                    db_result = subprocess.run(
                        ["python3", str(db_script), str(target_repo)],
                        stdout=fh,
                        env=env,
                    )
                # Don't fail the whole command if db-schema fails - the
                # primary codemap is the load-bearing artifact. Surface
                # the failure to stderr so the user can investigate.
                if db_result.returncode != 0:
                    typer.echo(
                        f"warning: fno codemap --db-schema companion exited "
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
