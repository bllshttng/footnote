"""dispatch_spawn / cmd_spawn thread the routing role to the create path (x-d2fe).

The provider boundary (bg_create) is covered by test_provider_role_routing.py;
these guards pin the wiring above it so a future refactor cannot silently drop
the ``role`` kwarg between the CLI flag and the claude create path.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


def _setup_tmp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    for k in ("FNO_AGENT_SELF", "FNO_AGENT_PROVIDER", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(k, raising=False)


def test_dispatch_spawn_threads_role_to_create_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn

    captured: Dict[str, Any] = {}

    def fake_create(**kw: Any) -> DispatchAskResult:
        captured.update(kw)
        return DispatchAskResult(kind="create", short_id="abc12345")

    monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)

    result = dispatch_spawn(
        name="dreamer",
        message="consolidate memory",
        provider="claude",
        cwd=tmp_path,
        role="consolidate",
    )
    assert result.kind == "created"
    assert captured["role"] == "consolidate"


def test_dispatch_spawn_defaults_role_to_none(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn

    captured: Dict[str, Any] = {}

    def fake_create(**kw: Any) -> DispatchAskResult:
        captured.update(kw)
        return DispatchAskResult(kind="create", short_id="abc12345")

    monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)

    dispatch_spawn(
        name="builder",
        message="build it",
        provider="claude",
        cwd=tmp_path,
    )
    # Regression guard: the default spawn passes role=None (today's behavior).
    assert captured["role"] is None


def test_direct_dispatch_spawn_refuses_managed_role_route(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The in-process spawn API cannot bypass atomic route composition."""
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import model_routing
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    monkeypatch.setattr(
        model_routing,
        "resolve_route",
        lambda *_a, **_k: {
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "secret",
            "ANTHROPIC_MODEL": "glm-5.2",
        },
    )

    with pytest.raises(DispatchAskError, match="managed OAuth provider 'makers'"):
        dispatch_spawn(
            name="direct-route",
            message="work",
            provider="claude",
            cwd=tmp_path,
            role="tidy",
        )


