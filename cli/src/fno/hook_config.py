"""Which scripts hook config references, and which files a revision range deleted.

The harness reads hook config once at session start and answers from that
snapshot for the session's life. A commit that deletes a script the previous
release's config referenced leaves every pre-merge session holding a
registration for a missing file; on the Bash PreToolUse matcher a hook that
cannot launch fails the whole tool call, so one deletion removes every shell
verb at once for those sessions. The ``hook-tombstones`` lint gate and the
doctor plugin-cache signal both ask this module the same question against a
revision range: did it delete a script the base revision's config referenced?

Failure discipline: every git-reading function returns ``None`` when the git
call fails, never an empty collection. An empty set means "the call succeeded
and found nothing"; ``None`` means the instrument never ran, so a caller that
trusted an empty set on failure would read a broken probe as a clean pass.
"""
from __future__ import annotations

import re
import subprocess
from pathlib import Path
from typing import Optional

# hooks/hooks.json spells the plugin root CLAUDE_PLUGIN_ROOT, the codex config
# PLUGIN_ROOT. The captured path runs to the next whitespace or quote, so a
# command carrying several refs (the context-observe wrapper shape, and the
# `uv run --project ... python3 <gate>.py` incident shape) yields every one,
# including scripts/ paths outside hooks/.
_PLUGIN_ROOT_RE = re.compile(
    r"\$\{(?:CLAUDE_PLUGIN_ROOT|CODEX_PLUGIN_ROOT|PLUGIN_ROOT)\}/([^\s\"'\\]+)"
)

HOOK_CONFIG_SUFFIX = "hooks.json"


def referenced_scripts(config_text: str) -> set[str]:
    """Repo-relative script paths named by one hook config document."""
    return set(_PLUGIN_ROOT_RE.findall(config_text))


def _git(repo_root: Path, *args: str) -> Optional[str]:
    try:
        result = subprocess.run(
            ["git", "-C", str(repo_root), *args],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout if result.returncode == 0 else None


def hook_configs_at(repo_root: Path, rev: str) -> Optional[list[str]]:
    """Every ``*hooks.json`` under hooks/ tracked at ``rev``, repo-relative."""
    out = _git(repo_root, "ls-tree", "-r", "--name-only", rev, "--", "hooks/")
    if out is None:
        return None
    return [line for line in out.splitlines() if line.endswith(HOOK_CONFIG_SUFFIX)]


def referenced_at_revision(repo_root: Path, rev: str) -> Optional[set[str]]:
    """Union of script paths referenced by all hook configs at ``rev``."""
    configs = hook_configs_at(repo_root, rev)
    if configs is None:
        return None
    refs: set[str] = set()
    for config in configs:
        text = _git(repo_root, "show", f"{rev}:{config}")
        if text is not None:
            refs |= referenced_scripts(text)
    return refs


def deleted_in_range(repo_root: Path, base: str, head: str) -> Optional[set[str]]:
    """Files removed between ``base`` and ``head`` (two-dot: all of it).

    ``--no-renames`` is load-bearing: a same-content move otherwise reads as
    R, not D, and a session cached against the old path is just as bricked.
    """
    out = _git(
        repo_root,
        "diff",
        "--no-renames",
        "--name-only",
        "--diff-filter=D",
        f"{base}..{head}",
    )
    if out is None:
        return None
    return {line for line in out.splitlines() if line}


def stubless_deletions(repo_root: Path, base: str, head: str) -> Optional[list[str]]:
    """Scripts the base config referenced that the range deleted, sorted.

    This is the violation set: deleting one of these bricks every session
    still answering from the base revision's cached hook config.
    """
    refs = referenced_at_revision(repo_root, base)
    deleted = deleted_in_range(repo_root, base, head)
    if refs is None or deleted is None:
        return None
    return sorted(refs & deleted)
