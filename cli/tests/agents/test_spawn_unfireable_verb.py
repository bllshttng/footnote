"""A verb-shaped codex seed the session cannot expand is refused, and the
receipt separates delivered from fired (x-aac2)."""
from __future__ import annotations

import pytest

from fno.agents import harness_map
from fno.agents.harness_map import (
    cannot_fire_refusal,
    render_seed,
    spawn_seed_receipt_fields,
    spawn_seed_receipt_fragment,
    verb_fired_marker,
)
from fno.agents.rust_runtime import _refuse_unfireable_seed


@pytest.fixture(autouse=True)
def _plugin_state(monkeypatch):
    """Pin the codex plugin verdict; tests set it via `state`."""
    from fno.setup import codex_plugin

    holder = {"status": "fresh"}
    monkeypatch.setattr(
        codex_plugin, "inspect_freshness", lambda **_: {"status": holder["status"]}
    )
    yield holder


@pytest.mark.parametrize("status", ["missing", "wrong-channel"])
def test_a_codex_seed_without_an_enabled_plugin_is_refused(status, _plugin_state):
    _plugin_state["status"] = status
    reason = cannot_fire_refusal("$fno:target x-1", "codex")
    assert reason is not None
    assert "'$fno:target'" in reason
    assert "fno config setup codex-plugin" in reason


@pytest.mark.parametrize("status", ["fresh", "stale", "unknown", "conflict", "error"])
def test_a_measured_or_unreadable_plugin_state_passes(status, _plugin_state):
    _plugin_state["status"] = status
    assert cannot_fire_refusal("$fno:target x-1", "codex") is None


def test_a_prose_seed_is_never_judged(_plugin_state):
    _plugin_state["status"] = "missing"
    assert cannot_fire_refusal("fix the login bug", "codex") is None


def test_only_the_measured_surface_is_gated(_plugin_state):
    _plugin_state["status"] = "missing"
    assert cannot_fire_refusal("/fno:target x-1", "claude") is None
    assert cannot_fire_refusal("/fno:target x-1", "opencode") is None


def test_an_unreadable_codex_state_fails_open(_plugin_state, monkeypatch):
    from fno.setup import codex_plugin

    def _boom(**_):
        raise FileNotFoundError("codex")

    monkeypatch.setattr(codex_plugin, "inspect_freshness", _boom)
    assert cannot_fire_refusal("$fno:target x-1", "codex") is None


@pytest.mark.parametrize(
    ("seed", "expected"),
    [
        ("$fno:target x-1", "fno agents claim status node:x-1"),
        ("/fno:target ab-9f5a1f8c", "fno agents claim status node:ab-9f5a1f8c"),
        ("$fno:target x-1\ncontext lines follow", "fno agents claim status node:x-1"),
        ("$fno:review high", None),
        ("/fno:target --no-merge x-1", None),
        ("fix the login bug", None),
        ("$fno:target", None),
    ],
)
def test_the_marker_names_the_verbs_own_positive_proof(seed, expected):
    assert verb_fired_marker(seed) == expected


def test_the_seam_refuses_an_unfireable_codex_seed(_plugin_state, capsys):
    _plugin_state["status"] = "missing"
    with pytest.raises(SystemExit) as exc:
        _refuse_unfireable_seed(["spawn", "$fno:target x-1", "-H", "codex"])
    assert exc.value.code == 2
    assert "would not fire" in capsys.readouterr().err


def test_the_seam_ignores_prose_and_non_codex(_plugin_state):
    _plugin_state["status"] = "missing"
    _refuse_unfireable_seed(["spawn", "fix the login bug", "-H", "codex"])
    _refuse_unfireable_seed(["spawn", "/fno:target x-1", "-H", "claude"])


def test_render_seed_refuses_and_normalizes(_plugin_state):
    _plugin_state["status"] = "missing"
    with pytest.raises(harness_map.DispatchResolveError):
        render_seed("/fno:target x-1", "codex")
    _plugin_state["status"] = "fresh"
    assert render_seed("/fno:target x-1", "codex") == "$fno:target x-1"
    assert render_seed("fix the login bug", "codex") == "fix the login bug"


def test_the_receipt_fields_separate_delivered_from_fired():
    fields = spawn_seed_receipt_fields("$fno:target x-1")
    assert fields == {
        "effective_message": "$fno:target x-1",
        "verb_fired": "pending",
        "verb_marker": "fno agents claim status node:x-1",
    }
    assert "verb_marker" not in spawn_seed_receipt_fields("$fno:review high")


def test_the_receipt_fragment_is_empty_for_prose():
    assert spawn_seed_receipt_fragment(None) == ""
    fragment = spawn_seed_receipt_fragment("$fno:target x-1")
    assert '"verb_fired": "pending"' in fragment
    assert '"verb_marker": "fno agents claim status node:x-1"' in fragment
