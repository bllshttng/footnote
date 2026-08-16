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
    # The short form and its Click attachments route the same way; the attached
    # -kVAL form (no space, no =) is the one the old detector missed, routing a
    # bg spawn to the Rust binary that exits 'unknown flag'.
    assert _is_crown_bearing_spawn("spawn", ["spawn", "w", "-k", "x-d7e4"])
    assert _is_crown_bearing_spawn("spawn", ["spawn", "w", "-k=x-d7e4"])
    assert _is_crown_bearing_spawn("spawn", ["spawn", "w", "-kx-d7e4"])
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


def test_resume_and_route_detectors_catch_attached_short_forms() -> None:
    """The -k fix's sibling detectors (-r resume, -P route) must catch the same
    Click attached-short-option form, or a --substrate bg spawn spelled -r<id>
    or -P<vendor> routes to the Rust binary and fails. One _has_flag matcher
    covers all three; a per-detector copy matched -X and -X=V but missed -XV."""
    from fno.agents.rust_runtime import (
        _is_resume_bearing_spawn,
        _is_route_bearing_spawn,
    )

    assert _is_resume_bearing_spawn("spawn", ["spawn", "w", "-rabc"])
    assert _is_resume_bearing_spawn("spawn", ["spawn", "w", "-r", "abc"])
    assert _is_route_bearing_spawn("spawn", ["spawn", "w", "-Pzai"])
    assert _is_route_bearing_spawn("spawn", ["spawn", "w", "--route=zai/glm"])
    # Non-matching flags and other verbs do not trip them.
    assert not _is_resume_bearing_spawn("spawn", ["spawn", "w", "--name", "n"])
    assert not _is_route_bearing_spawn("crown", ["crown", "w", "-Pzai"])


def test_long_only_detectors_detect_and_respect_argv_boundary() -> None:
    """The detectors with no short form (role, monitor, dispatch-account) route
    through the same _has_flag matcher as the short-bearing ones. Two properties
    must hold: they detect their flag in both spellings, and - the bug this pins
    - a flag AFTER the --argv break belongs to the spawned payload, not fno, so
    it must not keep an otherwise-Rustable spawn in Python. _is_dispatch_account
    _bearing_spawn once iterated raw ``args`` past that boundary (every sibling
    used _args_before_argv); migrating it to _has_flag closes that one-of-N gap."""
    from fno.agents.rust_runtime import (
        _is_dispatch_account_bearing_spawn,
        _is_monitor_bearing_spawn,
        _is_role_bearing_spawn,
    )

    assert _is_role_bearing_spawn("spawn", ["spawn", "w", "--role", "build"])
    assert _is_monitor_bearing_spawn("spawn", ["spawn", "w", "--monitor"])
    assert _is_dispatch_account_bearing_spawn(
        "spawn", ["spawn", "w", "--dispatch-account", "ci"]
    )
    assert _is_dispatch_account_bearing_spawn(
        "spawn", ["spawn", "w", "--dispatch-account=ci"]
    )
    # The load-bearing assertion: a flag after --argv is the payload's, not fno's.
    # dispatch-account is the one that used to read past the boundary.
    for det, flag in (
        (_is_role_bearing_spawn, "--role"),
        (_is_monitor_bearing_spawn, "--monitor"),
        (_is_dispatch_account_bearing_spawn, "--dispatch-account"),
    ):
        assert not det("spawn", ["spawn", "w", "--argv", flag, "x"])
    # Other verbs never route.
    assert not _is_dispatch_account_bearing_spawn(
        "crown", ["crown", "w", "--dispatch-account", "ci"]
    )


def test_pane_detector_stops_at_the_passthrough_fence() -> None:
    """x-1caa: a fenced provider token is not fno's flag. Before the fence
    broke this scan, a passthrough ``-p``/``--once``/``--substrate`` flipped
    the pane detector to False and rerouted a pane-default spawn onto the
    Rust binary lane - past the billing guard, the headless-form refusal, and
    the pane-only passthrough refusal (the guard-on-one-path trap)."""
    from fno.agents.rust_runtime import _is_pane_substrate_spawn

    assert _is_pane_substrate_spawn("spawn", ["spawn", "hi", "--", "-p"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "hi", "--", "--once", "x"])
    assert _is_pane_substrate_spawn("spawn", ["spawn", "hi", "--", "--substrate", "bg"])
    # Pre-fence spellings still count, and the --argv payload boundary holds.
    assert not _is_pane_substrate_spawn("spawn", ["spawn", "hi", "-p"])
    assert not _is_pane_substrate_spawn(
        "spawn", ["spawn", "hi", "--substrate", "headless"]
    )
    assert _is_pane_substrate_spawn("spawn", ["spawn", "hi", "--argv", "-p"])
