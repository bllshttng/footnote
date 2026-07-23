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


def test_quiet_contract_failure_tails_log(tmp_path, monkeypatch, capsys):
    """Default mode captures output to .fno/last-test.log and prints only the
    TAIL on failure (errors live at the end; expand upward via the log)."""
    # A suite larger than the tail window, so the pytest header can only reach
    # the transcript by leaking - a one-test suite fits entirely in the tail.
    body = "\n".join(
        f"def test_bad{i}():\n    assert False, 'boom{i}'" for i in range(12)
    )
    f = _write(tmp_path / "test_fail.py", body + "\n")
    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("", encoding="utf-8")

    rc = test_cmd._run([str(f)])
    assert rc == 1
    log = tmp_path / ".fno" / "last-test.log"
    assert log.exists()
    full = log.read_text(encoding="utf-8")
    assert "test session starts" in full  # full output lives in the log...
    out = capsys.readouterr().out
    assert "test session starts" not in out  # ...not in the transcript
    assert "FAIL" in out
    assert str(log) in out
    assert "boom11" in out  # the tail carries the end of the failure output
    assert len(out.splitlines()) <= test_cmd._TAIL_LINES + 3


def test_quiet_contract_success_is_terse(tmp_path, monkeypatch, capsys):
    f = _write(tmp_path / "test_pass.py", "def test_ok():\n    assert True\n")
    monkeypatch.setattr(test_cmd, "_resolve_interpreter", lambda root: sys.executable)
    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("", encoding="utf-8")

    assert test_cmd._run([str(f)]) == 0
    out = capsys.readouterr().out
    assert "PASS" in out
    assert "1 passed" in out  # the one-line pytest summary survives
    assert "test session starts" not in out
    assert len(out.splitlines()) <= 4


def test_stream_mode_inherits_stdio(tmp_path, monkeypatch):
    """--stream keeps the old inherited-stdio behavior: no capture kwargs."""
    captured = _fake_run_capture(monkeypatch, tmp_path)
    captured_kw = {}
    real_fake = test_cmd.subprocess.run

    def fake_run(cmd, env=None, **kw):
        captured_kw.update(kw)
        return real_fake(cmd, env=env, **kw)

    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)
    assert test_cmd._run(["-q"], stream=True) == 0
    assert "stdout" not in captured_kw


def _fake_checkout_with_crates(tmp_path, monkeypatch, n=2):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "cli" / "src" / "fno").mkdir(parents=True)
    (tmp_path / "cli" / "src" / "fno" / "__init__.py").write_text("", encoding="utf-8")
    for name in ["alpha", "beta"][:n]:
        d = tmp_path / "crates" / name
        d.mkdir(parents=True)
        (d / "Cargo.toml").write_text("[package]\n", encoding="utf-8")


def test_rust_mode_runs_each_crate_quietly(tmp_path, monkeypatch, capsys):
    """`fno test rust` runs cargo test -q per crates/*/Cargo.toml (no nextest)."""
    cmds = []

    class _Proc:
        returncode = 0

    def fake_run(cmd, env=None, **kw):
        cmds.append(cmd)
        assert env["RTK_DISABLED"] == "1"
        return _Proc()

    _fake_checkout_with_crates(tmp_path, monkeypatch)
    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)
    monkeypatch.setattr(test_cmd.shutil, "which", lambda name: None)
    assert test_cmd._run_rust([]) == 0
    assert len(cmds) == 2
    for cmd in cmds:
        assert cmd[:3] == ["cargo", "test", "-q"]
        assert "--manifest-path" in cmd


def test_rust_mode_prefers_nextest(tmp_path, monkeypatch):
    cmds = []

    class _Proc:
        returncode = 0

    def fake_run(cmd, env=None, **kw):
        cmds.append(cmd)
        return _Proc()

    _fake_checkout_with_crates(tmp_path, monkeypatch, n=1)
    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)
    monkeypatch.setattr(
        test_cmd.shutil, "which", lambda name: "/x/cargo-nextest" if name == "cargo-nextest" else None
    )
    assert test_cmd._run_rust([]) == 0
    assert cmds[0][:3] == ["cargo", "nextest", "run"]


def test_rust_mode_explicit_manifest_single_run(tmp_path, monkeypatch):
    cmds = []

    class _Proc:
        returncode = 3

    def fake_run(cmd, env=None, **kw):
        cmds.append(cmd)
        return _Proc()

    _fake_checkout_with_crates(tmp_path, monkeypatch)
    monkeypatch.setattr(test_cmd.subprocess, "run", fake_run)
    monkeypatch.setattr(test_cmd.shutil, "which", lambda name: None)
    rc = test_cmd._run_rust(["--manifest-path", "crates/alpha/Cargo.toml"])
    assert rc == 3  # real cargo exit code propagated
    assert len(cmds) == 1  # user's manifest respected, no crate sweep


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
