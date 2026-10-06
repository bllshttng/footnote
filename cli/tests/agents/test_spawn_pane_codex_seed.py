"""A bounded codex pane holds its seed and delivers it as a fno turn/start.

``codex --remote`` refuses ``--add-dir``, so the pane argv can carry no grant
and an argv seed becomes the TUI's own first turn, which no fno turn/start
ever widens. The bounded lane therefore holds the seed out of the argv and
sends it through ``codex_pane.deliver_seed`` once the thread has bound; the
turn/start carries the widened sandbox policy. Yolo panes keep the argv seed.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

import fno.agents.codex_pane as codex_pane
import fno.agents.mux_spawn as mux_spawn
from fno.agents.writable_dirs import worker_writable_dirs
from tests.agents.test_spawn_pane import FakeRunner, _spawn, use_tmpdir

BOUND_ID = "019fb024-2327-75f3-8b80-06e9d5ade05f"

SEED = "do the thing\n\n<enrich>seed body</enrich>"


def _capture_deliver(monkeypatch, delivered: bool = True) -> list:
    calls: list = []

    def fake_deliver(thread_id, seed, cwd, dirs):
        calls.append((thread_id, seed, cwd, list(dirs)))
        return delivered

    monkeypatch.setattr("fno.agents.codex_pane.deliver_seed", fake_deliver)
    return calls


def test_bounded_codex_spawn_holds_seed_and_delivers_after_bind(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP + AC2-HP: the argv carries no seed; the turn/start delivery does."""
    use_tmpdir(monkeypatch, tmp_path)
    calls = _capture_deliver(monkeypatch)
    result, runner = _spawn(
        monkeypatch,
        tmp_path,
        provider="codex",
        name="seeded",
        message=SEED,
    )

    run_call = next(c for c in runner.calls if c[1:4] == ["mux", "pane", "run"])
    assert not any("do the thing" in tok for tok in run_call), f"seed rode argv: {run_call}"
    assert calls == [(BOUND_ID, calls[0][1], tmp_path, worker_writable_dirs(tmp_path))]
    assert "do the thing" in calls[0][1], "the enriched seed is what gets delivered"
    assert (result.seed, result.seed_source) == ("submitted", "turn-start")
    assert not any(c[1:4] == ["mux", "pane", "send"] for c in runner.calls)


def test_yolo_codex_spawn_keeps_the_argv_seed(tmp_path: Path, monkeypatch) -> None:
    """AC1-ERR: the bypass posture has no strip, so the seed rides as before."""
    use_tmpdir(monkeypatch, tmp_path)
    calls = _capture_deliver(monkeypatch)
    result, runner = _spawn(
        monkeypatch,
        tmp_path,
        provider="codex",
        name="wild",
        message=SEED,
        yolo=True,
    )

    run_call = next(c for c in runner.calls if c[1:4] == ["mux", "pane", "run"])
    assert any("do the thing" in tok for tok in run_call), f"seed left argv: {run_call}"
    assert calls == []
    assert (result.seed, result.seed_source) == ("submitted", "argv")


def test_unconfirmed_typed_seed_after_bind_reaps_and_raises(
    tmp_path: Path, monkeypatch
) -> None:
    """A refused typed fallback fails the spawn like the pre-bind path did."""
    from fno.agents.dispatch_errors import DispatchAskError

    use_tmpdir(monkeypatch, tmp_path)
    calls = _capture_deliver(monkeypatch, delivered=False)
    monkeypatch.setattr(
        mux_spawn,
        "_submit_spawn_seed",
        lambda *a, **k: ("unconfirmed", "text delivered, submission unconfirmed", "typed", "blank"),
    )
    monkeypatch.setattr(mux_spawn, "_reap_spawned_pane", lambda *a, **k: (True, ""))
    runner = FakeRunner()
    with pytest.raises(DispatchAskError) as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            provider="codex",
            name="noseed",
            message=SEED,
            runner=runner,
        )

    assert "never submitted after bind" in str(exc.value)
    assert len(calls) == 1


def test_failed_delivery_falls_back_to_typing_once(tmp_path: Path, monkeypatch) -> None:
    """AC2-ERR: a `delivered: false` answer types the seed, once."""
    use_tmpdir(monkeypatch, tmp_path)
    calls = _capture_deliver(monkeypatch, delivered=False)
    result, runner = _spawn(
        monkeypatch,
        tmp_path,
        provider="codex",
        name="fallback",
        message=SEED,
    )

    assert len(calls) == 1
    sends = [c for c in runner.calls if c[1:4] == ["mux", "pane", "send"]]
    assert len(sends) == 1
    assert result.seed == "submitted"
    assert result.seed_source != "turn-start"


def test_late_bind_after_window_is_stamped_and_kept(tmp_path: Path, monkeypatch) -> None:
    """A bind landing just past the window keeps its row.

    The old path reaped the pane and dropped the row once reconcile came up
    empty, orphaning a live mid-task worker; the re-probe (the _spawn fixture's
    patched backfill stands in for it) stamps the late id onto the id-less row.
    """
    from fno.agents.registry import load_registry

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_binding",
        lambda *a, **k: mux_spawn.PaneBinding(
            session_id=None,
            pane_alive=True,
            reason="binding-window-expired",
            tail="",
        ),
    )
    result, runner = _spawn(
        monkeypatch,
        tmp_path,
        provider="codex",
        name="latebound",
        message=SEED,
    )

    row = next(r for r in load_registry() if r.name == "latebound")
    assert row.harness_session_id == BOUND_ID
    assert row.status == "live"
    assert result.session_uuid == BOUND_ID
    assert not runner.kill_calls


