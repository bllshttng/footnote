"""Contract tests for scripts/ci/check-session-id-provenance-rule.sh.

The gate keeps the two session-id docstrings agreeing: registry.py and
self_identity.py must both carry, verbatim, the canonical provenance
sentence that lives once under a marker in docs/architecture/coordination.md.
Output is captured via subprocess so the asserted returncode is the real
one. Exit contract: 0 agree, 1 diverged, 2 misuse.
"""
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
GATE = ROOT / "scripts" / "ci" / "check-session-id-provenance-rule.sh"

SENTENCE = (
    "Provenance decides a hit: self when the id under test is this "
    "session's own, foreign otherwise."
)
BEGIN = "<!-- session-id-provenance-rule:begin -->"
END = "<!-- session-id-provenance-rule:end -->"
COORD_REL = "docs/architecture/coordination.md"
SITE_RELS = (
    "cli/src/fno/agents/registry.py",
    "cli/src/fno/claims/self_identity.py",
)


def _run(cwd: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(GATE), *args], cwd=cwd, capture_output=True, text=True
    )


def _fixture_repo(
    tmp_path: Path,
    coord: str | None = None,
    sites: tuple[str, str] = (SENTENCE, SENTENCE),
    commit: bool = True,
) -> Path:
    coord_block = (
        f"{BEGIN}\n{SENTENCE}\n{END}\n" if coord is None else coord
    )
    repo = tmp_path / "repo"
    (repo / "docs" / "architecture").mkdir(parents=True)
    (repo / "cli" / "src" / "fno" / "agents").mkdir(parents=True)
    (repo / "cli" / "src" / "fno" / "claims").mkdir(parents=True)
    (repo / COORD_REL).write_text(
        f"# Coordination\n\n{coord_block}\n", encoding="utf-8"
    )
    template = 'def f():\n    """Docstring.\n\n    {s}\n    """\n'
    (repo / SITE_RELS[0]).write_text(template.format(s=sites[0]), encoding="utf-8")
    (repo / SITE_RELS[1]).write_text(template.format(s=sites[1]), encoding="utf-8")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
    if commit:
        subprocess.run(
            ["git", "-c", "user.email=t@t", "-c", "user.name=t",
             "commit", "-qm", "fixture"],
            cwd=repo, check=True,
        )
    return repo


def test_shipped_repo_agrees() -> None:
    r = _run(ROOT)
    assert r.returncode == 0, r.stderr


def test_fixture_repo_agrees(tmp_path: Path) -> None:
    repo = _fixture_repo(tmp_path)
    r = _run(repo)
    assert r.returncode == 0, r.stderr


def test_one_site_diverged_fails_naming_both_sites(tmp_path: Path) -> None:
    repo = _fixture_repo(tmp_path, sites=(SENTENCE, "a hit is always foreign."))
    r = _run(repo)
    assert r.returncode == 1
    for rel in SITE_RELS:
        assert rel in r.stderr, r.stderr
    assert SENTENCE in r.stderr


def test_zero_sites_is_a_loud_refusal(tmp_path: Path) -> None:
    repo = _fixture_repo(tmp_path, sites=("", ""))
    r = _run(repo)
    assert r.returncode == 2
    assert "zero" in r.stderr.lower()


def test_missing_marker_is_misuse(tmp_path: Path) -> None:
    repo = _fixture_repo(tmp_path, coord=f"{SENTENCE}\n")
    r = _run(repo)
    assert r.returncode == 2


def test_missing_site_file_is_misuse(tmp_path: Path) -> None:
    repo = _fixture_repo(tmp_path)
    (repo / SITE_RELS[1]).unlink()
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
    subprocess.run(
        ["git", "-c", "user.email=t@t", "-c", "user.name=t",
         "commit", "-qm", "drop site"],
        cwd=repo, check=True,
    )
    r = _run(repo)
    assert r.returncode == 2


def test_write_mode_is_refused(tmp_path: Path) -> None:
    repo = _fixture_repo(tmp_path)
    r = _run(repo, "--write")
    assert r.returncode == 2


def test_worktree_mode_sees_uncommitted_edit(tmp_path: Path) -> None:
    repo = _fixture_repo(tmp_path)
    (repo / SITE_RELS[1]).write_text(
        'def f():\n    """Docstring.\n\n    drifted\n    """\n', encoding="utf-8"
    )
    assert _run(repo).returncode == 0, "committed bytes still agree"
    r = _run(repo, "--worktree")
    assert r.returncode == 1
