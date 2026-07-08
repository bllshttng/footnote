"""WorktreeManager: Python wrapper for git worktree operations.

Used by the megawalk walker to create, manage, and tear down per-node
worktrees. Does NOT shell out to scripts/lib/worktree-manager.sh - uses
subprocess.run(["git", "worktree", ...]) directly.

The shell wrapper (scripts/lib/worktree-manager.sh) remains for skill-level
callers; this module is exclusively for the Python walker.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional


# ---------------------------------------------------------------------------
# Branch naming (x-ff83 W3)
# ---------------------------------------------------------------------------

# The <slug> component is truncated on length; the node id is NEVER truncated -
# it is the load-bearing round-trip token. 60 keeps the whole ref well under
# git's ergonomic limit.
_BRANCH_SLUG_MAX = 60
# Drop everything but [a-z0-9_-]; dots are excluded so a slug can never produce
# a `..` (which git rejects in a ref) or a leading-dot component (gemini PR#149).
_REF_UNSAFE_RE = re.compile(r"[^a-z0-9_-]")


def _branch_prefix() -> str:
    """config.branch.prefix (default 'fno'); degrade to 'fno' if settings fail."""
    try:
        from fno.config import load_settings

        return load_settings().branch.prefix or "fno"
    except Exception:  # noqa: BLE001 - naming must never wedge on a settings read
        return "fno"


def _slug_for_node(node_id: str) -> str:
    """Look up a node's immutable slug from the graph; "" if unresolvable."""
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        for e in read_graph(graph_json()):
            if e.get("id") == node_id:
                return e.get("slug") or ""
    except Exception:  # noqa: BLE001 - no graph => no slug => bare <prefix>/<node>
        return ""
    return ""


def branch_name(
    node_id: str, *, slug: Optional[str] = None, prefix: Optional[str] = None
) -> str:
    """Legible dispatch branch name ``<prefix>/<slug>-<node>`` (x-ff83 W3).

    The full node id is preserved so the branch round-trips back to its node;
    the slug is ref-sanitized and truncated on length, never the id. An empty or
    unresolvable slug degrades to ``<prefix>/<node>`` (never ``<prefix>/-<node>``).
    """
    pfx = (prefix or _branch_prefix()).strip("/")
    if slug is None:
        slug = _slug_for_node(node_id)
    s = re.sub(r"-+", "-", _REF_UNSAFE_RE.sub("-", (slug or "").lower())).strip("-")
    s = s[:_BRANCH_SLUG_MAX].rstrip("-")
    return f"{pfx}/{s}-{node_id}" if s else f"{pfx}/{node_id}"


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


class WorktreeError(Exception):
    """Base exception for all worktree operations."""


class WorktreeStaleError(WorktreeError):
    """Raised when a worktree registration is stale and unrecoverable."""


class WorktreeDiskPressureError(WorktreeError):
    """Raised when free disk space falls below the 2GB minimum threshold."""


# ---------------------------------------------------------------------------
# Data types
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Worktree:
    """Represents a git worktree managed by WorktreeManager."""

    node_id: str
    path: Path
    branch: str
    created_at: str  # ISO timestamp
    base_ref: str    # usually "main"


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------


def _now_iso() -> str:
    """Return current time as an ISO-8601 UTC string."""
    return datetime.now(timezone.utc).isoformat()


def _use_conductor_canonical(repo_root: Path) -> bool:
    """Return True when worktree.use_conductor_canonical is set in settings.

    Conservative: any load/parse failure returns False so callers fall back
    to the repo-local default. Reading directly from
    ``{repo_root}/.fno/settings.yaml`` avoids depending on the global
    settings loader's caching/cwd behavior (the walker invokes this from
    arbitrary cwds).
    """
    settings_path = repo_root / ".fno" / "settings.yaml"
    if not settings_path.exists():
        return False
    try:
        import yaml  # type: ignore[import-untyped]
        data = yaml.safe_load(settings_path.read_text(encoding="utf-8")) or {}
    except Exception:
        return False
    if not isinstance(data, dict):
        return False
    # Read the flag from BOTH config.worktree (nested) and top-level worktree:
    # the bash hook's wt_config reads the top-level block, while real settings.yaml
    # stores it top-level - so a config.worktree-only read made the walker disagree
    # with the hook for the same file (codex P1 on PR #67). Either location wins.
    config = data.get("config")
    for container in (
        config.get("worktree") if isinstance(config, dict) else None,
        data.get("worktree"),
    ):
        if isinstance(container, dict) and bool(container.get("use_conductor_canonical", False)):
            return True
    return False


