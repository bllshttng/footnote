"""Tests for the dispatch verb registry (node x-c8e4).

`dispatch.allowed_verbs` is a bare string list: it cannot answer which
harnesses a verb exists on, how it is spelled there, what it requires, or what
a completion proves. `dispatch.verb_registry` carries those as a
`DispatchVerbDescriptor`, mirroring `review.reviewer_registry` field for
field. The load-bearing invariant is the same as the reviewer table's: a
registry entry never redefines a shipped verb, and the resolver renders a
registry verb's spelling VERBATIM - the fno-namespace normalizer would mint a
phantom `$fno:sec:audit`-shaped skill on codex.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents.harness_map import DispatchResolveError, resolve_dispatch
from fno.config import DispatchVerbDescriptor, load_settings, resolvable_verbs
from fno.review_capability import resolve_skill_presence

REG = {
    "/security-audit": DispatchVerbDescriptor(
        invocation="/sec:audit",
        invocations={"claude": "/sec:audit", "codex": "$sec:audit"},
    )
}


@pytest.fixture
def isolated(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A cwd and a HOME with no skill roots, so each test declares its own."""
    home = tmp_path / "home"
    project = tmp_path / "project"
    home.mkdir()
    project.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.chdir(project)
    return tmp_path


def _install_skill(base: Path, name: str) -> None:
    root = base / ".claude" / "skills" / name
    root.mkdir(parents=True)
    (root / "SKILL.md").write_text("---\nname: x\n---\n")


# ── 1.1: config parse + collision rule ──────────────────────────────────


def test_registry_parses_to_descriptor(tmp_path, monkeypatch):
    cfg = tmp_path / "settings.yaml"
    cfg.write_text(
        """\
dispatch:
  verb_registry:
    "/security-audit":
      invocation: /sec:audit
      invocations:
        claude: /sec:audit
        codex: "$sec:audit"
      requires: skill
      asserts: doc
"""
    )
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    s = load_settings()
    d = s.dispatch.verb_registry["/security-audit"]
    assert isinstance(d, DispatchVerbDescriptor)
    assert d.invocation == "/sec:audit"
    assert d.invocations == {"claude": "/sec:audit", "codex": "$sec:audit"}
    assert d.requires == "skill"
    assert d.takes_node_id is True
    assert d.asserts == "doc"


def test_resolvable_verbs_canonicalizes_keys_and_drops_builtins():
    reg = {
        "security-audit": DispatchVerbDescriptor(invocation="/sec:audit"),
        "/target": DispatchVerbDescriptor(invocation="/evil"),
        "fno:think": DispatchVerbDescriptor(invocation="/evil"),
    }
    out = resolvable_verbs(reg, ["/target", "/think", "/blueprint"])
    assert set(out) == {"/security-audit"}
    assert out["/security-audit"].asserts == "invocation"


def test_registry_key_colliding_with_allowlist_extension_drops():
    """The collision rule covers the operator's OWN allowlist too: a shipped
    or allowlisted spelling can never be redefined into a weaker descriptor."""
    reg = {"/marketing": DispatchVerbDescriptor(invocation="/m")}
    assert resolvable_verbs(reg, ["/target", "/marketing"]) == {}


def test_resolvable_verbs_empty_registry_is_empty():
    assert resolvable_verbs(None, ["/target"]) == {}


# ── 1.2: the shared skill-presence probe ────────────────────────────────


def test_probe_satisfiable_names_the_root(isolated: Path):
    _install_skill(isolated / "project", "sec-audit")
    status, reason = resolve_skill_presence("sec-audit", "claude")
    assert status == "satisfiable"
    assert "sec-audit" in reason


def test_probe_unavailable_lists_roots_and_context(isolated: Path):
    # One readable root with no matching skill: the unavailable outcome needs
    # at least one root searched; with none it is unverifiable by contract.
    (isolated / "project" / ".claude" / "skills").mkdir(parents=True)
    status, reason = resolve_skill_presence(
        "absent-skill", "claude", context="config.dispatch.verb_registry"
    )
    assert status == "unavailable"
    assert "absent-skill" in reason
    assert "config.dispatch.verb_registry" in reason
    assert ".claude" in reason  # the roots searched are named


def test_probe_is_behavior_preserving_for_reviewers(isolated: Path):
    """The private reviewer path reads through the promoted probe."""
    from fno.config import ReviewerDescriptor
    from fno.review_capability import SessionCapability, resolve_reviewers

    SKILL = ReviewerDescriptor(
        kind="harness-skill",
        requires="skill",
        invocation="/sec-audit",
        asserts="invocation",
    )
    _install_skill(isolated / "project", "sec-audit")
    session = SessionCapability(harness="claude", substrate="pane", attended=False)
    v = resolve_reviewers(["sec-audit"], session, {"sec-audit": SKILL})[0]
    assert v.status == "satisfiable"
    _install_skill(isolated / "home", "home-only")
    v = resolve_reviewers(["home-only"], session, {"home-only": SKILL})[0]
    assert v.status == "satisfiable"


# ── 1.3: resolve, refuse, render ────────────────────────────────────────


