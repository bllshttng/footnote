"""`fno agents spawn --route provider,model` explicit fail-closed override (x-b0b4).

Layers:
- rust_runtime detector keeps --route Python-only (parity with --role).
- cmd_spawn resolves + fails CLOSED before the gate (AC3-ERR).
- dispatch_spawn threads route_env to the claude create path.
- bg_create applies route_env, winning over --role.
"""
from __future__ import annotations

import os
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
    for k in ("FNO_AGENT_SELF", "FNO_AGENT_HARNESS", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(k, raising=False)


# ---------------------------------------------------------------------------
# rust_runtime: --route is Python-only, exactly like --role
# ---------------------------------------------------------------------------


def test_route_bearing_spawn_detected() -> None:
    from fno.agents.rust_runtime import _is_route_bearing_spawn

    assert _is_route_bearing_spawn("spawn", ["spawn", "--name", "w", "--route", "zai,glm-5.2"])
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
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
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
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
         "--route", "nope,glm-5.2"],
    )
    assert result.exit_code == 2, result.output


def test_route_allowed_on_capability_enabled_pane(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents import mux_spawn, spawn_gate
    from fno.agents.cli import agents_app
    from fno.agents.model_routing import (
        DEFAULT_ZAI_BASE_URL,
        DEFAULT_ZAI_HAIKU_MODEL,
        MODEL_ENV_KEYS,
    )
    from fno.agents.mux_spawn import MuxSpawnResult

    _setup_tmp_home(tmp_path, monkeypatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())
    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    captured: Dict[str, Any] = {}

    def fake_dispatch(**kwargs: Any) -> MuxSpawnResult:
        captured.update(kwargs)
        return MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="main",
            pane_id=1,
            child_pid=None,
            session_uuid="u",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    assert captured["provider"] == "claude"
    assert captured["route_provider"] == "zai"
    route_env = captured["route_env"]
    assert route_env["ANTHROPIC_BASE_URL"] == DEFAULT_ZAI_BASE_URL
    assert route_env["ANTHROPIC_AUTH_TOKEN"] == "zk-live"
    for key in MODEL_ENV_KEYS:
        if key == "ANTHROPIC_DEFAULT_HAIKU_MODEL":
            continue
        assert route_env[key] == "glm-5.2"
    assert route_env["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == DEFAULT_ZAI_HAIKU_MODEL


@pytest.mark.parametrize("missing", [False, True])
def test_route_on_pane_capability_fails_closed_before_gate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, missing: bool
) -> None:
    import fno.agents.harness_map as harness_map
    from fno.agents import mux_spawn, spawn_gate
    from fno.agents.cli import agents_app

    _setup_tmp_home(tmp_path, monkeypatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setenv("FNO_REPO_ROOT", os.getcwd())
    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    if missing:
        monkeypatch.delitem(harness_map._HARNESS_CAPS["claude"], "route_on_pane")
    else:
        monkeypatch.setitem(harness_map._HARNESS_CAPS["claude"], "route_on_pane", False)
    gate_calls: list[int] = []
    monkeypatch.setattr(
        spawn_gate,
        "run_gate",
        lambda *a, **k: gate_calls.append(1) or _Gate(),
    )
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **kwargs: pytest.fail("pane dispatch called"),
    )

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--route", "zai,glm-5.2"],
    )

    assert result.exit_code == 2, result.output
    assert "route_on_pane" in result.output
    assert gate_calls == []


