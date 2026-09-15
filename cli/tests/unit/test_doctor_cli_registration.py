"""The doctor_cli import-binding trap, pinned in both orders.

Registration binds ``update_command`` at FIRST import of ``fno.doctor_cli``
(``doctor_cli.py`` does ``from fno.update import update_command`` and then
``doctor_app.command("update")(update_command)``). A test that patches
``fno.update.update_command`` before that first import registers the fake
permanently: monkeypatch teardown restores the module attribute, but nothing
re-registers, and the fake's signature has no ``--check``, so a later
in-process ``doctor update --check`` is a UsageError (SystemExit 2). This was
the exact smoke-pytest shard-7 failure on PR 1562.

``cli/tests/conftest.py`` imports ``fno.doctor_cli`` at collection, so the
clean order (import first, patch second) is the only order a real test run
can see. These subprocess cases pin both orders against a fresh interpreter,
with ``update_readiness`` stubbed (a call-time read) so no network or git runs.
"""

import os
import subprocess
import sys
from pathlib import Path

PRELUDE = """
from typer.testing import CliRunner
from fno import update
from fno.cli import app


def _fake(source=None, dry_run=False, force=False):
    raise AssertionError("the patch fake must never run")


update.update_readiness = lambda source=None: {"readiness": "stub"}
runner = CliRunner()
"""

TRAP_CASE = (
    PRELUDE
    + """
update.update_command = _fake
import fno.doctor_cli  # noqa: F401  - the poison order: patch BEFORE first import

result = runner.invoke(app, ["doctor", "update", "--check"])
print("EXIT", result.exit_code)
print(result.output)
"""
)

GUARD_CASE = (
    PRELUDE
    + """
import fno.doctor_cli  # noqa: F401  - the guard order: import FIRST
update.update_command = _fake  # the patch now cannot reach the registration

result = runner.invoke(app, ["doctor", "update", "--check"])
print("EXIT", result.exit_code)
print(result.output)
"""
)


def _run_child(code: str) -> subprocess.CompletedProcess:
    src = Path(__file__).resolve().parents[2] / "src"
    env = dict(os.environ)
    env["PYTHONPATH"] = os.pathsep.join([str(src), env.get("PYTHONPATH", "")])
    return subprocess.run(
        [sys.executable, "-c", code],
        capture_output=True,
        text=True,
        env=env,
        timeout=120,
    )


def test_trap_patch_before_first_import_breaks_doctor_update_check():
    """AC1-TRAP: patch -> import -> invoke exits 2 (documentation of the trap)."""
    proc = _run_child(TRAP_CASE)
    assert proc.returncode == 0, proc.stderr
    assert "EXIT 2" in proc.stdout


def test_guard_import_before_patch_keeps_check_green():
    """AC2-GUARD: import -> patch -> invoke exits 0 with the stub readiness JSON."""
    proc = _run_child(GUARD_CASE)
    assert proc.returncode == 0, proc.stderr
    assert "EXIT 0" in proc.stdout
    assert '"readiness": "stub"' in proc.stdout
