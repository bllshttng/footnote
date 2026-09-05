"""Coverage for `fno agents reseat`: the registry half of the re-seat move."""
from __future__ import annotations

import subprocess
from types import SimpleNamespace

import pytest

import fno.agents.reseat as reseat_mod
from fno.agents.registry import AgentResolutionError
from fno.agents.reseat import ReseatError, run_reseat


def _pane_row(name: str = "pane-worker") -> AgentEntryLike:
    return SimpleNamespace(
        name=name,
        mux={"session": "main", "pane_id": 42},
    )


AgentEntryLike = SimpleNamespace


def _ok_runner(argv, **kwargs):
    assert argv[1:4] == ["mux", "thread", "reseat"], argv
    assert "42" in argv, "the pane id from the mux ref rides the command"
    return subprocess.CompletedProcess(argv, 0, stdout="reseat -> pane-worker (portal 0, pane 42)\n", stderr="")


def _refusing_runner(argv, **kwargs):
    return subprocess.CompletedProcess(argv, 1, stdout="", stderr="fno mux thread reseat: portal 0 is live; close it or name another\n")


def _offline_runner(argv, **kwargs):
    return subprocess.CompletedProcess(argv, 1, stdout="", stderr="fno mux thread reseat: no live mux server (connect failed)\n")


def _install(monkeypatch, row, calls=None):
    def fake_resolve(token, path=None, **kwargs):
        if calls is not None:
            calls.append(("resolve", token))
        return SimpleNamespace(entry=row, matched_session_id=None)

    def fake_update(updater, path=None, **kwargs):
        if calls is not None:
            calls.append(("update",))
        entries = [row]
        updater(entries)
        return entries

    monkeypatch.setattr(reseat_mod, "resolve_agent", fake_resolve)
    monkeypatch.setattr(reseat_mod, "update_registry", fake_update)


def test_reseat_drives_the_verb_then_flips_the_mux_ref(monkeypatch):
    row = _pane_row()
    _install(monkeypatch, row)

    receipt = run_reseat("pane-worker", runner=_ok_runner)

    assert receipt["status"] == "reseated"
    assert receipt["pane_id"] == 42
    assert receipt["landing"].startswith("reseat ->")
    assert row.mux is None, "the single flip every reader shares: pane-hosted -> thread"


def test_reseat_passes_the_portal_flag_through(monkeypatch):
    seen = {}

    def runner(argv, **kwargs):
        seen["argv"] = argv
        return subprocess.CompletedProcess(argv, 0, stdout="reseat -> pane-worker (portal 3, pane 42)\n", stderr="")

    _install(monkeypatch, _pane_row())
    run_reseat("pane-worker", portal=3, runner=runner)
    assert seen["argv"][-2:] == ["--portal", "3"], seen["argv"]


def test_a_thread_row_refuses_without_touching_anything(monkeypatch):
    row = SimpleNamespace(name="thread-worker", mux=None)
    calls: list = []
    _install(monkeypatch, row, calls)

    with pytest.raises(ReseatError) as exc:
        run_reseat("thread-worker", runner=_ok_runner)

    assert "not_pane_hosted" in str(exc.value)
    assert calls == [("resolve", "thread-worker")], "no verb, no registry write"


def test_a_server_refusal_leaves_the_registry_alone(monkeypatch):
    row = _pane_row()
    calls: list = []
    _install(monkeypatch, row, calls)

    with pytest.raises(ReseatError) as exc:
        run_reseat("pane-worker", runner=_refusing_runner)

    assert "server_refused" in str(exc.value)
    assert row.mux is not None, "the refusal kept the mux ref exactly as it was"
    assert ("update",) not in calls


def test_a_missing_mux_server_is_its_own_refusal(monkeypatch):
    row = _pane_row()
    _install(monkeypatch, row)

    with pytest.raises(ReseatError) as exc:
        run_reseat("pane-worker", runner=_offline_runner)

    assert "mux_unreachable" in str(exc.value)
    assert row.mux is not None


def test_an_unknown_row_refuses(monkeypatch):
    def fake_resolve(token, path=None, **kwargs):
        raise AgentResolutionError("no agent answers that token")

    monkeypatch.setattr(reseat_mod, "resolve_agent", fake_resolve)

    with pytest.raises(ReseatError) as exc:
        run_reseat("ghost", runner=_ok_runner)

    assert "unknown_row" in str(exc.value)


def test_a_rerun_after_a_half_completed_move_converges(monkeypatch):
    """The server moved; the registry write failed. A re-run must not re-move
    the pane (the server answers already-seated) and the flip must be a
    no-op success on the row that still carries the stale ref."""
    row = _pane_row()
    _install(monkeypatch, row)

    # First call: the verb lands, the registry write fails.
    def failing_update(updater, path=None, **kwargs):
        raise OSError("disk full")

    monkeypatch.setattr(reseat_mod, "update_registry", failing_update)
    with pytest.raises(ReseatError):
        run_reseat("pane-worker", runner=_ok_runner)
    assert row.mux is not None

    # Re-run: the already-seated answer still exits 0, and the flip lands.
    _install(monkeypatch, row)
    receipt = run_reseat("pane-worker", runner=_ok_runner)
    assert receipt["status"] == "reseated"
    assert row.mux is None