def _read_worktrees_base_from(settings_path: Path) -> Optional[str]:
    """Return config.paths.worktrees_base from one settings file, or None.

    Every intermediate key is isinstance-checked so a malformed settings.yaml
    (e.g. ``config:`` set to a string) returns None instead of raising.
    """
    if not settings_path.exists():
        return None
    try:
        import yaml  # type: ignore[import-untyped]
        data = yaml.safe_load(settings_path.read_text(encoding="utf-8")) or {}
    except Exception:
        return None
    if not isinstance(data, dict):
        return None
    config = data.get("config")
    paths = config.get("paths") if isinstance(config, dict) else None
    base = paths.get("worktrees_base") if isinstance(paths, dict) else None
    return base if isinstance(base, str) and base else None


def _worktrees_base_override(repo_root: Path) -> Optional[Path]:
    """Return config.paths.worktrees_base for ``repo_root``, or None when unset.

    Reads ``{repo_root}/.fno/settings.yaml`` then the global settings file
    (project-local wins, global fallback) - matching the precedence the hook
    gets from ``fno config get`` and how the maintainer sets the base GLOBALLY.
    The global path resolves via ``fno.config._global_settings_path`` (honors
    ``$FNO_GLOBAL_SETTINGS_PATH``) rather than a hardcoded ``~/.fno``. Read
    directly (not via the settings loader) for the same reason as
    ``_use_conductor_canonical``: the walker runs from arbitrary cwds and must
    not depend on the loader's cwd/caching. A leading ``~`` is expanded; any
    load/parse failure falls through to None.
    """
    from fno.config import _global_settings_path

    for settings_path in (
        repo_root / ".fno" / "settings.yaml",
        _global_settings_path(),
    ):
        base = _read_worktrees_base_from(settings_path)
        if base is not None:
            return Path(os.path.expanduser(base))
    return None


def _canonical_base_dir(repo_root: Path) -> Path:
    """Compute ~/conductor/workspaces/<repo_root.name> as the conductor worktree home.

    The DEPRECATED ``worktree.use_conductor_canonical`` back-compat target.
    Prefer ``config.paths.worktrees_base`` (honored ahead of this in
    ``WorktreeManager.__init__``); this literal is retained only so the legacy
    flag keeps landing under conductor.
    """
    return Path.home() / "conductor" / "workspaces" / repo_root.name


def _run_setup_worktree_hook(repo_root: Path, worktree_path: Path) -> tuple[int, str]:
    """Best-effort: run scripts/setup/setup-worktree.sh inside the new worktree.

    The script symlinks gitignored shared state (.fno/, internal/,
    .claude/ subdirs) from the canonical project into the worktree. Without
    this step, megawalk dispatches into worktrees that have no link to the
    canonical .fno/ state, so target gates can't see backlog mutations
    from sibling worktrees, codemap goes stale, and inbox drain breaks.

    Returns (returncode, stderr_tail). returncode == -1 indicates the script
    was not found (silently tolerated). Any non-zero is logged via stderr
    but never raised - the worktree itself is still usable.
    """
    script = repo_root / "scripts" / "setup" / "setup-worktree.sh"
    if not script.exists():
        return (-1, "")
    proc = subprocess.run(
        ["bash", str(script)],
        cwd=str(worktree_path),
        capture_output=True,
        text=True,
        env={**os.environ, "CANONICAL": str(repo_root), "WORKTREE": str(worktree_path)},
    )
    tail = (proc.stderr or proc.stdout or "")[-500:]
    return (proc.returncode, tail)


