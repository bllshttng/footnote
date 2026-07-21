#!/usr/bin/env python3
"""Compatibility shim. Real implementation lives in fno.graph.cli.

In-development path: if fno is not installed, automatically prepends
cli/src to sys.path when running from the repo root, so existing callers
(hooks, tests, skills) work without `uv tool install`.
"""
import sys
from pathlib import Path

# In-development fallback: detect repo root and prepend cli/src if the
# fno.graph package is not already importable but cli/src exists.
_repo_root = Path(__file__).resolve().parents[1]
_cli_src = _repo_root / "cli" / "src"
if not _cli_src.is_dir():
    sys.stderr.write(
        f"error: fno CLI shim broken: expected cli/src at {_cli_src}, not found\n"
        f"       (shim location: {Path(__file__).resolve()}; parents[1]={_repo_root})\n"
        "       Either restore the shim to scripts/ at the repo root, or update parents[1] to match.\n"
    )
    sys.exit(3)
if str(_cli_src) not in sys.path:
    sys.path.insert(0, str(_cli_src))

try:
    from fno.graph.cli import cli as app
except ImportError as exc:
    # Name the import that actually failed. cli/src is already on sys.path
    # above, so the usual cause is a MISSING DEPENDENCY under whichever
    # interpreter ran this shim (a bare system python3 has no typer), not an
    # absent fno. Hence the remedy is a dependency-complete interpreter, NOT
    # `uv tool install` - that installs a separate tool venv and leaves the
    # interpreter running this shim exactly as broken.
    sys.stderr.write(
        f"error: cannot import fno.graph.cli under {sys.executable}: {exc}\n"
        "       Re-run under an interpreter that has fno's dependencies:\n"
        f"       uv run --project '{_repo_root / 'cli'}' "
        f"python '{Path(__file__).resolve()}' ...\n"
    )
    sys.exit(3)

if __name__ == "__main__":
    # Typer apps are invoked via app(standalone_mode=...) or app().
    # Use standalone_mode=True (default) to preserve argparse-compatible exit codes.
    app(standalone_mode=True)
