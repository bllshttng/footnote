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


def test_codex_shell_env_args_renders_json_quoted_leaves() -> None:
    """Each mesh pair becomes one config-set leaf; the JSON string is a valid
    TOML basic string and merges with the config.toml set table."""
    from fno.agents.codex_pane import codex_shell_env_args

    args = codex_shell_env_args(["FNO_AGENT_SELF=w1", "FNO_NODE=x-1"])
    assert args == [
        "-c",
        'shell_environment_policy.set.FNO_AGENT_SELF="w1"',
        "-c",
        'shell_environment_policy.set.FNO_NODE="x-1"',
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
    assert tail[codex_at + 1 : codex_at + 3] == [
        "-c",
        'shell_environment_policy.set.FNO_AGENT_SELF="w1"',
    ]
    assert 'shell_environment_policy.set.FNO_AGENT_HARNESS="codex"' in tail

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
    assert 'shell_environment_policy.set.FNO_AGENT_SELF="w1"' in tail_one
    assert 'shell_environment_policy.set.FNO_AGENT_SELF="w2"' not in tail_one
    assert 'shell_environment_policy.set.FNO_AGENT_SELF="w2"' in tail_two
    assert 'shell_environment_policy.set.FNO_AGENT_SELF="w1"' not in tail_two
