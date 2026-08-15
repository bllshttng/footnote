"""Tests for ``fno agents resume`` (resume_logic).

Task 3.4 from 2026-05-22-fno-agents-observability.md.

Covers:
- AC2-HP: codex resume builds the right argv + cwd.
- AC2-ERR: missing cwd → exit 13 with fno-agents-rm suggestion.
- AC2-UI: --print-command emits single-line shell snippet, no banner.
- AC2-EDGE: claude path uses ``claude attach <short_id>`` (attach substrate).
- AC2-FR: missing session_id → exit 13.
- Provider CLI not on PATH → exit 14.
- agent_resumed event emitted BEFORE execvp.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


@dataclass
class _FakeAgentEntry:
    name: str
    harness: str
    cwd: str
    log_path: str = "/tmp/log.jsonl"
    short_id: Optional[str] = None
    harness_session_id: Optional[str] = None


def _allow_all_path(_bin: str) -> bool:
    return True


def _deny_all_path(_bin: str) -> bool:
    return False


def _no_exec(*_args, **_kwargs) -> None:
    """Test stand-in for os.execvp; just records that it would have run."""


# ---------------------------------------------------------------------------
# AC2-HP — codex resume happy path
# ---------------------------------------------------------------------------


def test_codex_resume_builds_correct_argv_and_cwd() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha",
        harness="codex",
        cwd="/path/to/workdir",
        harness_session_id="00000000-1111-2222-3333-444444444444",
    )

    events_seen: list[dict] = []
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv == ["codex", "resume", "00000000-1111-2222-3333-444444444444"]
    assert res.exec_cwd == "/path/to/workdir"


def test_codex_resume_grants_git_metadata_write_in_a_repo(tmp_path) -> None:
    """A resumed codex in a linked worktree must still be able to commit.

    Its git metadata lives at <repo>/.git/worktrees/<name>/, outside the
    workspace a bounded sandbox makes writable, and `codex resume` takes no
    --add-dir - so the grant rides the global -c, ahead of the subcommand.
    """
    import json
    import pathlib
    import subprocess

    from fno.agents.resume_cli import resume_logic

    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(
        ["git", "-c", "user.email=t@t.t", "-c", "user.name=t",
         "commit", "-q", "--allow-empty", "-m", "base"],
        cwd=repo, check=True,
    )
    wt = tmp_path / "wt"
    subprocess.run(
        ["git", "worktree", "add", "-q", str(wt), "-b", "feat"], cwd=repo, check=True
    )

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [
            _FakeAgentEntry(
                name="alpha", harness="codex", cwd=str(wt),
                harness_session_id="00000000-1111-2222-3333-444444444444",
            )
        ],
        path_checker=_allow_all_path,
        emit_event=lambda kind, **kw: None,
        execvp=_no_exec,
    )

    argv = res.exec_argv
    assert argv[0] == "codex"
    assert argv[1] == "-c"
    key, _, value = argv[2].partition("=")
    assert key == "sandbox_workspace_write.writable_roots"
    assert pathlib.Path(json.loads(value)[0]).resolve() == (repo / ".git").resolve()
    # The grant is global, so it precedes the subcommand.
    assert argv[3] == "resume"


def test_agent_resumed_event_emitted_before_execvp() -> None:
    """agent_resumed must be emitted before execvp (execvp won't run our code)."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/path/x", harness_session_id="sess-1",
    )
    order: list[str] = []
    resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        emit_event=lambda kind, **_kw: order.append(f"emit:{kind}"),
        execvp=lambda file, args: order.append(f"exec:{file}"),
    )
    assert order == ["emit:agent_resumed", "exec:codex"]


# ---------------------------------------------------------------------------
# AC2-ERR — missing cwd → exit 13 with cleanup hint
# ---------------------------------------------------------------------------


def test_missing_cwd_exits_13_with_rm_hint() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="",  # explicit empty
        harness_session_id="sess-1",
    )
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "no recorded cwd" in res.stderr
    assert "fno agents rm alpha" in res.stderr


