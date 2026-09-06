"""x-3f84 W4 / x-5283: cost attribution and the per-king share of the one ceiling.

The share is a DIVISOR on max_live, never a second budget record. Six kings
dispatching into one undivided pool converge on the cap by construction. The
divisor counts CROWNS, read from the same ``crown_level`` field the court
reads (x-5283 LD1): an ordinary worker that spawned once never shrinks a
king's share, and ``held`` counts the caller's worker rows only - a crowned
peer is a king, never the crowner's worker (LD2)."""
from __future__ import annotations

import os

import pytest

from fno.agents import spawn_gate
from fno.agents.registry import AgentEntry

KING_A = "aaaaaaaa-1111-2222-3333-444455556666"
KING_B = "bbbbbbbb-1111-2222-3333-444455556666"
KING_C = "cccccccc-1111-2222-3333-444455556666"
KING_D = "dddddddd-1111-2222-3333-444455556666"
KING_E = "eeeeeeee-1111-2222-3333-444455556666"
WORKER_SPAWNER = "77777777-1111-2222-3333-444455556666"


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


def _row(
    name,
    pid,
    *,
    spawned_by=None,
    harness="claude",
    crown_level=None,
    session_id=None,
):
    return AgentEntry(
        name=name,
        harness=harness,
        cwd="/tmp",
        log_path="/tmp/l",
        status="live",
        pid=pid,
        spawned_by_session=spawned_by,
        crown_level=crown_level,
        harness_session_id=session_id,
    )


def _census(monkeypatch, rows):
    """A census over fixed registry rows (all pids alive, no bg sockets)."""
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: rows)
    return spawn_gate.census()


def _crowned_rows(kings, alive):
    """One registry row per crowned session: the row IS the king."""
    return [
        _row(f"king-{i}", alive, crown_level=2, session_id=king)
        for i, king in enumerate(kings)
    ]


def test_census_charges_worker_rows_to_their_spawner(monkeypatch):
    alive = os.getpid()
    c = _census(
        monkeypatch,
        [_row("w1", alive, spawned_by=KING_A), _row("w2", alive, spawned_by=KING_A),
         _row("w3", alive, spawned_by=KING_B), _row("w4", alive)],
    )
    assert {k: len(v) for k, v in c.worker_rows.items()} == {
        KING_A: 2, KING_B: 1, None: 1,
    }
    assert c.worker_rows == {KING_A: ["w1", "w2"], KING_B: ["w3"], None: ["w4"]}
    by_name = {w.name: w for w in c.workers}
    assert by_name["w1"].spawned_by == KING_A
    assert by_name["w4"].spawned_by is None


def test_census_reads_crowns_and_ignores_uncrowned_spawners(monkeypatch):
    """AC1-HP: four crowned rows and eight rows spawned by an uncrowned
    session - the divisor is 4, not 5. The spawner's eight workers never make
    it a king: this fixture refuses on main, where every spawner entered the
    divisor (30 // 5 == 6)."""
    alive = os.getpid()
    rows = _crowned_rows([KING_A, KING_B, KING_C, KING_D], alive)
    rows += [_row(f"w{i}", alive, spawned_by=WORKER_SPAWNER) for i in range(8)]
    c = _census(monkeypatch, rows)
    assert c.crowned_sessions == {KING_A, KING_B, KING_C, KING_D}
    reading = spawn_gate.share_reading(c, 30, WORKER_SPAWNER)
    assert reading["kings"] == 4
    assert reading["share"] == 7


def test_share_divides_by_crowns_only():
    # cap 30 across 4 crowns -> 7 each; the uncrowned caller does NOT join
    # the divisor (30 // 4 == 7, never 30 // 5 == 6).
    crowned = {KING_A, KING_B, KING_C, KING_D}
    assert spawn_gate._king_share(30, crowned, WORKER_SPAWNER) == 7
    # A crowned caller is already among the crowns: it adds nothing.
    assert spawn_gate._king_share(30, crowned, KING_A) == 7
    # The floor: a crowded fleet still admits one worker per king.
    assert spawn_gate._king_share(3, {f"k{i}" for i in range(40)}, WORKER_SPAWNER) == 1
    # No crowns anywhere: the floor holds the divisor above zero.
    assert spawn_gate._king_share(30, set(), WORKER_SPAWNER) == 1


