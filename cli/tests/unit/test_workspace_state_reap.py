"""The workspace state reap command is a transparent Rust-owned wrapper."""

from __future__ import annotations

import subprocess
from pathlib import Path

from typer.testing import CliRunner

from fno.workspace.cli import cli


runner = CliRunner()


def _stub_binary_resolution(monkeypatch, dev: Path | None, installed: Path | None) -> None:
    monkeypatch.setattr("fno.rust_binary.find_dev_binary", lambda: dev)
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: installed)


def test_reap_defaults_to_rust_state_files_only_dry_run(monkeypatch, tmp_path: Path) -> None:
    dev = tmp_path / "fno-agents-dev"
    installed = tmp_path / "fno-agents-installed"
    _stub_binary_resolution(monkeypatch, dev, installed)
    calls: list[tuple[list[str], bool]] = []

    def fake_run(argv: list[str], *, check: bool) -> subprocess.CompletedProcess[str]:
        calls.append((argv, check))
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr("subprocess.run", fake_run)

    result = runner.invoke(cli, ["reap"])

    assert result.exit_code == 0
    assert calls == [
        ([str(dev), "reap", "--state-files-only", "--dry-run"], False)
    ]


def test_reap_apply_forwards_apply_and_child_exit_code(monkeypatch, tmp_path: Path) -> None:
    installed = tmp_path / "fno-agents-installed"
    _stub_binary_resolution(monkeypatch, None, installed)
    calls: list[list[str]] = []

    def fake_run(argv: list[str], *, check: bool) -> subprocess.CompletedProcess[str]:
        assert check is False
        calls.append(argv)
        return subprocess.CompletedProcess(argv, 23)

    monkeypatch.setattr("subprocess.run", fake_run)

    result = runner.invoke(cli, ["reap", "--apply"])

    assert result.exit_code == 23
    assert calls == [
        [str(installed), "reap", "--state-files-only", "--apply"]
    ]


def test_reap_json_is_explicit_and_accepts_short_flag(monkeypatch, tmp_path: Path) -> None:
    binary = tmp_path / "fno-agents"
    _stub_binary_resolution(monkeypatch, binary, None)
    calls: list[list[str]] = []

    def fake_run(argv: list[str], *, check: bool) -> subprocess.CompletedProcess[str]:
        assert check is False
        calls.append(argv)
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr("subprocess.run", fake_run)

    for flag in ["--json", "-J"]:
        assert runner.invoke(cli, ["reap", flag]).exit_code == 0

    expected = [str(binary), "reap", "--state-files-only", "--dry-run", "--json"]
    assert calls == [expected, expected]


def test_reap_refuses_missing_rust_binary_loudly(monkeypatch) -> None:
    _stub_binary_resolution(monkeypatch, None, None)

    result = runner.invoke(cli, ["reap"])

    assert result.exit_code == 127
    assert "fno-agents binary was not found" in result.output
    assert "fno doctor update --rust" in result.output