def test_claude_resume_refuses_a_stale_cwd_without_waking() -> None:
    """code-review finding: the claude branch skipped the cwd-reachability
    check every other harness gets, so a deleted worktree burned a full
    ~19s wake attempt before surfacing a confusing exit-16 instead of the
    immediate, actionable exit-13 rm hint."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/gone", short_id="deadbeef",
    )

    def _must_not_wake(*a, **kw):
        raise AssertionError("must not wake a row with an unreachable cwd")

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda c: False,
        wake_fn=_must_not_wake,
        agents_state_fn=lambda: (_ for _ in ()).throw(AssertionError("must not verify")),
    )
    assert res.exit_code == 13
    assert "no longer reachable" in res.stderr
    assert "fno agents rm alpha" in res.stderr


def test_resume_cwd_override_wins_over_the_registrys_recorded_cwd() -> None:
    """The Rust binary resolves a claude row's EnterWorktree-moved transcript
    dir before delegating here (`resolve_resume_cwd`); --cwd carries that
    resolved value through so this fallback doesn't re-derive the stale
    pre-EnterWorktree cwd from the registry entry itself."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/stale/registry/cwd", short_id="deadbeef",
    )
    seen_cwd: list[str] = []

    def _wake(short_id, *, message, route_env, cwd):
        seen_cwd.append(cwd)

    states = iter(["Needs input", "Working"])
    res = resume_logic(
        name="alpha",
        cwd_override="/resolved/worktree/cwd",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda c: True,
        claim_fn=lambda _s: None,
        wake_fn=_wake,
        agents_state_fn=lambda: {"deadbeef": {"live_status": next(states)}},
    )
    assert res.exit_code == 0
    assert seen_cwd == ["/resolved/worktree/cwd"]


def test_default_acquire_resume_attach_claim_refuses_a_second_writer(tmp_path) -> None:
    """code-review finding: the claim guard on the Rust delegation had no
    counterpart on this module's own standalone entrypoint
    (FNO_AGENTS_RUNTIME=python, or no Rust binary installed at all) -- a
    guard on only one of two reachable paths into the same wake. Exercises
    the real (non-injected) claim function end to end, keyed identically to
    Rust's own resume-attach:{short_id} claim."""
    from fno.claims.core import acquire_claim
    from fno.agents.resume_cli import _default_acquire_resume_attach_claim

    short_id = "deadbeef"
    acquire_claim(
        f"resume-attach:{short_id}", "other-writer", root=tmp_path
    )

    err = _default_acquire_resume_attach_claim(short_id, root=tmp_path)
    assert err is not None
    assert "held live by another writer" in err
    assert "other-writer" in err

    # A different short_id: an unrelated row's wake is never blocked.
    assert _default_acquire_resume_attach_claim("other-id", root=tmp_path) is None


def test_claude_resume_refuses_when_claim_held_by_another_writer() -> None:
    """Full resume_logic path: a claim_fn conflict must surface as the
    resume's own exit code/stderr rather than proceeding to wake."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: "fno agents resume: session deadbeef is held live by another writer",
        wake_fn=lambda *a, **kw: (_ for _ in ()).throw(AssertionError("must not wake")),
        agents_state_fn=lambda: (_ for _ in ()).throw(AssertionError("must not verify")),
    )
    assert res.exit_code == 11
    assert "held live by another writer" in res.stderr


# ---------------------------------------------------------------------------
# AC2-UI — --print-command emits a clean one-liner
# ---------------------------------------------------------------------------


def test_print_command_emits_one_liner() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/path/with space",
        harness_session_id="sess-abc",
    )
    res = resume_logic(
        name="alpha",
        print_command=True,
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.output.count("\n") == 1, "output should be a single line + final newline"
    # cd into the quoted cwd; then exec the provider command.
    assert "cd " in res.output
    assert "exec codex resume sess-abc" in res.output
    # The space-containing path must be quoted.
    assert "'/path/with space'" in res.output
    # No banner / no leading prose.
    assert not res.output.startswith("resume:")
    assert not res.output.startswith("$")


# ---------------------------------------------------------------------------
# claude path wakes headlessly and verifies the state moved
# ---------------------------------------------------------------------------


def test_claude_resume_wakes_and_verifies_working() -> None:
    """A claude row with a live short_id is woken, not exec'd into attach."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude",
        cwd="/cwd",
        short_id="deadbeef",
    )
    wake_calls: list[dict] = []

    def _wake(short_id, *, message, route_env, cwd):
        wake_calls.append({"short_id": short_id, "message": message, "route_env": route_env})

    states = iter(["Needs input", "Working"])

    def _state():
        current = next(states)
        return {"deadbeef": {"live_status": current}}

    res = resume_logic(
        name="alpha",
        message="continue",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=_wake,
        agents_state_fn=_state,
    )
    assert res.exit_code == 0
    assert res.output == "alpha (deadbeef): Needs input -> Working\n"
    assert len(wake_calls) == 1
    assert wake_calls[0] == {"short_id": "deadbeef", "message": "continue", "route_env": None}


