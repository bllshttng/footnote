"""Parallel defaults for the full Python suite."""
from __future__ import annotations

import sys
from pathlib import Path

from fno import test_cmd


def test_bare_run_uses_capped_loadgroup_parallelism(tmp_path, monkeypatch):
    captured: dict[str, list[str]] = {}

    class _Proc:
        returncode = 0

    def fake_run(cmd, env=None, **kwargs):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("")
    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)

    assert test_cmd._run([]) == 0
    assert captured["cmd"][1:9] == [
        "-m",
        "pytest",
        "-n",
        "auto",
        "--maxprocesses=4",
        "--dist=loadgroup",
        str((tmp_path / "cli" / "tests").resolve()),
    ]


def test_bare_run_keeps_explicit_xdist_settings(tmp_path, monkeypatch):
    captured: dict[str, list[str]] = {}

    class _Proc:
        returncode = 0

    def fake_run(cmd, env=None, **kwargs):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("")
    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)

    assert test_cmd._run(["-n", "1", "--maxprocesses=2", "--dist", "loadscope"]) == 0
    cmd = captured["cmd"]
    assert cmd.index("auto") < cmd.index("1")
    assert cmd.index("--maxprocesses=4") < cmd.index("--maxprocesses=2")
    assert cmd.index("--dist=loadgroup") < cmd.index("--dist")


def test_smoke_suite_uses_loadgroup_scheduler():
    pytest_step = next(
        command
        for name, _cwd, command in test_cmd._STRUCTURAL_STEPS
        if name == "Pytest (unit + integration)"
    )

    assert "-n auto --maxprocesses=4 --dist=loadgroup" in pytest_step


def test_changed_packet_caps_auto_workers() -> None:
    selections = [
        {"kind": "pytest", "target": f"cli/tests/unit/test_{index}.py"}
        for index in range(4)
    ]

    pytest_step = test_cmd._changed_steps(Path.cwd(), selections)[0][2]

    assert "-n auto --maxprocesses=4 --dist=loadgroup" in pytest_step


def test_known_parallel_racers_are_marked_and_grouped(pytestconfig):
    conftest_path = (Path(__file__).parents[1] / "conftest.py").resolve()
    plugin = next(
        candidate
        for candidate in pytestconfig.pluginmanager.get_plugins()
        if Path(getattr(candidate, "__file__", "missing")).resolve() == conftest_path
    )
    hook = getattr(plugin, "pytest_collection_modifyitems", None)
    assert hook is not None

    class _Item:
        def __init__(self, nodeid: str):
            self.nodeid = nodeid
            self.markers = []

        def add_marker(self, marker):
            self.markers.append(marker)

    racer = _Item(
        "cli/tests/unit/test_graph_sidecar_window.py::"
        "test_ac3hp_concurrent_writes_never_surface_corruption"
    )
    ordinary = _Item("cli/tests/unit/test_fno_test_cmd.py::test_repo_root_finds_checkout")
    target_init = _Item(
        "tests/hooks/test_init_target_state_skip_flags.py::"
        "test_flat_skip_flags_per_size_profile[S]"
    )

    hook([racer, target_init, ordinary])

    assert [marker.name for marker in racer.markers] == ["serial", "xdist_group"]
    assert racer.markers[1].kwargs == {"name": "serial"}
    assert [marker.name for marker in target_init.markers] == ["serial", "xdist_group"]
    assert target_init.markers[1].kwargs == {"name": "serial"}
    assert ordinary.markers == []


def test_help_describes_parallel_default():
    assert "Bare `fno test` runs the Python suite in parallel" in test_cmd.test_command.help
    assert "Bare `fno test` is serial" not in test_cmd.test_command.help
