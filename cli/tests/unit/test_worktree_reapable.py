"""Reapability classifier (x-5a30 task 1.1).

A worktree blocks removal only when it holds content removal would DESTROY.
A tracked file missing from disk is not that: its content is in the object
store at HEAD, so removing the worktree loses nothing.

Measured 2026-08-13: 17 of 20 dirty worktrees were dirty only because the same
76 tracked paths were missing, every one under a directory named `target`. The
old "is `git status --porcelain` empty" predicate blocked all 17, which is the
one class of dirt that cannot cause data loss.

The tests drive real temp git repos, not mocked porcelain strings, because the
classifier's job is to be right about what git actually prints.
"""
import re
import subprocess
from pathlib import Path

import pytest

from fno.worktree_reapable import _is_setup_link, branch_merged, classify, reapable


def _git(cwd: Path, *args: str) -> str:
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    assert r.returncode == 0, f"git {' '.join(args)} failed: {r.stderr}"
    return r.stdout


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
    wt = tmp_path / "wt"
    wt.mkdir()
    _git(wt, "init", "-q", "-b", "main")
    _git(wt, "config", "user.email", "t@example.com")
    _git(wt, "config", "user.name", "t")
    (wt / "keep.py").write_text("x = 1\n")
    (wt / "also.py").write_text("y = 2\n")
    (wt / ".gitignore").write_text("ignored/\n")
    _git(wt, "add", "-A")
    _git(wt, "commit", "-qm", "seed")
    return wt


# -- AC1-HP: deletions are recoverable ---------------------------------------


def test_deletions_only_is_reapable_and_counts_them(repo: Path) -> None:
    (repo / "keep.py").unlink()
    (repo / "also.py").unlink()

    v = reapable(repo)

    assert v.reapable is True
    assert v.reason == "clean"
    assert v.recoverable_deletions == 2


def test_staged_deletion_is_also_recoverable(repo: Path) -> None:
    _git(repo, "rm", "-q", "keep.py")

    v = reapable(repo)

    assert v.reapable is True
    assert v.recoverable_deletions == 1


def test_clean_worktree_is_reapable_with_zero_deletions(repo: Path) -> None:
    v = reapable(repo)

    assert v.reapable is True
    assert v.reason == "clean"
    assert v.recoverable_deletions == 0


# -- AC1-EDGE: modified tracked content blocks -------------------------------


def test_modified_tracked_file_blocks_and_names_it(repo: Path) -> None:
    (repo / "keep.py").write_text("x = 999\n")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "modified-tracked"
    assert "keep.py" in v.detail


def test_one_modification_beside_many_deletions_still_blocks(repo: Path) -> None:
    (repo / "also.py").unlink()
    (repo / "keep.py").write_text("x = 999\n")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "modified-tracked"


def test_staged_addition_blocks(repo: Path) -> None:
    (repo / "new.py").write_text("z = 3\n")
    _git(repo, "add", "new.py")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "modified-tracked"


# -- AC1-ERR: untracked non-ignored content blocks ---------------------------


def test_untracked_file_blocks(repo: Path) -> None:
    (repo / "scratch.py").write_text("nope\n")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "untracked"
    assert "scratch.py" in v.detail


def test_ignored_file_is_invisible_and_does_not_block(repo: Path) -> None:
    (repo / "ignored").mkdir()
    (repo / "ignored" / "junk.bin").write_text("junk\n")

    v = reapable(repo)

    assert v.reapable is True
    assert v.reason == "clean"


# -- Conflicts are never recoverable, even when both sides deleted -----------


@pytest.mark.parametrize("code", ["DD", "AU", "UD", "UA", "DU", "AA", "UU"])
def test_unmerged_codes_block_even_when_only_D_chars(code: str) -> None:
    """`DD` is "both deleted", a CONFLICT, not two recoverable deletions.

    Classifying it on its letters alone reads it as recoverable and throws
    away a merge the user has not resolved.
    """
    v = classify(f"{code} conflicted.py\n")

    assert v.reapable is False
    assert v.reason == "unmerged"