def test_held_counts_worker_rows_not_crowned_peers(monkeypatch):
    """AC2-HP: a caller attributed five worker rows and two crowned rows
    holds 5 - the crowned peers are kings, never the crowner's workers. On
    main the same census read held 7."""
    alive = os.getpid()
    rows = [
        _row(f"w{i}", alive, spawned_by=KING_A) for i in range(5)
    ] + [
        _row("peer-b", alive, spawned_by=KING_A, crown_level=1, session_id=KING_B),
        _row("peer-c", alive, spawned_by=KING_A, crown_level=1, session_id=KING_C),
    ]
    c = _census(monkeypatch, rows)
    reading = spawn_gate.share_reading(c, 30, KING_A)
    assert reading["held"] == 5
    assert sorted(reading["held_rows"]) == ["w0", "w1", "w2", "w3", "w4"]


def test_unattributed_rows_name_one_bucket_and_divide_nothing(monkeypatch):
    """AC5-HP: three live rows with no lineage land in one named bucket with
    their names attached, and their presence never moves the divisor."""
    alive = os.getpid()
    rows = _crowned_rows([KING_A, KING_B, KING_C, KING_D], alive)
    rows += [_row(f"ghost{i}", alive) for i in range(3)]
    rows += [_row("w0", alive, spawned_by=KING_A)]
    c = _census(monkeypatch, rows)
    reading = spawn_gate.share_reading(c, 30, KING_A)
    assert reading["unattributed"] == {
        "count": 3,
        "rows": ["ghost0", "ghost1", "ghost2"],
    }
    assert reading["share"] == 7
    assert reading["held"] == 1


def test_king_at_share_refuses_under_cap(monkeypatch, capsys):
    """The commons specimen: fleet under cap, one king holds its full share,
    its next spawn refuses while other kings still have room."""
    alive = os.getpid()
    # 5 crowns; cap 30 so share = 6. KING_A holds exactly 6 worker rows, the
    # fleet holds 6+5+5+4+4 = 24 < 30: under cap, at share.
    held = [6, 5, 5, 4, 4]
    rows = _crowned_rows([KING_A, KING_B, KING_C, KING_D, KING_E], alive)
    for i, n in enumerate(held):
        king = [KING_A, KING_B, KING_C, KING_D, KING_E][i]
        for j in range(n):
            rows.append(_row(f"w{i}-{j}", alive, spawned_by=king))
    c = _census(monkeypatch, rows)
    assert c.slot_count == 29

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


def test_uncrowned_caller_is_refused_but_never_divides(monkeypatch, capsys):
    """AC8-EDGE: a caller with no crown is still share-checked, and its rows
    do not shrink the share - the divisor stays at the crown count (4), so
    the refusal reads share 7 across 4 kings, never 6 across 5."""
    alive = os.getpid()
    rows = _crowned_rows([KING_A, KING_B, KING_C, KING_D], alive)
    rows += [_row(f"w{i}", alive, spawned_by=WORKER_SPAWNER) for i in range(7)]
    c = _census(monkeypatch, rows)
    with pytest.raises(spawn_gate.GateRefused) as exc:
        spawn_gate._check_king_share(c, 30, caller_session=WORKER_SPAWNER)
    assert exc.value.code == spawn_gate.EXIT_KING_SHARE
    err = capsys.readouterr().err
    assert "across 4 kings (share 7)" in err
    assert f"holds 7 of max_live 30" in err


