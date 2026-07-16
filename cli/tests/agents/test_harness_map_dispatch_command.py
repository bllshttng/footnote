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
    dispatch_command,
    normalize_command,
    resolve_dispatch,
)


@pytest.mark.parametrize(
    "harness,expected_prefix",
    [
        ("claude", "/target no-merge "),
        ("codex", "$fno:target no-merge "),
        ("agy", "/target no-merge "),
    ],
)
def test_skill_invoking_harnesses_get_native_command(harness, expected_prefix):
    out = resolve_dispatch(harness=harness, node_id="x-abcd")
    assert out["command"] == f"{expected_prefix}x-abcd"


def test_opencode_gets_native_fno_slash_command():
    # opencode's fno plugin expands `/fno:verb` (palette + `run --command`), so
    # dispatch renders the native slash form, not a prose brief (x-de43).
    out = resolve_dispatch(harness="opencode", node_id="x-abcd")
    assert out["command"] == "/fno:target no-merge x-abcd"


def test_gemini_dispatch_refused_naming_agy():
    # gemini is deprecated (successor: agy); its dispatch lane is a loud refusal.
    with pytest.raises(DispatchResolveError, match="agy"):
        resolve_dispatch(harness="gemini", node_id="x-abcd")


def test_config_command_overrides_the_per_harness_builtin():
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        dispatch_cfg={"command": "$fno:do {id}"},
    )
    assert out["command"] == "$fno:do x-abcd"


def test_explicit_command_wins_over_config_and_builtin():
    # The explicit slash template is canonical claude syntax, normalized on the
    # chosen harness (x-f0e2): `/custom` -> `$fno:custom` on codex. Precedence is
    # unchanged - explicit still beats the config `$fno:do`.
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        command="/custom {id}",
        dispatch_cfg={"command": "$fno:do {id}"},
    )
    assert out["command"] == "$fno:custom x-abcd"


def test_template_without_id_is_rejected():
    with pytest.raises(DispatchResolveError):
        resolve_dispatch(harness="claude", node_id="x-abcd", command="no placeholder here")


def test_map_version_bumped_for_dispatch_command():
    # A consumer asserting the shape it was written against must see the bump.
    assert MAP_VERSION >= 3
    assert resolve_dispatch(harness="claude")["map_version"] == MAP_VERSION


# --- the normalizer (x-a5e4) ------------------------------------------------ #


@pytest.mark.parametrize(
    "harness,expected",
    [
        ("claude", "/target no-merge {id}"),
        ("agy", "/target no-merge {id}"),
        ("codex", "$fno:target no-merge {id}"),
    ],
)
def test_normalize_command_slash_and_codex(harness, expected):
    assert normalize_command("/target no-merge {id}", harness) == expected


def test_normalize_command_opencode_namespaces():
    # opencode: `/verb` -> `/fno:verb` (plugin palette + `run --command`).
    assert normalize_command("/target no-merge {id}", "opencode") == "/fno:target no-merge {id}"


def test_normalize_command_gemini_refused():
    with pytest.raises(DispatchResolveError, match="agy"):
        normalize_command("/target no-merge {id}", "gemini")


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
        assert dispatch_command(h) == normalize_command("/target no-merge {id}", h)


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