def test_registry_verb_renders_native_spelling():
    out = resolve_dispatch(
        harness="codex",
        verb="/security-audit",
        node_id="x-1234",
        dispatch_cfg={"verb_registry": REG},
    )
    assert out["command"] == "$sec:audit x-1234"
    assert "fno:" not in out["command"]
    assert any(
        "registry-verb(/security-audit, asserts=invocation)" in d
        for d in out["decision"]
    )


def test_fno_qualified_registry_verb_canonicalizes():
    out = resolve_dispatch(
        harness="codex",
        verb="/fno:security-audit",
        node_id="x-1",
        dispatch_cfg={"verb_registry": REG},
    )
    assert out["command"] == "$sec:audit x-1"


def test_registry_verb_undeclared_harness_refuses():
    with pytest.raises(DispatchResolveError) as exc:
        resolve_dispatch(
            harness="opencode",
            verb="/security-audit",
            node_id="x-1",
            dispatch_cfg={"verb_registry": REG},
        )
    msg = str(exc.value)
    assert "opencode" in msg
    assert "claude, codex" in msg


def test_registry_verb_without_invocations_map_renders_scalar():
    """No per-harness map = the scalar invocation is correct everywhere, and
    the normalizer must still skip it (the bypass, not the map, is the guard)."""
    cfg = {"verb_registry": {"/sec": DispatchVerbDescriptor(invocation="/sec run")}}
    out = resolve_dispatch(
        harness="codex", verb="/sec", node_id="x-1", dispatch_cfg=cfg
    )
    assert out["command"] == "/sec run x-1"
    assert "fno:" not in out["command"]


def test_registry_verb_missing_skill_refuses_naming_roots(isolated: Path):
    # A readable-but-empty root makes the probe's answer unavailable (with no
    # root at all it would be unverifiable, which proceeds by design).
    (isolated / "project" / ".claude" / "skills").mkdir(parents=True)
    cfg = {
        "verb_registry": {
            "/security-audit": DispatchVerbDescriptor(
                invocation="/sec-audit", requires="skill"
            )
        }
    }
    with pytest.raises(DispatchResolveError) as exc:
        resolve_dispatch(
            harness="claude", verb="/security-audit", node_id="x-1",
            dispatch_cfg=cfg,
        )
    msg = str(exc.value)
    assert "roots searched" in msg
    assert "config.dispatch.verb_registry" in msg


def test_registry_verb_required_skill_present_resolves(isolated: Path):
    _install_skill(isolated / "project", "sec-audit")
    cfg = {
        "verb_registry": {
            "/security-audit": DispatchVerbDescriptor(
                invocation="/sec-audit", requires="skill"
            )
        }
    }
    out = resolve_dispatch(
        harness="claude", verb="/security-audit", node_id="x-1", dispatch_cfg=cfg
    )
    assert out["command"] == "/sec-audit x-1"


def test_registry_verb_skill_probe_reads_first_token(isolated: Path):
    """The invocation may carry args; the probe reads the first token only,
    never the tail - a two-token invocation must not refuse an installed skill."""
    _install_skill(isolated / "project", "sec-audit")
    cfg = {
        "verb_registry": {
            "/security-audit": DispatchVerbDescriptor(
                invocation="/sec-audit --deep", requires="skill"
            )
        }
    }
    out = resolve_dispatch(
        harness="claude", verb="/security-audit", node_id="x-1", dispatch_cfg=cfg
    )
    assert out["command"] == "/sec-audit --deep x-1"


def test_bare_allowlist_path_is_unchanged():
    """Upgrade safety: a config setting only allowed_verbs resolves byte-identical.
    `/marketing` is not a shipped fno verb, so codex's normalizer passes the
    bare spelling through literally rather than minting a phantom skill - the
    pre-existing behavior, pinned here so the registry work cannot drift it."""
    cfg = {"allowed_verbs": ["/target", "/think", "/blueprint", "/marketing"]}
    out = resolve_dispatch(
        harness="codex", verb="/marketing", node_id="x-1", dispatch_cfg=cfg
    )
    assert out["command"] == "/marketing x-1"
    out = resolve_dispatch(
        harness="claude", verb="/think", node_id="x-1", dispatch_cfg=cfg
    )
    assert out["command"] == "/think x-1"


def test_verb_in_neither_surface_names_both():
    cfg = {
        "verb_registry": {"/other": DispatchVerbDescriptor(invocation="/other")}
    }
    with pytest.raises(DispatchResolveError) as exc:
        resolve_dispatch(
            harness="claude", verb="/nope", node_id="x-1", dispatch_cfg=cfg
        )
    msg = str(exc.value)
    assert "neither the allowlist" in msg
    assert "config.dispatch.verb_registry" in msg
    assert "/other" in msg


def test_takes_node_id_false_carries_no_id():
    cfg = {
        "verb_registry": {
            "/snapshot": DispatchVerbDescriptor(
                invocation="/snapshot", takes_node_id=False
            )
        }
    }
    out = resolve_dispatch(
        harness="claude", verb="/snapshot", node_id="x-1", dispatch_cfg=cfg
    )
    assert out["command"] == "/snapshot"
    assert "{id}" not in out["command"]