def test_route_on_pane_complete_environment_reaches_child_wrapper() -> None:
    from fno.agents.model_routing import MODEL_ENV_KEYS, resolve_explicit_route
    from fno.agents.mux_spawn import _mesh_env_wrapper

    route_env = resolve_explicit_route(
        "zai",
        "glm-5.2",
        env={"ZAI_API_KEY": "zk-live"},
    )
    assert route_env is not None

    wrapped = _mesh_env_wrapper(
        "peer",
        "claude",
        None,
        ["claude", "hello"],
        route_env=route_env,
    )

    first_assignment = next(i for i, token in enumerate(wrapped) if "=" in token)
    unset_region = wrapped[:first_assignment]
    assert "ANTHROPIC_API_KEY" in unset_region
    assert "CLAUDE_CODE_OAUTH_TOKEN" in unset_region
    for key in ("ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", *MODEL_ENV_KEYS):
        assert f"{key}={route_env[key]}" in wrapped


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
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
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
# x-5ed4 / x-2af5: an account overlay + a vendor route COMPOSE. The route wins
# endpoint+auth+model as one unit (atomic); the account keeps CLAUDE_CONFIG_DIR.
# This is the regression for the split-brain the old "refuse" guard existed to
# prevent (overlay endpoint+auth, route model -> foreign model on Anthropic).
# ---------------------------------------------------------------------------


