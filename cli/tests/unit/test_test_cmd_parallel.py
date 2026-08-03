"""Parallel defaults for the full Python suite."""
from __future__ import annotations

import sys

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
