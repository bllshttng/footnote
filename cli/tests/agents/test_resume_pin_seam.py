"""The Python spawn seam: an unpinned claude resume asks the Rust resume-pin
owner, pins the argv or refuses by name (x-9db4).

The transport stays hermetic through conftest's ``_hermetic_resume_pin``; the
tests here re-stub it only where the subject is the transport itself. The one
``@requires_rust`` case runs the real dev binary, so the owner's own answer
shape (refusal text, route lookup) is exercised end to end.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from fno.paths_testing import use_tmpdir

LINEAGE_UUID = "11111111-aaaa-bbbb-cccc-000000000001"
ROUTE_UUID = "11111111-aaaa-bbbb-cccc-000000000002"

ROUTE_ENV = {
    "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
    "ANTHROPIC_AUTH_TOKEN": "zai-secret-token",
    "ANTHROPIC_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-5.2",
    "ANTHROPIC_DEFAULT_FABLE_MODEL": "glm-5.2",
}


def _home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    for key in ("FNO_AGENT_SELF", "FNO_AGENT_HARNESS", "FNO_AGENT_SESSION"):
        monkeypatch.delenv(key, raising=False)


def _seed_row(
    name: str,
    uuid: str,
    *,
    requested_model: str | None = None,
    provider: str | None = None,
    route_settings_path: str | None = None,
):
    from fno.agents.registry import AgentEntry, update_registry

    row = AgentEntry(
        name=name,
        harness="claude",
        cwd="/tmp",
        log_path="/tmp/lineage.log",
        short_id="aaa11111",
        harness_session_id=uuid,
        requested_model=requested_model,
        provider=provider,
        route_settings_path=route_settings_path,
    )
    update_registry(lambda entries: entries + [row])


def _spawn_resume(tmp_path, monkeypatch, **kwargs):
    """Run dispatch_spawn with the claude bg_create stubbed; return
    (result, captured bg_create kwargs)."""
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.dispatch import dispatch_spawn

    captured: dict[str, Any] = {}
    monkeypatch.setattr(dispatch_mod.time, "sleep", lambda s: None)

    def fake_bg_create(**kw):
        captured.update(kw)
        return SimpleNamespace(session_id_out="bbbb2222", duration_ms=1)

    monkeypatch.setattr("fno.agents.harnesses.claude.bg_create", fake_bg_create)
    result = dispatch_spawn(
        name="wk-wakefork",
        message="continue",
        harness="claude",
        cwd=tmp_path,
        **kwargs,
    )
    return result, captured


def test_ac4_hp_unpinned_fork_pins_the_row_request(tmp_path, monkeypatch):
    _home(tmp_path, monkeypatch)
    _seed_row("wk-lineage", LINEAGE_UUID, requested_model="claude-opus-5")
    result, captured = _spawn_resume(
        tmp_path, monkeypatch, resume_session_id=LINEAGE_UUID
    )
    assert captured["model"] == "claude-opus-5"
    assert captured["resume_session_id"] == LINEAGE_UUID

    from fno.agents.registry import load_registry

    minted = [e for e in load_registry() if e.name == "wk-wakefork"]
    assert minted and minted[0].requested_model == "claude-opus-5"


def test_ac4_err_rowless_request_refuses_and_launches_nothing(tmp_path, monkeypatch):
    _home(tmp_path, monkeypatch)
    _seed_row("wk-lineage", LINEAGE_UUID)  # no model axis on the row
    from fno.agents.fork_lineage import ResumeUnpinned

    with pytest.raises(ResumeUnpinned) as excinfo:
        _spawn_resume(tmp_path, monkeypatch, resume_session_id=LINEAGE_UUID)
    assert excinfo.value.exit_code == 2

    from fno.agents.registry import load_registry

    assert [e for e in load_registry() if e.name == "wk-wakefork"] == []


def test_ac5_hp_routed_resume_records_route_model_without_pinning_argv(
    tmp_path, monkeypatch
):
    _home(tmp_path, monkeypatch)
    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    from fno.agents.spawn_gate import run_gate

    route_file = tmp_path / "route-settings" / "zai-route.json"
    route_file.parent.mkdir(exist_ok=True)
    route_file.write_text(json.dumps({"env": dict(ROUTE_ENV)}))
    _seed_row(
        "wk-lineage",
        ROUTE_UUID,
        requested_model="glm-5.2",
        provider="zai",
        route_settings_path=str(route_file),
    )
    result, captured = _spawn_resume(
        tmp_path,
        monkeypatch,
        resume_session_id=ROUTE_UUID,
        route_provider="zai",
        provider_gate=run_gate("wk-wakefork", "bg", route_provider="zai"),
    )
    # The route owns the argv model; the resolver's answer is recorded only.
    assert captured["model"] is None
    assert captured["route_env"] is not None

    from fno.agents.registry import load_registry

    minted = [e for e in load_registry() if e.name == "wk-wakefork"]
    assert minted and minted[0].requested_model == "glm-5.2"


def test_ac6_hp_explicit_model_skips_the_resolver(tmp_path, monkeypatch):
    _home(tmp_path, monkeypatch)
    _seed_row("wk-lineage", LINEAGE_UUID, requested_model="claude-opus-5")
    from fno.agents import fork_lineage

    def _must_not_ask(*a, **kw):
        raise AssertionError("resolver asked despite an explicit --model")

    monkeypatch.setattr(fork_lineage, "resume_axes", _must_not_ask)
    result, captured = _spawn_resume(
        tmp_path,
        monkeypatch,
        resume_session_id=LINEAGE_UUID,
        model="claude-opus-9",
    )
    assert captured["model"] == "claude-opus-9"


def test_ac6_edge_unavailable_owner_refuses_never_defaults(tmp_path, monkeypatch):
    _home(tmp_path, monkeypatch)
    _seed_row("wk-lineage", LINEAGE_UUID, requested_model="claude-opus-5")
    from fno.agents import fork_lineage
    from fno.agents.spawn_axes_client import SpawnAxesUnavailable

    def _dead(payload):
        raise SpawnAxesUnavailable("fno-agents binary missing")

    monkeypatch.setattr(fork_lineage, "spawn_axes_call", _dead)
    from fno.agents.fork_lineage import ResumeUnpinned

    with pytest.raises(ResumeUnpinned) as excinfo:
        _spawn_resume(tmp_path, monkeypatch, resume_session_id=LINEAGE_UUID)
    assert "unavailable" in str(excinfo.value)


requires_rust = pytest.mark.skipif(
    __import__("fno.rust_binary", fromlist=["find_dev_binary"]).find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


def _transcript(tmp_path: Path, uuid: str, model_id: str, marketing: str | None) -> None:
    proj = tmp_path / "projects" / "-tmp-proj"
    proj.mkdir(parents=True, exist_ok=True)
    identity = {
        "modelId": model_id,
        "marketingName": marketing,
        "knowledgeCutoff": None,
    }
    line = {
        "type": "attachment",
        "attachment": {"type": "model", "identity": identity, "text": "powered by"},
    }
    (proj / f"{uuid}.jsonl").write_text(json.dumps(line) + "\n")


@requires_rust
def test_live_owner_refuses_unserved_model_and_answers_default_served(
    tmp_path, monkeypatch
):
    """The real resume-pin owner: a glm transcript over a zai route file
    refuses naming -P zai; an Anthropic transcript no route file names
    answers its model (the route-dir miss reads unknown, and the Opus
    marketing name settles it)."""
    from fno.rust_binary import verb_call

    from fno.agents import fork_lineage
    from fno.agents.spawn_axes_client import SpawnAxesUnavailable

    _home(tmp_path, monkeypatch)
    monkeypatch.setattr(
        fork_lineage,
        "spawn_axes_call",
        lambda payload: verb_call("spawn-axes", payload, SpawnAxesUnavailable),
    )
    _transcript(tmp_path, LINEAGE_UUID, "glm-5.3-flash[1m]", None)
    monkeypatch.setenv("FNO_CLAUDE_PROJECTS_DIR", str(tmp_path / "projects"))
    routes = tmp_path / "routes"
    routes.mkdir()
    (routes / "zai-glm.json").write_text(
        json.dumps({"env": {"ANTHROPIC_MODEL": "glm-5.3-flash[1m]", "FNO_ROUTE_PROVIDER": "zai"}})
    )
    monkeypatch.setenv("FNO_ROUTE_SETTINGS_DIR", str(routes))

    with pytest.raises(fork_lineage.ResumeUnpinned) as excinfo:
        fork_lineage.resume_axes(None, LINEAGE_UUID, None, None, routed=False)
    assert "-P zai -m 'glm-5.3-flash[1m]'" in str(excinfo.value)

    opus_uuid = "11111111-aaaa-bbbb-cccc-000000000009"
    _transcript(tmp_path, opus_uuid, "claude-opus-5", "Opus 5")
    model, effort, route_model = fork_lineage.resume_axes(
        None, opus_uuid, None, None, routed=False
    )
    assert model == "claude-opus-5"
