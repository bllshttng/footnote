"""`fno agents spawn --route provider,model` explicit fail-closed override (x-b0b4).

Layers:
- rust_runtime detector keeps --route Python-only (parity with --role).
- cmd_spawn resolves + fails CLOSED before the gate (AC3-ERR).
- dispatch_spawn threads route_env to the claude create path.
- bg_create applies route_env, winning over --role.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Dict

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir

runner = CliRunner()


def _setup_tmp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    for k in ("FNO_AGENT_SELF", "FNO_AGENT_PROVIDER", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(k, raising=False)


# ---------------------------------------------------------------------------
# rust_runtime: --route is Python-only, exactly like --role
# ---------------------------------------------------------------------------


def test_route_bearing_spawn_detected() -> None:
    from fno.agents.rust_runtime import _is_route_bearing_spawn

    assert _is_route_bearing_spawn("spawn", ["spawn", "w", "--route", "zai,glm-5.2"])
    assert _is_route_bearing_spawn("spawn", ["spawn", "w", "--route=zai,glm-5.2"])
    assert not _is_route_bearing_spawn("spawn", ["spawn", "w", "--role", "build"])
    assert not _is_route_bearing_spawn("ask", ["ask", "w", "--route", "zai,glm-5.2"])


# ---------------------------------------------------------------------------
# cmd_spawn: fail CLOSED before the gate (AC3-ERR)
# ---------------------------------------------------------------------------


def test_route_missing_key_refused_before_gate(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents import dispatch, spawn_gate

    monkeypatch.delenv("ZAI_API_KEY", raising=False)

    gate_calls: list = []
    monkeypatch.setattr(
        spawn_gate, "run_gate", lambda *a, **k: gate_calls.append(1) or _Gate()
    )
    # If the refusal fails to fire, this stub prevents a real spawn.
    monkeypatch.setattr(
        "fno.agents.dispatch.dispatch_spawn",
        lambda **kw: dispatch.SpawnResult(
            kind="created", name=kw["name"], provider="claude", short_id="x"
        ),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg",
         "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 2, result.output
    assert "refused" in result.output.lower()
    # Fail-closed BEFORE the gate: no slot acquired, no worker launched.
    assert gate_calls == []


class _Gate:
    def release(self) -> None:  # noqa: D401
        pass


def test_route_unknown_provider_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(
        "fno.agents.dispatch.dispatch_spawn",
        lambda **kw: dispatch.SpawnResult(
            kind="created", name=kw["name"], provider="claude", short_id="x"
        ),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg",
         "--route", "nope,glm-5.2"],
    )
    assert result.exit_code == 2, result.output


def test_route_rejected_on_pane_substrate(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents.cli import agents_app

    # Default substrate is pane; --route is claude+bg only.
    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 2, result.output
    assert "bg" in result.output.lower()


# ---------------------------------------------------------------------------
# cmd_spawn -> dispatch_spawn threads route_env (resolved) to the create path
# ---------------------------------------------------------------------------


def test_route_threads_resolved_env_to_dispatch(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())

    captured: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        captured.update(kwargs)
        return dispatch.SpawnResult(
            kind="created", name=kwargs["name"], provider="claude", short_id="abcd1234"
        )

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg",
         "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    route_env = captured["route_env"]
    assert route_env["ANTHROPIC_AUTH_TOKEN"] == "zk-live"
    assert route_env["ANTHROPIC_MODEL"] == "glm-5.2"
    assert route_env["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"


# ---------------------------------------------------------------------------
# bg_create: route_env WINS over role; anthropic creds cleared (AC "--route wins")
# ---------------------------------------------------------------------------


def test_bg_create_route_env_wins_over_role(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    from fno.agents.providers import claude as claude_mod

    # A stale parent Anthropic credential must be cleared so the routed token wins.
    monkeypatch.setenv("ANTHROPIC_API_KEY", "stale-anthropic")

    seen: Dict[str, Any] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        seen["env"] = kwargs.get("env", {})
        from subprocess import CompletedProcess

        return CompletedProcess(
            argv, 0, stdout="backgrounded \xb7 abcd1234 \xb7 ok\n", stderr=""
        )

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)

    claude_mod.bg_create(
        name="w",
        message="hi",
        cwd=tmp_path,
        role="consolidate",  # would resolve to a different route; --route wins
        route_env={
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "explicit-token",
            "ANTHROPIC_MODEL": "glm-5.2",
        },
    )
    env = seen["env"]
    assert env["ANTHROPIC_AUTH_TOKEN"] == "explicit-token"
    assert env["ANTHROPIC_MODEL"] == "glm-5.2"
    # The stale parent Anthropic key is popped so it can't override the route.
    assert "ANTHROPIC_API_KEY" not in env


# ---------------------------------------------------------------------------
# x-6de8: routed spawn applies its route via a --settings file (survives the
# daemon fork that drops per-spawn env), on both bg and headless.
# ---------------------------------------------------------------------------


def test_materialize_route_settings_is_0600_and_content_addressed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import json
    import os

    monkeypatch.setenv("HOME", str(tmp_path))
    from fno.agents.model_routing import materialize_route_settings

    env = {"ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic", "ANTHROPIC_AUTH_TOKEN": "t"}
    p1 = materialize_route_settings(env)
    p2 = materialize_route_settings(dict(env))  # same content -> same file
    assert p1 == p2
    assert oct(os.stat(p1).st_mode & 0o777) == "0o600"
    assert json.load(open(p1))["env"] == env


def test_bg_create_routed_spawn_passes_settings_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    from fno.agents.providers import claude as claude_mod

    seen: Dict[str, Any] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        seen["argv"] = argv
        from subprocess import CompletedProcess

        return CompletedProcess(argv, 0, stdout="backgrounded \xb7 abcd1234 \xb7 ok\n", stderr="")

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    claude_mod.bg_create(
        name="w",
        message="hi",
        cwd=tmp_path,
        route_env={"ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic", "ANTHROPIC_AUTH_TOKEN": "t"},
    )
    argv = seen["argv"]
    assert "--settings" in argv
    assert argv[argv.index("--settings") + 1].endswith(".json")


def test_headless_create_routed_spawn_passes_settings_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    from fno.agents.providers import claude as claude_mod

    seen: Dict[str, Any] = {}

    def fake_run(argv, **kwargs):  # type: ignore[no-untyped-def]
        seen["argv"] = argv
        from subprocess import CompletedProcess

        return CompletedProcess(argv, 0, stdout="ok", stderr="")

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    claude_mod.headless_create(
        message="hi",
        cwd=tmp_path,
        route_env={"ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic", "ANTHROPIC_AUTH_TOKEN": "t"},
    )
    assert "--settings" in seen["argv"]


def test_route_allowed_on_headless(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    captured: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        captured.update(kwargs)
        return dispatch.SpawnResult(kind="created", name=kwargs["name"], provider="claude", short_id="a")

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--headless", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    assert captured["route_env"]["ANTHROPIC_AUTH_TOKEN"] == "zk-live"


def test_provider_zai_alias_and_bare_route(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    captured: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        captured.update(kwargs)
        return dispatch.SpawnResult(kind="created", name=kwargs["name"], provider="claude", short_id="a")

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    # `--provider zai` expands to claude + default 1M GLM model.
    result = runner.invoke(agents_app, ["spawn", "w1", "hi", "--provider", "zai", "--headless"])
    assert result.exit_code == 0, result.output
    assert captured["provider"] == "claude"
    assert captured["route_env"]["ANTHROPIC_MODEL"] == "glm-5.2[1m]"

    # Bare `--route zai` expands the same way.
    captured.clear()
    result = runner.invoke(
        agents_app, ["spawn", "w2", "hi", "--provider", "claude", "--headless", "--route", "zai"]
    )
    assert result.exit_code == 0, result.output
    assert captured["route_env"]["ANTHROPIC_MODEL"] == "glm-5.2[1m]"


# ---------------------------------------------------------------------------
# x-6de8 harness axis: --harness/-H canonical, --provider/-p deprecated alias
# ---------------------------------------------------------------------------


def test_zai_shorthand_honors_all_headless_spellings(monkeypatch: pytest.MonkeyPatch) -> None:
    """x-6de8 codex P2: --once/-o must reach the headless one-shot lane for zai,
    not be stolen by the pane->bg default (which left once=True + provider=claude
    and made dispatch reject the combo). All three headless spellings converge."""
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    captured: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        captured.update(kwargs)
        return dispatch.SpawnResult(kind="created", name=kwargs["name"], provider="claude", short_id="a")

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    for flag in ("--once", "-o", "--headless"):
        captured.clear()
        result = runner.invoke(agents_app, ["spawn", "w", "hi", "--harness", "zai", flag])
        assert result.exit_code == 0, f"{flag}: {result.output}"
        assert captured["once"] is True, f"{flag} should reach the headless one-shot lane"
        assert captured["provider"] == "claude"
        assert captured["route_env"]["ANTHROPIC_MODEL"] == "glm-5.2[1m]"

    # Bare zai (no one-shot flag) still defaults to the attachable bg thread.
    captured.clear()
    result = runner.invoke(agents_app, ["spawn", "w", "hi", "--harness", "zai"])
    assert result.exit_code == 0, result.output
    assert captured.get("once") is not True


def test_harness_zai_shorthand_matches_provider_zai(monkeypatch: pytest.MonkeyPatch) -> None:
    """The zai vendor shorthand fires through the canonical --harness spelling too."""
    from fno.agents import dispatch, spawn_gate
    from fno.agents.rust_runtime import _is_route_provider_spawn

    # The Python-only detector recognizes zai via every spelling of the axis.
    assert _is_route_provider_spawn("spawn", ["spawn", "w", "--harness", "zai"])
    assert _is_route_provider_spawn("spawn", ["spawn", "w", "-H", "zai"])
    assert _is_route_provider_spawn("spawn", ["spawn", "w", "--harness=zai"])

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    captured: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        captured.update(kwargs)
        return dispatch.SpawnResult(kind="created", name=kwargs["name"], provider="claude", short_id="a")

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "w1", "hi", "--harness", "zai", "--headless"])
    assert result.exit_code == 0, result.output
    assert captured["provider"] == "claude"
    assert captured["route_env"]["ANTHROPIC_MODEL"] == "glm-5.2[1m]"


def test_provider_alias_functions_and_stays_quiet_off_tty(monkeypatch: pytest.MonkeyPatch) -> None:
    """--provider/-p still selects the harness (deprecated alias), and the
    human-only deprecation note never pollutes a non-tty stream (the note is
    gated on stderr.isatty(), which is False under CliRunner and in any pipe)."""
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    captured: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        captured.update(kwargs)
        return dispatch.SpawnResult(kind="created", name=kwargs["name"], provider="codex", short_id="a")

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "w1", "hi", "--provider", "codex", "--headless"])
    assert result.exit_code == 0, result.output
    assert captured["provider"] == "codex"
    assert "deprecated" not in result.output  # quiet on the non-tty test stream

    # The canonical --harness spelling threads the same provider.
    captured.clear()
    clean = runner.invoke(agents_app, ["spawn", "w2", "hi", "--harness", "codex", "--headless"])
    assert clean.exit_code == 0, clean.output
    assert captured["provider"] == "codex"
    assert "deprecated" not in clean.output


def test_harness_provider_conflict_exits_2(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents import spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "w1", "hi", "--harness", "codex", "--provider", "claude"]
    )
    assert result.exit_code == 2, result.output
    assert "conflicts with --provider" in result.output

    # Same value on both is not a conflict (harness wins); no conflict message,
    # whatever the downstream codex-headless path decides to do.
    ok = runner.invoke(
        agents_app,
        ["spawn", "w2", "hi", "--harness", "codex", "--provider", "codex", "--headless"],
    )
    assert "conflicts with --provider" not in ok.output, ok.output
