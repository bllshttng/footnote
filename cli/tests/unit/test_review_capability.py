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
    v = resolve_reviewers(["code-review"], _claude(), REGISTRY)[0]
    assert v.status == "satisfiable"
    assert "/fno:review" in v.reason


def test_retired_sigma_refuses_naming_the_default_lane(isolated: Path):
    """The retired token is a refusal with a replacement in it, never an alias:
    an exit code alone does not tell a wedged config what to run instead."""
    v = resolve_reviewers(["sigma"], _claude(), REGISTRY)[0]
    assert v.status == "unavailable"
    assert v.blocks_autonomy
    assert "retired" in v.reason
    assert "/fno:review" in v.reason


def test_a_registry_entry_cannot_shadow_a_builtin_at_resolution(isolated: Path):
    """Built-ins win, so a project cannot downgrade `code-review` to a witnessed gate."""
    v = resolve_reviewers(["code-review"], _claude(), {"code-review": SKILL})[0]
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


def test_self_review_invocation_docstring_answers_two_questions_not_one():
    """The old docstring claimed the harness parameter was deliberately unused;
    the spelling change makes that false, and the stale claim would send the
    next reader re-deriving which reviewer owns the transport."""
    import inspect

    import fno.review_capability as rc

    doc = inspect.getdoc(rc.self_review_invocation) or ""
    assert "deliberately unused" not in doc
    assert "normalize_command" in doc


def test_self_review_invocation_is_the_lane_on_every_harness():
    import fno.review_capability as rc

    # The owned lane runs wherever the plugin runs, so no harness is floored
    # off the pre-ship review anymore.
    for harness in ("claude", "codex", "opencode", "agy", "gemini", None, "unknown"):
        assert rc.harness_can_self_review(harness) is True, harness
    # One recommendation for every harness: the fno lane with a level. WHICH
    # reviewer is harness-independent; only the SPELLING differs per surface.
    # agy has no plugin namespace, so its slash palette takes the verb bare -
    # the same lane, its own spelling.
    for harness in ("claude", "opencode", "gemini", None, "unknown"):
        assert rc.self_review_invocation(harness) == "/fno:review medium --comment", harness
    assert rc.self_review_invocation("agy") == "/review medium --comment"
    assert rc.self_review_invocation("codex") == "$fno:review medium --comment"
    # No native verb leaks into the recommendation from any harness.
    assert "/code-review" not in rc.self_review_invocation("claude")


def test_self_review_invocation_asks_for_comments_but_never_fixes():
    """The two flags are not symmetric, and the asymmetry is the whole point.

    `--fix` writes to the tree, moves HEAD, and voids the head-pinned
    attestation the round just earned, so machinery must never issue it.
    `--comment` writes to the PR and moves no commit, so it cannot void
    anything - and without it a machinery-issued review leaves its findings in
    a transcript nobody reads later, which is the silence x-c446 exists to end.
    """
    import fno.review_capability as rc

    for level in (None, "low", "medium", "high", "xhigh", "max"):
        rendered = rc.self_review_invocation("claude", level=level)
        assert "--comment" in rendered, level
        assert "--fix" not in rendered, level
        # The router grammar is [level] [--comment] [--fix] [target], and the
        # renderer appends the target AFTER this string. A flag that landed
        # past the target would be read as part of the target.
        assert rendered.endswith("--comment"), rendered
        head, _, tail = rendered.partition(" ")
        assert head == "/fno:review", rendered
        assert tail.split()[0] == (level or "<level>"), rendered


