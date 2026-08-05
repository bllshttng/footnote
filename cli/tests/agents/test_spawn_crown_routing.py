"""`fno agents spawn --crown` routing: load-bearing on the autonomous substrates.

Bestow-at-spawn (``--crown level=N,scope=X``) is implemented only in the Python
``cmd_spawn`` path; the Rust client does not parse ``--crown``. But pane is the
DEFAULT substrate and pane spawns already divert to Python via
``_is_pane_substrate_spawn`` - so a BARE ``spawn --crown`` already works. The
defect lives only on the autonomous substrates (``--substrate bg`` and the
headless spellings ``-p``/``--headless``/``--once``/``-o``), where
``_is_pane_substrate_spawn`` returns False and the spawn would otherwise reach
the Rust client and die with ``unknown flag: --crown``. Those are exactly the
substrates where succession matters most (an abdicating king bestows on a worker).

So these tests do NOT exercise a bare spawn (that passes before AND after the
fix and proves nothing). They exercise the autonomous substrates and assert the
crown detector is what diverts them - the pane exclusion does not.
"""
from __future__ import annotations

import pytest


def test_crown_bearing_spawn_detected_across_forms() -> None:
    from fno.agents.rust_runtime import _is_crown_bearing_spawn

    assert _is_crown_bearing_spawn(
        "spawn", ["spawn", "--name", "w", "--crown", "level=1,scope=x-d7e4"]
    )
    assert _is_crown_bearing_spawn(
        "spawn", ["spawn", "w", "--crown=level=1,scope=x-d7e4"]
    )
    # A non-crown flag does not trip it; only spawn carries it.
    assert not _is_crown_bearing_spawn("spawn", ["spawn", "w", "--role", "build"])
    assert not _is_crown_bearing_spawn("crown", ["crown", "w", "--crown", "level=1,scope=x"])


@pytest.mark.parametrize("substrate_args", [
    ["--substrate", "bg"],
    ["-p"],            # headless spellings route the same Rust path
    ["--once"],
])
def test_crown_fix_load_bearing_on_autonomous_substrates(substrate_args) -> None:
    """The fix is needed here and ONLY here. On bg/headless the pane exclusion
    returns False, so without _is_crown_bearing_spawn the spawn would reach the
    Rust client and die. Asserting pane=False alongside crown=True proves the
    crown detector is the load-bearing diversion - not a redundant copy of what
    pane already does. A green bare-spawn test would assert nothing; this does."""
    from fno.agents.rust_runtime import (
        _is_crown_bearing_spawn,
        _is_pane_substrate_spawn,
    )

    args = ["spawn", "w", *substrate_args, "--crown", "level=1,scope=x-d7e4"]
    assert _is_crown_bearing_spawn("spawn", args) is True
    assert _is_pane_substrate_spawn("spawn", args) is False


def test_pane_substrate_already_diverts_without_the_fix() -> None:
    """Documents why the reproducer must use bg/headless: a pane (or bare)
    --crown spawn diverts via the pane exclusion already, so it passes without
    this fix. This is the trap a bare-spawn test falls into."""
    from fno.agents.rust_runtime import _is_pane_substrate_spawn

    assert _is_pane_substrate_spawn(
        "spawn", ["spawn", "w", "--substrate", "pane", "--crown", "level=1,scope=x"]
    )
    assert _is_pane_substrate_spawn(
        "spawn", ["spawn", "w", "--crown", "level=1,scope=x"]  # bare = pane default
    )


def test_crown_bearing_spawn_respects_argv_separator() -> None:
    # --crown after the --argv break belongs to the spawned payload, not fno, so
    # it must NOT keep an otherwise-Rustable spawn in Python (parity with --role).
    from fno.agents.rust_runtime import _is_crown_bearing_spawn

    assert not _is_crown_bearing_spawn(
        "spawn", ["spawn", "w", "--argv", "--crown", "level=1,scope=x"]
    )
