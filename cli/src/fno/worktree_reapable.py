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

The same argument reaches one class of untracked content. `setup-worktree.sh`
symlinks the canonical checkout's shared state into every worktree it makes,
and those names are gitignored at the repo root only, so a nested copy reads
untracked. Measured 2026-09-06 on `.claude/worktrees/x-ba96`: the tree's ENTIRE
difference was four such links. footnote dirtied the tree at creation and the
rule then protected that dirt forever. A link whose target is one of the paths
setup writes carries no human work, so it is discounted and named in the
receipt. Everything else untracked still blocks.
"""
from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass, field
from fnmatch import fnmatch
from pathlib import Path
from typing import Callable, Optional, Union

# Unmerged (conflict) codes, per `git status` docs. These matter because two of
# them carry only `D` and `A` letters: reading `DD` ("both deleted") as two
# recoverable deletions throws away a merge the user has not resolved yet. The
# letters alone are not enough to classify a line; the conflict set is checked
# first.
_UNMERGED = frozenset({"DD", "AU", "UD", "UA", "DU", "AA", "UU"})

# The canonical-relative paths `scripts/setup/setup-worktree.sh` symlinks into
# every worktree it prepares. They are gitignored at the repo ROOT only, so a
# nested copy (`cli/.agents`, seen on 2026-09-06) reads untracked and the tree
# is dirty from the moment setup finishes. That is footnote's own dirt and it
# holds no human work: the file it names lives in the canonical checkout.
_SETUP_LINK_TARGETS = frozenset(
    {
        "internal",
        ".agents",
        ".codex",
        ".codex-plugin",
        ".gemini",
        ".claude/agents",
        ".claude/commands",
        ".claude/skills",
        ".claude/plans",
        ".claude/settings.local.json",
        ".claude/scheduled_tasks.json",
        ".claude/scheduled_tasks.lock",
        ".claude/.skill-scoping-state.json",
        ".claude/audit-progress.txt",
    }
)


@dataclass(frozen=True)
class Verdict:
    """One worktree's answer, plus the evidence a caller may want to print."""

    reapable: bool
    reason: str
    detail: str = ""
    recoverable_deletions: int = 0
    discounted: tuple[str, ...] = field(default_factory=tuple)

    def line(self) -> str:
        """The one-line receipt the bash and Rust callers parse.

        `detail` is last because a path may contain spaces; a caller reading
        fields left to right gets every fixed field intact and may take the
        remainder as the detail.
        """
        head = (
            f"reapable={'yes' if self.reapable else 'no'} "
            f"reason={self.reason} "
            f"recoverable_deletions={self.recoverable_deletions} "
            f"discounted={len(self.discounted)}"
        )
        if self.detail:
            head += f" detail={self.detail}"
        return head


def _path_of(entry: str) -> str:
    """The path from a porcelain line, minus the two status chars and a space."""
    return entry[3:].strip() if len(entry) > 3 else entry.strip()


