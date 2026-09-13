"""Tests for the codex pane lane's daemon-owned threads (x-a095).

The create form asserts the shared app-server daemon (`--remote unix://`,
`pre_exec` daemon start), the spawn refuses when that start fails, the mesh
identity reaches daemon-run tools as config-set leaves, and the trust screen
is named, never answered.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno.agents import codex_pane
from tests.agents.test_spawn_pane import CODEX_HARNESS, FakeRunner, _spawn


def test_ensure_codex_daemon_runs_the_form_declared_pre_exec() -> None:
    """The daemon command is read from the capability toml, never hardcoded."""
    from fno.agents import codex_pane

    calls = []

    def runner(argv, **kwargs):
        calls.append((list(argv), kwargs.get("timeout")))
        return subprocess.CompletedProcess(argv, 0, '{"status":"alreadyRunning"}', "")

    codex_pane.ensure_codex_daemon(runner)
    assert calls == [(["codex", "app-server", "daemon", "start"], 15)]


def test_ensure_codex_daemon_failure_and_timeout_name_the_command() -> None:
    from fno.agents import codex_pane
    from fno.agents.dispatch import DispatchAskError

    def failing(argv, **kwargs):
        return subprocess.CompletedProcess(argv, 1, "", "daemon start exploded")

    with pytest.raises(DispatchAskError, match="daemon start exploded"):
        codex_pane.ensure_codex_daemon(failing)

    def hanging(argv, **kwargs):
        raise subprocess.TimeoutExpired(argv, 15)

    with pytest.raises(DispatchAskError, match="timed out"):
        codex_pane.ensure_codex_daemon(hanging)


def test_codex_shell_env_args_renders_one_inline_table() -> None:
    """One `-c` override carries the whole set table: repeated leaves of the
    same table do not merge on the daemon lane (measured 2026-09-13)."""
    from fno.agents.codex_pane import codex_shell_env_args

    args = codex_shell_env_args(["FNO_AGENT_SELF=w1", "FNO_NODE=x-1"])
    assert args == [
        "-c",
        'shell_environment_policy.set={"FNO_AGENT_SELF": "w1", "FNO_NODE": "x-1"}',
    ]


def test_codex_shell_env_args_refuses_a_key_outside_the_leaf_shape() -> None:
    """A dotted or quoted key would write a different config path."""
    from fno.agents import codex_pane
    from fno.agents.dispatch import DispatchAskError

    with pytest.raises(DispatchAskError, match="A.B=1"):
        codex_pane.codex_shell_env_args(["A.B=1"])
    with pytest.raises(DispatchAskError, match="JUST_A_KEY"):
        codex_pane.codex_shell_env_args(["JUST_A_KEY"])


def test_codex_pane_spawn_refused_when_daemon_start_fails(
    tmp_path: Path, monkeypatch
) -> None:
    """A dead daemon start refuses the spawn before any pane exists: the
    create form asserts the daemon, so launching without one would mint the
    private thread this lane exists to prevent."""
    from fno.agents.dispatch import DispatchAskError

    class DaemonStartFails(FakeRunner):
        def __call__(self, argv, **kwargs):
            if list(argv)[:4] == ["codex", "app-server", "daemon", "start"]:
                self.calls.append(list(argv))
                return subprocess.CompletedProcess(argv, 1, "", "daemon start exploded")
            return super().__call__(argv, **kwargs)

    runner = DaemonStartFails()
    with pytest.raises(DispatchAskError, match="daemon start exploded"):
        _spawn(
            monkeypatch, tmp_path, provider=CODEX_HARNESS, runner=runner,
            codex_binding=False,
        )
    assert not any(call[1:4] == ["mux", "pane", "run"] for call in runner.calls)


def test_codex_trust_screen_refusal_fires_despite_hook_trust_bypass() -> None:
    """`--remote` ignores a config trust override, so an untrusted cwd parks
    on the project trust screen even under the bypass posture; the readiness
    probe names it and never answers the security decision."""
    from fno.agents.mux_spawn import _codex_trust_refusal

    refusal = _codex_trust_refusal(
        "? Do you trust the contents of this directory?",
        cwd=Path("/w/proj"),
        hook_trust_bypassed=True,
    )
    assert refusal is not None
    assert refusal.startswith("Codex project trust required for /w/proj")


def _codex_pane_run_tail(runner: FakeRunner) -> list[str]:
    run_call = next(c for c in runner.calls if c[1:4] == ["mux", "pane", "run"])
    return run_call[run_call.index("--") + 1 :]


def test_codex_pane_mesh_identity_rides_config_set_args(
    tmp_path: Path, monkeypatch
) -> None:
    """A daemon-run tool sees none of the TUI's env, so the mesh pairs ride
    into the tool shell as `-c shell_environment_policy.set` leaves; the
    env(1) wrapper still carries them for the TUI process itself."""
    from fno.agents import codex_pane

    monkeypatch.setattr(codex_pane, "ensure_codex_daemon", lambda *_a, **_k: None)

    runner = FakeRunner()
    _spawn(
        monkeypatch, tmp_path, provider=CODEX_HARNESS, name="w1", runner=runner
    )

    tail = _codex_pane_run_tail(runner)
    assert tail[0] == "env"
    assert "FNO_AGENT_SELF=w1" in tail
    codex_at = tail.index("codex")
    assert tail[codex_at + 1] == "-c"
    assert 'FNO_AGENT_SELF": "w1"' in tail[codex_at + 2]
    assert '"FNO_AGENT_HARNESS": "codex"' in tail[codex_at + 2]

    claude_runner = FakeRunner()
    _spawn(monkeypatch, tmp_path, name="w2", runner=claude_runner)
    claude_tail = _codex_pane_run_tail(claude_runner)
    assert not any("shell_environment_policy" in tok for tok in claude_tail)


def test_two_codex_workers_carry_only_their_own_identity(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import codex_pane

    monkeypatch.setattr(codex_pane, "ensure_codex_daemon", lambda *_a, **_k: None)

    first = FakeRunner()
    _spawn(monkeypatch, tmp_path, provider=CODEX_HARNESS, name="w1", runner=first)
    second = FakeRunner()
    _spawn(monkeypatch, tmp_path, provider=CODEX_HARNESS, name="w2", runner=second)

    tail_one = _codex_pane_run_tail(first)
    tail_two = _codex_pane_run_tail(second)
    leaf_one = tail_one[tail_one.index("codex") + 2]
    leaf_two = tail_two[tail_two.index("codex") + 2]
    assert '"FNO_AGENT_SELF": "w1"' in leaf_one
    assert '"FNO_AGENT_SELF": "w2"' not in leaf_one
    assert '"FNO_AGENT_SELF": "w2"' in leaf_two
    assert '"FNO_AGENT_SELF": "w1"' not in leaf_two


def test_codex_binds_through_the_daemon_oracle_when_the_fd_probe_misses(
    tmp_path: Path, monkeypatch
) -> None:
    """With the pane TUI on `--remote unix://` the thread is the daemon's, so
    the fd probe misses every time and the daemon oracle binds the row.

    Wired at ``_make_codex_bind_probe`` rather than the daemon-candidate
    function it composes: that function's own stability gate needs two
    probes spaced by ``_CODEX_DAEMON_PROBE_INTERVAL_S``, which the dispatch
    lane's collapsed ``FNO_PANE_BINDING_WINDOW_S`` window has no room for.
    The gate's own timing is covered directly in
    test_spawn_codex_session_capture.py; this test only checks the dispatch
    wiring picks up whatever the probe returns.
    """
    from fno.agents.registry import load_registry

    session_id = "019fb024-2327-75f3-8b80-06e9d5ade05f"
    monkeypatch.setattr(
        codex_pane, "_make_codex_bind_probe", lambda **_kwargs: (lambda: session_id)
    )

    result, _ = _spawn(
        monkeypatch, tmp_path, provider=CODEX_HARNESS, codex_binding=False
    )

    row = load_registry()[0]
    assert row.harness_session_id == session_id
    assert result.session_uuid == session_id
    assert result.bound is True


def test_codex_daemon_ambiguity_still_reaps_rather_than_guessing(
    tmp_path: Path, monkeypatch
) -> None:
    """Two new session ids in this cwd is a race between sibling panes; the
    daemon oracle refuses to guess, so the spawn fails exactly like the
    binding-window-expired case rather than misbinding."""
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.registry import load_registry

    monkeypatch.setattr(
        codex_pane, "_make_codex_bind_probe", lambda **_kwargs: (lambda: None)
    )

    runner = FakeRunner()
    with pytest.raises(DispatchAskError, match="session binding.*reaped"):
        _spawn(
            monkeypatch, tmp_path, provider=CODEX_HARNESS, runner=runner,
            codex_binding=False,
        )
    assert load_registry() == []
    assert runner.kill_calls
