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
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


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


# ---------------------------------------------------------------------------
# Worktree policy resolution (x-168b)
# ---------------------------------------------------------------------------
# Per-project opt-out from auto-isolation. The c3po incident: an Obsidian vault
# whose working tree IS the product got auto-worktree'd. The gate reads this
# resolver as step 0 of `fno worktree ensure`; a `never` project launches in
# place, and a parse error / out-of-enum value REFUSES creation (fail closed).

VALID_WORKTREE_POLICIES = ("never", "harness-native", "external")
# Harnesses with a native worktree-creation mechanism in THIS PR. Anything else
# under a `harness-native` policy degrades to `external` (Locked Decision 5).
_NATIVE_WORKTREE_HARNESSES = ("claude",)


class WorktreePolicyError(Exception):
    """Policy could not be affirmatively resolved (parse error or out-of-enum).

    Callers translate this to a creation REFUSAL: exit non-zero, empty stdout,
    launch in the prior cwd. The c3po guarantee: a worktree is created only
    after a clean, in-enum policy read.
    """


@dataclass(frozen=True)
class WorktreePolicy:
    policy: str   # one of VALID_WORKTREE_POLICIES (post harness degradation)
    base: Path    # target worktrees base (informational for a `never` result)
    project: str  # resolved project id (for the receipt line)
    source: str   # "per-project" | "global" | "default"


def _flat_config_or_raise(settings_path: Path) -> Optional[dict]:
    """Flat-unwrapped config dict for one file; None if absent, raise on parse error."""
    if not settings_path.exists():
        return None
    from fno.config_io import _load_raw, _unwrap_config_dict

    parsed, ok = _load_raw(settings_path)
    if not ok:
        raise WorktreePolicyError(f"config parse error: {settings_path}")
    return _unwrap_config_dict(parsed)


def _repo_config_path(repo_root: Path) -> Optional[Path]:
    fno_dir = repo_root / ".fno"
    for name in ("config.toml", "settings.yaml"):
        p = fno_dir / name
        if p.exists():
            return p
    return None


def _global_config_path() -> Path:
    """The global config file, config.toml preferred over settings.yaml.

    Honors FNO_GLOBAL_SETTINGS_PATH (via _global_settings_path) so a test
    pinning HOME cannot leak the developer's real ~/.fno config.
    """
    from fno.config_io import _global_settings_path

    yaml_path = _global_settings_path()
    toml_path = yaml_path.with_name("config.toml")
    return toml_path if toml_path.exists() else yaml_path


def _match_project_entry(
    cfg: dict, repo_root: Path, project_id: str
) -> Optional[dict]:
    """Find the work.workspaces.*.projects[] entry for repo_root, or None.

    Path match (realpath of entry.path == repo_root) wins over name match
    (entry.name == project_id).
    """
    work = cfg.get("work")
    workspaces = work.get("workspaces") if isinstance(work, dict) else None
    if not isinstance(workspaces, dict):
        return None
    entries: list[dict] = []
    for ws in workspaces.values():
        projects = ws.get("projects") if isinstance(ws, dict) else None
        if isinstance(projects, list):
            entries.extend(e for e in projects if isinstance(e, dict))
    for entry in entries:
        path = entry.get("path")
        if isinstance(path, str) and path:
            try:
                if Path(path).expanduser().resolve() == repo_root:
                    return entry
            except OSError:
                pass
    for entry in entries:
        if entry.get("name") == project_id:
            return entry
    return None


def resolve_worktree_policy(
    repo_root: Path, harness: str | None = None
) -> WorktreePolicy:
    """Resolve the worktree policy for ``repo_root`` under ``harness``.

    Precedence: per-project ``work.workspaces.<slug>.projects[].worktree`` >
    global ``worktree.policy`` > built-in ``harness-native``. A config file that
    exists but fails to parse RAISES (fail closed); an absent key is not an
    error. ``harness-native`` with no native mechanism for ``harness`` (anything
    but claude in this PR) resolves to ``external``.
    """
    repo_root = repo_root.resolve()
    from fno.config_io import _deep_merge

    repo_path = _repo_config_path(repo_root)
    repo_cfg = (_flat_config_or_raise(repo_path) if repo_path else None) or {}
    global_cfg = _flat_config_or_raise(_global_config_path()) or {}
    merged = _deep_merge(dict(global_cfg), repo_cfg)

    try:
        project_id = resolve_project_id(repo_root)
    except Exception:
        project_id = repo_root.name

    raw_policy: object = None
    source = "default"
    entry = _match_project_entry(merged, repo_root, project_id)
    if entry is not None and entry.get("worktree") is not None:
        raw_policy = entry.get("worktree")
        source = "per-project"
    else:
        wt = merged.get("worktree")
        if isinstance(wt, dict) and wt.get("policy") is not None:
            raw_policy = wt.get("policy")
            source = "global"
    if raw_policy is None:
        raw_policy = "harness-native"

    if not isinstance(raw_policy, str) or raw_policy not in VALID_WORKTREE_POLICIES:
        raise WorktreePolicyError(
            f"worktree policy {raw_policy!r} is invalid; valid: "
            + " | ".join(VALID_WORKTREE_POLICIES)
        )

    policy = raw_policy
    if policy == "harness-native" and harness not in _NATIVE_WORKTREE_HARNESSES:
        policy = "external"

    return WorktreePolicy(
        policy=policy, base=_worktrees_base_from(merged), project=project_id, source=source
    )


def _worktrees_base_from(merged: dict) -> Path:
    """Repo-scoped worktrees base from the merged (repo>global) config.

    Reads the same config the policy resolves against, so the receipt and the
    creation target never diverge on cwd. Falls back to the ambient default
    (~/.fno/worktrees) when no override is set. ponytail: does not expand
    ``{project}``-style templates (unused for a base dir); worktree_base()
    carries the full template machinery for the default path.
    """
    paths_cfg = merged.get("paths")
    raw = paths_cfg.get("worktrees_base") if isinstance(paths_cfg, dict) else None
    if isinstance(raw, str) and raw:
        return Path(os.path.expandvars(os.path.expanduser(raw))).resolve()
    return worktree_base()
