"""`fno agents spawn --crown` routes Python-only, exactly like --role/--route.

Bestow-at-spawn (``--crown level=N,scope=X``) is implemented only in the Python
``cmd_spawn`` path. ``spawn`` auto-routes to the Rust client, which has no
``--crown`` flag, so without this exclusion the documented grammar dies with
``unknown flag: --crown`` - the crown subsystem's recurring defect
(unreachable implementation, not missing). The detector IS the routing
decision (it is OR'd into ``py_spawn`` at rust_runtime.py, and ``route_to_rust``
is only ever called behind ``not py_spawn``), so this test cannot pass against
the unflagged path. Mirrors test_spawn_route_override::test_route_bearing_spawn_detected.
"""
from __future__ import annotations


def test_crown_bearing_spawn_detected() -> None:
    from fno.agents.rust_runtime import _is_crown_bearing_spawn

    assert _is_crown_bearing_spawn(
        "spawn", ["spawn", "--name", "w", "--crown", "level=1,scope=x-d7e4"]
    )
    assert _is_crown_bearing_spawn(
        "spawn", ["spawn", "w", "--crown=level=1,scope=x-d7e4"]
    )
    # A non-crown flag does not trip it; crown is not the only Python-only flag.
    assert not _is_crown_bearing_spawn("spawn", ["spawn", "w", "--role", "build"])
    # Only spawn carries --crown; the crown VERB is already Python-only by name.
    assert not _is_crown_bearing_spawn("crown", ["crown", "w", "--crown", "level=1,scope=x"])


def test_crown_bearing_spawn_respects_argv_separator() -> None:
    # --crown after the --argv break belongs to the spawned payload, not fno, so
    # it must NOT keep an otherwise-Routable spawn in Python (parity with --role).
    from fno.agents.rust_runtime import _is_crown_bearing_spawn

    assert not _is_crown_bearing_spawn(
        "spawn", ["spawn", "w", "--argv", "--crown", "level=1,scope=x"]
    )
