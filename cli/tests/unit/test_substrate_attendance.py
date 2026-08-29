"""The spawner stamps the substrate; nothing downstream guesses it (x-be78).

`attended` used to be derived from `FNO_AGENT_SELF` presence. Every spawn
substrate injects that variable, pane included, and pane is the default, so a
context-warm pane worker with an operator watching stamped `attended: false`.
The same guess labelled every headless one-shot a `pane` in the review-gate
refusal text.

These tests pin the one rule and the four places that used to repeat it.
"""

from __future__ import annotations

from fno.harness_identity import (
    AMBIENT_IDENTITY_ENV,
    ATTENDED_SUBSTRATES,
    FNO_AGENT_SUBSTRATE,
    env_marks_unattended,
    spawned_substrate,
    stamp_child_harness_identity,
)

SPAWNED = {"FNO_AGENT_SELF": "target-x-be78"}


def test_spawned_substrate_reads_only_what_a_spawner_stamped():
    assert spawned_substrate({FNO_AGENT_SUBSTRATE: "pane"}) == "pane"
    assert spawned_substrate({FNO_AGENT_SUBSTRATE: "  thread "}) == "thread"
    # Unknown is one answer and never a substrate: an operator's own shell and a
    # worker from a launcher that stamps nothing are different worlds.
    assert spawned_substrate({}) is None
    assert spawned_substrate({FNO_AGENT_SUBSTRATE: "   "}) is None
    assert spawned_substrate(SPAWNED) is None


def test_attendance_reads_the_substrate_not_the_mesh_identity():
    assert env_marks_unattended({**SPAWNED, FNO_AGENT_SUBSTRATE: "pane"}) is False
    assert env_marks_unattended({**SPAWNED, FNO_AGENT_SUBSTRATE: "thread"}) is True
    assert env_marks_unattended({**SPAWNED, FNO_AGENT_SUBSTRATE: "headless"}) is True
    # An operator's own shell carries no mesh identity at all.
    assert env_marks_unattended({}) is False
    # A spawn path that stamps nothing keeps the pre-x-be78 answer. It fails
    # toward skipping a blocking prompt rather than hanging on one.
    assert env_marks_unattended(SPAWNED) is True
    assert ATTENDED_SUBSTRATES == frozenset({"pane"})


def test_explicit_unattended_flag_outranks_an_attended_substrate():
    pane = {**SPAWNED, FNO_AGENT_SUBSTRATE: "pane"}
    assert env_marks_unattended(pane) is False
    assert env_marks_unattended({**pane, "TARGET_UNATTENDED": "1"}) is True
    # Only the exact flag. A stray value is not a refusal.
    assert env_marks_unattended({**pane, "TARGET_UNATTENDED": "0"}) is False


def test_stamp_child_scrubs_the_parents_substrate_before_stamping():
    """A pane worker passes its whole env to any one-shot it launches. Without
    the scrub the child reports `pane` for life and reads as attended."""
    assert FNO_AGENT_SUBSTRATE in AMBIENT_IDENTITY_ENV

    inherited = {FNO_AGENT_SUBSTRATE: "pane", "FNO_AGENT_SELF": "the-pane"}
    child = stamp_child_harness_identity(
        dict(inherited), "claude", agent_self="the-one-shot", substrate="headless"
    )
    assert child[FNO_AGENT_SUBSTRATE] == "headless"

    # A launcher that does not know its substrate leaves the child unknown
    # rather than letting the parent's value ride through.
    silent = stamp_child_harness_identity(dict(inherited), "claude", agent_self="c")
    assert FNO_AGENT_SUBSTRATE not in silent


def test_pane_spawn_argv_stamps_pane_after_the_ambient_scrub():
    """env(1) applies every -u before the assignments, so the pane wrapper
    carries both and the assignment is the one that survives."""
    from fno.agents.mux_spawn import _mesh_env_wrapper

    argv = _mesh_env_wrapper(name="w1", provider="codex", role=None, argv=["codex"])
    unset = [argv[i + 1] for i, tok in enumerate(argv) if tok == "-u"]
    assert "FNO_AGENT_SUBSTRATE" in unset
    assert "FNO_AGENT_SUBSTRATE=pane" in argv