def test_bg_create_route_wins_over_account_overlay(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    from fno.agents.providers import claude as claude_mod

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
        route_env={
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "zai-token",
            "ANTHROPIC_MODEL": "glm-5.2",
        },
        account_env={
            "CLAUDE_CONFIG_DIR": "/x/.claude",
            "ANTHROPIC_AUTH_TOKEN": "account-managed-token",
        },
    )
    env = seen["env"]
    # The route wins endpoint+auth+model atomically - NOT the account's token.
    assert env["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert env["ANTHROPIC_AUTH_TOKEN"] == "zai-token"
    assert env["ANTHROPIC_MODEL"] == "glm-5.2"
    # The account's profile survives; its competing login did not win.
    assert env["CLAUDE_CONFIG_DIR"] == "/x/.claude"
    assert env["ANTHROPIC_AUTH_TOKEN"] != "account-managed-token"


def test_mesh_env_wrapper_route_wins_over_account() -> None:
    """Pane substrate (env(1) is left-to-right last-wins): the route's auth pairs
    must follow the account's so the route wins, while CLAUDE_CONFIG_DIR survives."""
    from fno.agents.mux_spawn import _mesh_env_wrapper

    argv = _mesh_env_wrapper(
        name="w",
        provider="claude",
        role=None,
        argv=["claude", "hi"],
        account_env={"CLAUDE_CONFIG_DIR": "/x/.claude", "ANTHROPIC_AUTH_TOKEN": "acct"},
        route_env={
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "zai",
            "ANTHROPIC_MODEL": "glm-5.2",
        },
    )
    # Both auth assignments are present; the route's must come LAST (env last-wins).
    assert "ANTHROPIC_AUTH_TOKEN=acct" in argv
    assert "ANTHROPIC_AUTH_TOKEN=zai" in argv
    assert argv.index("ANTHROPIC_AUTH_TOKEN=zai") > argv.index("ANTHROPIC_AUTH_TOKEN=acct")
    # The account profile and the route endpoint both ride the wrapper.
    assert "CLAUDE_CONFIG_DIR=/x/.claude" in argv
    assert "ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic" in argv


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

    from fno.agents.account_env import SCRUB_AUTH_VARS

    env = {"ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic", "ANTHROPIC_AUTH_TOKEN": "t"}
    p1 = materialize_route_settings(env)
    p2 = materialize_route_settings(dict(env))  # same content -> same file
    assert p1 == p2
    assert oct(os.stat(p1).st_mode & 0o777) == "0o600"
    blob = json.load(open(p1))["env"]
    assert {k: blob[k] for k in env} == env
    # Every inheritable auth var the route did not set is written empty (unset),
    # so an ambient key from the invoking shell cannot outrank the route.
    for var in SCRUB_AUTH_VARS:
        if var not in env:
            assert blob[var] == "", var
    # codex P2 (finding 8): published atomically via a temp + os.replace, so a
    # racing reader never sees a partial file and no .tmp sidecar is left behind.
    leftovers = list(Path(p1).parent.glob(".*.tmp"))
    assert leftovers == [], f"temp files not cleaned up: {leftovers}"


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


def test_composed_route_and_account_spawn_scrubs_inherited_auth(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A --route + --account spawn writes ONLY the route settings file (route-wins
    atomicity), so that file has to carry the auth scrub itself: the env scrub is
    discarded at the daemon fork, and without the floor the worker could still
    authenticate with an inherited key while its endpoint points elsewhere."""
    import json

    _setup_tmp_home(tmp_path, monkeypatch)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("ANTHROPIC_API_KEY", "stale-primary-key")
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
        account_env={"CLAUDE_CONFIG_DIR": str(tmp_path / "acct")},
        route_env={"ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic", "ANTHROPIC_AUTH_TOKEN": "t"},
    )
    argv = seen["argv"]
    blob = json.load(open(argv[argv.index("--settings") + 1]))["env"]
    assert blob["ANTHROPIC_API_KEY"] == ""
    assert blob["CLAUDE_CODE_OAUTH_TOKEN"] == ""
    assert blob["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"


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


# ---------------------------------------------------------------------------
# An --account spawn expresses its auth/model scrub via a --settings file too:
# the env scrub is dropped at the claude --bg daemon fork, but a settings file is
# read by the forked session itself. CLAUDE_CONFIG_DIR rides the env overlay
# (it selects the config the settings file is read FROM), never the file.
# ---------------------------------------------------------------------------


def test_materialize_account_scrub_settings_unsets_vendor_and_keeps_overlay(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import json
    import os

    monkeypatch.setenv("HOME", str(tmp_path))
    from fno.agents.account_env import SCRUB_AUTH_VARS
    from fno.agents.model_routing import materialize_account_scrub_settings

    # A config_dir account: the overlay only carries CLAUDE_CONFIG_DIR, which
    # must NOT land in the settings file. Every vendor auth/model var is written
    # empty, which claude reads as unset (so the account's own login wins).
    path = materialize_account_scrub_settings({"CLAUDE_CONFIG_DIR": "/x/.claude"})
    blob = json.load(open(path))["env"]
    assert "CLAUDE_CONFIG_DIR" not in blob
    for var in SCRUB_AUTH_VARS:
        assert blob[var] == ""

    # An api_key account: its resolved ANTHROPIC_* is re-applied over the scrub
    # so it survives the fork, AND every extra env the record pins (here a custom
    # header, outside SCRUB_AUTH_VARS) is retained too, not just the scrub vars.
    # CLAUDE_CONFIG_DIR is still excluded.
    overlay = {
        "CLAUDE_CONFIG_DIR": "/x/.claude",
        "ANTHROPIC_BASE_URL": "https://api.example/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "acct-token",
        "ANTHROPIC_MODEL": "claude-opus-4-5",
        "ANTHROPIC_CUSTOM_HEADERS": "X-Org: foo",
        "HTTPS_PROXY": "http://proxy:8080",
    }
    path2 = materialize_account_scrub_settings(overlay)
    blob2 = json.load(open(path2))["env"]
    assert blob2["ANTHROPIC_BASE_URL"] == "https://api.example/anthropic"
    assert blob2["ANTHROPIC_AUTH_TOKEN"] == "acct-token"
    assert blob2["ANTHROPIC_MODEL"] == "claude-opus-4-5"
    assert blob2["ANTHROPIC_CUSTOM_HEADERS"] == "X-Org: foo"  # extra overlay retained
    assert blob2["HTTPS_PROXY"] == "http://proxy:8080"  # extra overlay retained
    assert "CLAUDE_CONFIG_DIR" not in blob2
    for var in SCRUB_AUTH_VARS:  # vars the overlay did not supply stay unset
        if var not in overlay:
            assert blob2[var] == ""
    assert oct(os.stat(path2).st_mode & 0o777) == "0o600"  # carries a token
    assert materialize_account_scrub_settings(dict(overlay)) == path2  # content-addressed


def test_bg_create_account_spawn_passes_settings_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
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
        account_env={"CLAUDE_CONFIG_DIR": str(tmp_path / ".claude")},
    )
    argv = seen["argv"]
    assert "--settings" in argv
    assert argv[argv.index("--settings") + 1].endswith(".json")


def test_headless_create_account_spawn_passes_settings_flag(
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
        account_env={"CLAUDE_CONFIG_DIR": str(tmp_path / ".claude")},
    )
    assert "--settings" in seen["argv"]


def test_account_threads_overlay_to_bg_dispatch(monkeypatch: pytest.MonkeyPatch) -> None:
    """The account overlay must reach the bg create lane through the real CLI
    dispatch path, not only at the mocked provider seam - mirrors
    test_route_threads_resolved_env_to_dispatch for the account axis. Without it,
    dropping the `account_env=account_env` kwarg on the bg dispatch call would
    leave every other account test green."""
    from fno.agents import account_env as ae, dispatch, spawn_gate

    # Force the Python dispatch path so the spawn reaches cmd_spawn regardless of
    # whether an fno-agents binary is installed (an --account spawn is not in the
    # py_spawn set, so auto-routing would otherwise exec the binary).
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    overlay = ae.AccountOverlay(
        account_id="acct", env={"CLAUDE_CONFIG_DIR": "/x/.claude"}, lane="config-dir"
    )
    monkeypatch.setattr(ae, "resolve_account_overlay_or_exit", lambda _aid: overlay)

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
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
         "--account", "acct"],
    )
    assert result.exit_code == 0, result.output
    # The overlay threaded through to the bg create lane.
    assert captured["account_env"] == {"CLAUDE_CONFIG_DIR": "/x/.claude"}
    # account and route are mutually exclusive; route_env is absent on this spawn.
    assert captured.get("route_env") in (None, {})


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
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--headless", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    assert captured["route_env"]["ANTHROPIC_AUTH_TOKEN"] == "zk-live"


# ---------------------------------------------------------------------------
# Harness axis: --harness/-H canonical, --provider/-p the older spelling.
# A model VENDOR is never a harness value -- that axis is --route.
# ---------------------------------------------------------------------------


def test_vendor_name_is_not_a_harness_value(monkeypatch: pytest.MonkeyPatch) -> None:
    """`zai` is a model vendor, not a CLI binary. It must be refused on the harness
    axis (through every spelling) rather than silently rewritten into a routed
    claude worker -- routing has exactly one surface, `--route`."""
    from fno.agents import spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    from fno.agents.cli import agents_app

    for flag in ("--harness", "-H", "--provider", "-p"):
        result = runner.invoke(agents_app, ["spawn", "hi", flag, "zai", "--headless"])
        assert result.exit_code == 2, f"{flag}: {result.output}"


def test_routed_once_reaches_the_headless_lane(monkeypatch: pytest.MonkeyPatch) -> None:
    """`--once` is the pre-substrate spelling of headless, but Python leaves it on
    the pane default. Without converging it a routed one-shot reaches dispatch as
    claude+once+not-headless and dies on the "claude peers are persistent bg
    threads" refusal, so assert BOTH flags: a pure once==True check passes even
    when the real dispatch would reject the spawn."""
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
        result = runner.invoke(
            agents_app,
            ["spawn", "--name", "hi", "--harness", "claude", "--route", "zai,glm-5.2", flag],
        )
        assert result.exit_code == 0, f"{flag}: {result.output}"
        assert captured["once"] is True, f"{flag} should reach the one-shot lane"
        assert captured["headless"] is True, f"{flag} must set headless for claude+once"


def test_bare_route_vendor_is_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    """A bare `--route zai` names no model. It is refused rather than expanded from
    a hardcoded default: the vendor/model pair is the caller's choice."""
    from fno.agents import spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app, ["spawn", "--name", "w", "hi", "--harness", "claude", "--headless", "--route", "zai"]
    )
    assert result.exit_code == 2, result.output
    assert "provider,model" in result.output


def test_provider_is_the_older_spelling_of_harness(monkeypatch: pytest.MonkeyPatch) -> None:
    """--provider/-p and --harness/-H name one axis: the CLI binary."""
    from fno.agents import dispatch, spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    captured: Dict[str, Any] = {}

    def fake_dispatch_spawn(**kwargs: Any) -> Any:
        captured.update(kwargs)
        return dispatch.SpawnResult(kind="created", name=kwargs["name"], provider="codex", short_id="a")

    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "--name", "w1", "hi", "--harness", "codex", "--headless"])
    assert result.exit_code == 0, result.output
    assert captured["provider"] == "codex"

    # The canonical --harness spelling threads the same provider.
    captured.clear()
    clean = runner.invoke(agents_app, ["spawn", "--name", "w2", "hi", "--harness", "codex", "--headless"])
    assert clean.exit_code == 0, clean.output
    assert captured["provider"] == "codex"


