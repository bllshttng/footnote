"""Tests for starting workers in panes placed by a separate mux verb."""
from __future__ import annotations

import json
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir
from tests.agents.test_spawn_pane import FakeRunner


def _spawn(tmp_path: Path, runner: FakeRunner, **kwargs):
    from fno.agents.mux_spawn import dispatch_spawn_pane

    return dispatch_spawn_pane(
        name=kwargs.pop("name", "existing-pane-peer"),
        message=kwargs.pop("message", "hello"),
        provider="claude",
        cwd=tmp_path,
        session="main",
        runner=runner,
        **kwargs,
    )


def _idle_row(tmp_path: Path, pane_id: int) -> dict:
    return {
        "pane_id": pane_id,
        "squad_id": 1,
        "tab_id": 4,
        "cwd": str(tmp_path),
        "child_pid": 4242 + pane_id,
        "fno_id": None,
        "pristine_idle_shell": True,
    }


def test_existing_pane_spawn_keeps_the_requested_pane_id(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    runner = FakeRunner(ls_stdout=json.dumps([_idle_row(tmp_path, 19)]))

    result = _spawn(tmp_path, runner, pane=19)

    assert result.pane_id == 19
    send_calls = [call for call in runner.calls if call[1:4] == ["mux", "pane", "send"]]
    assert len(send_calls) == 1
    send_call = send_calls[0]
    assert send_call[send_call.index("--server") + 1] == "main"
    text = send_call[send_call.index("--text") + 1]
    assert text.startswith("cd -- ") and " && exec " in text
    assert "--guarded" in send_call
    assert not any(call[1:4] == ["mux", "pane", "run"] for call in runner.calls)


def test_cli_threads_existing_pane_to_bounded_dispatch(monkeypatch) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.mux_spawn as mux_spawn

    captured = {}

    def fake_dispatch(**kwargs):
        captured.update(kwargs)
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"], provider=kwargs["provider"], session="main",
            pane_id=2, child_pid=None, session_uuid="claude-session",
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_bounded_pane", fake_dispatch)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    result = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "existing", "--harness", "claude", "--pane", "2", "work"],
    )

    assert result.exit_code == 0, result.output
    assert captured["pane"] == 2


@pytest.mark.parametrize(
    ("rows", "expected"),
    [([], "was not found"), ([{"pane_id": 19, "fno_id": "live-worker"}], "occupied")],
)
def test_existing_pane_refuses_missing_or_occupied_target(
    tmp_path: Path, monkeypatch, rows, expected: str
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError

    runner = FakeRunner(ls_stdout=json.dumps(rows))

    with pytest.raises(DispatchAskError, match=expected):
        _spawn(tmp_path, runner, pane=19)

    assert not any(call[1:4] == ["mux", "pane", "send"] for call in runner.calls)
    assert not any(call[1:4] == ["mux", "pane", "run"] for call in runner.calls)


def test_existing_pane_refuses_creation_placement_conflict(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError

    runner = FakeRunner()

    with pytest.raises(DispatchAskError, match=r"--pane.*--split"):
        _spawn(tmp_path, runner, pane=19, split="right")

    assert runner.calls == []


def test_concurrent_preplaced_panes_keep_ids_and_tab_without_focus_lookup(
    tmp_path: Path, monkeypatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)

    def start(pane_id: int):
        runner = FakeRunner(ls_stdout=json.dumps([_idle_row(tmp_path, pane_id)]))
        return _spawn(tmp_path, runner, name=f"preplaced-{pane_id}", pane=pane_id), runner

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(start, (19, 20)))

    assert {result.pane_id for result, _ in results} == {19, 20}
    assert all(
        not any(call[1:4] == ["mux", "pane", "run"] for call in runner.calls)
        for _, runner in results
    )
