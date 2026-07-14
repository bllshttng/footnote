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
    assert MAP_VERSION >= 2
    assert resolve_dispatch(harness="claude")["map_version"] == MAP_VERSION
