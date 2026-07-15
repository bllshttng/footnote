"""Tests for `fno agents watch` (Group 2, Task 4.3): the observe surface for a
held stream-json thread. Read-only; renders the turn lifecycle from the worker's
frame log. The poll loop's IO is injected so no real socket is needed."""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.cli import (
    _render_stream_frame,
    _watch_loop,
    agents_app,
)


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def test_render_frame_lifecycle() -> None:
    assert "session ready" in _render_stream_frame({"kind": "system", "subtype": "init"})
    assert "delivered" in _render_stream_frame({"kind": "user_echo"})
    assert _render_stream_frame({"kind": "assistant", "text": "hi"}).strip().endswith("hi")
    assert "complete" in _render_stream_frame({"kind": "result", "is_error": False})
    assert "errored" in _render_stream_frame({"kind": "result", "is_error": True})
    # An empty partial delta and an unknown frame render nothing.
    assert _render_stream_frame({"kind": "stream_event", "delta": None}) is None
    assert _render_stream_frame({"kind": "other", "type_name": "x"}) is None


def test_watch_loop_renders_then_exits_on_child_dead(capsys) -> None:
    seq = iter([
        {"frames": [{"kind": "user_echo"}, {"kind": "assistant", "text": "hello"},
                    {"kind": "result", "is_error": False}], "next": 3, "child_alive": True},
        {"frames": [], "next": 3, "child_alive": False},
    ])
    rc = _watch_loop(lambda c: next(seq), sleep_fn=lambda: None)
    assert rc == 0
    out = capsys.readouterr().out
    assert "delivered" in out and "hello" in out and "complete" in out
    assert "thread exited" in out


def test_watch_loop_unreachable_returns_1(capsys) -> None:
    rc = _watch_loop(lambda c: None, sleep_fn=lambda: None)
    assert rc == 1
    assert "not live" in capsys.readouterr().err


def test_watch_loop_advances_cursor_and_bounds_polls() -> None:
    seen_cursors: list[int] = []

    def _read(cursor: int) -> dict:
        seen_cursors.append(cursor)
        return {"frames": [], "next": cursor + 1, "child_alive": True}

    rc = _watch_loop(_read, max_polls=3, sleep_fn=lambda: None)
    assert rc == 0
    assert seen_cursors == [0, 1, 2], "cursor must advance via the worker's next"


def test_watch_resolves_worker_short_via_shared_resolver(
    tmp_path: Path, monkeypatch
) -> None:
    """`watch` now resolves through the shared resolver (x-1b1e): a name maps to
    the row's worker short_id, and an unknown token raises the not-found error."""
    from fno.agents.registry import AgentResolutionError, resolve_agent

    home = tmp_path / "agents"
    home.mkdir()
    (home / "registry.json").write_text(json.dumps({
        "schema_version": 9,
        "agents": [
            {"name": "alpha", "short_id": "sw-alpha", "provider": "claude",
             "cwd": "/w", "log_path": "/tmp/a.log"},
            {"name": "beta", "short_id": "sw-beta", "provider": "claude",
             "cwd": "/w", "log_path": "/tmp/b.log"},
        ],
    }))
    reg = home / "registry.json"
    assert resolve_agent("beta", path=reg).worker_short_id == "sw-beta"
    # Addressable by the stored worker short too (shape-agnostic rule).
    assert resolve_agent("sw-beta", path=reg).entry.name == "beta"
    with pytest.raises(AgentResolutionError):
        resolve_agent("ghost", path=reg)


def test_cmd_watch_unknown_name_exits_2(tmp_path: Path, monkeypatch, runner: CliRunner) -> None:
    # No registry.json -> name cannot resolve -> exit 2. Also pins that `watch`
    # routes to the Python command (it is not a Rust client verb).
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / "agents"))
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    result = runner.invoke(agents_app, ["watch", "nobody"])
    assert result.exit_code == 2, (result.stdout or "") + (result.stderr or "")