def test_harness_name_on_the_provider_axis_is_refused_by_name(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """`--provider claude` used to select the CLI binary. Now that --provider is
    the model-vendor axis it must refuse a harness name and NAME the fix, rather
    than launching a worker with a nonsense route."""
    from fno.agents import spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    from fno.agents.cli import agents_app

    for h in ("claude", "codex", "gemini", "opencode", "agy"):
        result = runner.invoke(agents_app, ["spawn", "--name", "w1", "hi", "--provider", h])
        assert result.exit_code == 2, f"{h}: {result.output}"
        assert f"{h} is a harness, not a provider" in result.output
        assert f"use --harness {h}" in result.output


def test_provider_and_model_build_the_route(monkeypatch: pytest.MonkeyPatch) -> None:
    """--provider + --model is the decomposed spelling of --route vendor,model.
    The model rides the route, NOT a `claude --model` token: handing the claude
    CLI a vendor model id would make it resolve a model it does not have."""
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
        ["spawn", "--name", "w1", "hi", "--substrate", "bg", "--provider", "zai", "--model", "glm-5.2"],
    )
    assert result.exit_code == 0, result.output
    assert captured["route_env"]["ANTHROPIC_MODEL"] == "glm-5.2"
    assert captured["route_env"]["ANTHROPIC_AUTH_TOKEN"] == "zk-live"
    assert captured["model"] is None
    assert captured["provider"] == "claude"


