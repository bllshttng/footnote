"""Per-harness dispatch_command resolution (x-567d).

Each harness resolves to the right worker command: a native skill invocation
where one is verified (claude ``/target``, codex ``$fno:target``, agy ``/target``)
or the prose-brief lane (opencode/gemini, which have no footnote slash surface).
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


@pytest.mark.parametrize("harness", ["opencode", "gemini"])
def test_prose_brief_harnesses_never_get_a_literal_slash_command(harness):
    # opencode/gemini have no footnote slash surface: a literal `/target` would
    # run verbatim and no-op. They get a prose brief that names the node.
    out = resolve_dispatch(harness=harness, node_id="x-abcd")
    assert not out["command"].startswith("/target")
    assert not out["command"].startswith("$fno")
    assert "x-abcd" in out["command"]
    assert "merge" in out["command"].lower()  # brief tells the worker not to merge


def test_prose_brief_substitutes_every_id_occurrence():
    # The brief references {id} more than once; str.replace hits all of them.
    out = resolve_dispatch(harness="opencode", node_id="x-9f9f")
    assert "{id}" not in out["command"]
    assert out["command"].count("x-9f9f") >= 2


def test_config_command_overrides_the_per_harness_builtin():
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        dispatch_cfg={"command": "$fno:do {id}"},
    )
    assert out["command"] == "$fno:do x-abcd"


def test_explicit_command_wins_over_config_and_builtin():
    out = resolve_dispatch(
        harness="codex",
        node_id="x-abcd",
        command="/custom {id}",
        dispatch_cfg={"command": "$fno:do {id}"},
    )
    assert out["command"] == "/custom x-abcd"


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


@pytest.mark.parametrize("harness", ["opencode", "gemini"])
def test_normalize_command_prose_returns_the_brief(harness):
    out = normalize_command("/target no-merge {id}", harness)
    assert not out.startswith("/")
    assert not out.startswith("$fno")
    assert "{id}" in out  # the brief still names the node for substitution


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
    for h in ("claude", "codex", "agy", "opencode", "gemini"):
        assert dispatch_command(h) == normalize_command("/target no-merge {id}", h)


def test_command_surface_is_reported():
    assert resolve_dispatch(harness="codex")["command_surface"] == "codex-skill"
    assert resolve_dispatch(harness="claude")["command_surface"] == "slash"
    assert resolve_dispatch(harness="opencode")["command_surface"] == "prose"


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


def test_verb_path_falls_to_prose_for_prose_harness():
    out = resolve_dispatch(harness="opencode", node_id="x-abcd", verb="/target")
    assert not out["command"].startswith("/")
    assert not out["command"].startswith("$fno")
    assert "x-abcd" in out["command"]