# -- Probe failure fails CLOSED ----------------------------------------------


def test_non_repo_path_fails_closed(tmp_path: Path) -> None:
    plain = tmp_path / "not-a-repo"
    plain.mkdir()

    v = reapable(plain)

    assert v.reapable is False
    assert v.reason == "probe-failed"


def test_missing_path_fails_closed(tmp_path: Path) -> None:
    v = reapable(tmp_path / "gone")

    assert v.reapable is False
    assert v.reason == "probe-failed"


# -- The receipt line the bash and rust callers parse ------------------------


def test_receipt_line_is_one_parseable_line(repo: Path) -> None:
    (repo / "keep.py").unlink()

    line = reapable(repo).line()

    assert line.startswith("reapable=yes ")
    assert "reason=clean" in line
    assert "recoverable_deletions=1" in line
    assert "\n" not in line


def test_blocking_receipt_names_the_reason_and_detail(repo: Path) -> None:
    (repo / "scratch.py").write_text("nope\n")

    line = reapable(repo).line()

    assert line.startswith("reapable=no ")
    assert "reason=untracked" in line
    assert "detail=scratch.py" in line


def test_detail_never_breaks_the_line_grammar(repo: Path) -> None:
    """A path with a space must not split the receipt into fake fields."""
    (repo / "two words.py").write_text("nope\n")

    line = reapable(repo).line()

    assert "\n" not in line
    assert line.count("reapable=") == 1


# -- Pure classify: the contract the equivalence test pins --------------------


def test_classify_is_pure_over_porcelain_text() -> None:
    text = " D a.py\nD  b.py\n D c.py\n"

    v = classify(text)

    assert v.reapable is True
    assert v.recoverable_deletions == 3


def test_classify_empty_is_clean() -> None:
    assert classify("").reapable is True
    assert classify("").recoverable_deletions == 0


# -- The merge check: the rm door's half of the third bucket ------------------
#
# `reapable()` stays a CONTENT answer (the sweep merge-filters before asking;
# the equivalence corpus pins that). `branch_merged` is the separate question
# a caller without a merge pre-filter must ask, so a row removal can never be
# the fourth door around "clean-and-unmerged is never auto-pruned".


def _linked_wt(tmp_path: Path, repo: Path, name: str, branch: str) -> Path:
    wt = tmp_path / name
    _git(repo, "worktree", "add", "-q", str(wt), "-b", branch)
    return wt


def test_clean_but_unmerged_branch_blocks_the_rm_question(
    repo: Path, tmp_path: Path
) -> None:
    wt = _linked_wt(tmp_path, repo, "leaf", "feature")
    (wt / "new.py").write_text("n = 1\n")
    _git(wt, "add", "-A")
    _git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-qm", "work")

    assert branch_merged(wt) is False


def test_a_fast_forwarded_branch_reads_merged(repo: Path, tmp_path: Path) -> None:
    wt = _linked_wt(tmp_path, repo, "leaf2", "done")
    _git(wt, "commit", "--allow-empty", "-qm", "w")
    _git(repo, "merge", "-q", "done")

    assert branch_merged(wt) is True


def test_detached_head_answers_unknown(repo: Path, tmp_path: Path) -> None:
    wt = _linked_wt(tmp_path, repo, "leaf3", "scratch")
    _git(wt, "checkout", "-q", "--detach")

    assert branch_merged(wt) is None


# -- x-11d8: footnote's own setup symlinks are not dirt ----------------------
#
# scripts/setup/setup-worktree.sh links the canonical checkout's shared state
# into every worktree it prepares. Those names are gitignored at the repo ROOT
# only, so a nested copy (cli/.agents, specimen 2026-09-06) reads untracked and
# the tree was kept forever. The reap half is one test; the guard half is five,
# because that is the direction that loses work.


