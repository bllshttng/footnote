"""Per-harness dispatch_command resolution (x-567d).

Each harness resolves to the right worker command: a native skill invocation
where one is verified (claude/agy ``/target``, opencode ``/fno:target``, codex
``$fno:target``). gemini is deprecated (successor: agy) and its dispatch lane is
a loud refusal - no prose brief (x-de43).
"""
from __future__ import annotations

import pytest

from fno.agents.harness_map import (
    MAP_VERSION,
    DispatchResolveError,
    capabilities,
    dispatch_command,
    normalize_command,
    resolve_dispatch,
)


@pytest.mark.parametrize(
    "harness,expected_prefix",
    [
        ("claude", "/target --no-merge "),
        ("codex", "$fno:target --no-merge "),
        ("agy", "/target --no-merge "),
    ],
)
def test_skill_invoking_harnesses_get_native_command(harness, expected_prefix):
    out = resolve_dispatch(harness=harness, node_id="x-abcd")
    assert out["command"] == f"{expected_prefix}x-abcd"


def test_opencode_gets_native_fno_slash_command():
    # opencode's fno plugin expands `/fno:verb` (palette + `run --command`), so
    # dispatch renders the native slash form, not a prose brief (x-de43).
    out = resolve_dispatch(harness="opencode", node_id="x-abcd")
    assert out["command"] == "/fno:target --no-merge x-abcd"


def test_gemini_dispatch_refused_naming_agy():
    # gemini is deprecated (successor: agy); its dispatch lane is a loud refusal.
    with pytest.raises(DispatchResolveError, match="agy"):
        resolve_dispatch(harness="gemini", node_id="x-abcd")


def test_config_command_overrides_the_per_harness_builtin():
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        dispatch_cfg={"command": "$fno:execute {id}"},
    )
    assert out["command"] == "$fno:execute x-abcd"


def test_explicit_command_wins_over_config_and_builtin():
    # The explicit slash template is canonical claude syntax, normalized on the
    # chosen harness (x-f0e2): `/custom` -> `$fno:custom` on codex. Precedence is
    # unchanged - explicit still beats the config `$fno:execute`.
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        command="/custom {id}",
        dispatch_cfg={"command": "$fno:execute {id}"},
    )
    assert out["command"] == "$fno:custom x-abcd"


def test_qualified_dispatch_verb_canonicalizes_before_the_allowlist():
    # US7 review: the court contract sets `--dispatch-verb /fno:target` (every
    # dispatched verb is plugin-qualified). The bare-only allowlist must not
    # refuse it - it canonicalizes to `/target`, then renders per-harness.
    out = resolve_dispatch(harness="claude", node_id="x-abcd", verb="/fno:target")
    assert out["command"] == "/target x-abcd"
    # opencode's surface re-adds the /fno: prefix at render.
    out_oc = resolve_dispatch(harness="opencode", node_id="x-abcd", verb="/fno:target")
    assert out_oc["command"] == "/fno:target x-abcd"
    # /fno:think canonicalizes the same way.
    out_think = resolve_dispatch(harness="claude", node_id="x-abcd", verb="/fno:think")
    assert out_think["command"] == "/think x-abcd"
    # a bare verb still works unchanged.
    assert resolve_dispatch(harness="claude", node_id="x-abcd", verb="/target")["command"] == "/target x-abcd"


def test_template_without_id_is_rejected():
    with pytest.raises(DispatchResolveError):
        resolve_dispatch(harness="claude", node_id="x-abcd", command="no placeholder here")


def test_map_version_bumped_for_dispatch_command():
    # A consumer asserting the shape it was written against must see the bump.
    assert MAP_VERSION >= 3
    assert resolve_dispatch(harness="claude")["map_version"] == MAP_VERSION


# --- autonomous pane capabilities ----------------------------------------- #


def test_codex_autonomous_pane_is_capability_allowed_without_changing_default():
    explicit = resolve_dispatch(
        harness="codex",
        substrate="pane",
        node_id="x-abcd",
        trigger="autonomous",
    )

    assert capabilities("codex")["autonomous_pane"] is True
    assert explicit["substrate"] == "pane"
    assert resolve_dispatch(harness="codex", node_id="x-abcd")["substrate"] == "headless"


@pytest.mark.parametrize("harness", ["claude", "agy", "opencode"])
def test_unverified_harness_autonomous_pane_fails_closed(harness):
    assert capabilities(harness)["autonomous_pane"] is False

    with pytest.raises(
        DispatchResolveError,
        match=rf"harness {harness!r}.*autonomous_pane",
    ):
        resolve_dispatch(
            harness=harness,
            substrate="pane",
            node_id="x-abcd",
            trigger="autonomous",
        )


