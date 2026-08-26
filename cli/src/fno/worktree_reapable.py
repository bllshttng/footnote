"""Is removing this worktree safe? The single answer, for three callers.

A worktree blocks removal only when it holds content that removal would
DESTROY. A tracked file missing from disk is not that: HEAD holds its content,
so `git worktree remove` loses nothing and `git restore` brings it back.

That distinction is load-bearing rather than pedantic. Measured 2026-08-13 on
47 worktrees, `fno agents workspace worktree cleanup --merged` found 2 eligible and kept 44; the
largest single blocker was 20 "dirty", and 17 of those were dirty ONLY because
the same 76 tracked paths were missing from disk. The old predicate ("is
`git status --porcelain` empty") therefore blocked almost exclusively on the
one class of dirt that cannot cause data loss.

Three call sites ask this question and each used to answer it itself:

    scripts/lib/worktree-lifecycle.sh   the `--merged` sweep
    scripts/setup/archive-worktree.sh   the archive strict-check
    crates/fno-agents/src/daemon.rs     the row-GC cleanliness probe

N implementations of one operation is a defect class this repo already
documents, so they now call `fno agents workspace worktree reapable` and an equivalence test
pins that they agree. When the verb cannot be reached, every caller keeps its
own fail-closed default, which is today's behaviour exactly.
"""
from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Union

# Unmerged (conflict) codes, per `git status` docs. These matter because two of
# them carry only `D` and `A` letters: reading `DD` ("both deleted") as two
# recoverable deletions throws away a merge the user has not resolved yet. The
# letters alone are not enough to classify a line; the conflict set is checked
# first.
_UNMERGED = frozenset({"DD", "AU", "UD", "UA", "DU", "AA", "UU"})


@dataclass(frozen=True)
class Verdict:
    """One worktree's answer, plus the evidence a caller may want to print."""

    reapable: bool
    reason: str
    detail: str = ""
    recoverable_deletions: int = 0

    def line(self) -> str:
        """The one-line receipt the bash and Rust callers parse.

        `detail` is last because a path may contain spaces; a caller reading
        fields left to right gets every fixed field intact and may take the
        remainder as the detail.
        """
        head = (
            f"reapable={'yes' if self.reapable else 'no'} "
            f"reason={self.reason} "
            f"recoverable_deletions={self.recoverable_deletions}"
        )
        if self.detail:
            head += f" detail={self.detail}"
        return head


def _path_of(entry: str) -> str:
    """The path from a porcelain line, minus the two status chars and a space."""
    return entry[3:].strip() if len(entry) > 3 else entry.strip()


def classify(porcelain: str) -> Verdict:
    """Classify `git status --porcelain` output. Pure: no clock, no disk.

    Blocking, in precedence order: an unmerged conflict, untracked content,
    then any staged or unstaged modification of tracked content. Everything
    else is a deletion of a tracked file, which is recoverable from HEAD.
    """
    deletions = 0
    for raw in porcelain.splitlines():
        if not raw.strip():
            continue
        code = raw[:2]
        if code in _UNMERGED:
            return Verdict(False, "unmerged", _path_of(raw))
        if code == "??":
            return Verdict(False, "untracked", _path_of(raw))
        letters = set(code) - {" "}
        if letters == {"D"}:
            deletions += 1
            continue
        return Verdict(False, "modified-tracked", _path_of(raw))
    return Verdict(True, "clean", "", deletions)


def reapable(path: Union[str, Path]) -> Verdict:
    """Classify a worktree on disk. Fails CLOSED on any probe it cannot trust.

    A probe that cannot answer must not read as "safe to remove": an absence of
    reported dirt has two explanations, and only one of them is a clean tree.
    """
    target = Path(path)
    if not target.is_dir():
        return Verdict(False, "probe-failed", "path is not a directory")
    try:
        r = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=str(target),
            capture_output=True,
            text=True,
            timeout=30.0,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return Verdict(False, "probe-failed", f"git-error: {exc}")
    if r.returncode != 0:
        return Verdict(False, "probe-failed", "git status exited non-zero")
    return classify(r.stdout)
