"""All three removal call sites must answer one question the same way (AC2-HP).

Three independent implementations used to ask "is `git status --porcelain`
empty" and each blocked on any output:

    scripts/lib/worktree-lifecycle.sh   the `--merged` sweep
    scripts/setup/archive-worktree.sh   the archive strict-check
    crates/fno-agents/src/daemon.rs     the row-GC cleanliness probe

`AGENTS.md` records that N implementations of one operation is a defect class:
a guard placed on one of N reachable paths is decorative. Converging them on
one classifier only helps while they STAY converged, so this test drives the
real bash entry points over a fixture corpus and fails when any two disagree.

The Rust probe is covered by parsing contract rather than by running the
daemon: it consumes the same `fno worktree reapable` receipt, so the assertion
that matters is that the receipt grammar it parses is what the verb emits.
"""
import subprocess
from pathlib import Path

import pytest

from fno.worktree_reapable import reapable

REPO_ROOT = Path(__file__).resolve().parents[3]
REAPABLE_LIB = REPO_ROOT / "scripts" / "lib" / "worktree-reapable.sh"


def _git(cwd: Path, *args: str) -> None:
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    assert r.returncode == 0, f"git {' '.join(args)} failed: {r.stderr}"


def _make_repo(root: Path) -> Path:
    """A real LINKED worktree, not a standalone repo.

    archive-worktree.sh refuses a canonical checkout (exit 1) before it reaches
    any strict check, and a standalone `git init` repo IS its own canonical. A
    fixture built that way never exercises the predicate under test and reports
    a cheerful pass, so the corpus builds a parent repo and hands back a linked
    worktree of it - the shape every real caller passes.
    """
    parent = root.parent / f"{root.name}-origin"
    parent.mkdir(parents=True)
    _git(parent, "init", "-q", "-b", "main")
    _git(parent, "config", "user.email", "t@example.com")
    _git(parent, "config", "user.name", "t")
    (parent / "a.py").write_text("a = 1\n")
    (parent / "b.py").write_text("b = 2\n")
    _git(parent, "add", "-A")
    _git(parent, "commit", "-qm", "seed")
    _git(parent, "worktree", "add", "-q", str(root), "-b", "feature/fixture")
    return root


# The corpus: (name, mutate, expected_reapable)
def _clean(_: Path) -> None:
    pass


def _deletions_only(p: Path) -> None:
    (p / "a.py").unlink()
    (p / "b.py").unlink()


def _one_modification(p: Path) -> None:
    (p / "a.py").write_text("a = 999\n")


def _untracked(p: Path) -> None:
    (p / "scratch.py").write_text("nope\n")


def _mixed(p: Path) -> None:
    (p / "a.py").unlink()
    (p / "b.py").write_text("b = 999\n")


CORPUS = [
    ("clean", _clean, True),
    ("deletions-only", _deletions_only, True),
    ("one-modification", _one_modification, False),
    ("untracked", _untracked, False),
    ("deletion-plus-modification", _mixed, False),
]


def _bash_verdict(path: Path) -> bool:
    """Run the shared bash helper exactly as both bash call sites do."""
    script = (
        f'source "{REAPABLE_LIB}"\n'
        f'if wt_reapable "{path}"; then echo YES; else echo NO; fi\n'
    )
    r = subprocess.run(
        ["bash", "-c", script],
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
    )
    assert "YES" in r.stdout or "NO" in r.stdout, f"helper emitted nothing: {r.stderr}"
    return "YES" in r.stdout


def _archive_script_verdict(path: Path) -> bool:
    """Run archive-worktree.sh far enough to see its dirty verdict.

    Exit 2 is its "strict check failed" code. The script's own docs pin that,
    and the dirty check is the first strict check it runs, so a non-2 exit
    means the tree did not block removal.
    """
    script = REPO_ROOT / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), str(path), "--yes"],
        capture_output=True,
        text=True,
        cwd=str(REPO_ROOT),
        timeout=60,
    )
    # POSITIVE CONTROL. A "not blocked" verdict must mean the script reached and
    # passed the check, never that it bailed earlier for an unrelated reason.
    # The fixture bug this catches was real: a standalone repo made the script
    # exit 1 with "refusing to archive canonical checkout", which read as
    # "not blocked" and passed every clean case without testing anything.
    assert "refusing to archive canonical checkout" not in r.stderr, (
        f"fixture is not a linked worktree; the predicate never ran: {r.stderr}"
    )
    assert r.returncode in (0, 2), (
        f"script failed for an unrelated reason (rc={r.returncode}): {r.stderr}"
    )
    blocked_on_dirt = r.returncode == 2 and (
        "reapable=no" in r.stderr or "dirty working tree" in r.stderr
    )
    return not blocked_on_dirt