def test_direct_pane_spawn_refuses_managed_route_before_mux(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The direct pane API crosses the same composition refusal."""
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")

    def unexpected_runner(*_a: Any, **_k: Any) -> Any:
        pytest.fail("managed route must refuse before mux spawn")

    with pytest.raises(DispatchAskError, match="managed OAuth provider 'makers'"):
        dispatch_spawn_pane(
            name="direct-pane-route",
            message="work",
            provider="claude",
            cwd=tmp_path,
            role="tidy",
            route_env={
                "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
                "ANTHROPIC_AUTH_TOKEN": "secret",
                "ANTHROPIC_MODEL": "glm-5.2",
            },
            runner=unexpected_runner,
        )


@pytest.mark.parametrize("adapter", ["bg_create", "headless_create"])
def test_direct_claude_adapter_refuses_managed_route_before_subprocess(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, adapter: str
) -> None:
    """Even a direct provider-adapter call crosses the composition refusal."""
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents.model_routing import RouteCompositionError
    from fno.agents.providers import claude

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    monkeypatch.setattr(
        claude,
        "_subprocess_run",
        lambda *_a, **_k: pytest.fail("refusal must precede the claude subprocess"),
    )
    route = {
        "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "secret",
        "ANTHROPIC_MODEL": "glm-5.2",
    }
    kwargs: dict[str, Any] = {
        "message": "work",
        "cwd": tmp_path,
        "route_env": route,
    }
    if adapter == "bg_create":
        kwargs["name"] = "direct-bg"

    with pytest.raises(RouteCompositionError, match="managed OAuth provider 'makers'"):
        getattr(claude, adapter)(**kwargs)


class _Gate:
    def release(self) -> None:
        pass


@pytest.mark.parametrize(
    ("substrate", "extra"),
    [
        ("pane", []),
        ("bg", ["--substrate", "bg"]),
        ("headless", ["--substrate", "headless"]),
    ],
)
def test_cmd_spawn_resolves_role_route_once_before_substrate_fanout(
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
    extra: list[str],
) -> None:
    """Every routed CLI spawn carries one pre-resolved endpoint/auth/model unit."""
    from fno.agents import dispatch, model_routing, mux_spawn, spawn_gate

    route = {
        "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "secret",
        "ANTHROPIC_MODEL": "glm-5.2",
    }
    resolutions: list[str | None] = []
    received: dict[str, Any] = {}

    def resolve(role: str | None, **_kwargs: Any) -> dict[str, str]:
        resolutions.append(role)
        return route

    monkeypatch.setattr(model_routing, "resolve_route", resolve)
    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda *a, **k: {})
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **kwargs: received.update(kwargs)
        or mux_spawn.MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="s",
            pane_id=1,
            child_pid=None,
            session_uuid=None,
        ),
    )
    monkeypatch.setattr(
        dispatch,
        "dispatch_spawn",
        lambda **kwargs: received.update(kwargs)
        or dispatch.SpawnResult(
            kind="once" if substrate == "headless" else "created",
            name=kwargs["name"],
            provider=kwargs["provider"],
            short_id="abcd1234",
            reply="ok" if substrate == "headless" else None,
        ),
    )

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn",
            "--name",
            f"route-{substrate}",
            "--harness",
            "claude",
            "--role",
            "tidy",
            "--here",
            *extra,
            "work",
        ],
    )

    assert result.exit_code == 0, result.output
    assert resolutions == ["tidy"]
    assert received["route_env"] == route


@pytest.mark.parametrize(
    ("routing", "extra", "intent"),
    [
        (["--role", "tidy"], [], "routed role 'tidy'"),
        (["--role", "tidy"], ["--substrate", "bg"], "routed role 'tidy'"),
        (["--role", "tidy"], ["--substrate", "headless"], "routed role 'tidy'"),
        (["--route", "zai,glm-5.2"], ["--substrate", "bg"], "route 'zai,glm-5.2'"),
        (
            ["--route", "zai,glm-5.2"],
            ["--substrate", "headless"],
            "route 'zai,glm-5.2'",
        ),
    ],
    ids=["role-pane", "role-bg", "role-headless", "route-bg", "route-headless"],
)
def test_cmd_spawn_refuses_role_route_over_managed_oauth_overlay_before_gate(
    monkeypatch: pytest.MonkeyPatch,
    routing: list[str],
    extra: list[str],
    intent: str,
) -> None:
    """The provider snapshot cannot half-compose with a routed role."""
    from fno.agents import dispatch, model_routing, mux_spawn, spawn_gate

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    route = {
        "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "secret",
        "ANTHROPIC_MODEL": "glm-5.2",
    }
    monkeypatch.setattr(model_routing, "resolve_route", lambda *_a, **_k: route)
    monkeypatch.setattr(model_routing, "resolve_explicit_route", lambda *_a, **_k: route)
    gate_calls: list[object] = []
    spawn_calls: list[object] = []
    monkeypatch.setattr(
        spawn_gate, "run_gate", lambda *a, **k: gate_calls.append(object()) or _Gate()
    )
    monkeypatch.setattr(
        mux_spawn, "dispatch_spawn_pane", lambda **kwargs: spawn_calls.append(kwargs)
    )
    monkeypatch.setattr(
        dispatch, "dispatch_spawn", lambda **kwargs: spawn_calls.append(kwargs)
    )

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn",
            "--name",
            "unsafe-route",
            "--harness",
            "claude",
            *routing,
            "--here",
            *extra,
            "work",
        ],
    )

    assert result.exit_code == 2, result.output
    assert "managed OAuth provider 'makers'" in result.output
    assert f"refusing {intent}" in result.output
    assert gate_calls == []
    assert spawn_calls == []
