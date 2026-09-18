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
daemon: it consumes the same receipt, so the assertion that matters is that
the receipt grammar it parses is what the verb emits.
"""
import json
import os
import subprocess
import time
from pathlib import Path

import pytest

from fno.rust_binary import resolve_binary


def _gate_binary():
    """THIS checkout's build first: a deployed fno-agents on PATH may predate
    the verb and answer nothing, which would read as a code defect."""
    for cand in (
        REPO_ROOT / "crates" / "fno-agents" / "target" / "release" / "fno-agents",
        REPO_ROOT / "target" / "release" / "fno-agents",
        REPO_ROOT / "crates" / "fno-agents" / "target" / "debug" / "fno-agents",
        REPO_ROOT / "target" / "debug" / "fno-agents",
    ):
        if cand.is_file():
            return cand
    found = resolve_binary()
    if found is None:
        pytest.skip(
            "no fno-agents binary resolves; the gate corpus runs in the cargo "
            "suite, this lane only pins bash/binary agreement"
        )
    return found


def _gate_receipt(path: Path, done_node: bool = False) -> str:
    """The gate's one-line receipt."""
    flags = ["--done-node"] if done_node else []
    r = subprocess.run(
        [str(_gate_binary()), "worktree-reapable", *flags, str(path)],
        capture_output=True, text=True, timeout=60,
    )
    line = (r.stdout or "").strip().splitlines()
    assert line, f"gate emitted no receipt: {r.stderr}"
    return line[0]


def _gate_verdict(path: Path) -> bool:
    """The gate binary's answer, parsed from its one-line receipt."""
    return _gate_receipt(path).startswith("reapable=yes")

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
    # Backdate past the setup window: every corpus row but `unborn` describes
    # an ESTABLISHED tree, and a fresh linked worktree is refused by the
    # unborn gate before its dirt is ever classified.
    old = time.time() - 7200
    os.utime(root / ".git", (old, old))
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


def _unborn(p: Path) -> None:
    # A tree git created moments ago, branch never moved: `_make_repo` aged the
    # `.git` file so the other rows describe established trees; this row makes
    # it fresh again, which is the shape of every worker mid-setup.
    os.utime(p / ".git", None)