def test_missing_autonomous_pane_capability_fails_closed(monkeypatch):
    import fno.agents.harness_map as harness_map

    monkeypatch.delitem(harness_map._HARNESS_CAPS["opencode"], "autonomous_pane")

    with pytest.raises(
        DispatchResolveError,
        match=r"harness 'opencode'.*autonomous_pane",
    ):
        resolve_dispatch(
            harness="opencode",
            substrate="pane",
            node_id="x-abcd",
            trigger="autonomous",
        )


def test_malformed_trigger_fails_closed_on_capability_enabled_pane():
    with pytest.raises(DispatchResolveError, match="unknown dispatch trigger"):
        resolve_dispatch(
            harness="codex",
            substrate="pane",
            node_id="x-abcd",
            trigger="autonamous",
        )


def test_pane_capability_does_not_enable_codex_bg():
    with pytest.raises(DispatchResolveError, match="bg is claude \\+ opencode"):
        resolve_dispatch(
            harness="codex",
            substrate="bg",
            node_id="x-abcd",
            trigger="autonomous",
        )


# --- the normalizer (x-a5e4) ------------------------------------------------ #


@pytest.mark.parametrize(
    "harness,expected",
    [
        ("claude", "/target --no-merge {id}"),
        ("agy", "/target --no-merge {id}"),
        ("codex", "$fno:target --no-merge {id}"),
    ],
)
def test_normalize_command_slash_and_codex(harness, expected):
    assert normalize_command("/target --no-merge {id}", harness) == expected


def test_normalize_command_opencode_namespaces():
    # opencode: `/verb` -> `/fno:verb` (plugin palette + `run --command`).
    assert normalize_command("/target --no-merge {id}", "opencode") == "/fno:target --no-merge {id}"


def test_normalize_command_gemini_refused():
    with pytest.raises(DispatchResolveError, match="agy"):
        normalize_command("/target --no-merge {id}", "gemini")


@pytest.mark.parametrize(
    "verb_cmd,expected",
    [
        ("/blueprint {id}", "$fno:blueprint {id}"),
        ("/pr create", "$fno:pr create"),
        ("/think {id}", "$fno:think {id}"),
    ],
)
def test_normalize_command_is_verb_agnostic_for_codex(verb_cmd, expected):
    # ANY footnote /verb -> $fno:verb on codex, not just /target.
    assert normalize_command(verb_cmd, "codex") == expected


def test_dispatch_command_builtin_matches_normalize():
    # The builtin is exactly the normalize of the canonical autonomous command.
    # gemini excluded: it refuses (test_normalize_command_gemini_refused).
    for h in ("claude", "codex", "agy", "opencode"):
        assert dispatch_command(h) == normalize_command("/target --no-merge {id}", h)


def test_command_surface_is_reported():
    assert resolve_dispatch(harness="codex")["command_surface"] == "codex-skill"
    assert resolve_dispatch(harness="claude")["command_surface"] == "slash"
    assert resolve_dispatch(harness="opencode")["command_surface"] == "slash"


# --- the verb-path fix (the codex P1 the handoff names) --------------------- #
# A node's `dispatch_verb=/target` must be NORMALIZED per-harness, not left as
# claude-syntax `/target` for every harness (which handed codex a slash command
# it cannot run).


def test_verb_path_normalizes_to_codex_skill():
    out = resolve_dispatch(harness="codex", node_id="x-abcd", verb="/target")
    assert out["command"] == "$fno:target x-abcd"


@pytest.mark.parametrize("harness", ["claude", "agy"])
def test_verb_path_keeps_slash_for_slash_harnesses(harness):
    out = resolve_dispatch(harness=harness, node_id="x-abcd", verb="/target")
    assert out["command"] == "/target x-abcd"


def test_verb_path_normalizes_to_opencode_slash():
    out = resolve_dispatch(harness="opencode", node_id="x-abcd", verb="/target")
    assert out["command"] == "/fno:target x-abcd"


def test_opencode_renders_any_verb():
    # opencode's single prefix-swap renders ANY verb, not just /target.
    out = resolve_dispatch(harness="opencode", node_id="x-abcd", verb="/think")
    assert out["command"] == "/fno:think x-abcd"


def test_gemini_verb_path_refused():
    with pytest.raises(DispatchResolveError, match="agy"):
        resolve_dispatch(harness="gemini", node_id="x-abcd", verb="/target")


def test_normalize_command_opencode_renders_any_verb():
    assert normalize_command("/think {id}", "opencode") == "/fno:think {id}"
    assert normalize_command("/blueprint quick x", "opencode") == "/fno:blueprint quick x"