def test_claude_resume_retries_once_before_giving_up() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    wake_calls: list[int] = []

    def _wake(short_id, *, message, route_env, cwd):
        wake_calls.append(1)

    # Stays "Needs input" through the pre-check and both post-attempt reads.
    def _state():
        return {"deadbeef": {"live_status": "Needs input"}}

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=_wake,
        agents_state_fn=_state,
    )
    assert res.exit_code == 16
    assert len(wake_calls) == 2, "must retry once before reporting failure"
    assert "Needs input" in res.stderr
    assert "deadbeef" in res.stderr


def test_claude_resume_emits_no_event_on_a_failed_wake() -> None:
    """Mirrors the codex/exec path's chdir-failure convention: an event named
    "agent_resumed" must never fire on a wake that did not reach Working, or
    it misreports the failure as a success to anyone reading the log."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    events_seen: list[dict] = []

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        wake_fn=lambda *a, **kw: None,
        agents_state_fn=lambda: {"deadbeef": {"live_status": "Needs input"}},
    )
    assert res.exit_code == 16
    assert events_seen == []


def test_claude_resume_emits_no_event_on_a_skipped_already_working_row() -> None:
    """A skipped row (already Working) never entered the wake loop: emitting
    "agent_resumed" for it would claim a resume happened when nothing was
    attempted, the same event-naming concern the failed-wake case above
    guards against."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    events_seen: list[dict] = []

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        wake_fn=lambda *a, **kw: None,
        agents_state_fn=lambda: {"deadbeef": {"live_status": "Working"}},
    )
    assert res.exit_code == 0
    assert events_seen == []


def test_claude_resume_passes_the_agents_cwd_to_wake_fn() -> None:
    """The wake subprocess must run from the agent's own recorded cwd,
    matching the non-claude exec path's os.chdir(cwd) and the Rust exec
    fallback's set_current_dir(cwd) -- claude attach finds the session by
    short_id, not by directory, but a wrong cwd would still leak into
    anything the attaching process reads project-locally."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/the/agents/worktree", short_id="deadbeef",
    )
    seen_cwd: list[str] = []

    def _wake(short_id, *, message, route_env, cwd):
        seen_cwd.append(cwd)

    states = iter(["Needs input", "Working"])
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=_wake,
        agents_state_fn=lambda: {"deadbeef": {"live_status": next(states)}},
    )
    assert res.exit_code == 0
    assert seen_cwd == ["/the/agents/worktree"]


def test_claude_resume_restores_routed_env() -> None:
    """A routed row's env must reach the wake attempt."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    entry.route_settings_path = "/tmp/route.json"  # type: ignore[attr-defined]
    seen_env: list[Optional[dict]] = []

    def _wake(short_id, *, message, route_env, cwd):
        seen_env.append(route_env)

    def _read_route_settings(path):
        assert path == "/tmp/route.json"
        return {"ANTHROPIC_BASE_URL": "https://api.z.ai/api/paas/v4"}

    import fno.agents.model_routing as model_routing_mod

    orig = model_routing_mod.read_route_settings
    model_routing_mod.read_route_settings = _read_route_settings
    # Starts "Needs input" so the wake loop actually runs (an already-Working
    # row is skipped by design and _wake would never be called).
    states = iter(["Needs input", "Working"])
    try:
        res = resume_logic(
            name="alpha",
            registry_loader=lambda: [entry],
            path_checker=_allow_all_path,
            cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
            execvp=_no_exec,
            emit_event=lambda *a, **kw: None,
            wake_fn=_wake,
            agents_state_fn=lambda: {"deadbeef": {"live_status": next(states)}},
        )
    finally:
        model_routing_mod.read_route_settings = orig

    assert res.exit_code == 0
    assert seen_env == [{"ANTHROPIC_BASE_URL": "https://api.z.ai/api/paas/v4"}]