CORPUS = [
    ("clean", _clean, True),
    ("deletions-only", _deletions_only, True),
    ("one-modification", _one_modification, False),
    ("untracked", _untracked, False),
    ("deletion-plus-modification", _mixed, False),
    ("unborn", _unborn, False),
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
def test_gate_binary_and_bash_agree(tmp_path: Path, name: str, mutate, expected: bool) -> None:
    repo = _make_repo(tmp_path / name)
    mutate(repo)

    gate = _gate_verdict(repo)
    sh = _bash_verdict(repo)

    assert gate == expected, f"{name}: gate said {gate}, corpus says {expected}"
    assert sh == gate, f"{name}: bash helper said {sh}, gate said {gate}"


@pytest.mark.parametrize("name,mutate,expected", CORPUS, ids=[c[0] for c in CORPUS])
def test_archive_script_agrees(tmp_path: Path, name: str, mutate, expected: bool) -> None:
    repo = _make_repo(tmp_path / name)
    mutate(repo)

    # The archive script is the MANUAL path: a human named this one tree, so
    # the setup-window refusal does not stand in front of it (the sweeps and
    # daemon probes are the strict callers, and the bash-helper row above
    # pins those). `unborn` is the one corpus row where that diverges.
    expected_archive = True if name == "unborn" else expected

    assert _archive_script_verdict(repo) == expected_archive


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
    """A gate that cannot answer must never read as permission.

    An absence of "no" is not a yes. This drives the helper with a binary
    override that exits non-zero with no receipt and a PATH stripped of both
    `fno-agents` and `fno` - the shape a partial deploy produces.
    """
    repo = _make_repo(tmp_path / "stale")
    fake = tmp_path / "broken-gate"
    fake.write_text("#!/bin/sh\nexit 2\n")
    fake.chmod(0o755)

    script = (
        f'source "{REAPABLE_LIB}"\n'
        f'export FNO_AGENTS_BIN="{fake}"\n'
        f'if wt_reapable "{repo}"; then echo YES; else echo NO; fi\n'
        f'echo "$WT_REAPABLE_LINE"\n'
    )
    # STRIP THE RESCUERS FOR REAL. The override is accepted (it is executable),
    # so the repo's own build is not consulted; stripping PATH removes the
    # installed CLI fallback. Every lane answers nothing, and the helper must
    # degrade to its fail-closed receipt.
    r = subprocess.run(["bash", "-c", script], capture_output=True, text=True,
                       cwd=str(REPO_ROOT), env={"PATH": "/usr/bin:/bin"})

    assert "NO" in r.stdout
    assert "probe-failed" in r.stdout


def test_gate_binary_emits_the_grammar_the_callers_parse(tmp_path: Path) -> None:
    """Pin the receipt grammar at its one source: the gate binary.

    The bash helper keys on `reapable=yes` with exit 0 and `reapable=no` on
    exit 1, and the daemon's in-process probe reads the same verdict field. A
    receipt rename would leave every caller silently keeping everything,
    which is invisible.
    """
    clean = _make_repo(tmp_path / "yes")
    dirty = _make_repo(tmp_path / "no")
    (dirty / "scratch.py").write_text("nope\n")

    assert _gate_verdict(clean) is True
    assert _gate_verdict(dirty) is False

    gate_src = (
        REPO_ROOT / "crates" / "fno-agents" / "src" / "worktree_reapable.rs"
    ).read_text()
    assert 'reapable={}' in gate_src or "reapable=" in gate_src
    sh_src = REAPABLE_LIB.read_text()
    assert 'reapable=yes' in sh_src and 'reapable=no' in sh_src


# -- the node-token scanners agree across languages ---------------------------
#
# The done-node arm resolves branch tokens in Rust (scan_node_tokens); the PR
# closure produces them in Python (closure.branch_node_ids). The port copied
# the shape, so a future edit to one copy would drift the two silently. Each
# case builds a tree whose branch names the tokens and asserts the gate's
# evidence equals exactly what the Python producer lists.

TOKEN_CASES = [
    # non-overlap: once x-cccc is consumed, "-1234" is not letter-led
    ("feature/x-cccc-1234", ["x-cccc"]),
    # delimiter-bounded neighbors
    ("feature/x-aaaa-x-bbbb", ["x-aaaa", "x-bbbb"]),
    # greedy hex takes the longest valid id
    ("repro/x-ab123-repro", ["x-ab123"]),
]


@pytest.mark.parametrize("branch,expected", TOKEN_CASES, ids=[c[0] for c in TOKEN_CASES])
def test_rust_token_scanner_matches_the_python_producer(tmp_path, branch, expected):
    from fno.pr.closure import branch_node_ids

    assert branch_node_ids(branch) == expected, "the Python producer moved under the corpus"

    repo = tmp_path / "repo"
    repo.mkdir()
    _git(repo, "init", "-q", "-b", "main")
    _git(repo, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-qm", "seed")
    wt = tmp_path / "wt"
    _git(repo, "worktree", "add", "-q", str(wt), "-b", branch)
    old = time.time() - 49 * 3600
    os.utime(wt / ".git", (old, old))
    (wt / "scratch.py").write_text("nope\n")

    home = tmp_path / "graph-home" / "agents"
    home.mkdir(parents=True)
    rows = [{"id": tok, "status": "done"} for tok in expected]
    (home.parent / "graph-archive.json").write_text(json.dumps({"entries": rows}))
    env = dict(os.environ, FNO_AGENTS_HOME=str(home))

    r = subprocess.run(
        [str(_gate_binary()), "worktree-reapable", "--done-node", str(wt)],
        capture_output=True, text=True, timeout=60, env=env,
    )
    receipt = (r.stdout or "").strip().splitlines()
    assert receipt, f"gate emitted no receipt: {r.stderr}"
    evidence = [f for f in receipt[0].split() if f.startswith("evidence=")]
    assert evidence == [f"evidence=node:{','.join(expected)}"], receipt[0]
