"""Session tripwire: a pytest run that wrote into a live root fails at exit.

The hermetic env bounds where tests READ config from; it cannot stop a binary
that ignores the bound (the Rust config readers before their ceiling). This
guard watches the real roots for new entries carrying THIS session's markers,
so a leak names itself and the run fails. It never deletes anything: another
session's entries and a live probe's canary are never blamed, and every token
and marker is session-specific.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

# Known-noisy trees a legitimate process under a root may own; never walked.
_PRUNE = {"worktrees", "cargo-build", "target", ".git"}

# A live probe's canary shares the machine with every session; it warns.
_PROBE = "fno-probe-"

_MIB = 1_048_576


def _slug_token(marker: str) -> str:
    """The marker path rendered the way a space slug renders it.

    A space slug replaces every ``/`` with ``-``, so a basetemp
    ``pytest-of-<user>/pytest-7`` becomes ``pytest-of-<user>-pytest-7``:
    session-specific, and never the bare ``pytest-of-`` prefix every other
    pytest run on the machine also carries.
    """
    path = Path(marker)
    parent = path.parent.name
    name = path.name
    return f"{parent}-{name}" if parent else name


def live_roots(
    home: Path,
    checkout: Path,
    plans_dir: Path | None,
    exclude: frozenset[str] = frozenset(),
) -> list[tuple[Path, int]]:
    """The live roots to watch, deduped by realpath.

    ``<home>/.fno`` at depth 2, ``<home>/.claude`` at depth 1, the canonical
    checkout (the first ``worktree`` line of ``git -C <checkout> worktree list
    --porcelain``) at depth 1, the running checkout's toplevel at depth 1, and
    ``plans_dir`` at depth 1 when given. A candidate whose realpath falls
    inside an ``exclude`` root (a sandbox the outer runner declared
    explicitly) is not a live root.
    """
    roots: list[tuple[Path, int]] = []
    seen: set[Path] = set()
    excluded = {Path(e) for e in exclude}

    def add(path: Path, depth: int) -> None:
        resolved = Path(os.path.realpath(path))
        if any(resolved == e or resolved.is_relative_to(e) for e in excluded):
            return
        if resolved in seen:
            return
        seen.add(resolved)
        roots.append((path, depth))

    for sub, depth in ((".fno", 2), (".claude", 1)):
        candidate = home / sub
        if candidate.is_dir():
            add(candidate, depth)

    def git(*args: str) -> str:
        out = subprocess.run(
            ["git", "-C", str(checkout), *args], capture_output=True, text=True
        )
        return out.stdout if out.returncode == 0 else ""

    porcelain = git("worktree", "list", "--porcelain")
    for line in porcelain.splitlines():
        if line.startswith("worktree "):
            add(Path(line[len("worktree ") :]), 1)
            break
    toplevel = git("rev-parse", "--show-toplevel").strip()
    if toplevel:
        add(Path(toplevel), 1)
    if plans_dir is not None and plans_dir.is_dir():
        add(plans_dir, 1)
    return roots


def _walk(root: Path, depth: int) -> set[Path]:
    """Entry paths under `root`, to `depth` levels, pruning `_PRUNE` names."""
    found: set[Path] = set()
    if depth <= 0:
        return found
    try:
        scanner = os.scandir(root)
    except OSError:
        return found
    with scanner:
        for entry in scanner:
            path = Path(entry.path)
            if entry.is_dir(follow_symlinks=False):
                if entry.name in _PRUNE:
                    continue
                found.add(path)
                if depth > 1:
                    found |= _walk(path, depth - 1)
            else:
                found.add(path)
    return found


def snapshot(roots: list[tuple[Path, int]]) -> set[Path]:
    """The set of entry paths under each root, to the root's depth."""
    found: set[Path] = set()
    for root, depth in roots:
        found |= _walk(root, depth)
    return found


def find_leaks(before: set[Path], roots: list[tuple[Path, int]], markers: set[str]) -> list[Path]:
    """New entries carrying this session's markers, most useful order.

    A leak is a new entry whose NAME contains a session name token, or that is
    a regular file of at most 1 MiB whose bytes contain a marker. A new
    ``fno-probe-`` entry is printed as a warning and never fails the run.
    """
    new = snapshot(roots) - before
    tokens = {_slug_token(marker) for marker in markers}
    raw = {marker.encode() for marker in markers}
    leaks: list[Path] = []
    for path in sorted(new):
        if _PROBE in path.name:
            print(f"prod tripwire: probe entry {path} is a warning, not a leak", file=sys.stderr)
            continue
        if any(token in path.name for token in tokens):
            leaks.append(path)
            continue
        data: bytes | None = None
        try:
            if path.is_file() and path.stat().st_size <= _MIB:
                data = path.read_bytes()
        except OSError:
            data = None
        if data is not None and any(marker in data for marker in raw):
            leaks.append(path)
    return leaks


def resolve_real_plans_dir(real_env: dict[str, str], canonical: Path) -> Path | None:
    """The REAL plans dir, answered by the Rust binary under the real env.

    The binary is the source of truth: re-deriving the plans chain here would
    drift exactly the way the bug this guards drifts. A missing or stale
    binary skips the root (one stderr line); it never fails the run.
    """
    exe = shutil.which("fno-agents", path=real_env.get("PATH", ""))
    if exe is None:
        print("prod tripwire: fno-agents not on PATH; the plans dir stays unwatched", file=sys.stderr)
        return None
    try:
        out = subprocess.run(
            [exe, "state", "plan-dir", str(canonical)],
            env=real_env,
            capture_output=True,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"prod tripwire: plan-dir probe failed ({exc}); the plans dir stays unwatched", file=sys.stderr)
        return None
    if out.returncode != 0 or not out.stdout.strip():
        print(
            f"prod tripwire: plan-dir probe failed (rc={out.returncode}); the plans dir stays unwatched",
            file=sys.stderr,
        )
        return None
    return Path(out.stdout.strip().splitlines()[-1])