def test_still_silent_reprobe_reaps_as_before(tmp_path: Path, monkeypatch) -> None:
    """Negative arm: a silent re-probe leaves the reap untouched."""
    from fno.agents.dispatch_errors import DispatchAskError

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_binding",
        lambda *a, **k: mux_spawn.PaneBinding(
            session_id=None,
            pane_alive=True,
            reason="binding-window-expired",
            tail="",
        ),
    )
    monkeypatch.setattr(mux_spawn, "_codex_session_id_for_pid", lambda pid, **k: None)
    monkeypatch.setattr("time.sleep", lambda _s: None)
    monkeypatch.setattr(mux_spawn, "_reap_spawned_pane", lambda *a, **k: (True, ""))
    runner = FakeRunner()
    with pytest.raises(DispatchAskError) as exc:
        _spawn(
            monkeypatch,
            tmp_path,
            provider="codex",
            name="silent",
            message=SEED,
            runner=runner,
            codex_binding=False,
        )

    assert "binding-window-expired" in str(exc.value)


def test_unbound_live_pane_types_the_seed_before_the_required_gate(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-ERR: a blown binding window types the seed, no turn/start.

    Codex binding is required, so an id-less row whose re-probe is still
    silent is reaped by the post-reconcile gate; the typed seed preserves
    today's parity (the seed is attempted, the pane's fate is the binding
    contract's, not the seed's).
    """
    from fno.agents.dispatch_errors import DispatchAskError

    use_tmpdir(monkeypatch, tmp_path)
    calls = _capture_deliver(monkeypatch)
    runner = FakeRunner()
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_binding",
        lambda *a, **k: mux_spawn.PaneBinding(
            session_id=None,
            pane_alive=True,
            reason="binding-window-expired",
            tail="",
        ),
    )
    monkeypatch.setattr(mux_spawn, "_codex_session_id_for_pid", lambda pid, **k: None)
    monkeypatch.setattr("time.sleep", lambda _s: None)
    with pytest.raises(DispatchAskError):
        _spawn(
            monkeypatch,
            tmp_path,
            provider="codex",
            name="unbound",
            message=SEED,
            runner=runner,
            codex_binding=False,
        )

    assert calls == []
    sends = [c for c in runner.calls if c[1:4] == ["mux", "pane", "send"]]
    assert len(sends) == 1


def test_dead_pane_gets_neither_delivery_nor_typed_seed(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-ERR: a confirmed-dead pane is given nothing to run."""
    from fno.agents.dispatch_errors import DispatchAskError

    use_tmpdir(monkeypatch, tmp_path)
    calls = _capture_deliver(monkeypatch)
    runner = FakeRunner()
    monkeypatch.setattr(
        mux_spawn,
        "_await_pane_binding",
        lambda *a, **k: mux_spawn.PaneBinding(
            session_id=None,
            pane_alive=False,
            reason="pane exited",
            tail="boom",
        ),
    )
    with pytest.raises(DispatchAskError):
        _spawn(
            monkeypatch,
            tmp_path,
            provider="codex",
            name="died",
            message=SEED,
            runner=runner,
        )

    assert calls == []
    assert not any(c[1:4] == ["mux", "pane", "send"] for c in runner.calls)


def test_deliver_seed_shells_mail_inject_with_the_computed_dirs(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP: the delivery rides the binary's --seed mode, env dirs explicit."""
    seen: dict = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["kwargs"] = kwargs
        return subprocess.CompletedProcess(argv, 0, '{"delivered": true}', "")

    monkeypatch.setattr(codex_pane.subprocess, "run", fake_run)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: Path("/fake/fno-agents"))
    dirs = ["/tmp/a", "/tmp/b"]

    assert codex_pane.deliver_seed("tid", "the seed", Path("/tmp/w"), dirs) is True
    assert seen["argv"] == [
        "/fake/fno-agents",
        "mail-inject",
        "--harness",
        "codex",
        "--session",
        "tid",
        "--seed",
        "/tmp/w",
    ]
    assert seen["kwargs"]["input"] == "the seed"
    assert seen["kwargs"]["env"]["FNO_WORKER_ADD_DIRS"] == os.pathsep.join(dirs)
    assert seen["kwargs"]["timeout"] == 30

    def bad_run(argv, **kwargs):
        return subprocess.CompletedProcess(argv, 0, "not json", "")

    monkeypatch.setattr(codex_pane.subprocess, "run", bad_run)
    assert codex_pane.deliver_seed("tid", "the seed", Path("/tmp/w"), dirs) is False

    def unacked_run(argv, **kwargs):
        return subprocess.CompletedProcess(
            argv, 0, '{"delivered": false, "reason": "turn-start-unacked"}', ""
        )

    monkeypatch.setattr(codex_pane.subprocess, "run", unacked_run)
    assert (
        codex_pane.deliver_seed("tid", "the seed", Path("/tmp/w"), dirs) is True
    ), "an unacked turn/start is in flight; typing would seed twice"
