"""dispatch_spawn / cmd_spawn thread the routing role to the create path (x-d2fe).

The provider boundary (bg_create) is covered by test_harness_role_routing.py;
these guards pin the wiring above it so a future refactor cannot silently drop
the ``role`` kwarg between the CLI flag and the claude create path.
"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import Any, Dict

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


def _setup_tmp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    for k in ("FNO_AGENT_SELF", "FNO_AGENT_HARNESS", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(k, raising=False)


def _route_unit(model: str = "glm-5.2") -> dict[str, str]:
    """A resolved route as the real resolver emits it: endpoint, auth, and
    every model tier as one unit (resolve_spawn_route refuses less)."""
    from fno.agents.model_routing import MODEL_ENV_KEYS

    return {
        "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "secret",
        **{k: model for k in MODEL_ENV_KEYS},
    }


def _resolved_zai(route: dict[str, str]):
    def resolve(*_args: Any, **kwargs: Any) -> dict[str, str]:
        callback = kwargs.get("resolved_provider")
        if callback is not None:
            callback("zai")
        return route

    return resolve


def _zai_admission(monkeypatch, name: str, substrate: str = "bg"):
    from fno.agents.spawn_gate import run_gate

    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    return run_gate(name, substrate, route_provider="zai")


def test_dispatch_spawn_threads_captured_role_route_to_create_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod, model_routing
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn

    captured: Dict[str, Any] = {}

    def fake_create(**kw: Any) -> DispatchAskResult:
        captured.update(kw)
        return DispatchAskResult(kind="create", short_id="abc12345")

    monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)
    route = _route_unit("business-model")
    monkeypatch.setattr(model_routing, "resolve_route", _resolved_zai(route))

    result = dispatch_spawn(
        name="dreamer",
        message="consolidate memory",
        provider="claude",
        cwd=tmp_path,
        role="consolidate",
        provider_gate=_zai_admission(monkeypatch, "dreamer"),
    )
    assert result.kind == "created"
    assert captured["role"] is None
    assert captured["route_env"] == route


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


def test_direct_dispatch_spawn_composes_managed_role_route(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A complete route composes even under an inherited managed marker.

    The managed-OAuth ambient guard is gone (measured 2026-08-15: claude
    prefers an env credential over a Keychain login, so a self-authed route
    leaves the managed login dormant). FNO_PROVIDER_AUTH describes the CALLING
    process's slot, not this spawn, so it must not refuse anything.
    """
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import dispatch as dispatch_mod, model_routing
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn

    route = _route_unit()
    captured: Dict[str, Any] = {}

    def fake_create(**kw: Any) -> DispatchAskResult:
        captured.update(kw)
        return DispatchAskResult(kind="create", short_id="abc12345")

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    monkeypatch.setattr(model_routing, "resolve_route", _resolved_zai(route))
    monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)

    result = dispatch_spawn(
        name="direct-route",
        message="work",
        provider="claude",
        cwd=tmp_path,
        role="tidy",
        provider_gate=_zai_admission(monkeypatch, "direct-route"),
    )
    assert result.kind == "created"
    assert captured["route_env"] == route