def test_every_dict_spawn_lane_names_its_substrate():
    """The three dict-env launchers stamp; a lane that stops stamping silently
    reverts its workers to `unknown`, which is the whole defect."""
    import inspect

    from fno.agents.harnesses import claude as claude_lane
    from fno.agents.harnesses import codex as codex_lane

    claude_src = inspect.getsource(claude_lane)
    assert 'substrate="headless"' in claude_src  # headless_create
    assert 'substrate="thread"' in claude_src  # bg_create
    assert 'substrate="headless"' in inspect.getsource(codex_lane)


def test_detect_session_reports_the_stamp_or_says_unknown():
    from fno.review_capability import detect_session

    claude = {"CLAUDE_CODE_SESSION_ID": "s1"}
    for substrate in ("pane", "thread", "headless"):
        s = detect_session(
            {**claude, **SPAWNED, FNO_AGENT_SUBSTRATE: substrate},
            unattended_configured=False,
        )
        assert s.substrate == substrate
        assert s.attended is (substrate in ATTENDED_SUBSTRATES)

    unstamped = detect_session({**claude, **SPAWNED}, unattended_configured=False)
    assert (unstamped.substrate, unstamped.attended) == ("unknown", False)

    operator = detect_session(claude, unattended_configured=False)
    assert (operator.substrate, operator.attended) == ("interactive", True)


def test_init_hands_the_bootstrap_the_substrate_verdict(tmp_path, monkeypatch):
    """The seam the manifest is written from: `fno do target init` folds the
    verdict into TARGET_UNATTENDED, and `init-target-state.sh` turns that into
    `attended:`. A pane must reach the writer with the variable UNSET."""
    import subprocess

    from typer.testing import CliRunner

    from fno import target_cli
    from fno.cli import app

    seen: list[dict] = []

    def fake_run(argv, **kwargs):
        # Stop before the bash writer runs: the env it would have been handed
        # is the whole assertion, and a non-zero code skips init's own
        # post-success side effects.
        seen.append(dict(kwargs.get("env") or {}))
        return subprocess.CompletedProcess(argv, 1)

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)

    def _bootstrap_env(extra: dict) -> dict:
        seen.clear()
        for var in ("TARGET_UNATTENDED", "FNO_BG", "FNO_AGENT_SELF", FNO_AGENT_SUBSTRATE):
            monkeypatch.delenv(var, raising=False)
        for key, value in extra.items():
            monkeypatch.setenv(key, value)
        result = CliRunner().invoke(
            app, ["do", "target", "init", "--input", "some-feature"]
        )
        assert seen, f"init never reached the bootstrap script: {result.output}"
        return seen[-1]

    pane = _bootstrap_env({**SPAWNED, FNO_AGENT_SUBSTRATE: "pane"})
    assert pane["TARGET_START"] == "1"
    assert "TARGET_UNATTENDED" not in pane

    thread = _bootstrap_env({**SPAWNED, FNO_AGENT_SUBSTRATE: "thread"})
    assert thread["TARGET_UNATTENDED"] == "1"

    unstamped = _bootstrap_env(SPAWNED)
    assert unstamped["TARGET_UNATTENDED"] == "1"


def test_think_presence_follows_the_same_rule(tmp_path):
    """`fno backlog idea` classifies the filing session with the same question,
    and had its own copy of the FNO_AGENT_SELF guess."""
    from fno.provenance.spawn_think import classify_presence

    claude = {"CLAUDE_CODE_SESSION_ID": "s1"}
    assert classify_presence(project_root=tmp_path, env=claude) == "attended"
    assert (
        classify_presence(
            project_root=tmp_path,
            env={**claude, **SPAWNED, FNO_AGENT_SUBSTRATE: "pane"},
        )
        == "attended"
    )
    for substrate in ("thread", "headless"):
        assert (
            classify_presence(
                project_root=tmp_path,
                env={**claude, **SPAWNED, FNO_AGENT_SUBSTRATE: substrate},
            )
            == "away"
        )
    assert classify_presence(project_root=tmp_path, env={**claude, **SPAWNED}) == "away"