def _canonical_root(worktree: Path) -> Optional[Path]:
    """The main checkout this worktree links back to, or None if unresolvable."""
    try:
        r = subprocess.run(
            ["git", "rev-parse", "--git-common-dir"],
            cwd=str(worktree),
            capture_output=True,
            text=True,
            timeout=30.0,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if r.returncode != 0 or not r.stdout.strip():
        return None
    common = Path(r.stdout.strip())
    if not common.is_absolute():
        common = worktree / common
    return common.parent


def _is_setup_link(link: Path, canonical: Path) -> bool:
    """Did setup-worktree.sh write this symlink?

    Read the link ONE hop instead of resolving it. Setup writes an absolute
    ``$CANONICAL/$rel``, so the raw target IS the attribution. Resolving would
    follow the canonical entry's own symlink (``internal`` is one) out of the
    checkout and lose it.
    """
    try:
        target = os.readlink(link)
    except OSError:
        return False
    if not os.path.isabs(target):
        return False
    for base in (str(canonical), os.path.realpath(canonical)):
        rel = os.path.relpath(target, base)
        if rel.startswith(".."):
            continue
        return rel in _SETUP_LINK_TARGETS or fnmatch(rel, ".claude/*.local.md")
    return False


def _is_setup_dirt(path: Path, canonical: Path) -> bool:
    """A setup symlink, or a directory holding nothing but setup dirt.

    ``mkdir -p "$WORKTREE/.claude"`` makes a REAL directory and fills it with
    links, so git reports the directory and never its contents. An empty
    directory reads False: git does not report one, and saying yes would
    discount something this function never looked at.
    """
    if path.is_symlink():
        return _is_setup_link(path, canonical)
    if path.is_dir():
        try:
            children = list(path.iterdir())
        except OSError:
            return False
        return bool(children) and all(_is_setup_dirt(c, canonical) for c in children)
    return False


def is_linked_worktree(path: Union[str, Path]) -> bool:
    """A linked worktree's ``.git`` is a FILE pointing at its admin dir.

    A main checkout's ``.git`` is a directory and a plain directory has none,
    so both read False: only a linked leaf owns something
    ``git worktree remove`` could take. Lives here because this module is the
    single answer for worktree-removal questions.
    """
    try:
        return (Path(path) / ".git").is_file()
    except OSError:
        return False


def branch_merged(path: Union[str, Path]) -> Optional[bool]:
    """Is the worktree's branch merged into the repo's main line?

    The worktree contract's third bucket: content-clean is not enough for an
    automatic prune, because a clean-and-unmerged branch is exactly where
    abandoned-but-real work lives, and a human judges that (the
    ``--merged`` sweep merge-filters BEFORE asking the gate; a caller without
    that pre-filter must ask here). ``None``: nothing names the work or the
    main line - detached HEAD, no main ref, git error - and the caller keeps
    the tree.
    """
    target = Path(path)
    bases = ["origin/main", "main"]
    try:
        head = subprocess.run(
            ["git", "symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            cwd=str(target),
            capture_output=True,
            text=True,
            timeout=30.0,
        )
        if head.returncode == 0 and head.stdout.strip():
            bases.insert(0, head.stdout.strip())
        for base in bases:
            known = subprocess.run(
                ["git", "rev-parse", "--verify", "--quiet", base],
                cwd=str(target),
                capture_output=True,
                text=True,
                timeout=30.0,
            )
            if known.returncode == 0:
                break
        else:
            return None
        branch = subprocess.run(
            ["git", "branch", "--show-current"],
            cwd=str(target),
            capture_output=True,
            text=True,
            timeout=30.0,
        )
        if branch.returncode != 0 or not branch.stdout.strip():
            return None
        merged = subprocess.run(
            ["git", "merge-base", "--is-ancestor", branch.stdout.strip(), base],
            cwd=str(target),
            capture_output=True,
            text=True,
            timeout=30.0,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if merged.returncode == 0:
        return True
    if merged.returncode == 1:
        return False
    return None


def classify(porcelain: str, discount: Optional[Callable[[str], bool]] = None) -> Verdict:
    """Classify `git status --porcelain` output. Pure: no clock, no disk.

    Blocking, in precedence order: an unmerged conflict, untracked content,
    then any staged or unstaged modification of tracked content. Everything
    else is a deletion of a tracked file, which is recoverable from HEAD.

    `discount` names untracked paths that carry no human work - today, the
    symlinks setup-worktree.sh writes into the worktree it prepares. It is
    consulted for `??` lines only, so a modified tracked file and an unmerged
    conflict block exactly as before. Omit it and this function answers what
    it always answered.
    """
    deletions = 0
    discounted: list[str] = []
    for raw in porcelain.splitlines():
        if not raw.strip():
            continue
        code = raw[:2]
        if code in _UNMERGED:
            return Verdict(False, "unmerged", _path_of(raw))
        if code == "??":
            path = _path_of(raw)
            if discount is not None and discount(path):
                discounted.append(path)
                continue
            return Verdict(False, "untracked", path)
        letters = set(code) - {" "}
        if letters == {"D"}:
            deletions += 1
            continue
        return Verdict(False, "modified-tracked", _path_of(raw))
    if discounted:
        return Verdict(
            True,
            "setup-links",
            ", ".join(discounted),
            deletions,
            tuple(discounted),
        )
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

    # Resolved on the first untracked line and not before, so a clean tree
    # still costs one `git status` and nothing else.
    canonical: list[Optional[Path]] = []

    def _discount(rel: str) -> bool:
        if not canonical:
            canonical.append(_canonical_root(target))
        root = canonical[0]
        return root is not None and _is_setup_dirt(target / rel, root)

    return classify(r.stdout, _discount)