def test_claude_resume_refuses_when_route_cannot_be_restored() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    entry.route_settings_path = "/tmp/gone.json"  # type: ignore[attr-defined]

    import fno.agents.model_routing as model_routing_mod

    def _raise(path):
        raise model_routing_mod.RouteRestoreError(f"{path} is unreadable")

    orig = model_routing_mod.read_route_settings
    model_routing_mod.read_route_settings = _raise
    try:
        res = resume_logic(
            name="alpha",
            registry_loader=lambda: [entry],
            path_checker=_allow_all_path,
            cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
            execvp=_no_exec,
            wake_fn=lambda *a, **kw: (_ for _ in ()).throw(AssertionError("must not wake")),
            agents_state_fn=lambda: {},
        )
    finally:
        model_routing_mod.read_route_settings = orig

    assert res.exit_code == 2
    assert "gone.json" in res.stderr


def test_claude_resume_skips_a_broken_route_on_an_already_skip_eligible_row() -> None:
    """code-review finding: route restore ran before the skip check, so an
    already-Working row with a stale route file got refused with exit 2
    instead of reporting the no-op success it actually was -- nothing was
    ever going to be woken, so the route was never going to be used."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    entry.route_settings_path = "/tmp/gone.json"  # type: ignore[attr-defined]

    import fno.agents.model_routing as model_routing_mod

    def _raise(path):
        raise model_routing_mod.RouteRestoreError(f"{path} is unreadable")

    orig = model_routing_mod.read_route_settings
    model_routing_mod.read_route_settings = _raise
    try:
        res = resume_logic(
            name="alpha",
            registry_loader=lambda: [entry],
            path_checker=_allow_all_path,
            cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
            execvp=_no_exec,
            wake_fn=lambda *a, **kw: (_ for _ in ()).throw(AssertionError("must not wake")),
            agents_state_fn=lambda: {"deadbeef": {"live_status": "Working"}},
        )
    finally:
        model_routing_mod.read_route_settings = orig

    assert res.exit_code == 0
    assert res.output == "alpha (deadbeef): Working -> Working\n"


# ---------------------------------------------------------------------------
# x-b84f - claude pane row: canonical id fallback + safe refusal
# ---------------------------------------------------------------------------


def test_session_id_for_falls_back_to_canonical_id_on_a_claude_pane_row() -> None:
    # Parity (T2.3): a claude pane row has a canonical harness_session_id but no
    # transport short_id (empty by design). Both implementations must resolve the
    # same id - the Rust loader mirrors harness_session_id -> claude_session_uuid
    # (pinned in client_verbs::tests), and the Python resolver falls back to
    # harness_session_id here. Assert the uuid itself, not mutual refusal.
    from fno.agents.resume_cli import _session_id_for

    uuid = "44012de2-1528-44dd-a32d-a81f2f0db728"
    entry = _FakeAgentEntry(
        name="pane-worker", harness="claude", cwd="/cwd",
        harness_session_id=uuid,  # no short_id: the mux-row shape
    )
    assert _session_id_for(entry) == uuid


def test_claude_pane_row_refuses_pointing_at_the_smart_runtime() -> None:
    # The Python fallback cannot restore the recorded route a happy pane worker
    # was launched on, so it refuses a claude pane row (no short_id) rather than
    # `claude attach ""` or a route-less `--resume` on the default (wrong)
    # account. The smart (default) runtime owns the relaunch+route path.
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="pane-worker", harness="claude", cwd="/cwd",
        harness_session_id="44012de2-1528-44dd-a32d-a81f2f0db728",
    )
    res = resume_logic(
        name="pane-worker",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "FNO_AGENTS_RUNTIME=python" in res.stderr


# ---------------------------------------------------------------------------
# AC2-FR — missing session_id → exit 13
# ---------------------------------------------------------------------------


def test_missing_session_id_exits_13() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/cwd",
        harness_session_id=None,  # explicit absent
    )
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "session_id" in res.stderr


# ---------------------------------------------------------------------------
# CLI not on PATH → exit 14
# ---------------------------------------------------------------------------


def test_provider_cli_not_on_path_exits_14() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/cwd",
        harness_session_id="sess-1",
    )
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_deny_all_path,
        execvp=_no_exec,
    )
    assert res.exit_code == 14
    assert "codex" in res.stderr
    assert "PATH" in res.stderr


# ---------------------------------------------------------------------------
# Unknown agent → exit 13
# ---------------------------------------------------------------------------


def test_unknown_agent_exits_13() -> None:
    from fno.agents.resume_cli import resume_logic

    res = resume_logic(
        name="ghost",
        registry_loader=lambda: [],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "ghost" in res.stderr
    # x-1b1e: the shared resolver's not-found message lists the accepted forms.
    assert "no agent matching" in res.stderr
    assert "accepted forms" in res.stderr


def test_unsupported_provider_exits_13_not_14() -> None:
    """Codex P2 round 2: unsupported provider must return exit 13.

    Pre-fix used exit 14, which collided with "CLI not on PATH" and made
    wrapper diagnostics ambiguous. Module contract reserves 14 for PATH.
    """
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="unknown_provider",
        cwd="/cwd",
    )
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "not supported" in res.stderr


def test_opencode_argv_attaches_the_tui_by_session() -> None:
    """AC2-HP: opencode resume builds `opencode --session <ses_id>`.

    Bare `opencode --session` is the interactive TUI attach; the provider's
    headless `opencode run ... --session` argv is a separate lane. Must stay
    byte-identical to the Rust build_resume_argv arm (parity test there).
    """
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="oc", harness="opencode",
        cwd="/cwd",
        harness_session_id="ses_09679f284ffeJv7NdBAoLQLnLZ",
    )
    res = resume_logic(
        name="oc",
        print_command=True,
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv == [
        "opencode", "--session", "ses_09679f284ffeJv7NdBAoLQLnLZ",
    ]
    assert "run" not in res.exec_argv


def test_opencode_without_captured_session_id_errors_clearly() -> None:
    """AC1-UI: an id-less opencode row (backfill missed) refuses, never execs.

    opencode joining HARNESS_SESSION_ID_FIELDS makes the row resolvable, so
    this is the state a live-only pane lands in until its id is captured.
    """
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="oc", harness="opencode", cwd="/cwd", harness_session_id=None,
    )
    res = resume_logic(
        name="oc",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "no recorded session_id" in res.stderr
    assert "oc" in res.stderr
    assert res.exec_argv is None


# ---------------------------------------------------------------------------
# Sigma-review fixes — regression guards
# ---------------------------------------------------------------------------


def test_print_command_uses_shlex_quote_for_special_chars() -> None:
    """sigma-review M: _shell_quote now delegates to shlex.quote.

    Pre-fix hand-roll missed `~`, `#`, `=`, newline. Post-fix shlex.quote
    handles all of these. Verify against a cwd containing ``~`` which
    bash would tilde-expand if left unquoted.
    """
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/tmp/~tilde-suffix",
        harness_session_id="sess-1",
    )
    res = resume_logic(
        name="alpha",
        print_command=True,
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
    )
    # shlex.quote will single-quote any string containing shell-special
    # chars, including `~` and `#`. The exact form is "'/tmp/~tilde-suffix'".
    assert "'/tmp/~tilde-suffix'" in res.output


def test_stale_cwd_exits_13_with_rm_hint() -> None:
    """sigma-review H2: missing cwd at chdir-time must NOT emit success.

    The real (default) cwd_checker -- os.path.isdir -- catches this stale
    path first, before the os.chdir branch below is ever reached; see
    test_stale_cwd_that_passes_isdir_but_fails_chdir_still_exits_13 for that
    branch specifically. This test still exercises the real, non-mocked cwd
    validation end to end, and no agent_resumed event fires on the
    failure path either way.
    """
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/this/path/almost/certainly/does/not/exist/" + ("x" * 40),
        harness_session_id="sess-1",
    )
    events_seen: list[dict] = []

    # Use the real cwd_checker/execvp=None so the full cwd-validation path
    # actually runs. Inject a path_checker that allows the codex binary and
    # an emit_event we can spy on.
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        execvp=None,
    )
    assert res.exit_code == 13
    assert "fno agents rm alpha" in res.stderr
    # Critically: no agent_resumed event was emitted on the failure path.
    assert events_seen == []


def test_stale_cwd_that_passes_isdir_but_fails_chdir_still_exits_13() -> None:
    """code-review finding: the cwd_checker gate added for the claude
    reachability fix now intercepts every stale-cwd test before os.chdir's
    own OSError branch runs, leaving that branch covered only by a
    production TOCTOU race rather than by CI. Force cwd_checker to pass so
    the real os.chdir call is what fails -- a directory deleted between the
    isdir check and the chdir call, not merely a directory that never
    existed."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/this/path/almost/certainly/does/not/exist/" + ("x" * 40),
        harness_session_id="sess-1",
    )
    events_seen: list[dict] = []

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        execvp=None,
    )
    assert res.exit_code == 13
    assert "fno agents rm alpha" in res.stderr
    assert events_seen == []


