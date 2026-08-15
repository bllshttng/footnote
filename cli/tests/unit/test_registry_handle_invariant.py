"""The resolvable-handle invariant (x-7bcd): a registry row that carries no
evidence of its own process can never be judged by anything downstream — the
reaper cannot corroborate its death, the agent view cannot represent its
state, an operator cannot remove it. It is immortal by construction.

At creation, every registry row must carry at least one of three legs an
outside observer can resolve without asking the worker anything:

1. ``pid`` + ``pid_start_time`` (the writer owns the process).
2. ``log_path`` (the writer created the file it records).
3. ``harness`` + ``harness_session_id`` (the writer owns neither).

Enforced at the single Python write choke point, ``write_registry``, mirroring
the Rust ``validate_resolvable_handle`` in ``crates/fno-agents/src/state.rs``
with an IDENTICAL error message (an operator grepping a refusal finds one
answer, not two).
"""
from __future__ import annotations

import json
from dataclasses import asdict
from pathlib import Path

import pytest

from fno.agents import registry as reg


def _handleless(name: str, **overrides) -> reg.AgentEntry:
    """A row with none of the three legs set.

    ``harness`` defaults to a valid identity token ("claude") because
    ``load_registry``'s pre-existing corruption guard (x-8dfc) refuses any
    row with neither a ``provider`` nor a ``harness`` token -- a separate
    invariant from the resolvable-handle one this file tests. Leg 3 still
    needs ``harness_session_id`` too (left ``None`` by default), so a bare
    ``harness`` alone does not accidentally satisfy it.
    """
    fields = {
        "name": name,
        "cwd": "/tmp/x",
        "log_path": "",
        "harness": "claude",
        "harness_session_id": None,
        "pid": None,
        "pid_start_time": None,
    }
    fields.update(overrides)
    return reg.AgentEntry(**fields)


# ---------------------------------------------------------------------------
# _validate_resolvable_handle — the pure per-entry check
# ---------------------------------------------------------------------------


def test_validate_resolvable_handle_refuses_all_three_legs_empty() -> None:
    with pytest.raises(ValueError) as exc_info:
        reg._validate_resolvable_handle(_handleless("ghost"))
    assert str(exc_info.value) == (
        "registry row 'ghost' carries no resolvable handle: needs one of "
        "(pid + pid_start_time), log_path, or (harness + harness_session_id)"
    )


def test_validate_resolvable_handle_passes_for_pid_leg_alone() -> None:
    entry = _handleless("pid-only", pid=4242, pid_start_time=9)
    reg._validate_resolvable_handle(entry)  # must not raise


def test_validate_resolvable_handle_passes_for_log_path_leg_alone() -> None:
    entry = _handleless("log-only", log_path="/tmp/some.log")
    reg._validate_resolvable_handle(entry)  # must not raise


def test_validate_resolvable_handle_passes_for_harness_leg_alone() -> None:
    entry = _handleless("harness-only", harness="claude", harness_session_id="sess-1")
    reg._validate_resolvable_handle(entry)  # must not raise


def test_validate_resolvable_handle_rejects_pid_without_start_time() -> None:
    entry = _handleless("half-pid", pid=4242)
    with pytest.raises(ValueError):
        reg._validate_resolvable_handle(entry)


def test_validate_resolvable_handle_rejects_harness_without_session_id() -> None:
    entry = _handleless("half-harness", harness="claude")
    with pytest.raises(ValueError):
        reg._validate_resolvable_handle(entry)


# ---------------------------------------------------------------------------
# write_registry — the choke point, new rows only
# ---------------------------------------------------------------------------


def test_write_registry_refuses_a_new_handleless_row(tmp_path: Path) -> None:
    path = tmp_path / "registry.json"
    # Seed one legitimate row so the "unchanged on refusal" check has real
    # content, and so this is a genuinely NEW-row refusal, not an empty file.
    seed = reg.AgentEntry(name="seed", cwd="/x", log_path="/x/seed.log", harness="claude")
    reg.write_registry([seed], path=path)
    before = path.read_text(encoding="utf-8")

    with pytest.raises(ValueError, match="carries no resolvable handle"):
        reg.write_registry([seed, _handleless("ghost")], path=path)

    assert path.read_text(encoding="utf-8") == before, (
        "a refused write must leave the registry unchanged"
    )


def test_write_registry_accepts_a_new_row_per_leg(tmp_path: Path) -> None:
    path = tmp_path / "registry.json"
    rows = [
        _handleless("owns-pid", pid=111, pid_start_time=1),
        _handleless("owns-log", log_path="/tmp/owns-log.log"),
        _handleless("owns-harness", harness="codex", harness_session_id="thread-1"),
    ]
    reg.write_registry(rows, path=path)

    entries = reg.load_registry(path)
    assert {e.name for e in entries} == {"owns-pid", "owns-log", "owns-harness"}


def test_write_registry_never_revalidates_a_preexisting_violating_row(tmp_path: Path) -> None:
    """AC3-FR, the wedge test: a registry already holding a row that violates
    the invariant must remain writable forever after. The guard only ever
    looks at rows absent from the pre-write snapshot of names on disk."""
    path = tmp_path / "registry.json"
    # Written directly (not through write_registry) so no guard has ever seen
    # this row — exactly how a pre-x-7bcd legacy row landed.
    path.write_text(
        json.dumps(
            {
                "schema_version": reg.SCHEMA_VERSION,
                "agents": [
                    {
                        "name": "legacy-ghost",
                        "cwd": "/x",
                        "log_path": "",
                        "harness": "claude",
                        "created_at": "2026-01-01T00:00:00Z",
                        "status": "live",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    # An unrelated mutation (a different, valid new row) must succeed even
    # though "legacy-ghost" still carries no resolvable handle.
    new_row = _handleless("new-and-valid", log_path="/tmp/new-and-valid.log")
    entries = reg.load_registry(path)
    reg.write_registry(entries + [new_row], path=path)

    after = {e.name: e for e in reg.load_registry(path)}
    assert "legacy-ghost" in after, "the pre-existing violating row must survive untouched"
    assert "new-and-valid" in after


# ---------------------------------------------------------------------------
# _existing_row_names / _read_raw_registry — the shared-read helper
# ---------------------------------------------------------------------------


def test_existing_row_names_reads_names_off_disk(tmp_path: Path) -> None:
    path = tmp_path / "registry.json"
    seed = reg.AgentEntry(name="alpha", cwd="/x", log_path="/x/a.log", harness="claude")
    reg.write_registry([seed], path=path)

    raw = reg._read_raw_registry(path)
    assert reg._existing_row_names(raw) == {"alpha"}


def test_existing_row_names_empty_for_a_missing_file(tmp_path: Path) -> None:
    path = tmp_path / "nonexistent.json"
    assert reg._existing_row_names(reg._read_raw_registry(path)) == set()
