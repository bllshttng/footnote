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
    census = spawn_gate.LiveCensus(king_counts={"kingA": 4, "kingB": 4})

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


def test_the_load_refusal_keeps_its_cause_stated_marker(journal: Path) -> None:
    """The cause-stated contract survives routing through the emit seam.

    run_gate appends a second, independently sampled footprint reading to a
    load refusal ONLY when the refusal could not say whose CPU it was. A
    refactor that dropped the marker would print two disagreeing measurements
    in one refusal - the exact defect x-7c0f removed.
    """
    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate._refuse_load_cause_stated(reason="fleet_cpu_share", share=0.9)

    assert getattr(excinfo.value, "cause_stated", False) is True
    rows = _refusals(journal)
    assert len(rows) == 1, rows
    assert rows[0]["reason"] == "fleet_cpu_share"
