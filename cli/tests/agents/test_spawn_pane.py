"""Tests for the mux-pane spawn back half (4a-G2, task 4.5).

``fno agents spawn --substrate pane`` hosts the agent as a mux pane via
``fno mux pane run`` and writes the registry row with the ``mux`` ref. The
mux subprocess is faked at the ``runner`` seam (the G1 e2e drives the real
socket); these tests pin the Python contract:

- AC1-HP  spawn -> `pane run --session --cwd -- env <mesh> <argv>`; row
          carries mux:{session, pane_id} + claude_session_uuid + child pid.
- AC1-ERR mux failure -> no half-created row, error names the mux session,
          no daemon fallback.
- AC1-FR  a claude argv carrying -p/--print is refused BEFORE any pane run.
- AC1-EDGE is Rust-side (pane run self-spawns the server; G1 e2e covers it).
- Routing: pane-substrate spawns never auto-route to the Rust client.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional

import pytest

from fno.paths_testing import use_tmpdir


class FakeRunner:
    """Record `fno mux ...` invocations; script the replies per verb."""

    def __init__(
        self,
        run_returncode: int = 0,
        run_stdout: str = "7\n",
        run_stderr: str = "",
        ls_stdout: Optional[str] = None,
    ) -> None:
        self.calls: list[list[str]] = []
        self.run_returncode = run_returncode
        self.run_stdout = run_stdout
        self.run_stderr = run_stderr
        self.ls_stdout = ls_stdout

    def __call__(self, argv, **kwargs):
        self.calls.append(list(argv))
        if argv[1:4] == ["mux", "pane", "run"]:
            return subprocess.CompletedProcess(
                argv, self.run_returncode, self.run_stdout, self.run_stderr
            )
        if argv[1:4] == ["mux", "pane", "ls"]:
            out = self.ls_stdout
            if out is None:
                out = json.dumps(
                    [{"pane_id": 7, "squad_id": 1, "tab_id": 1, "cwd": "/w", "child_pid": 4242}]
                )
            return subprocess.CompletedProcess(argv, 0, out, "")
        raise AssertionError(f"unexpected fno invocation: {argv}")


def _spawn(monkeypatch, tmp_path, **kwargs):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("FNO_SESSION", raising=False)
    from fno.agents.mux_spawn import dispatch_spawn_pane

    runner = kwargs.pop("runner", FakeRunner())
    result = dispatch_spawn_pane(
        name=kwargs.pop("name", "peer"),
        message=kwargs.pop("message", "hello"),
        provider=kwargs.pop("provider", "claude"),
        cwd=kwargs.pop("cwd", tmp_path),
        runner=runner,
        **kwargs,
    )
    return result, runner


def test_ac1_hp_spawn_pane_runs_mux_and_writes_mux_ref_row(
    tmp_path: Path, monkeypatch
) -> None:
    result, runner = _spawn(monkeypatch, tmp_path)

    # The hosting call is the G1 script API with the resolved session + cwd.
    run_call = runner.calls[0]
    assert run_call[1:4] == ["mux", "pane", "run"]
    assert "--claim" in run_call  # agent panes opt into the writer claim
    assert run_call[run_call.index("--session") + 1] == "main"
    assert run_call[run_call.index("--cwd") + 1] == str(tmp_path)
    # Mesh identity rides the env(1) wrapper after `--`.
    tail = run_call[run_call.index("--") + 1 :]
    assert tail[0] == "env"
    assert "FNO_AGENT_SELF=peer" in tail
    assert "FNO_AGENT_PROVIDER=claude" in tail
    assert "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1" in tail
    # The provider argv is interactive claude with the pinned session id.
    claude_at = tail.index("claude")
    assert tail[claude_at + 1] == "--session-id"
    assert tail[claude_at + 2] == result.session_uuid
    assert tail[claude_at + 3] == "hello"

    assert result.pane_id == 7
    assert result.session == "main"
    assert result.child_pid == 4242

    from fno.agents.registry import load_registry

    rows = load_registry()
    assert len(rows) == 1
    row = rows[0]
    assert row.mux == {"session": "main", "pane_id": 7}
    assert row.claude_session_uuid == result.session_uuid
    assert row.pid == 4242
    assert row.status == "live"
    assert row.short_id == ""  # one live ref: mux only


def test_ac1_hp_session_resolution_env_beats_default(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_SESSION", "work")
    from fno.agents.mux_spawn import dispatch_spawn_pane

    runner = FakeRunner()
    result = dispatch_spawn_pane(
        name="peer", message="", provider="claude", cwd=tmp_path, runner=runner
    )
    assert result.session == "work"
    run_call = runner.calls[0]
    assert run_call[run_call.index("--session") + 1] == "work"
    # An explicit session beats the env.
    runner2 = FakeRunner()
    result2 = dispatch_spawn_pane(
        name="peer2",
        message="",
        provider="claude",
        cwd=tmp_path,
        session="other",
        runner=runner2,
    )
    assert result2.session == "other"


def test_ac1_err_mux_failure_leaves_no_row_and_names_session(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError) as exc_info:
        _spawn(
            monkeypatch,
            tmp_path,
            runner=FakeRunner(run_returncode=1, run_stdout="", run_stderr="no pty"),
        )
    msg = str(exc_info.value)
    assert "'main'" in msg, f"error must name the mux session: {msg}"
    assert "no pty" in msg
    assert "fallback" in msg  # explicitly no daemon-PTY fallback

    from fno.agents.registry import load_registry

    assert load_registry() == [], "a failed spawn must not leave a half-created row"


def test_collision_refused_before_any_pane_spawn(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [AgentEntry(name="peer", provider="claude", cwd="/p", log_path="/l")]
    )
    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="already exists") as exc_info:
        dispatch_spawn_pane(
            name="peer", message="", provider="claude", cwd=tmp_path, runner=runner
        )
    assert exc_info.value.exit_code == 2
    assert runner.calls == [], "collision must refuse before any mux subprocess"


def test_ac1_fr_billing_guard_refuses_print_argv_before_pane(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    import fno.agents.mux_spawn as mux_spawn

    # The builder never emits -p by construction; force it to prove the guard
    # sits between argv resolution and the pane spawn.
    monkeypatch.setattr(
        mux_spawn, "build_pane_argv", lambda *a, **k: ["claude", "-p", "hi"]
    )
    runner = FakeRunner()
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="-p/--print"):
        mux_spawn.dispatch_spawn_pane(
            name="peer", message="hi", provider="claude", cwd=tmp_path, runner=runner
        )
    assert runner.calls == [], "the guard must fire BEFORE any pane exists"

    # The predicate itself, both spellings.
    assert not mux_spawn.claude_argv_is_interactive(["claude", "-p"])
    assert not mux_spawn.claude_argv_is_interactive(["claude", "--print", "x"])
    assert mux_spawn.claude_argv_is_interactive(["claude", "--session-id", "u", "msg"])


def test_build_pane_argv_provider_forms(tmp_path: Path) -> None:
    from fno.agents.mux_spawn import build_pane_argv

    claude = build_pane_argv("claude", "task", tmp_path, False, "uuid-1")
    assert claude == ["claude", "--session-id", "uuid-1", "task"]

    codex = build_pane_argv("codex", "task", tmp_path, False, None)
    assert codex[:3] == ["codex", "-C", str(tmp_path)]
    assert "--sandbox" in codex and codex[-1] == "task"
    codex_yolo = build_pane_argv("codex", "", tmp_path, True, None)
    assert "--dangerously-bypass-approvals-and-sandbox" in codex_yolo

    gemini = build_pane_argv("gemini", "task", tmp_path, False, None)
    assert gemini[:2] == ["gemini", "--skip-trust"]
    assert "-i" in gemini
    # Bare interactive session: no -i without a message.
    assert "-i" not in build_pane_argv("gemini", "", tmp_path, False, None)

    # x-8f7f US1: agy is never-prompt, stateless (no --session-id), message as
    # trailing positional; never `-p` (that is agy's headless/print form).
    agy = build_pane_argv("agy", "task", tmp_path, False, "ignored-uuid")
    assert agy == ["agy", "--dangerously-skip-permissions", "task"]
    assert "-p" not in agy and "--session-id" not in agy
    assert build_pane_argv("agy", "", tmp_path, False, None) == [
        "agy",
        "--dangerously-skip-permissions",
    ]


def test_pane_hostable_set_stays_in_sync_with_build_pane_argv(tmp_path: Path) -> None:
    """x-8f7f: PANE_HOSTABLE_PROVIDERS is the pane gate's source of truth and MUST
    match build_pane_argv's branches exactly - every listed provider builds argv,
    and a readable-but-argvless provider (opencode, staged inert until x-51f6) does
    NOT. This is the enforcement the borrowed READABLE_PROVIDERS list lacked."""
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import PANE_HOSTABLE_PROVIDERS, build_pane_argv
    from fno.agents.providers import READABLE_PROVIDERS

    for provider in PANE_HOSTABLE_PROVIDERS:
        argv = build_pane_argv(provider, "", tmp_path, False, None)
        assert argv and argv[0] == provider

    # opencode is readable (its manifest is bundled) but NOT pane-hostable yet:
    # the two sets are genuinely distinct, and the gate rides the correct one.
    assert "opencode" not in PANE_HOSTABLE_PROVIDERS
    for readable in READABLE_PROVIDERS:
        if readable not in PANE_HOSTABLE_PROVIDERS:
            with pytest.raises(DispatchAskError, match="no interactive pane form"):
                build_pane_argv(readable, "", tmp_path, False, None)


def test_ac1_host_pane_gate_admits_agy_rejects_unhosted(
    tmp_path: Path, monkeypatch
) -> None:
    """x-8f7f US1/US3: the pane gate is READABLE_PROVIDERS, not KNOWN_PROVIDERS.
    agy (readable, pane-hostable) is admitted and produces a mux-hosted row;
    opencode (not readable until x-51f6) is rejected at the gate."""
    from fno.agents.dispatch import DispatchAskError

    # agy spawns a real (faked) mux pane -> a row lands.
    result, runner = _spawn(monkeypatch, tmp_path, provider="agy")
    assert result.provider == "agy"
    assert runner.calls[0][1:4] == ["mux", "pane", "run"]

    # opencode is not pane-hostable yet -> refused before any mux subprocess.
    with pytest.raises(DispatchAskError, match="unknown provider 'opencode'"):
        _spawn(monkeypatch, tmp_path, provider="opencode", name="oc")


def test_unparseable_pane_id_is_a_loud_error(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="unparseable"):
        _spawn(
            monkeypatch,
            tmp_path,
            runner=FakeRunner(run_stdout="not-a-pane-id\n"),
        )
    from fno.agents.registry import load_registry

    assert load_registry() == []


def test_child_pid_lookup_is_best_effort(tmp_path: Path, monkeypatch) -> None:
    # A broken `pane ls` must not fail the spawn: pid stays None.
    result, _ = _spawn(
        monkeypatch, tmp_path, runner=FakeRunner(ls_stdout="not json")
    )
    assert result.child_pid is None
    from fno.agents.registry import load_registry

    assert load_registry()[0].pid is None


def test_routing_pane_substrate_spawn_stays_python() -> None:
    """4a-G2 routing carve-out: a pane spawn (explicit or default) never
    auto-routes to the Rust client; bg/headless spawns still do."""
    from fno.agents.rust_runtime import _is_pane_substrate_spawn

    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer", "--substrate", "pane"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "peer", "--substrate=pane"])
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "p", "--substrate", "bg"])
    assert not _is_pane_substrate_spawn(
        "spawn", ["spawn", "p", "--substrate=headless"]
    )
    assert not _is_pane_substrate_spawn("ask", ["ask", "peer", "hi"])
    # The scan stops at --argv: payload tokens cannot masquerade as our flag.
    assert _is_pane_substrate_spawn(
        "spawn", ["spawn", "p", "--argv", "--substrate", "bg"]
    )


def test_cmd_spawn_pane_receipt_shape(tmp_path: Path, monkeypatch) -> None:
    """The CLI receipt is one JSON line, a superset of the daemon-spawn shape
    ({"name","short_id","provider","status"}) plus the mux fields."""
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    from fno.agents.mux_spawn import MuxSpawnResult

    def fake_dispatch(**kwargs):
        return MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="main",
            pane_id=9,
            child_pid=111,
            session_uuid="u-1",
        )

    import fno.agents.mux_spawn as mux_spawn

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")

    runner = CliRunner()
    result = runner.invoke(agents_cli.agents_app, ["spawn", "peer", "--provider", "claude"])
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[-1])
    assert receipt == {
        "name": "peer",
        "short_id": "",
        "provider": "claude",
        "status": "live",
        "mux_session": "main",
        "pane_id": 9,
    }
