"""Every spawn-gate refusal leaves a machine-readable trace.

Measured 2026-09-01: `agents.provider_limits.zai.lanes = 7` was binding on the
live fleet (7 of 7 rows, machine load 1.3 per CPU, far under the trigger), and
nothing recorded the refusals. A census of the global journal returned 4815
`claim_acquired` rows - the positive control that the file is read and written -
and zero rows of any kind naming a gate refusal. So an operator could not ask
why a node did not launch; the answer lived only in the stderr of a process that
had already exited.

Every assertion here is on a POSITIVE marker: a parsed JSON line whose `kind` is
`spawn_gate_refused`. Never on a line count alone, and never on the absence of
an error - an absence has three explanations and only one of them is the
outcome.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from fno.agents import events as agent_events
from fno.agents import spawn_gate


@pytest.fixture
def journal(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect the agents events journal into the test's own tmp dir.

    Belt and braces with the conftest per-module pin: the fixture also sets
    FNO_EVENTS_PATH so any non-patched emitter in this module lands here too.
    """
    target = tmp_path / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(target))
    real_emit = agent_events.emit
    monkeypatch.setattr(
        agent_events,
        "emit",
        lambda kind, **data: real_emit(kind, path=target, **data),
    )
    return target


def _refusals(journal: Path) -> list[dict[str, Any]]:
    if not journal.exists():
        return []
    rows = [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]
    return [r for r in rows if r.get("kind") == "spawn_gate_refused"]


def test_provider_cap_refusal_names_provider_cap_and_count(journal: Path) -> None:
    """AC1-HP. The refusal that was binding on the live fleet, made visible."""
    spawn_gate._CURRENT_SPAWN.set(("t-probe", "thread"))

    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate._refuse_provider_cap("zai", 7, current=7)

    assert excinfo.value.code == spawn_gate.EXIT_PROVIDER_CAP

    rows = _refusals(journal)
    assert len(rows) == 1, rows
    row = rows[0]
    assert row["reason"] == "provider_cap"
    assert row["provider"] == "zai"
    assert row["cap"] == 7
    assert row["count"] == 7
    assert row["name"] == "t-probe"
    assert row["substrate"] == "thread"
    assert row["gate"] == "python"
    assert row["exit_code"] == spawn_gate.EXIT_PROVIDER_CAP


def test_ram_floor_refusal_carries_its_measurement_and_threshold(
    journal: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A refusal names the value it measured against the threshold it used."""
    monkeypatch.setattr(spawn_gate, "available_ram_gb", lambda: 1.5)

    with pytest.raises(spawn_gate.GateRefused):
        spawn_gate._check_ram_floor(4.0)

    rows = _refusals(journal)
    assert len(rows) == 1, rows
    assert rows[0]["reason"] == "ram_floor"
    assert rows[0]["available_gb"] == 1.5
    assert rows[0]["min_free_gb"] == 4.0


def test_king_share_refusal_emits_though_it_carries_no_receipt(journal: Path) -> None:
    """The king share refuses with receipt=None, and still has to be answerable.

    This is the branch the seam exists for: four refusals never built a
    receipt, so a design that only forwarded receipts would have left them as
    silent as before.
    """
    census = spawn_gate.LiveCensus(
        crowned_sessions={"kingA", "kingB"},
        worker_rows={
            "kingA": ["a0", "a1", "a2", "a3"],
            "kingB": ["b0", "b1", "b2", "b3"],
        },
    )

    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate._check_king_share(census, 8, caller_session="kingA")

    assert excinfo.value.receipt is None, "stdout receipt shape must not change"

    rows = _refusals(journal)
    assert len(rows) == 1, rows
    assert rows[0]["reason"] == "king_share"
    assert rows[0]["king"] == "kingA"
    assert rows[0]["held"] == 4
    assert rows[0]["max_live"] == 8


def test_an_unwritable_journal_never_changes_the_refusal(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC2-ERR. Telemetry never changes a gate outcome."""

    def _boom(kind: str, **data: Any) -> None:
        raise OSError("read-only file system")

    monkeypatch.setattr(agent_events, "emit", _boom)

    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate._refuse_provider_cap("zai", 7, current=7)

    assert excinfo.value.code == spawn_gate.EXIT_PROVIDER_CAP
    assert excinfo.value.receipt is not None
    assert excinfo.value.receipt["reason"] == "provider_cap"


def test_mutex_fault_refusal_names_the_mutex_not_the_cap(journal: Path) -> None:
    """AC: the claims-layer fault path gets its own reason. The old receipt
    read `reason: provider_cap, count: null` for a fault that never read a
    cap (2026-09-09, zai held 9 of 10 while the mutex was busy)."""
    spawn_gate._CURRENT_SPAWN.set(("t-probe", "thread"))
    fault = spawn_gate.ProviderCountUnavailable("spawn mutex is busy")

    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate._refuse_gate_fault("zai", fault)

    assert excinfo.value.code == spawn_gate.EXIT_PROVIDER_CAP, (
        "exit-code consumers are unaffected by the reason split"
    )
    rows = _refusals(journal)
    assert len(rows) == 1, rows
    assert rows[0]["reason"] == "gate_mutex_unavailable"
    assert rows[0]["error"] == "spawn mutex is busy"
    assert rows[0]["provider"] == "zai"
    assert "count" not in rows[0] and "cap" not in rows[0]


def test_a_provider_cap_receipt_cannot_carry_a_null_count() -> None:
    """The type is the guard: `current` is a required positional, so no
    provider_cap receipt can ever carry count: None again."""
    with pytest.raises(TypeError):
        spawn_gate._refuse_provider_cap("zai", 10)


def test_the_cpu_refusal_names_the_axis_that_decided(journal: Path) -> None:
    """x-7783 AC13: a CPU-axis refusal carries `axis` and the one-word verdict
    for every axis read before it, so the journal answers "what decided"."""
    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate._refuse(
            spawn_gate.EXIT_LOAD_REFUSED,
            {"status": "refused", "reason": "cpu_share_undecidable"},
            reason="cpu_share_undecidable",
            axis="fleet_cpu_share",
            axes_read={"ram": "ok", "load_15m": "ok", "cpu": "undecidable"},
        )

    rows = _refusals(journal)
    assert len(rows) == 1, rows
    assert rows[0]["axis"] == "fleet_cpu_share"
    assert rows[0]["axes_read"]["cpu"] == "undecidable"
    assert rows[0]["reason"] == "cpu_share_undecidable"
