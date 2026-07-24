"""Regression tests for scripts/ci/check-pitfalls.sh.

The gate parses AGENTS.md's `## Pitfalls corpus (capped)` section and fails on
an over-cap corpus, a missing field, or a stale entry. Output is captured via
subprocess (not piped through a tee) so the asserted returncode is the real one.
"""
import subprocess
from datetime import date, timedelta
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
LINT = ROOT / "scripts" / "ci" / "check-pitfalls.sh"
AGENTS = ROOT / "AGENTS.md"

SECTION = "## Pitfalls corpus (capped)"
NEXT_HEADING = "## Repository"


def _run(target: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(LINT), str(target)],
        capture_output=True,
        text=True,
    )


def _fixture(tmp_path: Path, entries):
    body = [f"# AGENTS\n\n{SECTION}\n\nrationale.\n\n"]
    for trap, grad, added in entries:
        body.append(
            f"### {trap}\n\n{trap} statement.\n\n"
            f"- graduates-to: {grad}\n- added: {added}\n\n"
        )
    body.append(f"{NEXT_HEADING}\n")
    path = tmp_path / "agents.md"
    path.write_text("".join(body), encoding="utf-8")
    return path


FRESH = date.today().isoformat()
STALE = (date.today() - timedelta(days=90)).isoformat()
GOOD = ("A trap", "a lint", FRESH)


def test_shipped_agents_md_passes() -> None:
    r = _run(AGENTS)
    assert r.returncode == 0, r.stderr
    assert "all valid" in r.stdout


def test_over_cap_fails(tmp_path: Path) -> None:
    path = _fixture(tmp_path, [GOOD] * 11)
    r = _run(path)
    assert r.returncode == 1
    assert "exceed the 10-entry cap" in r.stderr


def test_missing_graduates_to_fails(tmp_path: Path) -> None:
    path = _fixture(tmp_path, [("Bad", "", FRESH)])
    # _fixture always emits graduates-to; build a manual miss instead.
    path.write_text(
        f"# AGENTS\n\n{SECTION}\n\nrationale.\n\n### Bad\n\ntrap.\n\n- added: {FRESH}\n\n{NEXT_HEADING}\n",
        encoding="utf-8",
    )
    r = _run(path)
    assert r.returncode == 1
    assert "missing a 'graduates-to:' field" in r.stderr


def test_missing_added_fails(tmp_path: Path) -> None:
    path = tmp_path / "agents.md"
    path.write_text(
        f"# AGENTS\n\n{SECTION}\n\nrationale.\n\n### Bad\n\ntrap.\n\n- graduates-to: a lint\n\n{NEXT_HEADING}\n",
        encoding="utf-8",
    )
    r = _run(path)
    assert r.returncode == 1
    assert "missing an 'added:' field" in r.stderr


def test_stale_entry_fails(tmp_path: Path) -> None:
    path = _fixture(tmp_path, [("Old", "a lint", STALE)])
    r = _run(path)
    assert r.returncode == 1
    assert "over the 60-day limit" in r.stderr


def test_missing_section_fails(tmp_path: Path) -> None:
    path = tmp_path / "agents.md"
    path.write_text("# AGENTS\n\n## Repository\n", encoding="utf-8")
    r = _run(path)
    assert r.returncode == 1
    assert "no" in r.stderr and SECTION in r.stderr
