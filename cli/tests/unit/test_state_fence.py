"""Tests for the lifted source-ahead write fence (x-3d21 R1).

The fence's registry behavior is covered where it is bound, in
`test_registry_schema_guard.py`, which passes unchanged across the lift. What
lives here is the mechanism itself: the source discriminator that moved with
it, and the measured reason it reaches no state file beyond the registry.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno import state_fence


class _Boom(RuntimeError):
    pass


def _call(**over: object) -> None:
    kwargs: dict = {
        "target": Path("/shared/registry.json"),
        "shared": Path("/shared/registry.json"),
        "on_disk_version": 19,
        "code_version": 20,
        "source_root": Path("/checkout"),
        "error": _Boom,
        "what": "registry",
        "remedy": "point this checkout at its own registry",
    }
    kwargs.update(over)
    state_fence.refuse_source_ahead_write(**kwargs)  # type: ignore[arg-type]


# --------------------------------------------------------------------------
# The three conditions, one control each
# --------------------------------------------------------------------------


def test_a_source_ahead_write_to_the_shared_file_is_refused() -> None:
    """The positive control. The marker is the refusal naming the shared target."""
    with pytest.raises(_Boom) as exc:
        _call()

    message = str(exc.value)
    assert "/shared/registry.json" in message
    assert "/checkout" in message
    assert "fno doctor update" in message


def test_a_deployed_process_writes_normally() -> None:
    """`source_root=None` means deployed, and a deployed bump is the normal path."""
    _call(source_root=None)


def test_a_store_inside_the_checkout_is_worktree_local_by_construction() -> None:
    _call(target=Path("/checkout/.fno/registry.json"), shared=Path("/checkout/.fno/registry.json"))


def test_a_named_store_is_nobody_s_shared_state() -> None:
    """The escape hatch works by moving the TARGET, not by silencing the check."""
    _call(target=Path("/elsewhere/my-own-registry.json"))


def test_an_absent_version_is_not_a_raise() -> None:
    """AC7. Refusing here would leave a torn file unrepairable by the command
    meant to rewrite it."""
    _call(on_disk_version=None)
    _call(on_disk_version="20")  # type: ignore[arg-type]


def test_an_equal_or_newer_on_disk_version_is_not_a_raise() -> None:
    _call(on_disk_version=20)
    _call(on_disk_version=21)


def test_a_target_inside_a_shared_directory_is_covered() -> None:
    """`shared` may name a directory, so the same key reaches a per-key store."""
    with pytest.raises(_Boom):
        _call(target=Path("/shared/store/one.lock"), shared=Path("/shared/store"))


# --------------------------------------------------------------------------
# The discriminator itself
# --------------------------------------------------------------------------


def test_running_from_source_keys_on_the_module_not_the_cwd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A deployed fno invoked from inside a worktree is still deployed."""
    state_fence.running_from_source.cache_clear()
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".git").mkdir()

    assert state_fence.running_from_source() != tmp_path

    state_fence.running_from_source.cache_clear()


def test_a_worktrees_git_file_counts_as_a_checkout(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A linked worktree's ``.git`` is a FILE. ``is_dir()`` would miss every
    worktree, which is the only place this ever happens."""
    root = tmp_path / "checkout"
    module = root / "cli" / "src" / "fno" / "state_fence.py"
    module.parent.mkdir(parents=True)
    module.write_text("", encoding="utf-8")
    (root / ".git").write_text("gitdir: /elsewhere/.git/worktrees/x\n", encoding="utf-8")
    monkeypatch.setattr(state_fence, "__file__", str(module))
    state_fence.running_from_source.cache_clear()

    assert state_fence.running_from_source() == root

    state_fence.running_from_source.cache_clear()


def test_the_registry_still_reaches_the_discriminator_under_its_own_name(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """`registry._running_from_source` is the hook existing tests patch. The
    lift must not quietly break it into a second, unpatchable copy."""
    import fno.agents.registry as reg

    assert reg._running_from_source is state_fence.running_from_source


# --------------------------------------------------------------------------
# Why the fence stops at the registry, pinned so the inference is not repeated
# --------------------------------------------------------------------------


def test_claims_schema_version_is_a_shape_discriminator_not_a_build_version() -> None:
    """The measured reason the fence does NOT generalize to claims.

    Claims are the only other state file carrying a `schema_version` key, so
    they read as the obvious next place to arm the fence. They are not: 1 and
    2 are two coexisting claim SHAPES bound to the `pid_unavailable` field by a
    model validator, not two releases of one format. Neither supersedes the
    other, so "the on-disk version is below mine" is not a raise, and
    comparing them the registry's way would refuse an ordinary refresh of a
    version-1 claim from any source checkout.

    The assertion is the binding itself: a version-2 claim REQUIRES
    `pid_unavailable`, and a version-1 claim requires a pid. A build version
    could never carry that constraint.
    """
    from fno.claims.types import (
        MAX_SUPPORTED_SCHEMA_VERSION,
        PID_UNAVAILABLE_SCHEMA_VERSION,
        SCHEMA_VERSION,
        Claim,
    )

    base = {
        "key": "node:x-0000",
        "holder": "h",
        "acquired_at": 1,
        "host": "localhost",
    }

    # A version-2 claim is the pid_unavailable SHAPE, not a newer format.
    with pytest.raises(ValueError, match="schema_version=2 requires pid_unavailable"):
        Claim(**base, schema_version=PID_UNAVAILABLE_SCHEMA_VERSION, pid=1)

    # And a version-1 claim cannot carry that shape.
    with pytest.raises(ValueError, match="pid_unavailable claims require schema_version=2"):
        Claim(**base, schema_version=SCHEMA_VERSION, pid_unavailable=True, expires_at=2)

    # Both remain writable by the same build, which is what disqualifies the
    # comparison: neither is "behind" the other.
    assert SCHEMA_VERSION <= MAX_SUPPORTED_SCHEMA_VERSION
    assert PID_UNAVAILABLE_SCHEMA_VERSION <= MAX_SUPPORTED_SCHEMA_VERSION


def test_graph_ledger_and_bus_carry_no_schema_version_to_compare() -> None:
    """The other three state files give the fence nothing to compare at all.

    Read from the live operator files when they exist, so this reports the
    shape on disk rather than the shape the code intends.
    """
    import json

    from fno import paths

    for resolver in (paths.graph_json, paths.ledger_json):
        path = resolver()
        if not path.is_file():
            continue
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        assert isinstance(raw, dict)
        assert "schema_version" not in raw, (
            f"{path} now carries a schema_version; if it is a BUILD version, "
            "fno.state_fence.refuse_source_ahead_write can be armed here and "
            "this test should say so."
        )