# ---------------------------------------------------------------------------
# code-review high --comment --fix findings on the claude wake path
# ---------------------------------------------------------------------------


def test_claude_resume_skips_waking_an_already_working_row() -> None:
    """An already-Working row must never have keystrokes injected into it."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    wake_calls: list[int] = []

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=lambda *a, **kw: wake_calls.append(1),
        agents_state_fn=lambda: {"deadbeef": {"live_status": "Working"}},
    )
    assert res.exit_code == 0
    assert wake_calls == []
    assert res.output == "alpha (deadbeef): Working -> Working\n"


def test_claude_resume_skips_waking_an_already_idle_row() -> None:
    """An Idle row is live and reachable, not blocked: injecting keystrokes
    risks destroying unsubmitted composer text for no benefit, and must not
    be scored a failure just because Idle isn't the Working wake target."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    wake_calls: list[int] = []

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=lambda *a, **kw: wake_calls.append(1),
        agents_state_fn=lambda: {"deadbeef": {"live_status": "Idle"}},
    )
    assert res.exit_code == 0
    assert wake_calls == []
    assert res.output == "alpha (deadbeef): Idle -> Idle\n"


def test_claude_resume_rechecks_state_after_a_timed_out_attempt() -> None:
    """A wake that lands but whose subprocess outlives the timeout must not
    be scored a failure: the post-attempt state read must run even when
    wake_fn raised TimeoutExpired."""
    import subprocess as subprocess_mod

    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    states = iter(["Needs input", "Working"])

    def _wake(short_id, *, message, route_env, cwd):
        raise subprocess_mod.TimeoutExpired(cmd="bash", timeout=60.0)

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=_wake,
        agents_state_fn=lambda: {"deadbeef": {"live_status": next(states)}},
    )
    assert res.exit_code == 0
    assert res.output == "alpha (deadbeef): Needs input -> Working\n"