def test_provider_without_model_is_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents import spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    from fno.agents.cli import agents_app

    result = runner.invoke(agents_app, ["spawn", "--name", "w1", "hi", "--substrate", "bg", "--provider", "zai"])
    assert result.exit_code == 2, result.output
    assert "--model" in result.output


def test_provider_and_route_together_are_refused(monkeypatch: pytest.MonkeyPatch) -> None:
    """Two spellings of one route; taking both would silently drop one."""
    from fno.agents import spawn_gate

    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--substrate", "bg", "--provider", "zai",
         "--model", "glm-5.2", "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 2, result.output
    assert "two spellings" in result.output


def test_provider_bearing_spawn_stays_python_side() -> None:
    """Routing is materialized only in the Python spawn path, so the front door
    must keep a --provider spawn there; the Rust client would launch it on the
    primary model."""
    from fno.agents.rust_runtime import _is_route_bearing_spawn

    for args in (["spawn", "w", "--provider", "zai"], ["spawn", "w", "-P", "zai"],
                 ["spawn", "w", "--provider=zai"]):
        assert _is_route_bearing_spawn("spawn", args), args


# ---------------------------------------------------------------------------
# x-3db9 P1: the managed-OAuth refusal stays armed for a bare route (no account
# overlay), and only relaxes when an account overlay makes the composition atomic.
# ---------------------------------------------------------------------------


