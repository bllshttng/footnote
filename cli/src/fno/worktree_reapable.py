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

The same argument reaches one class of untracked content: the symlinks
`setup-worktree.sh` writes. On `.claude/worktrees/x-ba96` (2026-09-06) they were
the tree's ENTIRE difference, so footnote dirtied it at creation and the DIRTY
rule protected that dirt forever. Such a link holds no human work, so it is
discounted and named; everything else untracked still blocks.
"""
from __future__ import annotations

import os
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional, Union

# Unmerged (conflict) codes, per `git status` docs. These matter because two of
# them carry only `D` and `A` letters: reading `DD` ("both deleted") as two
# recoverable deletions throws away a merge the user has not resolved yet. The
# letters alone are not enough to classify a line; the conflict set is checked
# first.
_UNMERGED = frozenset({"DD", "AU", "UD", "UA", "DU", "AA", "UU"})

# What `setup-worktree.sh` links, canonical-relative: these five roots, and
# anything one level under `.claude/`. A shape, not a list: the script's grows.
_SETUP_LINK_ROOTS = frozenset({"internal", ".agents", ".codex", ".codex-plugin", ".gemini"})

# A tree git created minutes ago is not a finished tree. Measured 2026-09-12:
# a worktree on a new branch off origin/main reads `reapable=yes reason=clean`
# before its first commit, because a zero-commit branch is a literal ancestor
# of main. 29 removals in one night were that read, three of them live.
_SETUP_WINDOW_SECONDS = 1800


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
    args = ["git", "rev-parse", "--git-common-dir"]
    try:
        r = subprocess.run(args, cwd=str(worktree), capture_output=True, text=True, timeout=30.0)
    except (OSError, subprocess.SubprocessError):
        return None
    if r.returncode != 0 or not r.stdout.strip():
        return None
    common = Path(r.stdout.strip())
    if not common.is_absolute():
        common = worktree / common
    return common.parent


def _accepts(here: str, rel: str) -> bool:
    """Does a link at ``here`` sitting over target ``rel`` read as setup's?

    Setup writes each link at the same relative path as its target, one
    directory deeper at most, so the two must agree. Without that, a hand-made
    ``vault -> $CANONICAL/internal`` reaps under a receipt claiming setup wrote
    it. The boundary is a whole segment: ``x.agents`` is not ``.agents``.
    """
    parent, _, name = rel.rpartition("/")
    if not name or not (rel in _SETUP_LINK_ROOTS or parent == ".claude"):
        return False
    return here == rel or here.endswith("/" + rel)


def _is_setup_link(link: Path, worktree: Path, canonical: Path) -> bool:
    """Did setup-worktree.sh write this symlink?

    Read the link ONE hop first: setup writes an absolute ``$CANONICAL/$rel``,
    so the raw target IS the attribution, and resolving follows ``internal``
    (itself a symlink) out of the checkout. The realpath pairs are the fallback
    for a canonical reached by a different spelling (``/tmp`` vs ``/private``).
    """
    try:
        target = os.readlink(link)
        here = link.relative_to(worktree).as_posix()
    except (OSError, ValueError):
        return False
    if not os.path.isabs(target):
        return False
    for base in (str(canonical), os.path.realpath(canonical)):
        for candidate in (target, os.path.realpath(target)):
            rel = os.path.relpath(candidate, base)
            if not rel.startswith("..") and _accepts(here, rel):
                return True
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


def _inside_setup_window(path: Union[str, Path]) -> bool:
    """Was this worktree's `.git` file written within the setup window?

    Git writes the `.git` file once at `worktree add` and does not rewrite it
    in normal use, so its mtime is the tree's creation time (measured across
    seven live trees against the admin gitdir's). A stat that fails reads as
    inside the window: an unanswerable probe never shortens the keep.
    """
    try:
        mtime = (Path(path) / ".git").stat().st_mtime
    except OSError:
        return True
    return (time.time() - mtime) < _SETUP_WINDOW_SECONDS


def branch_unborn(path: Union[str, Path]) -> bool:
    """Has this worktree's branch never moved since `git worktree add` made it?

    The reflog is the discriminator the merge status cannot supply: creation
    writes one entry, any commit, reset or rebase writes more. Read alone it
    would also hold a landed branch whose reflog has expired, so the caller
    pairs it with the tree's age. A detached HEAD answers False: content, not
    a branch name, judges those, and the sweep already counts their unpushed
    commits. Any git read that fails answers True - a probe that cannot
    answer must not authorize a removal.
    """
    target = Path(path)
    try:
        branch = subprocess.run(
            ["git", "branch", "--show-current"],
            cwd=str(target),
            capture_output=True,
            text=True,
            timeout=30.0,
        )
        if branch.returncode != 0:
            return True
        name = branch.stdout.strip()
        if not name:
            return False
        log = subprocess.run(
            ["git", "reflog", "show", name, "--format=%gs"],
            cwd=str(target),
            capture_output=True,
            text=True,
            timeout=30.0,
        )
    except (OSError, subprocess.SubprocessError):
        return True
    if log.returncode != 0:
        return True
    entries = [line for line in log.stdout.splitlines() if line.strip()]
    return len(entries) <= 1


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

    `discount` names untracked paths carrying no human work. It is asked about
    `??` lines only, so tracked and unmerged dirt block as before; omit it and
    this answers exactly what it always answered.
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
        detail = ", ".join(discounted)
        return Verdict(True, "setup-links", detail, deletions, tuple(discounted))
    return Verdict(True, "clean", "", deletions)


def reapable(path: Union[str, Path]) -> Verdict:
    """Classify a worktree on disk. Fails CLOSED on any probe it cannot trust.

    A probe that cannot answer must not read as "safe to remove": an absence of
    reported dirt has two explanations, and only one of them is a clean tree.
    """
    target = Path(path)
    if not target.is_dir():
        return Verdict(False, "probe-failed", "path is not a directory")
    # `-uall`, so every untracked entry is a FILE. The default collapses a
    # directory to one line, and judging that from disk asks about children git
    # does not track: `.gitignore` carries `**/.claude/hooks/`, so a worktree
    # whose `cli/.claude/` holds setup's links beside an ignored `hooks/` read
    # as real work and stayed in the kept-forever bucket this exists to empty.
    try:
        r = subprocess.run(
            ["git", "status", "--porcelain", "--untracked-files=all"],
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
        return root is not None and _is_setup_link(target / rel, target, root)

    verdict = classify(r.stdout, _discount)
    # Cheapest reads first: an aged tree pays one stat and no git subprocess.
    if verdict.reapable and is_linked_worktree(target) and _inside_setup_window(target) and branch_unborn(target):
        return Verdict(False, "unborn", "branch has no commit of its own and the tree is inside the setup window")
    return verdict
