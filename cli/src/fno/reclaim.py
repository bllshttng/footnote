"""Reclaim disk bloat that footnote development piles up (``fno doctor reclaim``).

Ported from the machine-local stopgap ``~/.fno/bin/fno-reclaim`` (x-7ca7 task
4.1) so every clone ships the janitor instead of one developer's bin dir.
Everything the lanes remove is rebuilt or downloaded again on demand. Dry run
by default; ``--apply`` removes and writes ``<state_dir>/reclaim/last-run.json``
with the bytes each lane reclaimed.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

import typer

__all__ = ["run_reclaim", "reclaim_command"]

# A running test keeps its own fake HOME; two hours means nobody is using it.
LEAKED_HOME_MINUTES = 120
# No fno runtime state lives under an fno-* name in the temp dir; only tests
# write it.
SCRATCH_MINUTES = 24 * 60
# uv's prune waits for every running uv process first, so the wait is capped.
UV_PRUNE_TIMEOUT_SECONDS = 120

_LEAKED_HOME_MARKERS = (
    ".fno",
    ".cache/fno-bootstrap",
    ".cache/uv",
    ".local/share/uv",
    ".claude.json",
)


@dataclass
class Lane:
    name: str
    paths: list[Path] = field(default_factory=list)
    bytes: int = 0
    note: str = ""

    @property
    def count(self) -> int:
        return len(self.paths)


def _tree_bytes(path: Path) -> int:
    try:
        out = subprocess.run(
            ["du", "-sk", str(path)],
            capture_output=True,
            text=True,
            check=False,
        ).stdout
        return int(out.split()[0]) * 1024
    except (OSError, ValueError, IndexError):
        return 0


def _temp_root() -> Path:
    out = subprocess.run(
        ["getconf", "DARWIN_USER_TEMP_DIR"], capture_output=True, text=True, check=False
    ).stdout.strip()
    if out:
        return Path(out.rstrip("/"))
    return Path(tempfile.gettempdir())


def _plugin_cache_copies() -> list[Path]:
    """Cargo output and worktrees copied into harness plugin caches. The
    plugin runs from these copies, but nothing reads a cargo target or a
    worktree there."""
    found: list[Path] = []
    for root in Path.home().glob(".claude/plugins/cache/*/fno/*"):
        found.extend(_tagged_crate_targets(root))
        found.extend(sorted(root.glob(".claude/worktrees/*")))
        found.extend(sorted(root.glob(".tmp-worktrees/*")))
    for root in Path.home().glob(".codex/plugins/cache/*/fno/*"):
        found.extend(_tagged_crate_targets(root))
        found.extend(sorted(root.glob(".claude/worktrees/*")))
        found.extend(sorted(root.glob(".tmp-worktrees/*")))
    return found


def _tagged_crate_targets(root: Path) -> list[Path]:
    # Tagged by CACHEDIR.TAG, never name-matched: cli/src/fno/target and
    # friends are source dirs (a name-based sweep deleted 66 of them once).
    return [p for p in root.glob("crates/*/target") if (p / "CACHEDIR.TAG").is_file()]


def _leaked_test_homes() -> list[Path]:
    """Fake HOME dirs tests left in the temp dir: a tempfile dir holding fno
    or uv state, old enough that no live test owns it."""
    root = _temp_root()
    cutoff = time.time() - LEAKED_HOME_MINUTES * 60
    found: list[Path] = []
    for entry in root.iterdir():
        if not entry.name.startswith(".tmp") or not entry.is_dir():
            continue
        try:
            if entry.stat().st_mtime > cutoff:
                continue
        except OSError:
            continue
        if any((entry / marker).exists() for marker in _LEAKED_HOME_MARKERS):
            found.append(entry)
    return found


def _stale_test_scratch() -> list[Path]:
    root = _temp_root()
    cutoff = time.time() - SCRATCH_MINUTES * 60
    found: list[Path] = []
    for entry in root.iterdir():
        if not entry.name.startswith("fno-"):
            continue
        try:
            if entry.stat().st_mtime > cutoff:
                continue
        except OSError:
            continue
        found.append(entry)
    return found


def _registered_worktrees() -> set[str]:
    try:
        out = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        ).stdout
    except OSError:
        return set()
    return {
        line[len("worktree "):] for line in out.splitlines() if line.startswith("worktree ")
    }


def _untracked_worktree_targets() -> list[Path]:
    """Cargo output under a worktree dir git no longer tracks (a removed or
    half-made worktree). Only target/ goes: the rest may hold work."""
    repo = _canonical_root()
    if repo is None:
        return []
    registered = _registered_worktrees()
    bases = [
        repo / ".claude" / "worktrees",
        Path.home() / ".fno" / "worktrees" / repo.name,
    ]
    found: list[Path] = []
    for base in bases:
        if not base.is_dir():
            continue
        for tree in base.iterdir():
            if not tree.is_dir() or str(tree) in registered:
                continue
            found.extend(_tagged_crate_targets(tree))
    return found


def _canonical_root() -> Path | None:
    try:
        from fno.paths import resolve_canonical_repo_root

        return resolve_canonical_repo_root()
    except Exception:  # noqa: BLE001 - the lane degrades to skipped
        return None


def _uv_cache_dir() -> Path | None:
    try:
        out = subprocess.run(
            ["uv", "cache", "dir", "--color", "never"],
            capture_output=True,
            text=True,
            check=False,
        ).stdout.strip()
        return Path(out) if out else None
    except OSError:
        return None


def run_reclaim(apply: bool) -> list[Lane]:
    """Measure (and with ``apply``, remove) every lane; dry run by default."""
    lanes = [
        Lane("plugin_cache_build_copies", _plugin_cache_copies()),
        Lane("leaked_test_homes", _leaked_test_homes()),
        Lane("stale_test_scratch", _stale_test_scratch()),
        Lane("untracked_worktree_targets", _untracked_worktree_targets()),
    ]
    for lane in lanes:
        lane.bytes = sum(_tree_bytes(p) for p in lane.paths)
        if apply and lane.paths:
            for path in lane.paths:
                shutil.rmtree(path, ignore_errors=True)

    uv = Lane("uv_cache_prune")
    cache_dir = _uv_cache_dir()
    if cache_dir is None:
        uv.note = "uv not found"
    else:
        uv.paths = [cache_dir]
        uv.bytes = _tree_bytes(cache_dir)
        if apply:
            try:
                proc = subprocess.run(
                    ["uv", "cache", "prune", "--color", "never"],
                    capture_output=True,
                    text=True,
                    timeout=UV_PRUNE_TIMEOUT_SECONDS,
                    check=False,
                )
                uv.note = "pruned" if proc.returncode == 0 else f"prune failed rc={proc.returncode}"
            except subprocess.TimeoutExpired:
                uv.note = f"prune skipped: uv busy for {UV_PRUNE_TIMEOUT_SECONDS}s"
        else:
            uv.note = "would run: uv cache prune"
    lanes.append(uv)

    if apply:
        _write_receipt(lanes)
    return lanes


def _receipt_path() -> Path:
    from fno.paths import state_dir

    return state_dir() / "reclaim" / "last-run.json"


def _write_receipt(lanes: list[Lane]) -> None:
    import json

    path = _receipt_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "applied_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "lanes": {
            lane.name: {"paths": lane.count, "bytes": lane.bytes, "note": lane.note}
            for lane in lanes
        },
        "total_bytes": sum(l.bytes for l in lanes),
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def main(apply: bool, verbose: bool) -> int:
    lanes = run_reclaim(apply=apply)
    print("fno doctor reclaim: removing" if apply else "fno doctor reclaim: dry run (--apply to remove)")
    for lane in lanes:
        if lane.count == 0 and not lane.note:
            print(f"       -  {lane.name}: nothing")
            continue
        gb = lane.bytes / (1024 * 1024 * 1024)
        print(f"{gb:6.1f} GB  {lane.name}: {lane.count} path(s){' - ' + lane.note if lane.note else ''}")
        if verbose:
            for path in lane.paths:
                print(f"          {path}")
    if apply:
        print(f"receipt: {_receipt_path()}")
    return 0


def reclaim_command(
    apply: bool = typer.Option(False, "--apply", help="Remove what the lanes found."),
    verbose: bool = typer.Option(False, "-v", "--verbose", help="List every path."),
) -> None:
    """Reclaim disk bloat: plugin-cache build copies, leaked test HOMEs, stale scratch."""
    raise typer.Exit(main(apply=apply, verbose=verbose))