def test_direct_pane_spawn_composes_managed_route_before_mux(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The direct pane API composes the same route the bg lane does."""
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents.model_routing import bind_route_provider
    from fno.agents.mux_spawn import dispatch_spawn_pane

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")

    launched: list[list[str]] = []

    def runner(argv: list[str], **_k: Any) -> Any:
        launched.append(list(argv))
        if argv[1:4] == ["mux", "pane", "run"]:
            return SimpleNamespace(returncode=0, stdout="7\n", stderr="")
        if argv[1:4] == ["mux", "pane", "ls"]:
            return SimpleNamespace(returncode=0, stdout="[]", stderr="")
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    dispatch_spawn_pane(
        name="direct-pane-route",
        message="work",
        provider="claude",
        cwd=tmp_path,
        role="tidy",
        route_env=bind_route_provider(_route_unit(), "zai"),
        route_provider="zai",
        provider_gate=_zai_admission(monkeypatch, "direct-pane-route", "pane"),
        runner=runner,
    )
    run_argv = next(a for a in launched if a[1:4] == ["mux", "pane", "run"])
    assert "ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic" in run_argv
    assert "ANTHROPIC_AUTH_TOKEN=secret" in run_argv


@pytest.mark.parametrize("substrate", ["worker", "pane"])
def test_pre_resolved_route_without_provider_axis_refuses_before_launch(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane

    harness = "claude"
    with pytest.raises(
        DispatchAskError, match="no bound model-provider identity"
    ) as exc_info:
        if substrate == "worker":
            dispatch_spawn(
                name="missing-provider-worker",
                message="work",
                provider=harness,
                cwd=tmp_path,
                route_env=_route_unit(),
            )
        else:
            dispatch_spawn_pane(
                name="missing-provider-pane",
                message="work",
                provider=harness,
                cwd=tmp_path,
                route_env=_route_unit(),
                runner=lambda *_args, **_kwargs: pytest.fail(
                    "refusal must precede pane launch"
                ),
            )

    assert exc_info.value.exit_code == 2
    assert "cap cannot be evaluated" in str(exc_info.value)


@pytest.mark.parametrize("substrate", ["worker", "pane"])
def test_resolved_provider_must_match_admitted_provider(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    from fno.agents import dispatch as dispatch_mod, model_routing
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane
    from fno.agents.spawn_gate import run_gate

    route = _route_unit()
    monkeypatch.setattr(model_routing, "resolve_route", _resolved_zai(route))
    monkeypatch.setattr(
        dispatch_mod,
        "_claude_create_path",
        lambda **_kwargs: pytest.fail("refusal must precede worker launch"),
    )
    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    gate = run_gate("wrong-provider", "bg", route_provider="openai")
    harness = "claude"

    with pytest.raises(DispatchAskError, match="resolved provider.*zai.*openai"):
        if substrate == "worker":
            dispatch_spawn(
                name="wrong-provider-worker",
                message="work",
                provider=harness,
                cwd=tmp_path,
                role="tidy",
                route_provider="openai",
                provider_gate=gate,
            )
        else:
            dispatch_spawn_pane(
                name="wrong-provider-pane",
                message="work",
                provider=harness,
                cwd=tmp_path,
                role="tidy",
                route_provider="openai",
                provider_gate=gate,
                runner=lambda *_args, **_kwargs: pytest.fail(
                    "refusal must precede pane launch"
                ),
            )


def test_provider_admission_is_single_use_and_name_bound(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    from fno.agents import dispatch as dispatch_mod, model_routing
    from fno.agents.dispatch import DispatchAskError, DispatchAskResult, dispatch_spawn

    monkeypatch.setattr(
        model_routing, "resolve_route", _resolved_zai(_route_unit())
    )
    monkeypatch.setattr(
        dispatch_mod,
        "_claude_create_path",
        lambda **_kwargs: DispatchAskResult(kind="create", short_id="abc12345"),
    )
    gate = _zai_admission(monkeypatch, "single-use")
    harness = "claude"

    dispatch_spawn(
        name="single-use",
        message="first",
        provider=harness,
        cwd=tmp_path,
        role="tidy",
        provider_gate=gate,
    )
    with pytest.raises(DispatchAskError, match="no matching admission token"):
        dispatch_spawn(
            name="single-use",
            message="second",
            provider=harness,
            cwd=tmp_path,
            role="tidy",
            provider_gate=gate,
        )


@pytest.mark.parametrize("substrate", ["worker", "pane"])
def test_raw_route_env_cannot_borrow_another_provider_admission(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane
    from fno.agents.spawn_gate import run_gate

    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    name = f"raw-route-{substrate}"
    gate = run_gate(
        name,
        "pane" if substrate == "pane" else "bg",
        route_provider="openai",
    )
    harness = "claude"

    with pytest.raises(DispatchAskError, match="no bound model-provider identity"):
        if substrate == "worker":
            dispatch_spawn(
                name=name,
                message="work",
                provider=harness,
                cwd=tmp_path,
                route_env=_route_unit(),
                route_provider="openai",
                provider_gate=gate,
            )
        else:
            dispatch_spawn_pane(
                name=name,
                message="work",
                provider=harness,
                cwd=tmp_path,
                route_env=_route_unit(),
                route_provider="openai",
                provider_gate=gate,
                runner=lambda *_args, **_kwargs: pytest.fail(
                    "refusal must precede pane launch"
                ),
            )


@pytest.mark.parametrize("substrate", ["worker", "pane"])
def test_routed_direct_spawn_without_admission_token_refuses(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    from fno.agents import model_routing
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane

    monkeypatch.setattr(
        model_routing, "resolve_route", _resolved_zai(_route_unit())
    )
    harness = "claude"
    with pytest.raises(DispatchAskError, match="no matching admission token"):
        if substrate == "worker":
            dispatch_spawn(
                name="ungated-worker",
                message="work",
                provider=harness,
                cwd=tmp_path,
                role="tidy",
            )
        else:
            dispatch_spawn_pane(
                name="ungated-pane",
                message="work",
                provider=harness,
                cwd=tmp_path,
                role="tidy",
                runner=lambda *_args, **_kwargs: pytest.fail(
                    "refusal must precede pane launch"
                ),
            )


@pytest.mark.parametrize("substrate", ["worker", "pane"])
def test_tier_remap_preflight_normalizes_business_role_refusal(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents import model_routing
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane

    def blocked(*args: Any, **kwargs: Any) -> None:
        raise model_routing.BusinessRoleRoutingProjectionError(
            "invalid business role during tier preflight"
        )

    monkeypatch.setattr(model_routing, "check_spawn_tier_remap", blocked)

    with pytest.raises(
        DispatchAskError,
        match="invalid business role during tier preflight",
    ) as exc_info:
        if substrate == "worker":
            dispatch_spawn(
                name="tier-worker",
                message="work",
                provider="claude",
                cwd=tmp_path,
                role="publisher",
                model="opus",
            )
        else:
            dispatch_spawn_pane(
                name="tier-pane",
                message="work",
                provider="claude",
                cwd=tmp_path,
                role="publisher",
                model="opus",
                runner=lambda *_args, **_kwargs: pytest.fail(
                    "refusal must precede pane launch"
                ),
            )

    assert exc_info.value.exit_code == 2


@pytest.mark.parametrize("substrate", ["worker", "pane"])
def test_tier_remap_preflight_normalizes_actual_conflict(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    monkeypatch.setenv("ANTHROPIC_DEFAULT_OPUS_MODEL", "glm-5.2")

    from fno.agents.dispatch import DispatchAskError, dispatch_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane

    with pytest.raises(DispatchAskError, match="--model opus is ambiguous") as exc_info:
        if substrate == "worker":
            dispatch_spawn(
                name="remap-worker",
                message="work",
                provider="claude",
                cwd=tmp_path,
                model="opus",
            )
        else:
            dispatch_spawn_pane(
                name="remap-pane",
                message="work",
                provider="claude",
                cwd=tmp_path,
                model="opus",
                runner=lambda *_args, **_kwargs: pytest.fail(
                    "refusal must precede pane launch"
                ),
            )

    assert exc_info.value.exit_code == 2


@pytest.mark.parametrize("substrate", ["worker", "pane"])
def test_role_route_snapshot_is_resolved_once_before_tier_preflight_and_launch(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    substrate: str,
) -> None:
    _setup_tmp_home(tmp_path, monkeypatch)
    monkeypatch.setenv("ANTHROPIC_DEFAULT_OPUS_MODEL", "glm-5.2")

    from fno.agents import dispatch as dispatch_mod
    from fno.agents import model_routing
    from fno.agents.dispatch import DispatchAskResult, dispatch_spawn
    from fno.agents.mux_spawn import dispatch_spawn_pane

    route = _route_unit("business-model")
    resolutions: list[str | None] = []

    def stateful_resolution(role: str | None, **kwargs: Any) -> dict[str, str] | None:
        resolutions.append(role)
        if len(resolutions) != 1:
            return None
        kwargs["resolved_provider"]("zai")
        return route

    monkeypatch.setattr(model_routing, "resolve_route", stateful_resolution)
    if substrate == "worker":
        captured: dict[str, Any] = {}

        def fake_create(**kwargs: Any) -> DispatchAskResult:
            captured.update(kwargs)
            return DispatchAskResult(kind="create", short_id="abc12345")

        monkeypatch.setattr(dispatch_mod, "_claude_create_path", fake_create)
        dispatch_spawn(
            name="snapshot-worker",
            message="work",
            provider="claude",
            cwd=tmp_path,
            role="publisher",
            model="opus",
            provider_gate=_zai_admission(monkeypatch, "snapshot-worker"),
        )
        assert captured["route_env"] == route
        assert captured["role"] is None
    else:
        calls: list[list[str]] = []

        def runner(argv: list[str], **kwargs: Any) -> Any:
            calls.append(list(argv))
            if argv[1:4] == ["mux", "pane", "run"]:
                return SimpleNamespace(returncode=0, stdout="7\n", stderr="")
            if argv[1:4] == ["mux", "pane", "ls"]:
                return SimpleNamespace(returncode=0, stdout="[]", stderr="")
            raise AssertionError(argv)

        dispatch_spawn_pane(
            name="snapshot-pane",
            message="work",
            provider="claude",
            cwd=tmp_path,
            role="publisher",
            model="opus",
            provider_gate=_zai_admission(monkeypatch, "snapshot-pane", "pane"),
            runner=runner,
        )
        launched = next(call for call in calls if call[1:4] == ["mux", "pane", "run"])
        assert "ANTHROPIC_MODEL=business-model" in launched

    assert resolutions == ["publisher"]


@pytest.mark.parametrize("adapter", ["bg_create", "headless_create"])
def test_direct_claude_adapter_composes_managed_route(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, adapter: str
) -> None:
    """Even a direct provider-adapter call composes a complete route: the
    managed ambient marker neither refuses nor strips the route's env."""
    _setup_tmp_home(tmp_path, monkeypatch)

    from fno.agents.harnesses import claude

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "ambient-stale")
    captured: dict[str, Any] = {}

    class _Result:
        returncode = 0
        stdout = "backgrounded · 1234abcd · started\n"
        stderr = ""

    def fake_run(_argv: list[str], **kw: Any) -> _Result:
        captured.update(kw)
        return _Result()

    monkeypatch.setattr(claude, "_subprocess_run", fake_run)
    route = _route_unit()
    kwargs: dict[str, Any] = {
        "message": "work",
        "cwd": tmp_path,
        "route_env": route,
    }
    if adapter == "bg_create":
        kwargs["name"] = "direct-bg"

    getattr(claude, adapter)(**kwargs)
    child_env = captured["env"]
    assert child_env["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert child_env["ANTHROPIC_AUTH_TOKEN"] == "secret"
    # The scrub floor: the ambient stale credential must not survive.
    assert "ANTHROPIC_API_KEY" not in child_env


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

    route = _route_unit()
    resolutions: list[str | None] = []
    received: dict[str, Any] = {}

    gate_calls: list[dict[str, Any]] = []

    def resolve(role: str | None, **kwargs: Any) -> dict[str, str]:
        resolutions.append(role)
        kwargs["resolved_provider"]("zai")
        return route

    monkeypatch.setattr(model_routing, "resolve_route", resolve)
    monkeypatch.setattr(
        spawn_gate,
        "run_gate",
        lambda *a, **k: gate_calls.append(k) or _Gate(),
    )
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
    assert received["route_provider"] == "zai"
    assert gate_calls == [{
        "force": False,
        "no_wait": False,
        "route_provider": "zai",
    }]


@pytest.mark.parametrize(
    ("routing", "extra"),
    [
        (["--role", "tidy"], []),
        (["--role", "tidy"], ["--substrate", "bg"]),
        (["--role", "tidy"], ["--substrate", "headless"]),
        (["--route", "zai,glm-5.2"], ["--substrate", "bg"]),
        (["--route", "zai,glm-5.2"], ["--substrate", "headless"]),
    ],
    ids=["role-pane", "role-bg", "role-headless", "route-bg", "route-headless"],
)
def test_cmd_spawn_composes_role_route_over_managed_oauth_overlay(
    monkeypatch: pytest.MonkeyPatch,
    routing: list[str],
    extra: list[str],
) -> None:
    """A routed spawn under an inherited managed marker composes on every
    substrate: the marker describes the CALLING process's credential slot, not
    this spawn, and a self-authed route beats a Keychain login (measured
    2026-08-15), so the old refusal is gone."""
    from fno.agents import dispatch, model_routing, mux_spawn, spawn_gate

    monkeypatch.setenv("FNO_PROVIDER_AUTH", "managed")
    monkeypatch.setenv("FNO_PROVIDER_ID", "makers")
    route = _route_unit()
    monkeypatch.setattr(model_routing, "resolve_route", _resolved_zai(route))
    monkeypatch.setattr(model_routing, "resolve_explicit_route", lambda *_a, **_k: route)
    gate_calls: list[object] = []
    spawn_calls: list[dict[str, Any]] = []
    monkeypatch.setattr(
        spawn_gate, "run_gate", lambda *a, **k: gate_calls.append(object()) or _Gate()
    )
    monkeypatch.setattr(
        mux_spawn,
        "dispatch_spawn_pane",
        lambda **kwargs: spawn_calls.append(kwargs)
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
        lambda **kwargs: spawn_calls.append(kwargs)
        or dispatch.SpawnResult(
            kind="once" if "headless" in extra else "created",
            name=kwargs["name"],
            provider=kwargs["provider"],
            short_id="abcd1234",
            reply="ok" if "headless" in extra else None,
        ),
    )

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(
        agents_app,
        [
            "spawn",
            "--name",
            "composed-route",
            "--harness",
            "claude",
            *routing,
            "--here",
            *extra,
            "work",
        ],
    )

    assert result.exit_code == 0, result.output
    assert len(gate_calls) == 1
    assert len(spawn_calls) == 1
    assert spawn_calls[0]["route_env"] == route