def test_king_under_share_passes_and_operator_skips(monkeypatch):
    alive = os.getpid()
    rows = _crowned_rows([KING_A, KING_B, KING_C, KING_D], alive)
    rows += [_row("w1", alive, spawned_by=KING_A), _row("w2", alive, spawned_by=KING_B)]
    c = _census(monkeypatch, rows)

    # A crowned king holding 1 of its share-of-7 passes...
    spawn_gate._check_king_share(c, 30, caller_session=KING_A)
    # ...and a caller with no resolved session (operator / cron) skips the
    # check entirely: an unattributed spawn is not competing for the commons.
    spawn_gate._check_king_share(c, 30, caller_session=None)


def test_unreadable_registry_yields_unknown_and_enforces_nothing(monkeypatch):
    """AC9-ERR: an unreadable registry returns unknown for every count -
    never zero, which would read as a healthy crownless fleet - and the
    share check enforces nothing on it, exactly as before."""
    def _boom():
        raise OSError("registry locked")

    monkeypatch.setattr("fno.agents.registry.load_registry", _boom)
    c = spawn_gate.census()
    assert c.registry_readable is False
    reading = spawn_gate.share_reading(c, 30, KING_A)
    assert reading["kings"] is None
    assert reading["share"] is None
    assert reading["held"] is None
    assert reading["held_rows"] is None
    assert reading["unattributed"] is None
    # No refusal fires: unknown is not a violation.
    spawn_gate._check_king_share(c, 30, caller_session=KING_A)


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


def test_one_reading_feeds_the_gate_and_the_lanes_census(monkeypatch):
    """AC3-HP (x-5283): the gate's refusal numbers and the lanes census share
    key come from ONE share_reading call on ONE census, including with a
    roster-only claude row carrying pid null in the union - such a row has no
    lineage and no crown, so it must move no count."""
    import json as _json
    import os as _os

    from fno import doctor_lanes

    alive = os.getpid()
    rows = _crowned_rows([KING_A, KING_B, KING_C, KING_D], alive)
    rows += [_row(f"w{i}", alive, spawned_by=KING_A) for i in range(2)]
    c = _census(monkeypatch, rows)

    # A roster-only claude row with pid null: display-only, never counted.
    roster = _os.path.join(_os.environ["FNO_CLAUDE_DAEMON_DIR"], "roster.json")
    with open(roster, "w", encoding="utf-8") as fh:
        _json.dump(
            {"workers": {"w": {"sessionId": "s-roster", "pid": None}}}, fh
        )
    c = spawn_gate.census()

    monkeypatch.setattr("fno.agents.spawn_gate.census", lambda: c)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda: type("I", (), {"session_id": KING_A, "harness": "claude"})(),
    )

    def fake_settings():
        class _A:
            max_live = 30

        class _S:
            agents = _A()

        return _S()

    monkeypatch.setattr("fno.config.load_settings", fake_settings)

    expected = spawn_gate.share_reading(c, 30, KING_A)
    assert expected["share"] == 7 and expected["held"] == 2
    lanes = doctor_lanes._census(None, rows, None, 0)
    assert lanes["share"] == expected


def test_refusal_prints_the_unattributed_bucket(monkeypatch, capsys):
    """AC5-HP: a refusal fired beside unattributed rows prints the bucket by
    name, so the tax a king pays for nobody's rows is visible in the refusal
    itself."""
    alive = os.getpid()
    rows = _crowned_rows([KING_A, KING_B], alive)
    rows += [_row(f"w{i}", alive, spawned_by=KING_A) for i in range(15)]
    rows += [_row(f"ghost{i}", alive) for i in range(3)]
    c = _census(monkeypatch, rows)
    with pytest.raises(spawn_gate.GateRefused) as exc:
        spawn_gate._check_king_share(c, 30, caller_session=KING_A)
    assert exc.value.code == spawn_gate.EXIT_KING_SHARE
    err = capsys.readouterr().err
    assert (
        "3 live row(s) name nobody and sit in the unattributed bucket "
        "(ghost0, ghost1, ghost2)" in err
    )
