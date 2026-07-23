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

import pytest


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
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv == ["codex", "resume", "00000000-1111-2222-3333-444444444444"]
    assert res.exec_cwd == "/path/to/workdir"


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
        execvp=_no_exec,
    )
    assert res.exit_code == 13
    assert "no recorded cwd" in res.stderr
    assert "fno agents rm alpha" in res.stderr


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
# AC2-EDGE — claude path uses claude attach
# ---------------------------------------------------------------------------


def test_claude_path_uses_attach_substrate() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="claude",
        cwd="/cwd",
        short_id="deadbeef",
    )
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        execvp=_no_exec,
    )
    assert res.exit_code == 0
    assert res.exec_argv == ["claude", "attach", "deadbeef"]


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
        execvp=_no_exec,
    )
    # shlex.quote will single-quote any string containing shell-special
    # chars, including `~` and `#`. The exact form is "'/tmp/~tilde-suffix'".
    assert "'/tmp/~tilde-suffix'" in res.output


def test_stale_cwd_exits_13_with_rm_hint() -> None:
    """sigma-review H2: missing cwd at chdir-time must NOT emit success.

    Pre-fix: emit_event("agent_resumed", ...) ran BEFORE os.chdir; a stale
    cwd produced a misleading success record then crashed. Post-fix: chdir
    runs first, OSError converts to exit 13 with the fno-agents-rm hint,
    and the event is never emitted on the failure path.
    """
    from fno.agents.resume_cli import resume_logic

    entry = _FakeAgentEntry(
        name="alpha", harness="codex",
        cwd="/this/path/almost/certainly/does/not/exist/" + ("x" * 40),
        harness_session_id="sess-1",
    )
    events_seen: list[dict] = []

    # Use real os.chdir/execvp=None so chdir actually runs. Inject a
    # path_checker that allows the codex binary and an emit_event we
    # can spy on.
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=_allow_all_path,
        emit_event=lambda kind, **kw: events_seen.append({"kind": kind, **kw}),
        # NOTE: execvp=None means the real os.execvp would be called,
        # but chdir fails first so we never reach it.
        execvp=None,
    )
    assert res.exit_code == 13
    assert "fno agents rm alpha" in res.stderr
    # Critically: no agent_resumed event was emitted on the failure path.
    assert events_seen == []