@pytest.mark.parametrize("name,mutate,expected", CORPUS, ids=[c[0] for c in CORPUS])
def test_python_and_bash_agree(tmp_path: Path, name: str, mutate, expected: bool) -> None:
    repo = _make_repo(tmp_path / name)
    mutate(repo)

    py = reapable(repo).reapable
    sh = _bash_verdict(repo)

    assert py == expected, f"{name}: python said {py}, corpus says {expected}"
    assert sh == py, f"{name}: bash helper said {sh}, python said {py}"


@pytest.mark.parametrize("name,mutate,expected", CORPUS, ids=[c[0] for c in CORPUS])
def test_archive_script_agrees(tmp_path: Path, name: str, mutate, expected: bool) -> None:
    repo = _make_repo(tmp_path / name)
    mutate(repo)

    assert _archive_script_verdict(repo) == expected


def test_deletions_only_worktree_is_actually_removed(tmp_path: Path) -> None:
    """Clearing the predicate is not enough: the removal must SUCCEED.

    `git worktree remove` counts a tracked file missing from disk as "modified"
    and refuses with exit 4. So a worktree our classifier affirmatively cleared
    still failed to go, and the whole change was inert on exactly the 17
    worktrees it targets. The end-to-end assertion is that the directory is
    gone, not that a check passed.
    """
    repo = _make_repo(tmp_path / "gone")
    (repo / "a.py").unlink()
    (repo / "b.py").unlink()

    script = REPO_ROOT / "scripts" / "setup" / "archive-worktree.sh"
    r = subprocess.run(
        ["bash", str(script), str(repo), "--yes"],
        capture_output=True, text=True, cwd=str(REPO_ROOT), timeout=60,
    )

    assert r.returncode == 0, f"archive failed: {r.stderr}"
    assert not repo.exists(), "predicate passed but the worktree is still on disk"


def test_helper_fails_closed_when_the_verb_cannot_answer(tmp_path: Path) -> None:
    """A stale CLI that does not know the verb must never read as permission.

    An absence of "no" is not a yes. This drives the helper with a PATH that has
    no `fno` and an FNO_PYTHON that exits non-zero, the shape a partial deploy
    produces.
    """
    repo = _make_repo(tmp_path / "stale")
    fake = tmp_path / "false-python"
    fake.write_text("#!/bin/sh\nexit 2\n")
    fake.chmod(0o755)

    script = (
        f'source "{REAPABLE_LIB}"\n'
        f'export FNO_PYTHON="{fake}"\n'
        f'if wt_reapable "{repo}"; then echo YES; else echo NO; fi\n'
        f'echo "$WT_REAPABLE_LINE"\n'
    )
    # STRIP `fno` FOR REAL. The docstring above always claimed a PATH without
    # it; leaving the real PATH in place let the developer's installed CLI
    # answer, so this asserted nothing once the helper grew its fallback.
    r = subprocess.run(["bash", "-c", script], capture_output=True, text=True,
                       cwd=str(REPO_ROOT), env={"PATH": "/usr/bin:/bin"})

    assert "NO" in r.stdout
    assert "probe-failed" in r.stdout


def test_rust_probe_parses_the_grammar_the_verb_emits(tmp_path: Path) -> None:
    """The Rust probe keys on two literals. Pin that the verb still emits them.

    daemon.rs reads `reapable=yes` on exit 0 and `reapable=no` otherwise. A
    receipt rename would leave that probe silently answering None forever,
    which reads as "keep everything" and is invisible.
    """
    clean = _make_repo(tmp_path / "yes")
    dirty = _make_repo(tmp_path / "no")
    (dirty / "scratch.py").write_text("nope\n")

    assert reapable(clean).line().startswith("reapable=yes ")
    assert reapable(dirty).line().startswith("reapable=no ")

    probe_src = (REPO_ROOT / "crates" / "fno-agents" / "src" / "daemon.rs").read_text()
    assert '"reapable=yes"' in probe_src
    assert '"reapable=no"' in probe_src
