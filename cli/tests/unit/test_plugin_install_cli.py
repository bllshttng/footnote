"""The plugin-install door forwards unknown flags to the Rust verb.

The opencode arm's --yes/--dry-run live in Rust; the Typer door must accept
them without declaring them (flag-registry ratchet) and pass them through
byte-exact.
"""

from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from fno.plugin_install_cli import plugin_app

runner = CliRunner()


class _Proc:
    returncode = 0


def test_door_forwards_extra_args_to_fno_agents(monkeypatch) -> None:
    import fno.plugin_install_cli as door

    captured: dict = {}

    def fake_run(argv, check=False):
        captured["argv"] = argv
        return _Proc()

    monkeypatch.setattr(door.subprocess, "run", fake_run)
    monkeypatch.setattr(door, "_binary", lambda: Path("/bin/true"))
    result = runner.invoke(plugin_app, ["install", "opencode", "--dry-run", "--yes"])
    assert result.exit_code == 0, result.output
    argv = captured["argv"]
    assert argv[1] == "plugin-install"
    assert argv[2:] == ["opencode", "--dry-run", "--yes"]