class WorktreeManager:
    """Create, remove, archive, and inspect git worktrees for the walker.

    Each active node gets one worktree at:
        {base_dir}/{node_id}
    with branch name (x-ff83 W3):
        <config.branch.prefix>/<slug>-<node_id>   (default prefix "fno")
    An explicit ``branch_suffix`` still produces the legacy ``feature/<suffix>``.

    Parameters
    ----------
    repo_root:
        Absolute path to the repository root (where .git lives).
    base_dir:
        Where worktrees are placed. When omitted, resolution reads
        `<repo_root>/.fno/settings.yaml`:
        1. `config.paths.worktrees_base` set -> `{base}/{repo_root.name}`.
        2. else `config.worktree.use_conductor_canonical: true` (DEPRECATED)
           -> `~/conductor/workspaces/{repo_root.name}` (back-compat).
        3. else -> harness-native `repo_root/.claude/worktrees` (OSS default).
        Tests should pass an explicit ``base_dir`` to avoid touching real
        filesystem state under $HOME.
    """

    def __init__(self, repo_root: Path, base_dir: Optional[Path] = None) -> None:
        self.repo_root = repo_root
        if base_dir is not None:
            self.base_dir = base_dir
        else:
            override = _worktrees_base_override(repo_root)
            if override is not None:
                self.base_dir = override / repo_root.name
            elif _use_conductor_canonical(repo_root):
                self.base_dir = _canonical_base_dir(repo_root)
            else:
                self.base_dir = repo_root / ".claude" / "worktrees"
        # Rolling snapshot: maps worktree path -> last seen git status bytes
        self._git_status_snapshots: dict[Path, bytes] = {}

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def disk_pressure_check(self) -> tuple[int, int]:
        """Check available disk space under base_dir (or its parent if absent).

        Returns (free_gb, total_gb) as integers.
        Raises WorktreeDiskPressureError if free space is below 2 GB.
        """
        # Walk up the path until we find an existing ancestor to stat
        check_path = self.base_dir
        while not check_path.exists() and check_path != check_path.parent:
            check_path = check_path.parent
        usage = shutil.disk_usage(check_path)
        free_gb = int(usage.free / (1024 ** 3))
        total_gb = int(usage.total / (1024 ** 3))
        if free_gb < 2:
            raise WorktreeDiskPressureError(
                f"Free disk space {free_gb} GB is below the 2 GB minimum threshold. "
                "Free up space before creating more worktrees."
            )
        return free_gb, total_gb

    def create(
        self,
        node_id: str,
        base_ref: str = "main",
        *,
        branch_suffix: Optional[str] = None,
    ) -> Worktree:
        """Create a git worktree for the given node.

        Steps:
        1. git worktree prune  (clear stale registrations)
        2. disk_pressure_check  (raise if < 2 GB free)
        3. git worktree add -b {branch} {path} {base_ref}

        Parameters
        ----------
        node_id:
            The graph node identifier (e.g. "ab-12345678").
        base_ref:
            The git ref to base the worktree branch on (default "main").
        branch_suffix:
            Override for the branch name suffix. If omitted, defaults to the
            last 8 characters of node_id.

        Returns
        -------
        Worktree
            Dataclass with path, branch, and metadata.

        Raises
        ------
        WorktreeDiskPressureError
            If free disk space falls below 2 GB.
        WorktreeError
            If git worktree add fails for any other reason.
        """
        # Prune stale registrations before creating
        subprocess.run(
            ["git", "worktree", "prune"],
            cwd=self.repo_root,
            check=True,
            capture_output=True,
        )

        # Disk space check
        self.disk_pressure_check()

        # x-ff83 W3: default to the legible <prefix>/<slug>-<node> name (round-trip
        # resolvable). An explicit branch_suffix override keeps the legacy feature/
        # shape so existing callers that pin a suffix are unchanged.
        branch = f"feature/{branch_suffix}" if branch_suffix else branch_name(node_id)
        wt_path = self.base_dir / node_id
        self.base_dir.mkdir(parents=True, exist_ok=True)

        result = subprocess.run(
            ["git", "worktree", "add", "-b", branch, str(wt_path), base_ref],
            cwd=self.repo_root,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            raise WorktreeError(
                f"git worktree add failed for node {node_id}: {result.stderr.strip()}"
            )

        # Wire gitignored shared state (.fno/, internal/, .claude/
        # subdirs) from canonical into the new worktree. Best-effort: a
        # missing or failing script does not block the create call.
        rc, tail = _run_setup_worktree_hook(self.repo_root, wt_path)
        if rc not in (0, -1):
            import sys
            print(
                f"worktree: setup-worktree.sh exited {rc} for {wt_path}: {tail}",
                file=sys.stderr,
            )

        return Worktree(
            node_id=node_id,
            path=wt_path,
            branch=branch,
            created_at=_now_iso(),
            base_ref=base_ref,
        )

    def remove(self, worktree: Worktree) -> None:
        """Remove a worktree and its directory. Idempotent.

        Calls ``git worktree remove --force`` then removes the directory tree.
        If the path is already gone, this is a no-op.
        """
        if worktree.path.exists():
            subprocess.run(
                ["git", "worktree", "remove", "--force", str(worktree.path)],
                cwd=self.repo_root,
                capture_output=True,
            )
            if worktree.path.exists():
                shutil.rmtree(worktree.path, ignore_errors=True)
        # Always prune stale registrations afterwards
        subprocess.run(
            ["git", "worktree", "prune"],
            cwd=self.repo_root,
            capture_output=True,
        )
        # Remove snapshot entry if present
        self._git_status_snapshots.pop(worktree.path, None)

    def archive(self, worktree: Worktree, reason: str) -> Path:
        """Move the worktree to an archive directory for forensics.

        The worktree is moved to:
            {base_dir}/.archived/{ts}-{node_id}/

        A ``.archive-reason.txt`` file is written with ``reason``.

        Note: This does NOT call ``git worktree remove`` - the git registration
        is intentionally left for forensic purposes.

        Parameters
        ----------
        worktree:
            The worktree to archive.
        reason:
            A short description of why this worktree is being archived.

        Returns
        -------
        Path
            The archive destination directory.
        """
        ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        archive_dir = self.base_dir / ".archived"
        archive_dir.mkdir(parents=True, exist_ok=True)
        dest = archive_dir / f"{ts}-{worktree.node_id}"

        if worktree.path.exists():
            shutil.move(str(worktree.path), str(dest))
        else:
            dest.mkdir(parents=True, exist_ok=True)

        (dest / ".archive-reason.txt").write_text(f"{reason}\n")

        # Clean up the snapshot entry
        self._git_status_snapshots.pop(worktree.path, None)

        return dest

    def list_active(self) -> list[Worktree]:
        """Return all worktrees currently registered under base_dir.

        Parses ``git worktree list --porcelain`` to get live git registrations,
        then filters to paths under base_dir. Skips the main worktree.

        Returns
        -------
        list[Worktree]
            Worktrees with node_id inferred from the directory name.
        """
        result = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            cwd=self.repo_root,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            return []

        worktrees: list[Worktree] = []
        current: dict[str, str] = {}

        for line in result.stdout.splitlines():
            line = line.strip()
            if not line:
                if current:
                    wt = self._parse_porcelain_entry(current)
                    if wt is not None:
                        worktrees.append(wt)
                    current = {}
            elif " " in line:
                key, _, value = line.partition(" ")
                current[key] = value
            else:
                current[line] = ""

        # Handle last entry if file doesn't end with blank line
        if current:
            wt = self._parse_porcelain_entry(current)
            if wt is not None:
                worktrees.append(wt)

        return worktrees

    def list_orphaned(self, graph: dict) -> list[Worktree]:
        """Return worktrees with no live graph entry or whose node is done.

        A worktree is orphaned if:
        - Its node_id is not present in graph, OR
        - Its graph node has ``_status: done``

        Scans the base_dir for directories (excluding .archived/).

        Parameters
        ----------
        graph:
            The graph dict (keyed by node_id).

        Returns
        -------
        list[Worktree]
            Orphaned worktrees the walker should archive or remove.
        """
        if not self.base_dir.exists():
            return []

        orphans: list[Worktree] = []
        for child in self.base_dir.iterdir():
            if not child.is_dir():
                continue
            if child.name.startswith("."):
                continue  # skip .archived/, .git artifacts

            node_id = child.name
            node = graph.get(node_id)
            if node is None or node.get("_status") == "done":
                orphans.append(
                    Worktree(
                        node_id=node_id,
                        path=child,
                        branch=f"feature/{node_id[-8:]}",
                        created_at="",
                        base_ref="main",
                    )
                )

        return orphans

    def is_stuck(self, worktree: Worktree, *, threshold_minutes: int = 20) -> bool:
        """Detect whether a worktree is stuck (no real progress happening).

        Returns True iff BOTH of these are true:
        1. Git status has not changed since the last call (or is clean).
        2. No agent-progress entries newer than (now - threshold_minutes).

        The rolling git-status snapshot is keyed by worktree.path and
        persists across calls on the same WorktreeManager instance.

        LSP autosave detection: if git status is non-empty but identical
        to the previous snapshot, it is treated as quiet (no real work).

        Parameters
        ----------
        worktree:
            The worktree to check.
        threshold_minutes:
            Age threshold for progress entries. Entries older than this
            are treated as inactive.

        Returns
        -------
        bool
            True if the worktree appears stuck; False if activity detected.
        """
        git_quiet = self._git_status_quiet_since_last_poll(worktree)
        progress_quiet = self._no_progress_entries_within(worktree, threshold_minutes)
        return git_quiet and progress_quiet

    # ------------------------------------------------------------------
    # Private helpers
    # ------------------------------------------------------------------

    def _parse_porcelain_entry(self, entry: dict[str, str]) -> Optional[Worktree]:
        """Convert a parsed porcelain block into a Worktree if it's under base_dir."""
        worktree_path_str = entry.get("worktree", "")
        if not worktree_path_str:
            return None

        wt_path = Path(worktree_path_str)

        # Skip the main worktree (the repo root itself)
        if wt_path == self.repo_root:
            return None

        # Only include paths under our base_dir
        try:
            wt_path.relative_to(self.base_dir)
        except ValueError:
            return None

        node_id = wt_path.name
        branch = entry.get("branch", "").replace("refs/heads/", "")
        if not branch:
            branch = f"feature/{node_id[-8:]}"

        return Worktree(
            node_id=node_id,
            path=wt_path,
            branch=branch,
            created_at="",
            base_ref="main",
        )

    def _git_status_quiet_since_last_poll(self, worktree: Worktree) -> bool:
        """Return True if the worktree's git status is unchanged since last poll.

        On the first call for a given path, captures the snapshot and returns
        True if the working tree is clean, False if there are changes.

        On subsequent calls, compares the current output to the saved snapshot.
        If the output is identical (even if non-empty), returns True - this
        handles the LSP autosave case where files appear modified but the
        content hasn't changed since the last check.
        """
        if not worktree.path.exists():
            return True

        result = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=worktree.path,
            capture_output=True,
        )
        current_status = result.stdout

        previous = self._git_status_snapshots.get(worktree.path)
        # Always update the snapshot
        self._git_status_snapshots[worktree.path] = current_status

        if previous is None:
            # First poll: treat as quiet if clean, active if dirty
            return current_status == b""

        # Subsequent polls: quiet if status unchanged from last snapshot
        return current_status == previous

    def _no_progress_entries_within(
        self, worktree: Worktree, threshold_minutes: int
    ) -> bool:
        """Return True if no agent-progress entry is newer than threshold_minutes ago.

        Reads {worktree.path}/.fno/agent-progress.jsonl (JSONL, one entry
        per line, each with a ``ts`` field as a Unix timestamp float).

        If the file is absent or empty, returns True (no progress = quiet).
        """
        progress_file = worktree.path / ".fno" / "agent-progress.jsonl"
        if not progress_file.exists():
            return True

        try:
            lines = progress_file.read_text(encoding="utf-8").strip().splitlines()
        except OSError:
            return True

        if not lines:
            return True

        cutoff = time.time() - (threshold_minutes * 60)

        for line in reversed(lines):
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
                ts = float(entry.get("ts", 0))
                if ts >= cutoff:
                    return False  # recent entry found -> not quiet
            except (json.JSONDecodeError, ValueError, TypeError):
                continue

        return True  # no recent entries found
