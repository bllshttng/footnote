"""Tests for `fno test` (x-8b64 G).

The load-bearing behaviour: the verb returns pytest's *real* exit code (no pipe
masking) and pins PYTHONPATH to the worktree source. We run a tiny generated
test file through `_run` and assert the exit code is the genuine pytest result.
"""
from __future__ import annotations

import sys
from pathlib import Path

from fno import test_cmd


def _write(p: Path, body: str) -> Path:
    p.write_text(body, encoding="utf-8")
    return p


def test_run_propagates_pass(tmp_path, monkeypatch):
    f = _write(tmp_path / "test_pass.py", "def test_ok():\n    assert True\n")
    # Force the running interpreter (it has pytest) regardless of any .venv.
    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    assert test_cmd._run([str(f)]) == 0


def test_run_propagates_failure(tmp_path, monkeypatch):
    f = _write(tmp_path / "test_fail.py", "def test_bad():\n    assert False\n")
    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    # pytest exits 1 on test failure - the real code, not a masked success.
    assert test_cmd._run([str(f)]) == 1


def test_run_missing_interpreter_is_127(monkeypatch):
    # A missing interpreter raises FileNotFoundError (an OSError) -> 127.
    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: "/nonexistent/python")
    assert test_cmd._run(["-q"]) == 127


def _fake_run_capture(monkeypatch, tmp_path):
    """Set up a tmp checkout + a fake subprocess.run that captures the cmd."""
    captured = {}

    class _Proc:
        returncode = 0

    def fake_run(cmd, env=None, **kw):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)
    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("", encoding="utf-8")
    return captured


def test_no_args_defaults_to_cli_tests(tmp_path, monkeypatch):
    """codex P2: bare `fno test` must run THE suite (cli/tests), not collect
    from cwd (which pulls in script-style tests/ that SystemExit at import)."""
    captured = _fake_run_capture(monkeypatch, tmp_path)
    assert test_cmd._run([]) == 0
    assert captured["cmd"][-1] == str((tmp_path / "cli" / "tests").resolve())


def test_flag_only_still_defaults_to_cli_tests(tmp_path, monkeypatch):
    """`-q` alone is not a collection target -> still defaults to cli/tests."""
    captured = _fake_run_capture(monkeypatch, tmp_path)
    assert test_cmd._run(["-q"]) == 0
    assert "-q" in captured["cmd"]
    assert captured["cmd"][-1] == str((tmp_path / "cli" / "tests").resolve())


def test_k_value_does_not_count_as_target(tmp_path, monkeypatch):
    """A `-k expr` value must not be mistaken for a collection path."""
    captured = _fake_run_capture(monkeypatch, tmp_path)
    assert test_cmd._run(["-k", "myexpr"]) == 0
    assert captured["cmd"][-1] == str((tmp_path / "cli" / "tests").resolve())


def test_explicit_target_not_overridden(tmp_path, monkeypatch):
    """An explicit path/nodeid is respected; cli/tests is not appended."""
    captured = _fake_run_capture(monkeypatch, tmp_path)
    assert test_cmd._run(["tests/foo.py::test_x"]) == 0
    assert captured["cmd"][-1] == "tests/foo.py::test_x"
    assert str((tmp_path / "cli" / "tests").resolve()) not in captured["cmd"]


def test_repo_root_finds_checkout():
    # This test file lives under <root>/cli/tests/unit; the resolver must find
    # the checkout root (the dir containing cli/src/fno/__init__.py).
    root = test_cmd._repo_root(Path(__file__).resolve())
    assert root is not None
    assert (root / "cli" / "src" / "fno" / "__init__.py").exists()


def test_pythonpath_pins_worktree_src(tmp_path, monkeypatch):
    """_run must prepend <root>/cli/src to PYTHONPATH so a worktree tests its
    own source. We capture the child env via a fake subprocess.run."""
    captured = {}

    class _Proc:
        returncode = 0

    def fake_run(cmd, env=None, **kw):
        captured["env"] = env
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)
    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("", encoding="utf-8")

    assert test_cmd._run(["-q"]) == 0
    pp = captured["env"]["PYTHONPATH"]
    assert pp.split(":")[0] == str((tmp_path / "cli" / "src").resolve())
    assert captured["env"]["RTK_DISABLED"] == "1"
    assert captured["cmd"][1:3] == ["-m", "pytest"]