def test_render_self_review_invocation_sizes_from_the_diff_never_the_default(monkeypatch):
    """The renderer is the surface refusal sites embed: its level must come
    from diff_review_level (the level_for_diff sizing path), never the
    builder's medium default, and a dead sizing read keeps the placeholder
    instead of fabricating a level."""
    import fno.review_capability as rc

    sized = rc.level_for_diff(30, 3000)
    monkeypatch.setattr(rc, "diff_review_level", lambda root: sized)
    rendered = rc.render_self_review_invocation("claude", project_root=None)
    assert rendered == f"/fno:review {sized} --comment"
    assert "<level>" not in rendered

    monkeypatch.setattr(rc, "diff_review_level", lambda root: None)
    assert rc.render_self_review_invocation("claude") == "/fno:review <level> --comment"

    # Harness-less invocation resolves the ambient session; sizing still rides
    # the same path. A dead render never raises - it degrades to the placeholder.
    monkeypatch.setattr(rc, "diff_review_level", lambda root: sized)
    assert "<level>" not in rc.render_self_review_invocation()


def test_render_self_review_invocation_names_the_final_pr_head_and_base():
    import fno.review_capability as rc

    codex = rc.render_self_review_invocation(
        "codex",
        project_root=None,
        pr_number=123,
        head_sha="abc1234",
        base_branch="main",
    )
    assert codex == "$fno:review <level> --comment 123 HEAD abc1234 against origin/main"

    claude = rc.render_self_review_invocation(
        "claude",
        project_root=None,
        pr_number=123,
        head_sha="abc1234",
        base_branch="main",
    )
    assert claude == (
        "/fno:review <level> --comment 123 HEAD abc1234 against origin/main"
    )


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

    assert rc.self_review_invocation("claude", level="high") == "/fno:review high --comment"
    # No diff in hand yet: the placeholder survives for a pre-diff surface.
    assert rc.self_review_invocation("claude", level=None) == "/fno:review <level> --comment"
    # The level travels on every harness alike.
    assert rc.self_review_invocation("codex", level="high") == "$fno:review high --comment"
    assert rc.self_review_invocation("agy", level="low") == "/review low --comment"


def test_satisfiable_verdict_names_the_lane():
    s = SessionCapability(harness="claude", substrate="pane", attended=True)
    v = resolve_reviewers(["code-review"], s)[0]
    assert v.status == "satisfiable"
    assert "run `/fno:review`" in v.reason
    codex = SessionCapability(harness="codex", substrate="pane", attended=True)
    cv = resolve_reviewers(["code-review"], codex)[0]
    assert cv.status == "satisfiable"
    assert "run `/fno:review`" in cv.reason


def test_code_review_resolves_on_every_harness():
    """The owned lane runs wherever the plugin runs, so the reviewer it
    satisfies resolves on every harness - including the ones the native-verb
    allowlist used to refuse (gemini) or leave unverifiable (unknown). A
    harness running fno at all has the skill surface the lane needs."""
    def on(harness: str):
        s = SessionCapability(harness=harness, substrate="pane", attended=True)
        return resolve_reviewers(["code-review"], s)[0]

    for harness in ("claude", "codex", "opencode", "agy", "gemini", "unknown"):
        v = on(harness)
        assert v.status == "satisfiable", f"{harness}: {v.reason}"
        assert "run `/fno:review`" in v.reason


def test_an_unknown_harness_with_a_skill_reviewer_stays_unverifiable(isolated: Path):
    """AC3-ERR: what the probe cannot answer proceeds with a note, never a
    refusal. The unknown-harness policy survives on the requirements that
    genuinely need a harness answer (a registered skill), while the lane needs
    none."""
    _install(isolated / "project", "my-security-skill")
    session = SessionCapability(harness="unknown", substrate="interactive", attended=True)
    v = _resolve(session=session)
    assert v.status == "unverifiable"
    assert not v.blocks_autonomy


def test_review_invocation_verb_prints_the_render(monkeypatch, tmp_path):
    """'fno do target review-invocation' is the bridge the refusal sites call:
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
    assert out.output.strip() == f"/fno:review {sized} --comment"

    bare = CliRunner().invoke(target_app, ["review-invocation", "--harness", "codex"])
    assert bare.exit_code == 0, bare.output
    assert bare.output.strip() == f"$fno:review {sized} --comment"

    portable = CliRunner().invoke(target_app, ["review-invocation", "--harness", "agy"])
    assert portable.exit_code == 0, portable.output
    assert portable.output.strip() == f"/review {sized} --comment"