def test_default_wake_fn_scrubs_ambient_auth_before_overlaying_the_route(
    monkeypatch,
) -> None:
    """The operator's own ANTHROPIC_API_KEY must not survive alongside a
    routed row's credential in the attaching subprocess's env."""
    import subprocess as subprocess_mod

    from fno.agents.resume_cli import _default_wake_fn

    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-operators-own-key")
    seen: dict = {}

    class _FakeProc:
        pid = 4242

        def wait(self, timeout=None):
            return 0

    def _fake_popen(argv, *, env, **kwargs):
        seen["env"] = env
        return _FakeProc()

    monkeypatch.setattr(subprocess_mod, "Popen", _fake_popen)

    _default_wake_fn(
        "deadbeef",
        message="continue",
        route_env={"ANTHROPIC_BASE_URL": "https://api.z.ai/api/paas/v4"},
        cwd="/the/agents/worktree",
    )

    assert seen["env"].get("ANTHROPIC_API_KEY") is None
    assert seen["env"]["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/paas/v4"


def test_default_wake_fn_leaves_ambient_auth_untouched_with_no_route(
    monkeypatch,
) -> None:
    """code-review finding: a route-less row (the common default-account
    case) must keep its ambient auth, matching bg_create/headless_create --
    scrubbing with nothing to restore breaks auth mid-wake for an
    api-key-authenticated account."""
    import subprocess as subprocess_mod

    from fno.agents.resume_cli import _default_wake_fn

    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-operators-own-key")
    seen: dict = {}

    class _FakeProc:
        pid = 4242

        def wait(self, timeout=None):
            return 0

    def _fake_popen(argv, *, env, **kwargs):
        seen["env"] = env
        return _FakeProc()

    monkeypatch.setattr(subprocess_mod, "Popen", _fake_popen)

    _default_wake_fn("deadbeef", message="continue", route_env=None, cwd="/the/agents/worktree")

    assert seen["env"]["ANTHROPIC_API_KEY"] == "sk-operators-own-key"


def test_claude_resume_skips_waking_an_already_done_row() -> None:
    """code-review finding: "Done" is a terminal status (KNOWN_LIVE_STATUSES
    in harnesses/claude.py), newly visible via `--all` -- it must be
    skip-eligible like Working/Idle rather than burning two ~60s wake
    attempts trying to nudge a session that was never going to move."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    wake_calls: list[int] = []

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=lambda *a, **kw: wake_calls.append(1),
        agents_state_fn=lambda: {"deadbeef": {"live_status": "Done"}},
    )
    assert res.exit_code == 0
    assert wake_calls == []
    assert res.output == "alpha (deadbeef): Done -> Done\n"


def test_claude_resume_skip_check_is_case_insensitive() -> None:
    """code-review finding: read.py's own diff fixed a lowercase-status miss
    with .lower() in this same PR; the wake's skip/target comparisons must
    apply the same normalization or an un-normalized "idle" gets keystrokes
    injected into a live session."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/cwd", short_id="deadbeef",
    )
    wake_calls: list[int] = []

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        execvp=_no_exec,
        emit_event=lambda *a, **kw: None,
        wake_fn=lambda *a, **kw: wake_calls.append(1),
        agents_state_fn=lambda: {"deadbeef": {"live_status": "idle"}},
    )
    assert res.exit_code == 0
    assert wake_calls == []


