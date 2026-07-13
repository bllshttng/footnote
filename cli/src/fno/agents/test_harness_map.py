"""Tests for the harness-capability map + shared dispatch resolver (US1)."""
from __future__ import annotations

import pytest

from fno.agents.harness_map import (
    DispatchResolveError,
    known_harnesses,
    resolve_dispatch,
    substrate_default,
)

# Config read is stubbed to empty in every resolve so the tests exercise the
# built-in precedence, not the ambient project config.
_NO_CFG: dict = {}


def _resolve(**kw):
    kw.setdefault("dispatch_cfg", _NO_CFG)
    return resolve_dispatch(**kw)


def test_default_harness_is_claude_bg_with_bypass():
    """AC1-HP: a node with no dispatch fields resolves to /target no-merge <id>
    on claude/bg with the permission-bypass flag (so the worker never hangs on
    an approval prompt)."""
    out = _resolve(node_id="x-4d85")
    assert out["harness"] == "claude"
    assert out["substrate"] == "bg"
    assert out["command"] == "/target no-merge x-4d85"
    assert out["permission_bypass"] == ["--dangerously-skip-permissions"]


def test_codex_defaults_to_headless():
    """Verify line: --harness codex resolves to the headless substrate."""
    out = _resolve(harness="codex")
    assert out["substrate"] == "headless"
    assert out["bg"] is False
    assert out["permission_bypass"] == ["--dangerously-bypass-approvals-and-sandbox"]


def test_unknown_harness_fails_loud_naming_the_map():
    """AC1-ERR: an unknown harness raises, naming the harness AND the map."""
    with pytest.raises(DispatchResolveError) as exc:
        _resolve(harness="nonexistent")
    msg = str(exc.value)
    assert "nonexistent" in msg
    assert "fno.agents.harness_map" in msg


def test_explicit_bg_on_non_claude_is_rejected():
    """bg is claude-only; an explicit bg on codex is a hard error -> headless."""
    with pytest.raises(DispatchResolveError, match="headless"):
        _resolve(harness="codex", substrate="bg")


def test_autonomous_pane_is_rejected():
    """Invariant: an autonomous trigger never resolves a stalling pane."""
    with pytest.raises(DispatchResolveError, match="pane"):
        _resolve(harness="claude", substrate="pane", trigger="autonomous")


def test_attended_pane_is_allowed():
    """A pane is valid for an attended trigger (a human drives it)."""
    out = _resolve(harness="claude", substrate="pane", trigger="attended")
    assert out["substrate"] == "pane"


def test_template_without_node_is_literal():
    """No node id -> the template is returned verbatim ({id} unsubstituted)."""
    out = _resolve(harness="codex")
    assert out["command"] == "/target no-merge {id}"


def test_bad_template_rejected_when_substituting():
    """A template lacking exactly one {id} cannot substitute a node id."""
    with pytest.raises(DispatchResolveError, match="{id}"):
        _resolve(node_id="x-1", command="/target no-merge")


def test_config_substrate_typo_fails_loud():
    """A config.dispatch.substrate typo is a trust boundary too - it must raise,
    not resolve silently to a launcher."""
    with pytest.raises(DispatchResolveError, match="unknown substrate"):
        resolve_dispatch(harness="claude", dispatch_cfg={"substrate": "panel"})


def test_pane_guard_fails_closed_on_unknown_trigger():
    """The autonomous-pane guard fails CLOSED: any non-'attended' trigger
    (typo, 'auto') still blocks a stalling pane."""
    with pytest.raises(DispatchResolveError, match="pane"):
        _resolve(harness="claude", substrate="pane", trigger="autonamous")


def test_config_overlay_precedence():
    """config.dispatch overlays the built-in but loses to an explicit flag."""
    cfg = {"harness": "codex", "substrate": "", "command": "/think {id}"}
    out = resolve_dispatch(node_id="x-9", dispatch_cfg=cfg)
    assert out["harness"] == "codex"
    assert out["command"] == "/think x-9"
    # explicit flag beats config
    out2 = resolve_dispatch(harness="claude", node_id="x-9", dispatch_cfg=cfg)
    assert out2["harness"] == "claude"


def test_substrate_default_table():
    assert substrate_default("claude") == "bg"
    for h in ("codex", "gemini", "agy", "opencode"):
        assert substrate_default(h) == "headless"


def test_known_harnesses_covers_readable_set():
    """The map covers the readable-provider set so US4 can wire opencode."""
    assert set(known_harnesses()) == {"claude", "codex", "gemini", "agy", "opencode"}
