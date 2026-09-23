"""Tests for ``fno agents resume`` (resume_logic).

Covers:
- AC2-HP: codex resume builds the right argv + cwd.
- AC2-ERR: missing cwd → exit 13 with fno-agents-rm suggestion.
- AC2-UI: --print-command emits single-line shell snippet, no banner.
- AC2-EDGE: Claude print commands keep the short-id attach form; Python execution refuses Claude rows.
- AC2-FR: missing session_id → exit 13.
- Provider CLI not on PATH → exit 14.
- agent_resumed event emitted BEFORE execvp.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

import pytest

@dataclass
class _FakeAgentEntry:
    name: str
    harness: str
    cwd: str
    log_path: str = "/tmp/log.jsonl"
    short_id: Optional[str] = None
    harness_session_id: Optional[str] = None
    substrate: Optional[str] = None

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
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv[0] == "codex"
    # The subcommand and the full session id come from the capability
    # contract, in that order. Nothing trails the positional: codex's globals
    # (the -c grant, --cd) all sit before the subcommand.
    assert "resume" in res.exec_argv
    sid = "00000000-1111-2222-3333-444444444444"
    assert res.exec_argv.index("resume") < res.exec_argv.index(sid)
    # Assert the tail too, not just the order: "resume", the id, then the
    # daemon-attach flag the resume_strategy form always appends.
    assert res.exec_argv[-4:] == ["resume", sid, "--remote", "unix://"]
    # The -c grant is global, so it must still precede the subcommand.
    assert any("writable_roots=" in arg for arg in res.exec_argv)
    grant_at = next(
        i for i, a in enumerate(res.exec_argv) if "writable_roots=" in a
    )
    assert grant_at < res.exec_argv.index("resume")
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
    # The grant is global, so it precedes the subcommand. So does --cd, which
    # is why this checks order rather than a fixed index: pinning argv[3] made
    # the test fail on a second global that was correctly placed.
    assert argv.index("-c") < argv.index("resume")
    assert argv.index("--cd") < argv.index("resume")
    assert argv[-4:] == [
        "resume", "00000000-1111-2222-3333-444444444444", "--remote", "unix://",
    ]

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
        emit_event=lambda kind, **_kw: order.append(f"emit:{kind}"),
        execvp=lambda file, args: order.append(f"exec:{file}"),
    )
    assert order == ["emit:agent_resumed", "exec:codex"]

# ---------------------------------------------------------------------------
# AC2-ERR — missing cwd → exit 13 with the handle-cost remedy
# ---------------------------------------------------------------------------

def test_missing_cwd_exits_13_with_handle_remedy() -> None:
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
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "no recorded cwd" in res.stderr
    assert "the row is the resume handle" in res.stderr
    assert "fno agents adopt <id>" in res.stderr

def test_claude_resume_refuses_a_stale_cwd_before_runtime_refusal() -> None:
    """A deleted worktree gets the existing actionable cwd refusal first."""
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude", cwd="/gone", short_id="deadbeef",
    )

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda c: False,
    )
    assert res.exit_code == 13
    assert "no longer reachable" in res.stderr
    assert "the row is the resume handle" in res.stderr
    assert "fno agents adopt <id>" in res.stderr

def test_store_resume_threads_cross_project_and_replacement_cwd(tmp_path, monkeypatch):
    """A pruned store session must resolve and launch in the explicit checkout."""
    from types import SimpleNamespace

    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="adopted",
        harness="codex",
        cwd="/deleted/worktree",
        harness_session_id="ab12cdef-0000-0000-0000-000000000004",
    )
    seen: dict[str, object] = {}

    def resolve(_entries, token, **kwargs):
        seen.update(token=token, **kwargs)
        return SimpleNamespace(entry=entry)

    monkeypatch.setattr("fno.agents.registry.resolve_agent_across_sources", resolve)
    replacement = str(tmp_path)
    res = resume_logic(
        name=entry.harness_session_id,
        cross_project=True,
        cwd_override=replacement,
        print_command=True,
        registry_loader=lambda: [],
        path_checker=_allow_all_path,
        cwd_checker=lambda cwd: cwd == replacement,
        execvp=_no_exec,
    )

    assert res.exit_code == 0
    assert res.exec_cwd == replacement
    assert seen == {
        "token": entry.harness_session_id,
        "scope_cwd": replacement,
        "cross_project": True,
    }

def test_store_resume_missing_replacement_cwd_fails_before_claim_or_launch(
    tmp_path, monkeypatch
):
    from types import SimpleNamespace

    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="adopted",
        harness="codex",
        cwd="/deleted/worktree",
        harness_session_id="ab12cdef-0000-0000-0000-000000000005",
    )
    monkeypatch.setattr(
        "fno.agents.registry.resolve_agent_across_sources",
        lambda *_args, **_kwargs: SimpleNamespace(entry=entry),
    )
    calls: list[str] = []

    res = resume_logic(
        name=entry.harness_session_id,
        cross_project=True,
        cwd_override=str(tmp_path / "missing-checkout"),
        registry_loader=lambda: [],
        path_checker=_allow_all_path,
        cwd_checker=lambda _cwd: False,
        execvp=lambda *_args: calls.append("exec"),
    )

    assert res.exit_code == 13
    assert "no longer reachable" in res.stderr
    assert calls == []

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
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.output.count("\n") == 1, "output should be a single line + final newline"
    # cd into the quoted cwd; then exec the provider command.
    assert "cd " in res.output
    assert "exec codex " in res.output
    assert " resume sess-abc" in res.output
    assert "writable_roots=" in res.output
    # The space-containing path must be quoted.
    assert "'/path/with space'" in res.output
    # No banner / no leading prose.
    assert not res.output.startswith("resume:")
    assert not res.output.startswith("$")

def test_claude_print_command_uses_short_id_attach_form() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="claude-worker",
        harness="claude",
        cwd="/path/to/workdir",
        short_id="deadbeef",
        harness_session_id="00000000-1111-2222-3333-444444444444",
    )
    res = resume_logic(
        name="claude-worker",
        print_command=True,
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv == ["claude", "attach", "deadbeef"]
    assert "00000000-1111-2222-3333-444444444444" not in res.output

# ---------------------------------------------------------------------------
# claude path wakes headlessly and verifies the state moved
# ---------------------------------------------------------------------------

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
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "FNO_AGENTS_RUNTIME=python" in res.stderr

def test_claude_python_runtime_refuses_without_launching_legacy_wake(monkeypatch) -> None:
    import subprocess

    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha",
        harness="claude",
        cwd="/cwd",
        short_id="deadbeef",
        harness_session_id="00000000-1111-2222-3333-444444444444",
    )
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setattr(
        "fno.agents.harnesses.claude.claude_agents_json",
        lambda: ({"deadbeef": {"live_status": "Working"}}, []),
    )
    monkeypatch.setattr(subprocess, "run", lambda *_a, **_kw: pytest.fail("spawned subprocess"))
    monkeypatch.setattr(subprocess, "Popen", lambda *_a, **_kw: pytest.fail("spawned subprocess"))

    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _cwd: True,
        execvp=_no_exec,
    )

    assert res.exit_code == 13
    assert "fno-agents runtime" in res.stderr
    assert "FNO_AGENTS_RUNTIME=python" in res.stderr
    assert "fno doctor update" in res.stderr

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
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv == [
        "opencode", "--session", "ses_09679f284ffeJv7NdBAoLQLnLZ",
    ]
    assert "run" not in res.exec_argv

def test_opencode_serve_thread_resume_routes_through_fno_ask() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="oc-thread",
        harness="opencode",
        cwd="/cwd",
        harness_session_id="ses_09679f284ffeJv7NdBAoLQLnLZ",
        substrate="thread",
    )
    res = resume_logic(
        name="oc-thread",
        message="continue the work",
        print_command=True,
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        cwd_checker=lambda _c: True,
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv[1:4] == ["ask", "oc-thread", "continue the work"]
    assert res.exec_argv[-2:] == ["--cwd", "/cwd"]

def test_resume_argv_delegates_identity_to_capability_contract(monkeypatch) -> None:
    from fno.agents import harness_map
    from fno.agents.resume_cli import _build_resume_argv

    calls = []

    def render(harness, lane, session_id):
        calls.append((harness, lane, session_id))
        return [harness, "contract-resume", session_id]

    monkeypatch.setattr(harness_map, "render_session_argv", render)
    assert _build_resume_argv("opencode", "ses_1") == [
        "opencode", "contract-resume", "ses_1"
    ]
    assert calls == [("opencode", "interactive_resume", "ses_1")]

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
        execvp=_no_exec,
    )
    # shlex.quote will single-quote any string containing shell-special
    # chars, including `~` and `#`. The exact form is "'/tmp/~tilde-suffix'".
    assert "'/tmp/~tilde-suffix'" in res.output

def test_stale_cwd_exits_13_with_handle_remedy() -> None:
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
    assert "the row is the resume handle" in res.stderr
    assert "fno agents adopt <id>" in res.stderr
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
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        execvp=None,
    )
    assert res.exit_code == 13
    assert "the row is the resume handle" in res.stderr
    assert "fno agents adopt <id>" in res.stderr
    assert events_seen == []

# ---------------------------------------------------------------------------
# code-review high --comment --fix findings on the claude wake path
# ---------------------------------------------------------------------------

def test_codex_resume_argv_places_the_worktree_and_forces_no_bypass() -> None:
    """A codex resume must land in the row's own tree.

    Codex asks session-directory vs current-directory and defaults to the
    SESSION directory, the canonical checkout recorded at spawn. That prompt
    was answered by hand during the 2026-08-25 fleet recovery. Unattended it
    is a hang, and answered wrong it is the wrong tree, which looks like
    success.

    WHAT THIS TEST DOES NOT PROVE. It asserts argv shape. It does not observe
    the modal being suppressed, and no test here does, so do not read a green
    run as proof the hang is gone. Suppression rests on two things instead.
    Codex's own reference documents the flag as `--cd, -C` and states that an
    explicit override takes precedence over the `tui.resume_cwd` config, with
    the prompt raised only when the process cwd differs from the session's
    saved directory. And on codex-cli 0.149.1 the flag was probed in both
    positions: a nonexistent directory fails with `No such file or directory`
    BEFORE the terminal check, which is a positive marker that the value is
    read rather than parsed and dropped.

    Neither is the behavior itself. Closing that needs a recorded session
    resumed against a real tty, which no unit test can host.
    """
    from fno.agents.resume_cli import _build_resume_argv

    argv = _build_resume_argv("codex", "01a03f51-4704-7f33-942a-e4e773d81cfd",
                              cwd="/tmp/wt/x-04b0")
    assert argv is not None
    # The row's own tree, not the session directory codex would otherwise pick.
    assert "--cd" in argv
    assert argv[argv.index("--cd") + 1] == "/tmp/wt/x-04b0"
    # A global belongs before the subcommand, where the -c grant already sits.
    assert argv.index("--cd") < argv.index("resume")
    # No permission bypass: the row records no sandbox posture, so this lane
    # cannot tell a bounded worker from a yolo one.
    assert "--dangerously-bypass-approvals-and-sandbox" not in argv
    assert "--dangerously-bypass-hook-trust" not in argv
    # Identity still comes from the contract.
    assert argv[0] == "codex"
    assert "01a03f51-4704-7f33-942a-e4e773d81cfd" in argv
    assert argv.index("resume") < argv.index("01a03f51-4704-7f33-942a-e4e773d81cfd")

def test_codex_resume_argv_omits_cd_when_no_cwd_is_known() -> None:
    """No cwd means no --cd: a bare flag would fail parsing, and inventing a
    directory is the wrong-tree failure this lane exists to prevent."""
    from fno.agents.resume_cli import _build_resume_argv

    argv = _build_resume_argv("codex", "sid-1")
    assert argv is not None
    assert "--cd" not in argv
    # With no cwd there is no grant either, so the identity render stands
    # alone with the daemon-attach flag the resume form always appends.
    assert argv == ["codex", "resume", "sid-1", "--remote", "unix://"]

def test_non_codex_resume_argv_is_untouched_by_the_codex_modal_flags() -> None:
    """The additions are codex-specific; no other harness accepts them."""
    from fno.agents.resume_cli import _build_resume_argv

    argv = _build_resume_argv("opencode", "ses_1", cwd="/tmp/wt/x")
    assert argv == ["opencode", "--session", "ses_1"]

# --- x-d285: the account axis rides the wake (task 2.1) ----------------------
