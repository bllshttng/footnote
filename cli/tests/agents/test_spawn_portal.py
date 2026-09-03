"""Portal placement on the thread substrate (x-9b60): the spawn-time
--portal flag, the refusals that keep the surface honest, and the forward
through the one portal door (the fno mux thread reach). Split out of
test_spawn_pane.py, which is over the file budget and may only shrink.
"""

import json

from pathlib import Path

def test_cmd_spawn_placement_rejected_on_bg_substrate(tmp_path: Path, monkeypatch) -> None:
    # AC4-ERR, reshaped by x-9b60: pane geometry flags on a thread still fail
    # closed before any spawn, and the refusal names the missing --portal.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "bg", "-x", "left"],
    )
    assert res.exit_code == 2, res.output
    assert "--portal" in res.output

def test_cmd_spawn_placement_still_rejected_on_headless(tmp_path: Path, monkeypatch) -> None:
    # A one-shot hosts no pane at all, so the flags refuse with the pane-only
    # contract, not the --portal one.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "headless", "-x", "left"],
    )
    assert res.exit_code == 2, res.output
    assert "--split/-x, --at, and --tab apply only to --substrate pane" in res.output

def test_cmd_spawn_portal_places_the_thread_in_one_call(
    tmp_path: Path, monkeypatch
) -> None:
    # (x-9b60, AC7-HP) One spawn call spawns the thread worker AND opens the
    # portal with the placement, through the one portal door (the `fno mux
    # thread` reach). The landing rides stdout AFTER the receipt line.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.dispatch as dispatch_mod
    import fno.agents.thread_portal as thread_portal
    from fno.agents.dispatch import SpawnResult

    calls: list[tuple] = []
    monkeypatch.setattr(
        dispatch_mod,
        "dispatch_spawn",
        lambda **kwargs: calls.append(("spawn", kwargs))
        or SpawnResult(
            kind="created", name=kwargs["name"], provider="claude", short_id=""
        ),
    )

    def fake_portal(name, portal, *, workspace=None, split=None, at=None, tab=None):
        calls.append(("portal", (name, portal, workspace, split, at, tab)))
        return "thread pane -> w2"

    monkeypatch.setattr(thread_portal, "place_thread_portal", fake_portal)
    monkeypatch.setattr(agents_cli, "_stamp_spawned_session_row", lambda **kw: None)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")

    res = CliRunner().invoke(
        agents_cli.agents_app,
        [
            "spawn", "--name", "w2", "/target x-9fd0", "--substrate", "thread",
            "--portal", "1", "--tab", "2", "--split", "right",
        ],
    )
    assert res.exit_code == 0, res.output
    assert [kind for kind, _ in calls] == ["spawn", "portal"], calls
    assert calls[1][1] == ("w2", 1, None, "right", None, "2")
    first = res.output.splitlines()[0]
    assert '"name": "w2"' in first, res.output
    assert "thread pane -> w2" in res.output

def test_cmd_spawn_thread_placement_refuses_without_portal(
    tmp_path: Path, monkeypatch
) -> None:
    # (x-9b60, AC8-EDGE) --tab with a thread and no portal refuses, naming
    # --portal as the missing piece, before any spawn.
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.dispatch as dispatch_mod

    def boom(**_kwargs):
        raise AssertionError("no spawn may run")

    monkeypatch.setattr(dispatch_mod, "dispatch_spawn", boom)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w2", "work", "--substrate", "thread", "--tab", "2"],
    )
    assert res.exit_code == 2, res.output
    assert "--portal" in res.output

def test_cmd_spawn_portal_refused_off_the_thread_substrate(
    tmp_path: Path, monkeypatch
) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli
    import fno.agents.dispatch as dispatch_mod

    def boom(**_kwargs):
        raise AssertionError("no spawn may run")

    monkeypatch.setattr(dispatch_mod, "dispatch_spawn", boom)
    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "--name", "w2", "work", "--portal", "1"],
    )
    assert res.exit_code == 2, res.output
    assert "--portal applies only to --substrate thread" in res.output

def test_place_thread_portal_builds_the_mux_thread_argv() -> None:
    # (x-9b60) The forwarder builds the SAME argv a manual second command
    # would type: one door, no parallel portal path.
    import fno.agents.thread_portal as thread_portal

    captured: dict = {}

    class FakeProc:
        returncode = 0
        stdout = "thread pane -> w2\n"
        stderr = ""

    def fake_run(args, **_kwargs):
        captured["args"] = args
        return FakeProc()

    landing = thread_portal.place_thread_portal(
        "w2", 1, workspace="api", split="right", tab="2", runner=fake_run
    )
    assert captured["args"] == [
        "fno", "mux", "thread", "w2", "--portal", "1",
        "--workspace", "api", "--split", "right", "--tab", "2",
    ]
    assert landing == "thread pane -> w2"

def test_place_thread_portal_failure_names_the_worker_as_live() -> None:
    import pytest

    import fno.agents.thread_portal as thread_portal
    from fno.agents.dispatch import DispatchAskError

    class FakeProc:
        returncode = 3
        stdout = ""
        stderr = "fno mux thread: no live mux server"

    with pytest.raises(DispatchAskError) as excinfo:
        thread_portal.place_thread_portal("w2", 1, runner=lambda args, **_kw: FakeProc())
    assert excinfo.value.exit_code == 1
    assert "the worker is live" in str(excinfo.value)

def test_cmd_spawn_tab_rejected_on_bg_substrate(tmp_path: Path, monkeypatch) -> None:
    from typer.testing import CliRunner

    import fno.agents.cli as agents_cli

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "python")
    res = CliRunner().invoke(
        agents_cli.agents_app,
        ["spawn", "peer", "--harness", "claude", "--substrate", "bg", "--tab", "name:x"],
    )
    assert res.exit_code == 2, res.output
    # x-9b60: a thread placement without --portal names the missing piece.
    assert "--tab" in res.output and "--portal" in res.output
