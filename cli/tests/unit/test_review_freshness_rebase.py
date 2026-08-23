"""x-e8db regression pair: attestation freshness keyed to content, not sha.

Freshness is an ancestry test at the recheck in ``_reviews``, so every rebase
invalidated every attestation - even one that reviewed byte-identical content
(the operator's infinite-reviews treadmill). The pair is the whole contract:
a rebase that changed nothing keeps the attestation, a rebase that resolved a
conflict (and so changed the delta) loses it. A test for only the survival
case would pass for a gate that never expires anything.

The Rust twin of this pair lives on ``FreshnessResolver`` in loopcheck.rs
(``resolver_carries_an_identical_rebase_and_expires_a_conflict``); the two
gates must agree on the same PR.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.pr import _reviews

_CODE_VERDICT = {
    "producer": "local_attestation",
    "name": "code-review",
    "verdict": "reviewed",
}


def _sh(*args, cwd):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    assert r.returncode == 0, f"{args}: {r.stderr}"
    return r.stdout.strip()


def _repo_with_feature_branch(tmp, *, conflicting_base: bool) -> tuple[Path, str, str]:
    """A repo whose feature branch was rebased onto a moved base.

    Returns ``(repo, reviewed_sha, head_sha)`` where ``reviewed_sha`` is the
    pre-rebase commit the reviewer read and ``head_sha`` the post-rebase head.
    The remote-tracking ``origin/main`` moves with the base (update-ref, not
    push: the machine's pre-push hook protects even scratch ``main``s)."""
    repo = Path(tmp) / "r"
    repo.mkdir()
    _sh("git", "init", "-q", "-b", "main", str(repo), cwd=Path(tmp))
    _sh("git", "config", "user.email", "t@t", cwd=repo)
    _sh("git", "config", "user.name", "t", cwd=repo)
    (repo / "f.txt").write_text("base\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "base", cwd=repo)

    _sh("git", "checkout", "-q", "-b", "feature", cwd=repo)
    if conflicting_base:
        # Both sides edit f.txt so the rebase must stop on a conflict.
        (repo / "f.txt").write_text("feature says B\n", encoding="utf-8")
    else:
        (repo / "code.txt").write_text("pr change\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "pr", cwd=repo)
    reviewed = _sh("git", "rev-parse", "HEAD", cwd=repo)

    _sh("git", "checkout", "-q", "main", cwd=repo)
    if conflicting_base:
        (repo / "f.txt").write_text("main says C\n", encoding="utf-8")
    else:
        (repo / "other.txt").write_text("base moved\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "base moved", cwd=repo)
    tip = _sh("git", "rev-parse", "HEAD", cwd=repo)
    _sh("git", "update-ref", "refs/remotes/origin/main", tip, cwd=repo)

    _sh("git", "checkout", "-q", "feature", cwd=repo)
    r = subprocess.run(
        ["git", "rebase", "origin/main"], cwd=repo, capture_output=True, text=True
    )
    if conflicting_base:
        assert r.returncode != 0, "scenario requires a conflict"
        # The resolution CHANGES the reviewed content: that is the delta the
        # gate must expire.
        (repo / "f.txt").write_text("resolved differently\n", encoding="utf-8")
        _sh("git", "add", "-A", cwd=repo)
        r = subprocess.run(
            ["git", "-c", "core.editor=true", "rebase", "--continue"],
            cwd=repo,
            capture_output=True,
            text=True,
        )
        assert r.returncode == 0, r.stderr
    else:
        assert r.returncode == 0, r.stderr
    head = _sh("git", "rev-parse", "HEAD", cwd=repo)
    return repo, reviewed, head


def _shaped(repo, reviewed, head):
    event = {
        "coverage": "covered",
        "reviewed_count": 1,
        "head_sha": head,
        "verdicts": [dict(_CODE_VERDICT, reviewed_sha=reviewed, freshness="fresh")],
    }
    return _reviews._shape_review_coverage(event, head, str(repo))


def test_identical_rebase_keeps_the_attestation(tmp_path):
    repo, reviewed, head = _repo_with_feature_branch(
        tmp_path, conflicting_base=False
    )
    assert head != reviewed, "a rebase must rewrite the commit"
    ancestry = _reviews.run(
        ["git", "merge-base", "--is-ancestor", reviewed, head], cwd=str(repo)
    )
    assert ancestry.returncode != 0, (
        "precondition: the ancestry test alone kills the attestation here"
    )
    shaped = _shaped(repo, reviewed, head)
    assert shaped["verdicts"][0]["freshness"] != "stale", (
        "a rebase that changed no content must not cost a re-review"
    )
    assert shaped["coverage"] == "covered" and shaped["reviewed_count"] == 1


def test_conflict_resolving_rebase_expires_the_attestation(tmp_path):
    repo, reviewed, head = _repo_with_feature_branch(
        tmp_path, conflicting_base=True
    )
    shaped = _shaped(repo, reviewed, head)
    assert shaped["verdicts"][0]["freshness"] == "stale", (
        "a conflict resolution changed the reviewed delta: it is a new review"
    )
    assert shaped["coverage"] == "uncovered" and shaped["reviewed_count"] == 0
    assert shaped["stale_verdicts"][0]["reviewed_sha"] == reviewed


def test_docs_only_conflict_rebase_keeps_the_attestation(tmp_path):
    """The Rust twin drops documentation paths from its code-diff identity, so
    a rebase whose conflict resolution touched only a .md carries there
    (CarriedDocsOnly). The Python patch-id must exclude docs paths too, or the
    merge gate demands a re-review the stop gate already waived - the two
    gates expiring different rebases."""
    import subprocess

    tmp = tmp_path
    repo = tmp / "r"
    repo.mkdir()
    _sh("git", "init", "-q", "-b", "main", str(repo), cwd=tmp)
    _sh("git", "config", "user.email", "t@t", cwd=repo)
    _sh("git", "config", "user.name", "t", cwd=repo)
    (repo / "code.txt").write_text("code\n", encoding="utf-8")
    (repo / "docs").mkdir()
    (repo / "docs" / "guide.md").write_text("docs line A\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "base", cwd=repo)

    _sh("git", "checkout", "-q", "-b", "feature", cwd=repo)
    (repo / "code.txt").write_text("code\npr change\n", encoding="utf-8")
    (repo / "docs" / "guide.md").write_text("docs line B\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "pr", cwd=repo)
    reviewed = _sh("git", "rev-parse", "HEAD", cwd=repo)

    _sh("git", "checkout", "-q", "main", cwd=repo)
    (repo / "docs" / "guide.md").write_text("docs line C\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "base moved the docs", cwd=repo)
    tip = _sh("git", "rev-parse", "HEAD", cwd=repo)
    _sh("git", "update-ref", "refs/remotes/origin/main", tip, cwd=repo)

    _sh("git", "checkout", "-q", "feature", cwd=repo)
    r = subprocess.run(
        ["git", "rebase", "origin/main"], cwd=repo, capture_output=True, text=True
    )
    assert r.returncode != 0, "scenario requires a docs conflict"
    # Resolution takes the BASE side of the docs conflict: docs content
    # differs from what was reviewed, the code delta is untouched.
    (repo / "docs" / "guide.md").write_text("docs line C\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    r = subprocess.run(
        ["git", "-c", "core.editor=true", "rebase", "--continue"],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    assert r.returncode == 0, r.stderr
    head = _sh("git", "rev-parse", "HEAD", cwd=repo)

    shaped = _shaped(repo, reviewed, head)
    assert shaped["verdicts"][0]["freshness"] != "stale", (
        "a docs-only resolution changed no code: the carry must match the "
        "Rust twin's CarriedDocsOnly"
    )


def _rebased_repo_variant(tmp, variant):
    """Rebase scenarios pinning the identity's exact tightness. All three were
    verified against the construction both gates now share: the raw line
    carries the PRE-IMAGE blob sha too, so a sibling edit to the same file
    (different region, clean rebase) still expires - the reviewer never saw
    the sibling's bytes - while a delta the base absorbed (subset) carries."""
    repo = tmp / "r"
    repo.mkdir()
    _sh("git", "init", "-q", "-b", "main", str(repo), cwd=tmp)
    _sh("git", "config", "user.email", "t@t", cwd=repo)
    _sh("git", "config", "user.name", "t", cwd=repo)
    (repo / "code.txt").write_text("a\nb\nc\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "base", cwd=repo)
    tip = _sh("git", "rev-parse", "HEAD", cwd=repo)
    _sh("git", "update-ref", "refs/remotes/origin/main", tip, cwd=repo)

    _sh("git", "checkout", "-q", "-b", "feature", cwd=repo)
    if variant == "sibling":
        (repo / "code.txt").write_text("a\nb\nc\npr\n", encoding="utf-8")
        _sh("git", "add", "-A", cwd=repo)
        _sh("git", "commit", "-q", "-m", "pr", cwd=repo)
    elif variant == "reindent":
        (repo / "code.txt").write_text("a\nb\nc\n    pr\n", encoding="utf-8")
        _sh("git", "add", "-A", cwd=repo)
        _sh("git", "commit", "-q", "-m", "pr", cwd=repo)
    else:  # subset: two files, base later absorbs one
        (repo / "x.txt").write_text("x\n", encoding="utf-8")
        _sh("git", "add", "-A", cwd=repo)
        _sh("git", "commit", "-q", "-m", "add x", cwd=repo)
        (repo / "y.txt").write_text("y\n", encoding="utf-8")
        _sh("git", "add", "-A", cwd=repo)
        _sh("git", "commit", "-q", "-m", "add y", cwd=repo)
    reviewed = _sh("git", "rev-parse", "HEAD", cwd=repo)

    _sh("git", "checkout", "-q", "main", cwd=repo)
    if variant == "sibling":
        (repo / "code.txt").write_text("A\nb\nc\n", encoding="utf-8")
    elif variant == "reindent":
        (repo / "code.txt").write_text("a\nb\nc\nbase tail\n", encoding="utf-8")
    else:
        (repo / "x.txt").write_text("x\n", encoding="utf-8")
    _sh("git", "add", "-A", cwd=repo)
    _sh("git", "commit", "-q", "-m", "base moved", cwd=repo)
    tip = _sh("git", "rev-parse", "HEAD", cwd=repo)
    _sh("git", "update-ref", "refs/remotes/origin/main", tip, cwd=repo)

    _sh("git", "checkout", "-q", "feature", cwd=repo)
    r = subprocess.run(
        ["git", "rebase", "origin/main"], cwd=repo, capture_output=True, text=True
    )
    if variant == "reindent":
        assert r.returncode != 0, "scenario requires a conflict"
        (repo / "code.txt").write_text("a\nb\nc\n        pr reindented\n", encoding="utf-8")
        _sh("git", "add", "-A", cwd=repo)
        r = subprocess.run(
            ["git", "-c", "core.editor=true", "rebase", "--continue"],
            cwd=repo, capture_output=True, text=True,
        )
    assert r.returncode == 0, r.stderr
    head = _sh("git", "rev-parse", "HEAD", cwd=repo)
    return repo, reviewed, head


def test_sibling_edit_to_the_same_file_expires_the_attestation(tmp_path):
    """A patch-id approximation called this carried (it strips line numbers
    and whitespace); the raw-diff identity both gates now share does not: the
    pre-image blob sha changed, so the reviewer never saw the shipping
    bytes of that file."""
    repo, reviewed, head = _rebased_repo_variant(tmp_path, "sibling")
    shaped = _shaped(repo, reviewed, head)
    assert shaped["verdicts"][0]["freshness"] == "stale"


def test_reindented_conflict_resolution_expires_the_attestation(tmp_path):
    repo, reviewed, head = _rebased_repo_variant(tmp_path, "reindent")
    shaped = _shaped(repo, reviewed, head)
    assert shaped["verdicts"][0]["freshness"] == "stale"


def test_base_absorbed_subset_rebase_keeps_the_attestation(tmp_path):
    """The Rust twin's CarriedSubset: the delta only SHRANK (every raw line
    still shipping was read). The patch-id approximation re-staled this shape,
    re-creating the treadmill for subset rebases."""
    repo, reviewed, head = _rebased_repo_variant(tmp_path, "subset")
    shaped = _shaped(repo, reviewed, head)
    assert shaped["verdicts"][0]["freshness"] != "stale"