@pytest.fixture()
def canonical(repo: Path) -> Path:
    """`repo` plus the shared state setup links, and a tracked `cli/`.

    The tracked file matters: with `cli/` wholly untracked git reports one
    `cli/` line and the per-path attribution is never exercised.
    """
    (repo / "cli").mkdir()
    (repo / "cli" / "keep.py").write_text("x = 1\n")
    _git(repo, "add", "cli/keep.py")
    _git(repo, "commit", "-qm", "cli")
    for rel in (".agents", ".codex", ".codex-plugin", ".claude", ".claude/skills"):
        (repo / rel).mkdir()
    (repo / ".claude" / "settings.local.json").write_text("{}\n")
    return repo


def _setup_links(wt: Path, canonical: Path) -> None:
    """What setup-worktree.sh leaves behind, one directory deeper."""
    (wt / "cli" / ".claude").mkdir(parents=True)
    (wt / "cli" / ".agents").symlink_to(canonical / ".agents")
    (wt / "cli" / ".codex").symlink_to(canonical / ".codex")
    (wt / "cli" / ".codex-plugin").symlink_to(canonical / ".codex-plugin")
    (wt / "cli" / ".claude" / "skills").symlink_to(canonical / ".claude" / "skills")
    (wt / "cli" / ".claude" / "settings.local.json").symlink_to(
        canonical / ".claude" / "settings.local.json"
    )


def test_setup_links_only_is_reapable_and_names_what_it_discounted(
    canonical: Path, tmp_path: Path
) -> None:
    wt = _linked_wt(tmp_path, canonical, "setup", "feature/setup")
    _setup_links(wt, canonical)

    v = reapable(wt)

    assert v.reapable is True
    assert v.reason == "setup-links"
    # Named, not counted: a developer's global ignore file or `.git/info/exclude`
    # can hide one of these from git, and the count is not the claim under test.
    for named in ("cli/.agents", "cli/.claude/skills", "cli/.codex", "cli/.codex-plugin"):
        assert named in v.detail
    assert f"discounted={len(v.discounted)}" in v.line()


def test_one_modified_tracked_file_beside_setup_links_still_blocks(
    canonical: Path, tmp_path: Path
) -> None:
    wt = _linked_wt(tmp_path, canonical, "modified", "feature/modified")
    _setup_links(wt, canonical)
    (wt / "cli" / "keep.py").write_text("x = 999\n")

    v = reapable(wt)

    assert v.reapable is False
    assert v.reason == "modified-tracked"


def test_one_plain_untracked_file_beside_setup_links_still_blocks(
    canonical: Path, tmp_path: Path
) -> None:
    wt = _linked_wt(tmp_path, canonical, "scratch", "feature/scratch")
    _setup_links(wt, canonical)
    (wt / "cli" / "scratch.py").write_text("real work\n")

    v = reapable(wt)

    assert v.reapable is False
    assert v.reason == "untracked"
    assert "scratch.py" in v.detail


def test_a_symlink_out_of_the_canonical_checkout_still_blocks(
    canonical: Path, tmp_path: Path
) -> None:
    wt = _linked_wt(tmp_path, canonical, "outward", "feature/outward")
    _setup_links(wt, canonical)
    (wt / "cli" / "elsewhere").symlink_to(tmp_path / "somewhere-else")

    v = reapable(wt)

    assert v.reapable is False
    assert v.reason == "untracked"
    assert "elsewhere" in v.detail


def test_a_canonical_symlink_setup_never_writes_still_blocks(
    canonical: Path, tmp_path: Path
) -> None:
    wt = _linked_wt(tmp_path, canonical, "unknown", "feature/unknown")
    _setup_links(wt, canonical)
    (wt / "cli" / "borrowed.py").symlink_to(canonical / "keep.py")

    v = reapable(wt)

    assert v.reapable is False
    assert v.reason == "untracked"
    assert "borrowed.py" in v.detail


