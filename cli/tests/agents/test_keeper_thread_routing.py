"""Keeper-harness thread spawns must divert to the Python dispatch.

The Rust client's thread match handles claude/codex/opencode and refuses
every other name, while the keeper arms live only in ``cmd_spawn`` ->
``_lane_b_thread_spawn``. Without the ``_is_keeper_thread_spawn`` carve-out,
an installed binary answers a working lane with "fno has not built this
harness's keeper lane spawn arm yet" - found in the x-fd31 review round on
grok, and equally latent for pi and cursor-agent, whose arms shipped
without the routing half.
"""
from __future__ import annotations

from fno.agents.rust_runtime import _is_keeper_thread_spawn, _KEEPER_THREAD_HARNESSES


def test_keeper_roster_matches_the_capability_lane() -> None:
    """The literal tuple and the capability contract must agree exactly.

    Before x-df08, opencode's row derived a keeper lane (interactive_attach
    unsupported, interactive_resume supported) while its working thread lane
    was the serve-HTTP arm the Rust client owns - so this test carved out a
    named exception. x-df08 fixed thread_lane() to read opencode's own
    features.attach claim (native) first, which now correctly answers
    "attach" for opencode, and the exception is no longer needed: the
    capability-derived roster and the literal tuple agree without it."""
    from fno.agents.harness_map import thread_lane
    from fno.harness_names import SPAWN_HARNESSES

    keeper_roster = {h for h in SPAWN_HARNESSES if thread_lane(h) == "keeper"}
    assert keeper_roster == set(_KEEPER_THREAD_HARNESSES)
    assert "opencode" not in keeper_roster, "opencode's row regressed to keeper"


def test_grok_thread_spawn_diverts_to_python() -> None:
    assert _is_keeper_thread_spawn(
        "spawn", ["spawn", "wk-grok", "--harness", "grok", "--substrate", "thread", "seed"]
    )
    assert _is_keeper_thread_spawn(
        "spawn", ["spawn", "wk-grok", "-H", "grok", "--substrate=thread", "seed"]
    )


def test_glued_short_harness_diverts_too() -> None:
    """``-Hgrok`` is a spelling Click parses and the Rust client does not
    (it matches ``-H`` as an exact token), so the carve-out must read it or
    the spawn routes to the binary and never reaches grok's lane."""
    assert _is_keeper_thread_spawn(
        "spawn", ["spawn", "wk-grok", "-Hgrok", "--substrate", "thread", "seed"]
    )


def test_sibling_keeper_harnesses_divert_too() -> None:
    """pi and cursor-agent carried the same latent gap; the predicate names
    them, not just the harness that found it."""
    for harness in ("pi", "cursor-agent"):
        assert _is_keeper_thread_spawn(
            "spawn",
            ["spawn", "wk-x", "--harness", harness, "--substrate", "thread", "seed"],
        )


def test_non_keeper_and_other_substrates_do_not_divert() -> None:
    """claude's thread is the rust client's own arm; a bare (pane) spawn and
    the headless spellings route as they always did."""
    assert not _is_keeper_thread_spawn(
        "spawn", ["spawn", "w", "--harness", "claude", "--substrate", "thread", "seed"]
    )
    assert not _is_keeper_thread_spawn(
        "spawn", ["spawn", "w", "--harness", "grok", "seed"]
    )
    assert not _is_keeper_thread_spawn(
        "spawn", ["spawn", "w", "--harness", "grok", "--substrate", "headless", "seed"]
    )
    assert not _is_keeper_thread_spawn(
        "spawn", ["spawn", "w", "--harness", "grok", "--once", "seed"]
    )


def test_fenced_tokens_cannot_masquerade_as_flags() -> None:
    """A harness name past the ``--`` fence is payload, and a fenced
    ``--substrate`` never picks the lane."""
    assert not _is_keeper_thread_spawn(
        "spawn", ["spawn", "w", "--harness", "claude", "--", "--harness", "grok"]
    )
    assert not _is_keeper_thread_spawn(
        "spawn", ["spawn", "w", "--harness", "grok", "--", "--substrate", "thread"]
    )
