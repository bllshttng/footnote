"""``fno agents king history`` - native-read relay.

Scan owner: the native ``king-history`` verb, which resolves the caller's
crown scope itself; contract: docs/architecture/reign.md.
"""
from __future__ import annotations

import subprocess
from pathlib import Path


class HistoryUnreadable(Exception):
    """No resolvable crown, or a corrupt journal line."""


def run_native(events_paths: list[Path], scope: str, as_json: bool) -> tuple[int, str, str]:
    """Relay to the native ``king-history`` read; ``(code, stdout, stderr)``.

    ``--scope`` rides the argv only when set; when empty the native verb
    resolves the caller's crown from the registry.
    """
    from fno.agents.rust_runtime import refuse_without_binary
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        refuse_without_binary("king history")
    argv = [str(binary), "king-history", *(["--scope", scope] if scope.strip() else [])]
    for path in events_paths:
        argv += ["--events-path", str(path)]
    if as_json:
        argv.append("--json")
    proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    return proc.returncode, proc.stdout, proc.stderr


def verdict_read(events_paths: "list[Path]", scope: "str | None", as_json: bool) -> "tuple[int, str, str]":
    """Relay to the native ``king-history --verdict`` read; ``(code, stdout, stderr)``."""
    from fno.agents.rust_runtime import refuse_without_binary
    from fno.rust_binary import resolve_binary

    argv = [
        str(resolve_binary() or refuse_without_binary("king verdict")),
        "king-history",
        "--verdict",
        "--cwd",
        str(Path.cwd()),
        *(["--scope", scope] if scope else []),
        *[x for p in events_paths for x in ("--events-path", str(p))],
        *(["--json"] if as_json else []),
    ]
    proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    return proc.returncode, proc.stdout, proc.stderr
