"""x-3f84 W4: cost attribution to the king that spawned, and the per-king
share of the one ceiling.

The share is a DIVISOR on max_live, never a second budget record. Six kings
dispatching into one undivided pool converge on the cap by construction;
attribution is what makes the division possible, which is why it follows the
process-tree resolution wave."""
from __future__ import annotations

import os

import pytest

from fno.agents import spawn_gate
from fno.agents.registry import AgentEntry


@pytest.fixture(autouse=True)
def _isolated_world(tmp_path, monkeypatch):
    daemon = tmp_path / "daemon"
    daemon.mkdir()
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(daemon))
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims-root"))
    monkeypatch.setenv("FNO_THINK_SPAWN", "0")
    # conftest disables the gate suite-wide; re-arm it here -- these tests
    # exercise the gate itself.
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)
    monkeypatch.setenv("FNO_CC_DAEMON_RV_ROOT", str(tmp_path / "no-farm"))
    yield


KING_A = "aaaaaaaa-1111-2222-3333-444455556666"
KING_B = "bbbbbbbb-1111-2222-3333-444455556666"


def _row(name, pid, *, spawned_by=None, harness="claude"):
    return AgentEntry(
        name=name,
        harness=harness,
        cwd="/tmp",
        log_path="/tmp/l",
        status="live",
        pid=pid,
        spawned_by_session=spawned_by,
    )


def _census(monkeypatch, rows):
    """A census over fixed registry rows (all pids alive, no bg sockets)."""
    alive = os.getpid()
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    return spawn_gate.census()


def test_census_attributes_rows_to_kings(monkeypatch):
    alive = os.getpid()
    c = _census(
        monkeypatch,
        [_row("w1", alive, spawned_by=KING_A), _row("w2", alive, spawned_by=KING_A),
         _row("w3", alive, spawned_by=KING_B), _row("w4", alive)],
    )
    assert c.king_counts == {KING_A: 2, KING_B: 1, None: 1}
    by_name = {w.name: w for w in c.workers}
    assert by_name["w1"].spawned_by == KING_A
    assert by_name["w4"].spawned_by is None


def test_share_divides_the_one_ceiling():
    # cap 30 across 6 kings -> 5 each; the caller counts even with no live rows.
    counts = {f"king{i:08d}-0000-0000-0000-000000000000": 1 for i in range(5)}
    assert spawn_gate._king_share(30, counts, "caller000-0000-0000-0000-000000000000") == 5
    # One king alone owns the whole ceiling; a first spawn is never refused.
    assert spawn_gate._king_share(30, {}, KING_A) == 30
    # The floor: a crowded fleet still admits one worker per king.
    crowded = {f"k{i:08d}-0000-0000-0000-000000000000": 1 for i in range(40)}
    assert spawn_gate._king_share(3, crowded, "caller000-0000-0000-0000-000000000000") == 1
    # Unattributed rows never divide the share.
    assert spawn_gate._king_share(30, {None: 28}, KING_A) == 30


def test_king_at_share_refuses_under_cap(monkeypatch, capsys):
    """The commons specimen: fleet under cap, one king holds its full share,
    its next spawn refuses while other kings still have room."""
    alive = os.getpid()
    # 5 kings; cap 30 so share = 6. KING_A holds exactly 6, the fleet holds
    # 6+5+5+4+4 = 24 < 30: under cap, at share.
    held = [6, 5, 5, 4, 4]
    rows = []
    for i, n in enumerate(held):
        king = KING_A if i == 0 else f"king{i:08d}-0000-0000-0000-000000000000"
        for j in range(n):
            rows.append(_row(f"w{i}-{j}", alive, spawned_by=king))
    c = _census(monkeypatch, rows)
    assert c.slot_count == 24

    def fake_settings():
        class _A:
            max_live = 30
            min_free_gb = 0.0
            max_load_per_cpu = 0.0
            provider_limits = {"zai": 5}

        class _S:
            agents = _A()

        return _S()

    monkeypatch.setattr("fno.config.load_settings", fake_settings)
    monkeypatch.setattr(spawn_gate, "census", lambda: c)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda: type("I", (), {"session_id": KING_A, "harness": "claude"})(),
    )
    with pytest.raises(SystemExit) as exc:
        spawn_gate.run_gate("w-new", "bg", no_wait=True)
    assert exc.value.code == spawn_gate.EXIT_KING_SHARE == 80
    err = capsys.readouterr().err
    assert "aaaaaaaa" in err and "share 6" in err and "across 5 kings" in err
    assert "waiting cannot help" in err


def test_king_under_share_passes_and_operator_skips(monkeypatch):
    alive = os.getpid()
    rows = [_row("w1", alive, spawned_by=KING_A), _row("w2", alive, spawned_by=KING_B)]
    c = _census(monkeypatch, rows)

    # A king holding 1 of its share-of-15 passes...
    spawn_gate._check_king_share(c, 30, caller_session=KING_A)
    # ...and a caller with no resolved session (operator / cron) skips the
    # check entirely: an unattributed spawn is not competing for the commons.
    spawn_gate._check_king_share(c, 30, caller_session=None)


def test_cap_refusal_wins_over_share_verdict(monkeypatch):
    """Ordering: the fleet cap sits BEFORE the share check in run_gate, so a
    full fleet explains itself as full, never as a share refusal (the two
    caps must not race to explain themselves). cap=3, three live rows all
    KING_A: the king is also at its share, but the cap fires first."""
    alive = os.getpid()
    rows = [_row(f"w{i}", alive, spawned_by=KING_A) for i in range(3)]
    c = _census(monkeypatch, rows)
    monkeypatch.setattr(spawn_gate, "census", lambda: c)

    def fake_settings():
        class _A:
            max_live = 3
            min_free_gb = 0.0
            max_load_per_cpu = 0.0
            provider_limits = {"zai": 5}

        class _S:
            agents = _A()

        return _S()

    monkeypatch.setattr("fno.config.load_settings", fake_settings)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda: type("I", (), {"session_id": KING_A, "harness": "claude"})(),
    )
    with pytest.raises(SystemExit) as exc:
        spawn_gate.run_gate("w-new", "bg", no_wait=True)
    assert exc.value.code == spawn_gate.EXIT_NO_WAIT
