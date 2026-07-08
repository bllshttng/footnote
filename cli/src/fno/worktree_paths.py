"""Canonical worktree path resolution: ~/.fno/worktrees/{proj}-{slug}/.

Single source of truth for where worktrees live. Replaces the previous
``.claude/worktrees/`` and ``~/conductor/workspaces/`` locations that
were hardcoded in multiple files (ab-3180b3f4).

The path shape is flat with a project prefix so a single
``~/.fno/worktrees/`` directory holds every worktree across every
project the user works in. ``project_id`` is the stable short identifier
declared in ``.fno/settings.yaml`` under ``project.id`` (or
derived from ``git remote get-url origin`` basename when absent).

Both ``project_id`` and ``name`` are validated against
``^[A-Za-z0-9][A-Za-z0-9._-]*$`` so path components can never escape
the worktree root (defense-in-depth against the path-traversal class
of bug previously flagged on PR #225).
"""
from __future__ import annotations

import os
import re
import subprocess
from pathlib import Path


_SAFE_COMPONENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


def _validate_component(value: str, *, kind: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{kind} must be a non-empty string")
    if ".." in value:
        raise ValueError(f"{kind} contains '..' (path traversal): {value!r}")
    if not _SAFE_COMPONENT.match(value):
        raise ValueError(
            f"{kind} must match ^[A-Za-z0-9][A-Za-z0-9._-]*$: {value!r}"
        )
    return value


def worktree_base() -> Path:
    """Return the canonical worktree base directory.

    Delegates to ``paths.worktrees_base()`` so the value respects
    ``config.paths.worktrees_base`` in settings.yaml. Falls back to
    ``~/.fno/worktrees/`` when the paths module is unavailable.
    The ``HOME`` env var is honoured so test fixtures can substitute a
    temp directory.
    """
    try:
        from fno import paths as _paths
        return _paths.worktrees_base()
    except Exception:
        return Path(os.path.expanduser("~")) / ".fno" / "worktrees"


def _read_settings_project_id(repo_root: Path) -> str | None:
    from fno.config import read_config_flat

    fno_dir = repo_root / ".fno"
    settings_path = fno_dir / "config.toml"
    if not settings_path.exists():
        settings_path = fno_dir / "settings.yaml"
    if not settings_path.exists():
        return None
    # read_config_flat parses config.toml (or a legacy settings.yaml) into the
    # FLAT dict, so project is top-level (config.project.id and the deprecated
    # top-level project.id both resolve to project.id after unwrap).
    container = read_config_flat(settings_path).get("project")
    # Legacy bare-string shorthand: `project = "<id>"` -> the id itself.
    if isinstance(container, str) and container:
        return container
    if isinstance(container, dict):
        pid = container.get("id")
        if isinstance(pid, str) and pid:
            return pid
    return None


def _derive_project_id_from_git(repo_root: Path) -> str | None:
    try:
        result = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            cwd=str(repo_root),
            capture_output=True,
            text=True,
            check=False,
        )
    except (FileNotFoundError, OSError):
        return None
    if result.returncode != 0:
        return None
    url = result.stdout.strip()
    if not url:
        return None
    # Split on BOTH `/` and `:` so SCP-style SSH URLs (e.g.
    # ``git@host:org/repo.git`` or even ``git@host:repo.git``) parse
    # correctly. The default `rsplit("/", 1)` left the entire
    # `git@host:repo.git` string as the "basename" when the URL had no
    # path component, which then failed `_SAFE_COMPONENT.match` (Gemini
    # MEDIUM PR #234).
    url_clean = url.rstrip("/")
    if url_clean.endswith(".git"):
        url_clean = url_clean[:-4]
    basename = re.split(r"[/:]", url_clean)[-1].strip()
    if not basename:
        return None
    return basename


def resolve_project_id(repo_root: Path | None = None) -> str:
    """Return the project_id for ``repo_root``.

    Resolution order:
      1. ``project.id`` in ``<repo_root>/.fno/settings.yaml``
      2. Basename of ``git remote get-url origin`` (``.git`` suffix stripped)
      3. Basename of ``repo_root`` itself

    The chosen id is validated against ``^[A-Za-z0-9][A-Za-z0-9._-]*$``;
    invalid ids raise ``ValueError`` rather than silently producing a
    bad path.
    """
    if repo_root is None:
        repo_root = Path.cwd()
    repo_root = repo_root.resolve()

    pid = _read_settings_project_id(repo_root)
    if pid is None:
        pid = _derive_project_id_from_git(repo_root)
    if pid is None:
        pid = repo_root.name

    return _validate_component(pid, kind="project_id")


def worktree_path(
    name: str,
    *,
    project_id: str | None = None,
    repo_root: Path | None = None,
) -> Path:
    """Return the canonical worktree path for ``name``.

    Shape: ``~/.fno/worktrees/{project_id}-{name}/``.

    ``project_id`` is resolved from settings/git when omitted. ``name``
    is validated to be a safe path component.
    """
    _validate_component(name, kind="name")
    if project_id is None:
        project_id = resolve_project_id(repo_root)
    else:
        project_id = _validate_component(project_id, kind="project_id")
    return worktree_base() / f"{project_id}-{name}"


def legacy_worktree_path(name: str, repo_root: Path | None = None) -> Path:
    """Return the OLD ``.claude/worktrees/{name}/`` path.

    Kept so AC7 (back-compat) can detect a worktree that lives at the
    old location and reuse its branch. Not used for new worktrees.
    """
    _validate_component(name, kind="name")
    if repo_root is None:
        repo_root = Path.cwd()
    return repo_root / ".claude" / "worktrees" / name
