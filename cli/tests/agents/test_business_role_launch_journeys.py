"""Real manifest discovery reaches each worker and pane launch boundary."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
from types import SimpleNamespace
from typing import Any

import pytest

from fno.company.contracts import FunctionRef, RoleRef
from fno.config import ModelRoutingBlock
from fno.paths_testing import use_tmpdir
from fno.roles import (
    AuthorityCeiling,
    DeliveryPolicy,
    ReviewPolicy,
    RoleDefinitionSource,
    RoleLayer,
    RoleManifest,
    RoutingHint,
)


OPENAI_PROVIDER = {
    "oai": {
        "protocol": "openai",
        "base_url": "https://example.test/v1",
        "api_key_env": "OPENAI_API_KEY",
    }
}


def _write_role(root: Path, *, provider: str, model: str) -> None:
    role = RoleRef(id="publisher", function_id="communications")
    source = RoleDefinitionSource(
        layer=RoleLayer.COMPANY,
        source_id="company/publisher.json",
        snapshot_revision="snapshot-1",
        role=role,
        manifest=RoleManifest(
            role=role,
            function=FunctionRef(id=role.function_id),
            mission="Publish one bounded artifact.",
            deliverable_kinds=("brief",),
            authority_ceiling=AuthorityCeiling.INTERNAL,
            review_policy=ReviewPolicy(required=True, minimum_reviewers=1),
            delivery_policy=DeliveryPolicy(required_evidence=("artifact",)),
            default_topology="direct",
            routing_hint=RoutingHint(provider=provider, model=model),
        ),
    )
    path = root / "company" / "publisher.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(source.model_dump(mode="json")), encoding="utf-8")


def _write_invalid_role(root: Path) -> None:
    path = root / "company" / "publisher.json"
    path.parent.mkdir(parents=True)
    path.write_text("{not-json", encoding="utf-8")


class _MuxRunner:
    def __init__(self) -> None:
        self.calls: list[list[str]] = []

    def __call__(self, argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        self.calls.append(list(argv))
        if argv[1:4] == ["mux", "pane", "run"]:
            return subprocess.CompletedProcess(argv, 0, "7\n", "")
        if argv[1:4] == ["mux", "pane", "ls"]:
            return subprocess.CompletedProcess(
                argv,
                0,
                json.dumps(
                    [
                        {
                            "pane_id": 7,
                            "squad_id": 1,
                            "tab_id": 1,
                            "cwd": "/work",
                            "child_pid": 4242,
                        }
                    ]
                ),
                "",
            )
        # x-cdca: a codex pane spawn now probes liveness and retains the pane's
        # output while it waits for a session binding. 11 = the pane is up.
        if argv[1:4] == ["mux", "pane", "wait"]:
            return subprocess.CompletedProcess(argv, 11, "", "")
        if argv[1:4] == ["mux", "pane", "read"]:
            return subprocess.CompletedProcess(argv, 0, "", "")
        raise AssertionError(f"unexpected mux call: {argv}")

    def launched_argv(self) -> list[str]:
        call = next(item for item in self.calls if item[1:4] == ["mux", "pane", "run"])
        return call[call.index("--") + 1 :]


def _configure_codex(monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.agents import model_routing

    monkeypatch.setenv("OPENAI_API_KEY", "openai-key")
    monkeypatch.setattr(
        model_routing,
        "_routing_block",
        lambda settings: ModelRoutingBlock(providers=OPENAI_PROVIDER),
    )


def test_real_manifest_reaches_claude_worker_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents.harnesses import claude

    use_tmpdir(monkeypatch, tmp_path)
    root = tmp_path / "roles"
    _write_role(root, provider="zai", model="business-model")
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    monkeypatch.setenv("ZAI_API_KEY", "zai-key")
    captured: dict[str, Any] = {}

    def fake_run(argv: list[str], **kwargs: Any) -> SimpleNamespace:
        captured.update(argv=argv, env=kwargs["env"])
        return SimpleNamespace(
            returncode=0,
            stdout="backgrounded · abc12345 · ok\n",
            stderr="",
        )

    monkeypatch.setattr(claude, "_subprocess_run", fake_run)
    claude.bg_create(name="publisher-worker", message="work", cwd=tmp_path, role="publisher")

    assert captured["env"]["ANTHROPIC_MODEL"] == "business-model"
    assert captured["env"]["ANTHROPIC_AUTH_TOKEN"] == "zai-key"


def test_real_manifest_reaches_codex_worker_argv_and_env(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents.harnesses import codex

    use_tmpdir(monkeypatch, tmp_path)
    root = tmp_path / "roles"
    _write_role(root, provider="oai", model="gpt-business")
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    _configure_codex(monkeypatch)
    captured: dict[str, Any] = {}

    def fake_run(**kwargs: Any) -> codex.CodexResult:
        captured.update(kwargs)
        return codex.CodexResult(0, "session-id", "ok", 1)

    monkeypatch.setattr(codex, "_run_codex", fake_run)
    codex.create(
        cwd=tmp_path,
        prompt="work",
        from_name="orchestrator",
        yolo=False,
        output_path=tmp_path / "output.jsonl",
        role="publisher",
    )

    assert "model='gpt-business'" in " ".join(captured["argv"])
    assert captured["route_env"] == {"OPENAI_API_KEY": "openai-key"}


@pytest.mark.parametrize(
    ("provider", "route_provider", "model", "env_key"),
    [
        ("claude", "zai", "business-model", "ANTHROPIC_MODEL=business-model"),
        ("codex", "oai", "gpt-business", "OPENAI_API_KEY=openai-key"),
    ],
)
def test_real_manifest_reaches_pane_launch_argv(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    provider: str,
    route_provider: str,
    model: str,
    env_key: str,
) -> None:
    from fno.agents import mux_spawn

    use_tmpdir(monkeypatch, tmp_path)
    root = tmp_path / "roles"
    _write_role(root, provider=route_provider, model=model)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    if provider == "claude":
        monkeypatch.setenv("ZAI_API_KEY", "zai-key")
    else:
        _configure_codex(monkeypatch)
        monkeypatch.setattr(mux_spawn, "_backfill_codex_session_id", lambda *a, **k: None)
    runner = _MuxRunner()

    name = f"{provider}-publisher-pane"
    gate = None
    if provider == "claude":
        from fno.agents.spawn_gate import run_gate

        monkeypatch.setenv("FNO_SPAWN_GATE", "0")
        gate = run_gate(name, "pane", route_provider=route_provider)
    try:
        mux_spawn.dispatch_spawn_pane(
            name=name,
            message="work",
            provider=provider,
            cwd=tmp_path,
            role="publisher",
            runner=runner,
            provider_gate=gate,
        )
    finally:
        if gate is not None:
            gate.release()

    launched = runner.launched_argv()
    assert env_key in launched
    if provider == "codex":
        assert "model='gpt-business'" in " ".join(launched)


@pytest.mark.parametrize("lane", ["claude-worker", "codex-worker", "claude-pane", "codex-pane"])
def test_invalid_real_manifest_refuses_before_any_launch(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    lane: str,
) -> None:
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.model_routing import RouteCompositionError
    from fno.agents import mux_spawn
    from fno.agents.harnesses import claude, codex
    from fno.agents.registry import load_registry

    use_tmpdir(monkeypatch, tmp_path)
    root = tmp_path / "roles"
    _write_invalid_role(root)
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    launched: list[object] = []
    monkeypatch.setattr(
        claude,
        "_subprocess_run",
        lambda *a, **k: launched.append((a, k)) or pytest.fail("claude launched"),
    )
    monkeypatch.setattr(
        codex,
        "_run_codex",
        lambda **k: launched.append(k) or pytest.fail("codex launched"),
    )

    if lane == "claude-worker":
        with pytest.raises(RouteCompositionError):
            claude.bg_create(name="invalid", message="work", cwd=tmp_path, role="publisher")
    elif lane == "codex-worker":
        _configure_codex(monkeypatch)
        with pytest.raises(RouteCompositionError):
            codex.create(
                cwd=tmp_path,
                prompt="work",
                from_name="orchestrator",
                yolo=False,
                output_path=tmp_path / "output.jsonl",
                role="publisher",
            )
    else:
        if lane == "codex-pane":
            _configure_codex(monkeypatch)
        runner = _MuxRunner()
        with pytest.raises(DispatchAskError):
            mux_spawn.dispatch_spawn_pane(
                name=lane,
                message="work",
                provider=lane.removesuffix("-pane"),
                cwd=tmp_path,
                role="publisher",
                runner=runner,
            )
        assert runner.calls == []
        assert load_registry() == []

    assert launched == []
