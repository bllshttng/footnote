"""Regression tests for scripts/ci/check-pitfalls.sh.

The gate parses AGENTS.md's `## Pitfalls corpus (capped)` section and fails on
an over-cap corpus, a missing field, a stale entry, or a title that names a
shipped `fno` verb as the trap. Output is captured via subprocess (not piped
through a tee) so the asserted returncode is the real one.
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


def _one_entry(tmp_path: Path, title: str, body: str = "trap.") -> Path:
    path = tmp_path / "agents.md"
    path.write_text(
        f"# AGENTS\n\n{SECTION}\n\nrationale.\n\n### {title}\n\n{body}\n\n"
        f"- graduates-to: a lint\n- added: {FRESH}\n\n{NEXT_HEADING}\n",
        encoding="utf-8",
    )
    return path


def test_title_naming_shipped_verb_fails(tmp_path: Path) -> None:
    # The exact entry this check exists to prevent re-acquiring: a trap titled
    # for the verb that already fixes it, so the verb IS the graduating carrier.
    path = _one_entry(tmp_path, "`fno doctor test` can report a false green")
    r = _run(path)
    assert r.returncode == 1
    assert "fno doctor test" in r.stderr
    assert "carrier" in r.stderr


def test_title_naming_shipped_verb_without_backticks_fails(tmp_path: Path) -> None:
    path = _one_entry(tmp_path, "fno backlog silently drops a node")
    r = _run(path)
    assert r.returncode == 1
    assert "fno backlog" in r.stderr


def test_title_naming_unshipped_verb_passes(tmp_path: Path) -> None:
    # No false positive on a verb that does not exist: nothing carries it yet,
    # so the corpus is still the right home.
    path = _one_entry(tmp_path, "`fno frobnicate` eats the graph")
    r = _run(path)
    assert r.returncode == 0, r.stderr


def test_body_may_cite_a_shipped_verb(tmp_path: Path) -> None:
    # Only the title claims what the trap IS; a body cites specimens freely.
    path = _one_entry(
        tmp_path,
        "Receipt lines have lied",
        body="`fno do target start` can print `plan: none` while a plan is bound.",
    )
    r = _run(path)
    assert r.returncode == 0, r.stderr


def test_title_with_pipe_parses_staleness(tmp_path: Path) -> None:
    # A `|` in the heading must not corrupt the staleness record (tab-delimited).
    path = tmp_path / "agents.md"
    path.write_text(
        f"# AGENTS\n\n{SECTION}\n\nrationale.\n\n### A | B trap\n\ntrap.\n\n"
        f"- graduates-to: a lint\n- added: {STALE}\n\n{NEXT_HEADING}\n",
        encoding="utf-8",
    )
    r = _run(path)
    assert r.returncode == 1
    assert "over the 60-day limit" in r.stderr
    assert "A | B trap" in r.stderr
    assert "unparseable" not in r.stderr


def _transplant(tmp_path: Path, registry_text: str | None) -> Path:
    """A copy of the gate whose sibling registry is ours.

    REGISTRY resolves from BASH_SOURCE, so moving the script moves the file it
    reads. That is what lets these two cases run without adding a test-only
    environment knob to the gate itself.
    """
    lint = tmp_path / "scripts" / "ci" / LINT.name
    lint.parent.mkdir(parents=True)
    lint.write_text(LINT.read_text(encoding="utf-8"), encoding="utf-8")
    if registry_text is not None:
        registry = tmp_path / "cli" / "src" / "fno" / "cli.py"
        registry.parent.mkdir(parents=True)
        registry.write_text(registry_text, encoding="utf-8")
    return lint


def test_a_registry_that_yields_no_verbs_fails_loud(tmp_path: Path) -> None:
    """An empty verb list makes every title match nothing.

    The gate would then pass on the exact entry it exists to reject, which is
    the absence trap it is written to enforce against. A PRESENT registry that
    parses to nothing is a broken read, not a CLI without verbs.
    """
    target = _one_entry(tmp_path, "fno backlog silently drops a node")
    lint = _transplant(tmp_path, "# a registry with no lazy-subcommand rows\n")

    r = subprocess.run(
        ["bash", str(lint), str(target)], capture_output=True, text=True
    )
    assert r.returncode == 1
    assert "read 0 verbs" in r.stderr
    assert "broken read" in r.stderr


def test_an_absent_registry_still_skips_the_verb_check(tmp_path: Path) -> None:
    """The legitimate skip stays a skip: a consumer repo has no CLI to read."""
    target = _one_entry(tmp_path, "fno backlog silently drops a node")
    lint = _transplant(tmp_path, None)

    r = subprocess.run(
        ["bash", str(lint), str(target)], capture_output=True, text=True
    )
    assert r.returncode == 0, r.stderr
    assert "read 0 verbs" not in r.stderr


# --- pinned claim-bearing qualifiers (x-f3be) -------------------------------
#
# A structural check counts headings, fields, dates and backticked tokens. A
# qualifier is none of those, so a prose diet can delete the word carrying the
# claim while every count still passes. These tests verify by DELETING the
# qualifier and asserting the gate fails, never by re-running the counts that
# already pass on that deletion.


def _agents_without(tmp_path: Path, old: str, new: str) -> Path:
    """The shipped corpus with one qualifier edited out."""
    text = AGENTS.read_text(encoding="utf-8")
    assert text.count(old) == 1, f"expected exactly one {old!r}"
    path = tmp_path / "AGENTS.md"
    path.write_text(text.replace(old, new, 1), encoding="utf-8")
    return path


def test_dropping_the_mail_probe_qualifier_fails(tmp_path: Path) -> None:
    """Broadened to "mail", it rejects a worker's own autonomous evidence."""
    path = _agents_without(tmp_path, "a mail probe", "mail")
    r = _run(path)
    assert r.returncode == 1
    assert "mail probe" in r.stderr


def test_an_absent_entry_releases_its_pinned_phrase(tmp_path: Path) -> None:
    """Eviction stays legal: a pin binds only while its own entry is present.

    Without this, the first entry to age out at 60 days wedges the gate on
    prose the corpus is supposed to have dropped.
    """
    path = _fixture(tmp_path, [GOOD])
    r = _run(path)
    assert r.returncode == 0, r.stderr
    assert "live lockfile" not in r.stderr