# ---------------------------------------------------------------------------
# x-8e59: the builtin rung reads config.dispatch.auto_merge
#
# x-4391 shipped the key to 2 of 3 dispatch paths. The builtin here was the
# deaf one, so an operator who set the key still got `/target --no-merge <id>`,
# a manifest frozen at auto_merge_approved: false, and a refused `fno do pr merge`.
# The two callers that honored it did so by passing an explicit command around
# the builtin rather than fixing it.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "harness,expected",
    [
        ("claude", "/target x-abcd"),
        ("agy", "/target x-abcd"),
        ("codex", "$fno:target x-abcd"),
        ("opencode", "/fno:target x-abcd"),
    ],
)
def test_builtin_drops_no_merge_when_auto_merge_configured(harness, expected):
    out = resolve_dispatch(
        harness=harness, node_id="x-abcd", dispatch_cfg={"auto_merge": True}
    )
    assert out["command"] == expected


@pytest.mark.parametrize("harness", ["claude", "codex", "opencode", "agy"])
def test_builtin_defaults_to_no_merge(harness):
    """No key, no grant. The default posture is unchanged for a fresh install."""
    out = resolve_dispatch(harness=harness, node_id="x-abcd", dispatch_cfg={})
    assert " --no-merge " in out["command"]


@pytest.mark.parametrize("bad", ["true", "True", 1, [1], object()])
def test_non_boolean_auto_merge_never_grants_merge(bad):
    """Granting merge authority is the irreversible direction, so the read is a
    strict identity check. Every one of these is truthy; none is a grant."""
    out = resolve_dispatch(
        harness="claude", node_id="x-abcd", dispatch_cfg={"auto_merge": bad}
    )
    assert out["command"] == "/target --no-merge x-abcd"


def test_unreadable_config_fails_safe_to_no_merge(monkeypatch):
    import fno.config as _config

    def _boom(*a, **k):
        raise RuntimeError("config is a smoking crater")

    monkeypatch.setattr(_config, "load_settings", _boom)
    out = resolve_dispatch(harness="claude", node_id="x-abcd")
    assert out["command"] == "/target --no-merge x-abcd"


def test_auto_merge_does_not_touch_an_explicit_command():
    """The posture applies to the builtin only: an explicit command already says
    what to run, and rewriting a caller's own template would be the surprise."""
    out = resolve_dispatch(
        harness="claude",
        node_id="x-abcd",
        command="/target --no-merge --reconcile /tmp/m.md {id}",
        dispatch_cfg={"auto_merge": True},
    )
    assert out["command"] == "/target --no-merge --reconcile /tmp/m.md x-abcd"


def test_auto_merge_does_not_touch_the_verb_rung():
    out = resolve_dispatch(
        harness="claude", node_id="x-abcd", verb="/think",
        dispatch_cfg={"auto_merge": True},
    )
    assert out["command"] == "/think x-abcd"


def test_decision_receipt_names_the_merge_posture():
    """The receipt is how an operator confirms the key was read at all - the
    bug this closes was invisible precisely because nothing said so."""
    allow = resolve_dispatch(
        harness="claude", node_id="x-abcd", dispatch_cfg={"auto_merge": True}
    )
    deny = resolve_dispatch(harness="claude", node_id="x-abcd", dispatch_cfg={})
    assert "command=builtin(merge)" in allow["decision"]
    assert "command=builtin(no-merge)" in deny["decision"]


def test_partial_settings_object_does_not_drop_auto_merge():
    """A settings stub carrying only `.auto_merge.grant` must still yield it.

    Field access used to be one try block over `d.harness`/`d.substrate`/
    `d.command`, so a settings object missing any one of them raised and threw
    the WHOLE dict away - one absent key silently disabling every other. That is
    the shape of the bug being fixed, so it gets its own test. x-4be1: the
    grant lives OUTSIDE the dispatch block, so a stub with an auto_merge block
    and no dispatch overlay still resolves the grant."""
    import types

    from fno.agents.harness_map import _load_dispatch_cfg

    stub = types.SimpleNamespace(
        auto_merge=types.SimpleNamespace(grant="dispatch")
    )
    assert _load_dispatch_cfg(stub)["auto_merge"] is True


def test_settings_without_dispatch_section_yields_empty_cfg():
    """x-4be1: no dispatch overlay and no grant -> only the (no-)grant key; the
    harness/substrate/command keys stay absent so their builtin rungs run."""
    import types

    from fno.agents.harness_map import _load_dispatch_cfg

    assert _load_dispatch_cfg(types.SimpleNamespace()) == {"auto_merge": False}


@pytest.mark.parametrize("allow,expected", [(True, "/target {id}"), (False, "/target --no-merge {id}")])
def test_dispatch_command_posture_argument(allow, expected):
    assert dispatch_command("claude", allow_merge=allow) == expected


def test_dispatch_command_defaults_to_no_merge():
    assert dispatch_command("claude") == "/target --no-merge {id}"
