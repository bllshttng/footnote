"""US8 spawn-seam injector: config.agents.defaults -> argv (x-de9d).

Precedence explicit flag > config > builtin, resolved field-by-field. Provider
validated (exit 2 on a bad name); config-sourced effort degrades open on a
no-surface provider while an explicit --effort stays fail-closed downstream.
"""
from __future__ import annotations
import io
import json
import pytest
from fno.agents.spawn_defaults import compose_spawn_argv, resolve_lane_vendor
requires_rust = pytest.mark.dev_build
@pytest.mark.parametrize(
    ("provider", "computed_dirs", "expected"),
    [
        ("codex", ["/tmp/state", "/tmp/claims-root"], True),
        ("codex", ["/tmp/state"], False),
        ("gemini", ["/tmp/state", "/tmp/claims-root"], None),
    ],
)
def test_claim_store_writable_is_tri_state(provider, computed_dirs, expected, monkeypatch):
    from pathlib import Path

    from fno.agents import mux_spawn

    monkeypatch.setattr(
        "fno.claims.io.global_claims_root", lambda: Path("/tmp")
    )
    monkeypatch.setattr(
        "fno.claims.io.claims_dir", lambda root=None: Path("/tmp/claims-root")
    )

    assert mux_spawn._claim_store_writable(provider, computed_dirs) is expected
def test_is_verb_seed_is_the_first_token_fire_test_over_both_sigils():
    """x-413d: the fire test accepts both sigils at index 0 and still reads a
    verb inside prose as a conversation."""
    from fno.config._dispatch_verbs import is_verb_seed

    assert is_verb_seed("$fno:target x-caf8") is True
    assert is_verb_seed("/fno:target x-caf8") is True
    assert is_verb_seed("do a /fno:blueprint") is False
    assert is_verb_seed("/absolute/path/to/thing") is False
    assert is_verb_seed("review /fno:target x-1") is False
def test_canonical_verb_key_accepts_both_sigils():
    """x-c976: the resolver's canonical answer no longer depends on which
    sigil named the verb."""
    from fno.config._dispatch_verbs import canonical_verb_key

    assert canonical_verb_key("$fno:target") == "/target"
    assert canonical_verb_key("/fno:target") == "/target"
    assert canonical_verb_key("target") == "/target"
    assert canonical_verb_key("$target") == "/target"
def test_canonical_verb_key_keeps_legacy_output_for_unparsed_keys():
    from fno.config._dispatch_verbs import canonical_verb_key

    assert canonical_verb_key("/Users/x") == "/Users/x"
    assert canonical_verb_key("fno:target") == "/target"
    assert canonical_verb_key("") == ""
def test_lane_vendor_resolves_unrouted_harness_from_final_argv():
    assert resolve_lane_vendor(["codex", "-C", "/tmp/workspace"]) == "openai"
_PROFILE_OVERLAY = {
    "target": {
        "permission_mode": "yolo",
        "effort": "high",
        "harness": {
            "claude": {"permission_mode": "bypassPermissions"},
            "codex": {"effort": "xhigh"},
        },
    },
}
# ---------------------------------------------------------------------------
# The transport (compose_spawn_argv): normalize, one verb call, apply.
# The composition itself is characterized by the Rust goldens
# (crates/fno-agents/tests/fixtures/spawn_compose/); these pin the transport
# contract with the verb stubbed.
# ---------------------------------------------------------------------------

class _Answer(dict):
    pass


@pytest.fixture
def overlay_answer(monkeypatch):
    calls = []

    def install(answer):
        def fake(payload, *a, **k):
            calls.append(payload)
            return dict(answer)

        monkeypatch.setattr(
            "fno.agents.spawn_overlay_client.spawn_overlay_call", fake
        )
        return calls

    return install


def test_transport_passes_non_spawn_through(overlay_answer):
    calls = overlay_answer({})
    argv = compose_spawn_argv(["think", "--fast"])
    assert argv == ["think", "--fast"]
    assert calls == []


def test_transport_help_renders_help_under_any_config(overlay_answer):
    calls = overlay_answer({})
    assert compose_spawn_argv(["spawn", "--help"]) == ["spawn", "--help"]
    assert calls == []


def test_transport_makes_exactly_one_verb_call(overlay_answer):
    calls = overlay_answer({"argv": ["spawn"], "stderr": [], "exit": 0})
    compose_spawn_argv(["spawn", "--name", "w", "/fno:target x-1"])
    assert len(calls) == 1


def test_transport_sends_the_scan_projection(overlay_answer):
    calls = overlay_answer({"argv": ["spawn"], "stderr": [], "exit": 0})
    compose_spawn_argv(
        ["spawn", "--name", "w", "-H", "codex", "-m", "m1", "--yolo",
         "/fno:target x-1"]
    )
    scan = calls[0]["scan"]
    assert scan["has_harness"] is True
    assert scan["explicit_harness"] == "codex"
    assert scan["has_model"] is True
    assert scan["permission_value"] == "yolo"
    assert scan["seed"] == "/fno:target x-1"
    assert scan["name"] == "w"
    assert calls[0]["permission_builtin"] == "bypassPermissions"
    assert calls[0]["kind"] == "compose"


def test_transport_applies_stderr_and_raises_the_verb_exit(overlay_answer):
    overlay_answer({
        "argv": ["spawn"], "exit": 2,
        "stderr": ["fno agents spawn: refusing"],
    })
    with pytest.raises(SystemExit) as exc:
        compose_spawn_argv(["spawn", "--name", "w", "/fno:nosuch x"])
    assert exc.value.code == 2


def test_transport_exit78_prints_the_exhausted_payload(capsys, overlay_answer):
    payload = {"status": "refused", "reason": "slot_exhausted", "lanes": []}
    overlay_answer({
        "argv": ["spawn"], "exit": 78, "stderr": [], "stdout": payload,
    })
    with pytest.raises(SystemExit) as exc:
        compose_spawn_argv(["spawn", "--name", "w", "/fno:target x-1"])
    assert exc.value.code == 78
    assert json.loads(capsys.readouterr().out)["reason"] == "slot_exhausted"


def test_transport_emits_the_verb_events(monkeypatch, overlay_answer):
    seen = []

    monkeypatch.setattr(
        "fno.agents.events.emit",
        lambda name, **fields: seen.append((name, fields)),
    )
    event = {"outcome": "warned", "model": "glm-5.3"}
    overlay_answer({
        "argv": ["spawn"], "exit": 0, "stderr": [], "events": [event],
        "injected": False,
    })
    compose_spawn_argv(["spawn", "--name", "w", "-H", "claude", "-m", "glm-5.3", "hi"])
    assert seen == [("model_vendor_mismatch", event)]


def test_transport_returns_the_composed_argv(overlay_answer):
    composed = ["spawn", "--harness", "codex", "--name", "w", "/fno:target x-1"]
    overlay_answer({"argv": composed, "stderr": [], "exit": 0, "injected": True})
    out = compose_spawn_argv(["spawn", "--name", "w", "/fno:target x-1"])
    assert out == composed


def test_transport_unavailable_degrades_open(capsys, monkeypatch):
    from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable

    def boom(payload, *a, **k):
        raise SpawnOverlayUnavailable("no binary")

    monkeypatch.setattr(
        "fno.agents.spawn_overlay_client.spawn_overlay_call", boom
    )
    argv = compose_spawn_argv(["spawn", "--name", "w", "/fno:target x-1"])
    err = capsys.readouterr().err
    assert "config defaults skipped (spawn-overlay unavailable: no binary)" in err
    assert argv[0] == "spawn"
