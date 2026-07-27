"""`requires: skill` resolution for project-registered reviewers (node x-a534).

A registered reviewer names a skill on the operator's harness. The predicate
answers "does that skill resolve here" at init, so an absent one refuses BEFORE
the work rather than wedging the stop gate after it. The third outcome is the
load-bearing one: anything the probe cannot answer resolves `unverifiable`,
which is already non-blocking, so a wrong guess about Claude's skill roots
degrades into "proceed with the stop gate as backstop" instead of bricking a
run over a reviewer that is actually installed.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.config import ReviewerDescriptor
from fno.review_capability import (
    SessionCapability,
    refusal_message,
    resolve_reviewers,
)

SKILL = ReviewerDescriptor(
    kind="harness-skill",
    requires="skill",
    invocation="/my-security-skill",
    asserts="invocation",
)
REGISTRY = {"my-security-skill": SKILL}


def _claude(attended: bool = True) -> SessionCapability:
    return SessionCapability(harness="claude", substrate="interactive", attended=attended)


def _install(root: Path, name: str) -> None:
    (root / ".claude" / "skills" / name).mkdir(parents=True)
    (root / ".claude" / "skills" / name / "SKILL.md").write_text("---\nname: x\n---\n")


@pytest.fixture
def isolated(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A cwd and a HOME with no skill roots, so each test declares its own."""
    home = tmp_path / "home"
    project = tmp_path / "project"
    home.mkdir()
    project.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.chdir(project)
    return tmp_path


def _resolve(name: str = "my-security-skill", session=None):
    return resolve_reviewers([name], session or _claude(), REGISTRY)[0]


def test_a_skill_in_the_project_root_is_satisfiable(isolated: Path):
    """AC6-HP: the gate resolves, so init proceeds and the attestation can clear it."""
    _install(isolated / "project", "my-security-skill")
    v = _resolve()
    assert v.status == "satisfiable"
    assert not v.blocks_autonomy


def test_a_skill_in_the_user_root_is_satisfiable(isolated: Path):
    _install(isolated / "home", "my-security-skill")
    assert _resolve().status == "satisfiable"


def test_an_absent_skill_refuses_at_init(isolated: Path):
    """AC7-ERR: refuses, names the roots searched, and never proposes `declare`."""
    _install(isolated / "project", "some-other-skill")  # a root exists, ours does not
    v = _resolve()
    assert v.status == "unavailable"
    assert v.blocks_autonomy
    assert ".claude/skills" in v.reason

    message = refusal_message([v], _claude())
    assert message is not None
    assert "my-security-skill" in message
    assert "install the named skill" in message
    assert "declare" not in message
    # A missing skill is not fixed by a harness that dispatches subagents.
    assert "dispatches subagents" not in message


def test_no_readable_root_is_unverifiable_and_proceeds(isolated: Path):
    """AC8-BOUND: the probe cannot answer, so the run proceeds with one note."""
    v = _resolve()
    assert v.status == "unverifiable"
    assert not v.blocks_autonomy
    assert refusal_message([v], _claude()) is None


def test_a_non_claude_harness_is_unverifiable(isolated: Path):
    """footnote only knows Claude's skill roots; reporting absence elsewhere
    would refuse a session over a reviewer that IS installed."""
    _install(isolated / "project", "my-security-skill")
    session = SessionCapability(harness="codex", substrate="pane", attended=False)
    v = _resolve(session=session)
    assert v.status == "unverifiable"
    assert not v.blocks_autonomy


def test_a_plugin_qualified_name_is_unverifiable(isolated: Path):
    """The plugin cache is versioned and marketplace-shaped; footnote does not
    read it, so a `plugin:skill` name degrades rather than being called absent."""
    _install(isolated / "project", "my-security-skill")
    registry = {"fno:review": SKILL}
    v = resolve_reviewers(["fno:review"], _claude(), registry)[0]
    assert v.status == "unverifiable"
    assert not v.blocks_autonomy


def test_the_invocation_rung_is_printed(isolated: Path):
    """An operator must learn what is behind the checkmark here, not at the
    stop gate: `invocation` proves the thing ran, nothing about its verdict."""
    _install(isolated / "project", "my-security-skill")
    line = _resolve().line()
    assert "[invocation:" in line
    assert "asserts nothing about its verdict" in line


def test_an_unregistered_name_stays_unknown(isolated: Path):
    """The union lookup must not invent a descriptor for a name nobody declared."""
    v = resolve_reviewers(["ghost"], _claude(), REGISTRY)[0]
    assert v.status == "unavailable"
    assert v.descriptor is None


def test_builtins_still_resolve_through_the_union(isolated: Path):
    v = resolve_reviewers(["sigma"], _claude(), REGISTRY)[0]
    assert v.status == "satisfiable"


def test_a_registry_entry_cannot_shadow_a_builtin_at_resolution(isolated: Path):
    """Built-ins win, so a project cannot downgrade `sigma` to a witnessed gate."""
    v = resolve_reviewers(["sigma"], _claude(), {"sigma": SKILL})[0]
    assert v.descriptor is not None
    assert v.descriptor.asserts == "review-evidence"
