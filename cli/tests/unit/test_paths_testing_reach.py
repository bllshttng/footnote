"""Reach guard: use_tmpdir must reach an import-time load_settings binding.

x-3d21 R5 specimen: a module that binds ``load_settings`` at import time
keeps the original cached function object, so a fixture that only swaps
module attributes never reaches it, and the module keeps serving the first
root the process resolved. That is the shape that let a test payload reach
the operator's live graph on 2026-09-06. The ambient ``load_settings()``
call below stands in for any earlier activity in the worker process: by the
time a fixture lands, the process has usually resolved state at least once.
"""
from __future__ import annotations

import importlib.util
from pathlib import Path

from fno import config as config_mod
from fno.paths_testing import use_tmpdir

_HELPER_SRC = """
from fno.config import load_settings


def state_dir():
    return load_settings().state_dir
"""


def _import_helper(tmp_path: Path):
    helper_file = tmp_path / "reach_helper.py"
    helper_file.write_text(_HELPER_SRC, encoding="utf-8")
    spec = importlib.util.spec_from_file_location("reach_helper", helper_file)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _assert_helper_sees_tmp(tmp_path: Path, monkeypatch) -> None:
    # Ambient resolve first: the zero-arg cache is populated before the
    # fixture runs, which is the state a real worker process is in.
    config_mod.load_settings()

    helper = _import_helper(tmp_path)
    use_tmpdir(monkeypatch, tmp_path)

    resolved = Path(str(helper.state_dir()).rstrip("/")).resolve()
    assert resolved == (tmp_path / ".fno").resolve(), (
        f"import-time binding resolved {resolved}, not the tmp root"
    )


def test_use_tmpdir_reaches_import_time_binding(tmp_path: Path, monkeypatch) -> None:
    _assert_helper_sees_tmp(tmp_path, monkeypatch)


def test_distinct_pinnings_in_one_worker_stay_apart(tmp_path: Path, monkeypatch) -> None:
    # Second pin in the same process, different FNO_CONFIG: each test must
    # read its own settings in either run order.
    _assert_helper_sees_tmp(tmp_path, monkeypatch)
