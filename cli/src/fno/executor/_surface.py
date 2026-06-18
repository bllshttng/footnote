"""Locked frontend-surface matcher - the SINGLE SOURCE OF TRUTH for the
operator's executor-routing surface globs.

Ported byte-for-byte from the retired ``scripts/lib/infer-task-executor.sh``
(internalized for self-contained packaging, ab-58645f63). This module is now
the one definition of the locked patterns; ``infer-has-ui.sh`` and any other
consumer reach the list through here so executor routing and the has_ui /
frontend-craft signals can never drift apart.

Locked patterns (per plan 2026-05-04-operator-impeccable-executor, locked
decision #2 - changing requires plan revision):
    **/*.tsx, **/*.jsx
    components/**, **/components/**
    routes/**, **/routes/**
    src/styles/**, **/src/styles/**

The list intentionally has NO unconditional ``app/**`` arm. ``app/`` is a
common Python/Go/Rust module root (app/main.py, app/models/user.py); a
directory match would silently misroute backend projects to the frontend
executor. Next.js App Router files (app/page.tsx, app/dashboard/page.tsx)
are still routed correctly via the .tsx/.jsx arms regardless of directory.

CLI (mirrors the old dual-mode script):
    printf 'src/components/Foo.tsx\nsrc/routes/api.ts' | python3 -m fno.executor._surface
    # -> impeccable

    echo 'cli/src/fno/loop.py' | python3 -m fno.executor._surface
    # -> do

    : | python3 -m fno.executor._surface   # empty stdin
    # -> do

    git diff --name-only main...HEAD | python3 -m fno.executor._surface --has-ui
    # -> true | false  (the infer-has-ui.sh contract)
"""
from __future__ import annotations

import sys
from typing import Iterable


def is_frontend_surface_path(path: str) -> bool:
    """Return True if ``path`` matches the locked frontend surface list.

    Reproduces the bash ``case`` statement exactly::

        *.tsx|*.jsx) return 0 ;;
        components/*|*/components/*|routes/*|*/routes/*) return 0 ;;
        src/styles/*|*/src/styles/*) return 0 ;;
        *) return 1 ;;

    In the shell ``case``, ``*`` matches any run of characters INCLUDING the
    empty string and ``/``. So ``components/*`` means "starts with
    ``components/``" (a bare ``components/`` with nothing after still matches,
    because the trailing ``*`` matches empty) and ``*/components/*`` means
    "contains ``/components/``". ``startswith(prefix)`` and ``mid in path`` are
    therefore the exact equivalents - no length guard.
    """
    # *.tsx | *.jsx
    if path.endswith(".tsx") or path.endswith(".jsx"):
        return True
    # components/* | */components/*  and  routes/* | */routes/*
    for token in ("components", "routes"):
        if path.startswith(token + "/"):
            return True
        if ("/" + token + "/") in path:
            return True
    # src/styles/* | */src/styles/*
    if path.startswith("src/styles/"):
        return True
    if "/src/styles/" in path:
        return True
    return False


def any_frontend_surface(paths: Iterable[str]) -> bool:
    """Return True if any path in ``paths`` matches the locked surface list.

    Blank entries are skipped, mirroring the bash ``[[ -z "$path" ]] && continue``
    so the helper is robust to trailing/empty lines.
    """
    for path in paths:
        if not path:
            continue
        if is_frontend_surface_path(path):
            return True
    return False


def _read_stdin_paths() -> list[str]:
    """Read a newline-separated path list from stdin.

    Splitting on ``\n`` and skipping empties reproduces the bash CLI's
    ``while IFS= read -r path`` loop including its no-trailing-newline
    robustness (the last line is processed whether or not stdin ends in a
    newline).
    """
    data = sys.stdin.read()
    return [line for line in data.split("\n") if line != ""]


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    has_ui_mode = "--has-ui" in argv
    matched = any_frontend_surface(_read_stdin_paths())
    if has_ui_mode:
        print("true" if matched else "false")
    else:
        print("impeccable" if matched else "do")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