def test_script_wrapped_attach_uses_bsd_form_on_darwin(monkeypatch) -> None:
    from fno.agents.resume_cli import _script_wrapped_attach

    monkeypatch.setattr("sys.platform", "darwin")
    cmd = _script_wrapped_attach("deadbeef")
    assert cmd == "script -q /dev/null claude attach deadbeef"


def test_script_wrapped_attach_uses_gnu_form_on_linux(monkeypatch) -> None:
    from fno.agents.resume_cli import _script_wrapped_attach

    monkeypatch.setattr("sys.platform", "linux")
    cmd = _script_wrapped_attach("deadbeef")
    assert cmd == "script -qc 'claude attach deadbeef' /dev/null"


def test_script_wrapped_attach_uses_bsd_form_on_real_bsd_platform_strings(monkeypatch) -> None:
    """A real sys.platform on BSD carries a version suffix (freebsd13,
    openbsd7, ...) and never ends in the literal substring "bsd" -- a
    `.endswith("bsd")` check silently never matches on real hardware."""
    from fno.agents.resume_cli import _script_wrapped_attach

    for platform in ("freebsd13", "openbsd7", "netbsd10"):
        monkeypatch.setattr("sys.platform", platform)
        cmd = _script_wrapped_attach("deadbeef")
        assert cmd == "script -q /dev/null claude attach deadbeef", platform