def test_route_under_managed_without_account_still_refused(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Regression guard: a bare route under an inherited FNO_PROVIDER_AUTH=managed
    (no --account) is still refused - the half-composition guard stays armed
    unless an account overlay pins a profile."""
    from fno.agents import spawn_gate

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w1", "hi", "--harness", "claude", "--substrate", "bg",
         "--route", "zai,glm-5.2"],
    )
    assert result.exit_code == 2, result.output
    assert "managed oauth" in result.output.lower()


# ---------------------------------------------------------------------------
# x-3db9 P1 (codex): the managed-OAuth bypass needs the route to carry its own
# auth, not just any account overlay. A direct dispatch_spawn caller can pass a
# hand-built partial route_env (base-URL-only); under managed + account that
# would pair the foreign endpoint with the account's Keychain OAuth, so the
# guard stays armed until the route is self-sufficient.
# ---------------------------------------------------------------------------


def test_managed_bypass_requires_route_self_auth(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents.model_routing import RouteCompositionError, resolve_spawn_route

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    partial = {"ANTHROPIC_BASE_URL": "https://foreign.example/anthropic"}  # no auth token
    # Account present, but the route brings no own credential -> still refused.
    with pytest.raises(RouteCompositionError):
        resolve_spawn_route(None, partial, account_overlay=True)
    # A self-authed route + account composes (no refusal).
    complete = {**partial, "ANTHROPIC_AUTH_TOKEN": "route-key"}
    assert resolve_spawn_route(None, complete, account_overlay=True) == complete


# ---------------------------------------------------------------------------
# x-3db9 P2: a route-bearing --account spawn must not scrub the route key at the
# seam. A vendor may name a SCRUB_AUTH_VARS member (e.g. ANTHROPIC_AUTH_TOKEN) as
# its api_key_env; resolve_explicit_route reads it from os.environ, so scrubbing
# first reports a valid route keyless only when --account is also present.
# Routed spawns are Python-only and the create lane scrubs + applies route-wins
# itself, so the seam scrub is skipped entirely for them.
# ---------------------------------------------------------------------------


def test_seam_scrub_skips_route_bearing_account_spawn(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    import os

    from fno.agents import rust_runtime

    monkeypatch.setenv("ANTHROPIC_AUTH_TOKEN", "route-key")
    args = ["spawn", "--name", "w", "hi", "--account", "acct", "--route", "zai,glm-5.2"]
    rust_runtime._scrub_account_auth_at_seam(args)
    assert os.environ.get("ANTHROPIC_AUTH_TOKEN") == "route-key"  # not scrubbed


def test_seam_scrub_runs_for_account_only_spawn(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The scrub still runs (and the overlay is applied) for an account spawn
    with no route: that is the Rust-path case the seam exists for."""
    import os

    from fno.agents import account_env as ae, rust_runtime

    monkeypatch.setenv("ANTHROPIC_AUTH_TOKEN", "inherited")
    # Track CLAUDE_CONFIG_DIR so the value the scrub applies is restored after.
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", "pre")
    monkeypatch.setattr(
        ae,
        "resolve_account_overlay",
        lambda aid: ae.AccountOverlay(aid, {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"),
    )
    args = ["spawn", "--name", "w", "hi", "--account", "acct"]
    rust_runtime._scrub_account_auth_at_seam(args)
    assert "ANTHROPIC_AUTH_TOKEN" not in os.environ  # scrubbed
    assert os.environ.get("CLAUDE_CONFIG_DIR") == "/x/.claude"  # overlay applied


def test_dispatch_account_refuses_a_harness_mismatch(monkeypatch: pytest.MonkeyPatch) -> None:
    """The cutover carrier must agree with the binary it spawns.

    A codex record's CODEX_HOME handed to a claude spawn authenticates nothing
    and launches the wrong binary - the exact miss this flag exists to prevent,
    so the record's harness is verified rather than trusted from the caller.
    """
    from types import SimpleNamespace

    from fno.agents import spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(
        "fno.adapters.providers.loader.load_providers",
        lambda *a, **k: SimpleNamespace(
            by_id={"codex-acct": SimpleNamespace(harness="codex")}
        ),
    )
    monkeypatch.setattr(
        "fno.adapters.providers.dispatch.dispatch_env",
        lambda *a, **k: pytest.fail("staged an overlay for the wrong harness"),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w", "hi", "--harness", "claude", "--headless",
         "--dispatch-account", "codex-acct"],
    )
    assert result.exit_code == 2, result.output
    assert "codex account but the spawn resolves claude" in result.output


def test_dispatch_account_refuses_an_unregistered_record(monkeypatch: pytest.MonkeyPatch) -> None:
    from types import SimpleNamespace

    from fno.agents import spawn_gate

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(
        "fno.adapters.providers.loader.load_providers",
        lambda *a, **k: SimpleNamespace(by_id={}),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "--name", "w", "hi", "--harness", "codex", "--headless",
         "--dispatch-account", "ghost"],
    )
    assert result.exit_code == 2, result.output
    assert "not a registered provider record" in result.output
