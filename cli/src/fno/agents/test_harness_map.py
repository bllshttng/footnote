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
    """No node id -> the template is returned verbatim ({id} unsubstituted).
    codex normalizes to its `$fno:` skill surface (x-a5e4)."""
    out = _resolve(harness="codex")
    assert out["command"] == "$fno:target no-merge {id}"


def test_bad_template_rejected_when_substituting():
    """A template lacking exactly one {id} cannot substitute a node id."""
    with pytest.raises(DispatchResolveError, match="{id}"):
        _resolve(node_id="x-1", command="/target no-merge")


def test_empty_explicit_harness_fails_loud():
    """An empty explicit --harness (unset env var interpolated into a flag) must
    fail loud, not silently fall through to config/claude."""
    with pytest.raises(DispatchResolveError, match="must not be empty"):
        _resolve(harness="")
    with pytest.raises(DispatchResolveError, match="must not be empty"):
        _resolve(harness="claude", substrate="  ")


def test_config_substrate_typo_fails_loud():
    """A config.dispatch.substrate typo is a trust boundary too - it must raise,
    not resolve silently to a launcher."""
    with pytest.raises(DispatchResolveError, match="unknown substrate"):
        resolve_dispatch(harness="claude", dispatch_cfg={"substrate": "panel"})


def test_pane_guard_fails_closed_on_unknown_trigger():
    """The autonomous-pane guard fails CLOSED: any non-'attended' trigger
    (typo, 'auto', or None) still blocks a stalling pane - and never crashes."""
    for t in ("autonamous", None):
        with pytest.raises(DispatchResolveError, match="pane"):
            _resolve(harness="claude", substrate="pane", trigger=t)


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


# --- US3: configurable dispatch verb + brief ------------------------------


def test_node_verb_assembles_command():
    """AC2-HP: a node verb resolves to `<verb> <id>` (not the /target default)."""
    out = _resolve(node_id="x-1", verb="/think")
    assert out["command"] == "/think x-1"
    assert out["env"] == {}


def test_node_brief_rides_env_never_command():
    """AC2-HP: the brief reaches the worker via TARGET_BRIEF env, and no brief
    text is shell-interpolated into the command line."""
    out = _resolve(node_id="x-1", verb="/think", brief="brainstorm the retry design")
    assert out["command"] == "/think x-1"
    assert out["env"]["TARGET_BRIEF"] == "brainstorm the retry design"
    assert "brainstorm" not in out["command"]


def test_out_of_allowlist_verb_rejected():
    """AC3-EDGE: an injection-shaped verb is refused, naming the verb + allowlist."""
    with pytest.raises(DispatchResolveError) as exc:
        _resolve(node_id="x-1", verb="rm -rf; /target")
    msg = str(exc.value)
    assert "rm -rf" in msg
    assert "/target" in msg  # the allowlist is named


def test_empty_verb_rejected():
    """An explicit empty verb fails loud rather than silently defaulting."""
    with pytest.raises(DispatchResolveError):
        _resolve(node_id="x-1", verb="   ")


def test_brief_over_8kb_rejected():
    """Verify 4: a brief larger than the 8 KB env budget is an explicit error,
    never silent truncation."""
    with pytest.raises(DispatchResolveError, match="8"):
        _resolve(node_id="x-1", verb="/think", brief="x" * 8193)


def test_brief_at_8kb_ok():
    out = _resolve(node_id="x-1", verb="/think", brief="x" * 8192)
    assert out["env"]["TARGET_BRIEF"] == "x" * 8192


def test_no_verb_leaves_default_and_empty_env():
    """Verify 3 (regression): no dispatch fields -> /target no-merge <id>, env empty."""
    out = _resolve(node_id="x-1")
    assert out["command"] == "/target no-merge x-1"
    assert out["env"] == {}


def test_config_extends_allowlist():
    """A per-project allowlist admits a domain workflow verb."""
    cfg = {"allowed_verbs": ["/target", "/think", "/marketing"]}
    out = _resolve(node_id="x-1", verb="/marketing", dispatch_cfg=cfg)
    assert out["command"] == "/marketing x-1"


def test_node_verb_wins_over_config_command():
    """Precedence: node verb > config.dispatch.command > builtin."""
    cfg = {"command": "/foo {id}"}
    out = _resolve(node_id="x-1", verb="/think", dispatch_cfg=cfg)
    assert out["command"] == "/think x-1"


def test_brief_without_verb_still_rides_env():
    """A brief on a default (/target) dispatch still travels via env."""
    out = _resolve(node_id="x-1", brief="ship carefully")
    assert out["command"] == "/target no-merge x-1"
    assert out["env"]["TARGET_BRIEF"] == "ship carefully"