def test_a_directory_mixing_a_setup_link_with_real_content_blocks(
    canonical: Path, tmp_path: Path
) -> None:
    wt = _linked_wt(tmp_path, canonical, "mixed", "feature/mixed")
    _setup_links(wt, canonical)
    (wt / "cli" / ".claude" / "notes.md").write_text("mine\n")

    v = reapable(wt)

    assert v.reapable is False
    assert v.reason == "untracked"
    assert "cli/.claude" in v.detail


def test_classify_without_a_discount_answers_exactly_as_before() -> None:
    v = classify("?? cli/.agents\n")

    assert v.reapable is False
    assert v.reason == "untracked"
    assert v.discounted == ()


# -- Parity: the discount must track what setup-worktree.sh actually links ---


def test_every_path_setup_links_is_discounted(tmp_path: Path) -> None:
    """Read the script's own link calls; each must pass the predicate.

    Without this, adding `link_dir ".cursor"` to setup-worktree.sh silently
    puts every fresh worktree back in the kept-forever bucket, and no test
    fails. The script is the authority; this asserts the classifier follows.
    """
    script = Path(__file__).resolve().parents[3] / "scripts" / "setup" / "setup-worktree.sh"
    body = script.read_text()
    sites = re.findall(r"^\s*link_(?:dir|file|artifact)\s+(\S.*)$", body, re.M)
    literals = [m for m in (re.fullmatch(r'"([^"$]+)"', arg.strip()) for arg in sites) if m]
    dynamic = [arg.strip() for arg in sites if not re.fullmatch(r'"[^"$]+"', arg.strip())]
    # Every call site is accounted for, so a new one cannot slip past the
    # parser the way a bare or interpolated argument would.
    assert len(literals) + len(dynamic) == len(sites) and len(sites) >= 10
    for arg in dynamic:
        assert arg.startswith('".claude/'), f"unknown dynamic link target {arg}"

    canonical = tmp_path / "canonical"
    worktree = tmp_path / "wt"
    for match in literals:
        rel = match.group(1)
        target = canonical / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("x")
        # Both placements setup uses: at the worktree root, and one directory
        # deeper, which is the shape that reads untracked.
        for link in (worktree / rel, worktree / "cli" / rel):
            link.parent.mkdir(parents=True, exist_ok=True)
            link.symlink_to(target)
            assert _is_setup_link(link, worktree, canonical), f"setup links {rel}, unknown"


def test_an_ignored_sibling_does_not_veto_the_discount(
    canonical: Path, tmp_path: Path
) -> None:
    """The default porcelain collapses a directory; its children may be ignored.

    `.gitignore` carries `**/.claude/hooks/`, and this repo's own global
    ignore and `.git/info/exclude` cover more. Judging a collapsed `cli/.claude/`
    from disk asks about files git does not track, and one of them vetoed the
    whole discount. Reading with `-uall` never collapses, so it never asks.
    """
    wt = _linked_wt(tmp_path, canonical, "ignored", "feature/ignored")
    (wt / ".gitignore").write_text("**/.claude/hooks/\n")
    _git(wt, "add", ".gitignore")
    _git(wt, "-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-qm", "ignore")
    _setup_links(wt, canonical)
    (wt / "cli" / ".claude" / "hooks").mkdir()
    (wt / "cli" / ".claude" / "hooks" / "log.txt").write_text("runtime noise\n")
    # The fixture really does reproduce the trap: the default read collapses.
    assert "?? cli/.claude/\n" in _git(wt, "status", "--porcelain")

    v = reapable(wt)

    assert v.reapable is True, f"an ignored sibling must not block: {v.line()}"
    assert v.reason == "setup-links"


def test_a_setup_target_linked_from_the_wrong_place_still_blocks(
    canonical: Path, tmp_path: Path
) -> None:
    """The receipt claims setup authorship, so the link's own path must agree."""
    wt = _linked_wt(tmp_path, canonical, "misplaced", "feature/misplaced")
    (canonical / "internal").mkdir(exist_ok=True)
    (wt / "vault").symlink_to(canonical / "internal")

    v = reapable(wt)

    assert v.reapable is False
    assert v.reason == "untracked"
    assert "vault" in v.detail
