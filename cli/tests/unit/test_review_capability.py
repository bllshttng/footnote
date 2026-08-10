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


def test_a_qualified_key_does_not_decide_resolvability(isolated: Path):
    """The INVOCATION decides, not the key. A `:` in the registry key with a
    plain, installed invocation must still resolve - the qualified-name
    degradation keys off the command footnote would actually run."""
    _install(isolated / "project", "my-security-skill")
    v = resolve_reviewers(["fno:review"], _claude(), {"fno:review": SKILL})[0]
    assert v.status == "satisfiable", v.reason


def test_the_invocation_rung_is_printed(isolated: Path):
    """An operator must learn what is behind the checkmark here, not at the
    stop gate: `invocation` proves the thing ran, nothing about its verdict."""
    _install(isolated / "project", "my-security-skill")
    line = _resolve().line()
    assert "[invocation:" in line
    assert "asserts nothing about its verdict" in line


def test_the_skill_id_comes_from_the_invocation_not_the_registry_key(
    isolated: Path,
):
    """codex P2: nothing constrains the key to equal the skill name, and
    `invocation` is documented as the exact command. Keying the probe on the key
    refuses a session over a skill that IS installed."""
    _install(isolated / "project", "my-security-skill")
    registry = {"security-review": SKILL}  # key != invocation
    v = resolve_reviewers(["security-review"], _claude(), registry)[0]
    assert v.status == "satisfiable", v.reason


def test_an_argument_bearing_invocation_resolves_its_first_token(isolated: Path):
    _install(isolated / "project", "my-security-skill")
    d = ReviewerDescriptor(
        kind="harness-skill",
        requires="skill",
        invocation="/my-security-skill --strict",
        asserts="invocation",
    )
    v = resolve_reviewers(["sec"], _claude(), {"sec": d})[0]
    assert v.status == "satisfiable", v.reason


def test_a_plugin_qualified_invocation_under_a_plain_key_is_unverifiable(
    isolated: Path,
):
    """The qualified form lives in the invocation, so a plain key must not make
    it look locally resolvable."""
    _install(isolated / "project", "my-security-skill")
    d = ReviewerDescriptor(
        kind="harness-skill",
        requires="skill",
        invocation="/fno:review",
        asserts="invocation",
    )
    v = resolve_reviewers(["myreview"], _claude(), {"myreview": d})[0]
    assert v.status == "unverifiable"
    assert not v.blocks_autonomy


def test_a_blank_invocation_falls_back_to_the_key(isolated: Path):
    """A malformed descriptor degrades to the old behavior rather than
    probing an empty skill name."""
    _install(isolated / "project", "fallback-skill")
    d = ReviewerDescriptor(
        kind="harness-skill", requires="skill", invocation="  ", asserts="invocation"
    )
    v = resolve_reviewers(["fallback-skill"], _claude(), {"fallback-skill": d})[0]
    assert v.status == "satisfiable", v.reason


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



# --- the 422 blind spot, and why the obvious fix is wrong (x-4a60) -----------


def test_a_login_with_no_account_stays_unverifiable_not_absent(
    monkeypatch: pytest.MonkeyPatch,
):
    """A configured entry is matched by the completion gate with
    `author_login.contains(configured)`, so a short alias like "reviewer" for
    `acme-reviewer[bot]` is a supported config that owns no account of its own.
    The search API 422s on it and both exact user lookups 404, so inferring
    absence from those would refuse a bot that reviews this repo normally.

    A false refusal blocks every run; the late wedge this gate shortens only
    costs one. So the 422 stays unverifiable until something can prove absence
    under substring semantics.
    """
    import subprocess as _sp

    import fno.review_capability as rc

    calls = []

    class _R:
        def __init__(self, returncode, stdout="", stderr=""):
            self.returncode, self.stdout, self.stderr = returncode, stdout, stderr

    def _run(args, **kwargs):
        joined = " ".join(args)
        calls.append(joined)
        if "nameWithOwner" in joined:
            return _R(0, "owner/repo\n")
        if "search/issues" in joined:
            return _R(1, "", "gh: Validation Failed (HTTP 422)")
        return _R(1, "", "gh: Not Found (HTTP 404)")

    monkeypatch.setattr(_sp, "run", _run)

    assert rc._app_ever_acted("reviewer") is None
    # And it must not have gone looking for an exact account to decide that.
    assert not any("users/" in c for c in calls)


def test_self_review_invocation_names_the_harness_verb():
    import fno.review_capability as rc

    # AC5-UI: each harness is told its own verb. Codex is bare (prose after it
    # flips codex to a no-merge-base target); claude carries its arg grammar.
    assert rc.self_review_invocation("codex") == "/review"
    assert rc.self_review_invocation("claude") == "/code-review medium --comment --fix"
    # Unknown harness falls back to the claude default, not a guess.
    assert rc.self_review_invocation(None) == "/code-review medium --comment --fix"
    assert rc.self_review_invocation("unknown") == "/code-review medium --comment --fix"
    # Codex stays bare even though claude carries args - no prose suffix leaks.
    assert " " not in rc.self_review_invocation("codex")
