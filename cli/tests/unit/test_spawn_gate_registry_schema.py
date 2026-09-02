"""A spawn into a write-refusing fleet must name the schema mismatch.

On 2026-08-28 the shared registry sat one version ahead of the deployed fno for
several hours. Claiming a node and stamping mail are both WRITES, so the whole
spawn path was refused, and no symptom named the registry. A blocked worker
reported "Claim store is not writable for this Codex session" and blamed a
sandbox permission profile, and the king accepted that reading. This is the
"symptom surfaces far from the cause" property ``registry.py`` already warns
about, observed a second time and still not caught in the moment.

So the gate refuses, loudly, and says which two integers disagree.

It also emits ONE event. Every degraded READ prints a banner, but a refused
WRITE returns an error to one caller, and that caller reports it in its own
words to a king who is not watching. Nothing collects those into "the fleet
cannot write."
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno import paths
from fno.agents import registry as reg
from fno.agents import spawn_gate


@pytest.fixture
def shared(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    target = tmp_path / "fno-home" / "agents" / "registry.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr(paths, "agents_registry_path", lambda: target)
    return target


def _write(path: Path, version: int) -> None:
    path.write_text(
        json.dumps({"schema_version": version, "agents": []}, indent=2),
        encoding="utf-8",
    )


def test_a_registry_ahead_of_this_fno_refuses_the_spawn(
    shared: Path, capsys: pytest.CaptureFixture
) -> None:
    ahead = reg.SCHEMA_VERSION + 1
    _write(shared, ahead)

    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate._check_registry_schema()

    assert excinfo.value.code == spawn_gate.EXIT_REGISTRY_SCHEMA
    receipt = excinfo.value.receipt
    assert receipt["reason"] == "registry_schema"
    assert receipt["on_disk"] == ahead
    assert receipt["understood"] == reg.SCHEMA_VERSION

    message = capsys.readouterr().err
    assert f"schema_version={ahead}" in message
    assert f"schema_version={reg.SCHEMA_VERSION}" in message
    assert str(shared) in message
    assert "fno agents registry-repair" in message


def test_the_refusal_leaves_a_machine_readable_trail(
    shared: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """One worker's prose to an unwatching king is not a fleet-wide signal."""
    events = tmp_path / "events.jsonl"
    monkeypatch.setattr(paths, "state_dir", lambda: tmp_path)
    _write(shared, reg.SCHEMA_VERSION + 1)

    with pytest.raises(spawn_gate.GateRefused):
        spawn_gate._check_registry_schema()

    rows = [json.loads(line) for line in events.read_text().splitlines() if line]
    # ONE event, not two. This branch once emitted a bespoke
    # `registry_schema_ahead` beside the refusal; every gate refusal now exits
    # through the same `_refuse` seam, and `reason` still isolates this case.
    assert [r["kind"] for r in rows] == ["spawn_gate_refused"]
    assert rows[0]["reason"] == "registry_schema"
    assert rows[0]["on_disk"] == reg.SCHEMA_VERSION + 1
    assert rows[0]["understood"] == reg.SCHEMA_VERSION


def test_force_does_not_bypass_the_schema_check(
    shared: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`--force` means "I know the machine is busy". A schema mismatch is not
    resource pressure: it is a worker that can neither claim its node nor stamp
    its mail. Forcing past it reproduces the 2026-08-28 failure with the
    diagnosis suppressed, and the force warning does not even name it."""
    _write(shared, reg.SCHEMA_VERSION + 1)
    # conftest disables the gate suite-wide; this test exercises run_gate itself,
    # so it re-arms the gate the way test_spawn_gate.py's own fixture does.
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setattr(
        spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
    )

    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate.run_gate("forced", "bg", force=True)

    assert excinfo.value.code == spawn_gate.EXIT_REGISTRY_SCHEMA


def test_the_dequeue_path_rechecks_the_schema(
    shared: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A spawn can sit in the queue for up to QUEUE_TIMEOUT_S, and another
    process can raise the shared schema inside that window. The entry check
    cannot see that; the dequeue re-check can, the way the RAM floor already
    re-checks on dequeue."""
    _write(shared, reg.SCHEMA_VERSION)
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setattr(
        spawn_gate, "census", lambda: spawn_gate.LiveCensus(workers=[])
    )
    # Entry sees a healthy registry; the file moves ahead before the dequeue
    # check reads it again.
    real_check = spawn_gate._check_ram_floor

    def _poison_then_check(floor_gb: float) -> None:
        _write(shared, reg.SCHEMA_VERSION + 1)
        real_check(floor_gb)

    monkeypatch.setattr(spawn_gate, "_check_ram_floor", _poison_then_check)
    spawn_gate.run_gate("first", "bg").release()

    with pytest.raises(spawn_gate.GateRefused) as excinfo:
        spawn_gate.run_gate("second", "bg")

    assert excinfo.value.code == spawn_gate.EXIT_REGISTRY_SCHEMA


@pytest.mark.parametrize("delta", [0, -1])
def test_a_registry_at_or_below_this_fno_passes(shared: Path, delta: int) -> None:
    _write(shared, reg.SCHEMA_VERSION + delta)

    spawn_gate._check_registry_schema()


def test_an_unresolvable_path_skips_but_leaves_a_trace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The skip is fail-open by contract, but it must not be invisible.

    Its sibling guards warn on the equivalent skip; this one cannot, because the
    gate's own `test_under_cap_passes_silently` pins an empty stderr on the pass
    path and this branch fires there. So it emits instead.
    """
    events = tmp_path / "events.jsonl"
    monkeypatch.setattr(paths, "state_dir", lambda: tmp_path)
    monkeypatch.setattr(
        paths, "agents_registry_path", lambda: (_ for _ in ()).throw(RuntimeError("no settings"))
    )

    spawn_gate._check_registry_schema()

    rows = [json.loads(line) for line in events.read_text().splitlines() if line]
    assert [r["kind"] for r in rows] == ["registry_schema_check_skipped"]
    assert "no settings" in rows[0]["reason"]


@pytest.mark.parametrize("body", [None, "", "{", "[]", '{"schema_version": "20"}'])
def test_an_unreadable_registry_skips_the_check(shared: Path, body) -> None:
    """Same contract as the RAM floor and the load ceiling: unreadable skips.

    A spawn is not the place to adjudicate a torn file, and refusing here would
    make the repair verb itself unspawnable.
    """
    if body is not None:
        shared.write_text(body, encoding="utf-8")

    spawn_gate._check_registry_schema()
