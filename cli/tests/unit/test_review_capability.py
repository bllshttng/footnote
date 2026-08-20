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

    assert rc.harness_can_self_review("claude") is True
    assert rc.harness_can_self_review("codex") is True
    assert rc.harness_can_self_review("opencode") is True
    assert rc.harness_can_self_review("gemini") is False
    assert rc.harness_can_self_review("agy") is False
    assert rc.harness_can_self_review(None) is False
    # AC5-UI: each harness is told its own verb. Codex and opencode are bare
    # (prose after the codex verb flips it to a no-merge-base target; opencode's
    # grammar is unverified); claude carries its arg grammar.
    assert rc.self_review_invocation("codex") == "/review"
    assert rc.self_review_invocation("opencode") == "/review-changes"
    assert rc.self_review_invocation("claude") == "/code-review medium --comment"
    # An unknown harness gets the portable fno review, NEVER claude's verb
    # silently - a wrong answer where no answer was available.
    assert rc.self_review_invocation("agy") == "/fno:review"
    assert rc.self_review_invocation(None) == "/fno:review"
    assert rc.self_review_invocation("unknown") == "/fno:review"
    # Codex stays bare even though claude carries args - no prose suffix leaks.
    assert " " not in rc.self_review_invocation("codex")
    assert " " not in rc.self_review_invocation("opencode")


def test_render_self_review_invocation_sizes_from_the_diff_never_the_default(monkeypatch):
    """The renderer is the surface refusal sites embed: its level must come
    from diff_review_level (the level_for_diff sizing path), never the
    builder's medium default, and a dead sizing read keeps the placeholder
    instead of fabricating a level."""
    import fno.review_capability as rc

    sized = rc.level_for_diff(30, 3000)
    monkeypatch.setattr(rc, "diff_review_level", lambda root: sized)
    rendered = rc.render_self_review_invocation("claude", project_root=None)
    assert rendered == f"/code-review {sized} --comment"
    assert "<level>" not in rendered

    monkeypatch.setattr(rc, "diff_review_level", lambda root: None)
    assert rc.render_self_review_invocation("claude") == "/code-review <level> --comment"

    # Harness-less invocation resolves the ambient session; sizing still rides
    # the same path. A dead render never raises - it degrades to the placeholder.
    monkeypatch.setattr(rc, "diff_review_level", lambda root: sized)
    assert "<level>" not in rc.render_self_review_invocation()


def test_level_for_diff_sizes_from_both_dimensions():
    """Either dimension alone pushes the tier up: a tier holds only when both
    the file count and the line count sit inside its caps."""
    import fno.review_capability as rc

    assert rc.level_for_diff(1, 40) == "low"
    assert rc.level_for_diff(3, 150) == "low"
    assert rc.level_for_diff(4, 40) == "medium"
    assert rc.level_for_diff(1, 151) == "medium"
    assert rc.level_for_diff(10, 600) == "medium"
    assert rc.level_for_diff(11, 10) == "high"
    assert rc.level_for_diff(1, 601) == "high"
    assert rc.level_for_diff(25, 2500) == "high"
    assert rc.level_for_diff(26, 5) == "xhigh"
    assert rc.level_for_diff(1, 2501) == "xhigh"
    assert rc.level_for_diff(60, 8000) == "xhigh"
    assert rc.level_for_diff(61, 3) == "max"
    assert rc.level_for_diff(1, 8001) == "max"
    for files, lines in ((0, 0), (500, 1000000)):
        assert rc.level_for_diff(files, lines) in rc.ALLOWED_REVIEW_LEVELS


def test_ultra_is_structurally_unreachable():
    import fno.review_capability as rc

    assert "ultra" not in rc.ALLOWED_REVIEW_LEVELS
    with pytest.raises(ValueError):
        rc.self_review_invocation("claude", level="ultra")
    for files, lines in ((0, 0), (1000, 10**9)):
        assert rc.level_for_diff(files, lines) != "ultra"


def test_self_review_invocation_takes_the_level():
    import fno.review_capability as rc

    assert (
        rc.self_review_invocation("claude", level="high")
        == "/code-review high --comment"
    )
    # No diff in hand yet: the placeholder survives for a pre-diff surface.
    assert (
        rc.self_review_invocation("claude", level=None)
        == "/code-review <level> --comment"
    )
    # Codex never grows args, whatever level is offered.
    assert rc.self_review_invocation("codex", level="high") == "/review"


def test_satisfiable_verdict_carries_the_arg_grammar():
    s = SessionCapability(harness="claude", substrate="pane", attended=True)
    v = resolve_reviewers(["code-review"], s)[0]
    assert v.status == "satisfiable"
    assert "--comment" in v.reason
    assert "<level>" in v.reason
    codex = SessionCapability(harness="codex", substrate="pane", attended=True)
    cv = resolve_reviewers(["code-review"], codex)[0]
    assert cv.status == "satisfiable"
    assert "run `/review`" in cv.reason


def test_code_review_is_scoped_to_harnesses_with_a_verb():
    """code-review resolves per its invocations map, mirroring subagent-dispatch:
    satisfiable on every harness with a recorded verb (claude, codex, opencode
    natively; agy via the /fno:review fallback), unavailable on a known harness
    with no verb (gemini), unverifiable on unknown. Resolving it satisfiable on
    a harness with NO reachable verb would floor the stop gate onto a reviewer
    whose attestation nothing there produces, wedging the loop."""
    def on(harness: str):
        s = SessionCapability(harness=harness, substrate="pane", attended=True)
        return resolve_reviewers(["code-review"], s)[0]

    assert on("claude").status == "satisfiable"
    assert on("codex").status == "satisfiable"
    assert on("opencode").status == "satisfiable"
    assert "run `/review-changes`" in on("opencode").reason
    # agy has no native verb; its recorded fallback IS the fno review, so the
    # verdict stays satisfiable with a runnable instruction.
    assert on("agy").status == "satisfiable"
    assert "run `/fno:review`" in on("agy").reason
    v = on("gemini")
    assert v.status == "unavailable", f"gemini: {v.reason}"
    assert "scoped to" in v.reason
    assert on("unknown").status == "unverifiable"


def test_review_invocation_verb_prints_the_render(monkeypatch, tmp_path):
    """'fno target review-invocation' is the bridge the refusal sites call:
    stdout is exactly one line, the render for the requested harness, sized by
    the diff at the caller's root. Expected strings are built through the same
    functions the verb uses, so no concrete level is spelled here."""
    import fno.review_capability as rc
    from typer.testing import CliRunner

    monkeypatch.chdir(tmp_path)
    sized = rc.level_for_diff(30, 3000)
    monkeypatch.setattr(rc, "diff_review_level", lambda root: sized)

    from fno.target_cli import target_app

    out = CliRunner().invoke(target_app, ["review-invocation", "--harness", "claude"])
    assert out.exit_code == 0, out.output
    assert out.output.strip() == f"/code-review {sized} --comment"

    bare = CliRunner().invoke(target_app, ["review-invocation", "--harness", "codex"])
    assert bare.exit_code == 0, bare.output
    assert bare.output.strip() == "/review"

    portable = CliRunner().invoke(target_app, ["review-invocation", "--harness", "agy"])
    assert portable.exit_code == 0, portable.output
    assert portable.output.strip() == "/fno:review"
