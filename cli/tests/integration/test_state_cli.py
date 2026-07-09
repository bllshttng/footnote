"""Integration tests for `fno state` subcommands.

Uses subprocess so we test the real CLI entry point, not just Python APIs.
Tests use tmp_path fixtures to isolate state files.
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest
import yaml


# -- Helpers --

MINIMAL_STATE = """\
---
status: IN_PROGRESS
iteration: 3
session_id: 20260421T093631Z-97817-920dac
graph_id: ab-eea09178
---
# Target Session State

Initialized for testing.
"""

VALID_TARGET_STATE = """\
---
status: IN_PROGRESS
iteration: 1
---
# Body
"""


def _cli_cmd() -> list[str]:
    """Return the CLI invocation prefix.

    Tries 'fno' from PATH first (installed entrypoint),
    falls back to python -c runner using typer's CLI app.
    """
    import shutil
    # The console script is `fno-py` (the Rust mux binary owns `fno`); the state
    # CLI is Python, so target it directly rather than through the mux front door.
    abilities_exe = shutil.which("fno-py")
    if abilities_exe:
        return [abilities_exe]
    # Fallback: invoke via typer's app directly
    return [sys.executable, "-c",
            "from fno.cli import app; app()"]


def run_cli(*args: str, cwd: Path | None = None) -> subprocess.CompletedProcess:
    """Run fno CLI with given args, capture stdout+stderr."""
    from typer.testing import CliRunner
    from fno.cli import app

    runner = CliRunner()
    result = runner.invoke(app, list(args))
    # Wrap in a CompletedProcess-like object
    return subprocess.CompletedProcess(
        args=list(args),
        returncode=result.exit_code,
        stdout=result.output or "",
        stderr="" if result.exception is None else str(result.exception),
    )


def make_state_file(tmp_path: Path, content: str = MINIMAL_STATE) -> Path:
    state = tmp_path / "target-state.md"
    state.write_text(content)
    return state


# -- AC1-HP: state show prints frontmatter as JSON --

def test_ac1_hp_show_json(tmp_path: Path) -> None:
    """AC1-HP: fno --json state show --path FILE outputs valid JSON."""
    state_file = make_state_file(tmp_path)
    result = run_cli("--json", "state", "show", "--path", str(state_file))
    assert result.returncode == 0, f"stderr: {result.stderr}"
    data = json.loads(result.stdout)
    assert data["status"] == "IN_PROGRESS"
    assert data["iteration"] == 3


def test_ac1_hp_show_field(tmp_path: Path) -> None:
    """AC1-HP: state show --field returns single value."""
    state_file = make_state_file(tmp_path)
    result = run_cli("state", "show", "--path", str(state_file), "--field", "status")
    assert result.returncode == 0, f"stderr: {result.stderr}"
    assert result.stdout.strip() == "IN_PROGRESS"


def test_ac1_hp_show_field_json(tmp_path: Path) -> None:
    """AC1-HP: state show --field --json returns {field: value}."""
    state_file = make_state_file(tmp_path)
    result = run_cli("--json", "state", "show", "--path", str(state_file), "--field", "status")
    assert result.returncode == 0, f"stderr: {result.stderr}"
    data = json.loads(result.stdout)
    assert data["status"] == "IN_PROGRESS"


# -- AC2-HP: state set updates a field atomically --

def test_ac2_hp_set_field(tmp_path: Path) -> None:
    """AC2-HP: state set --field status --value COMPLETE updates the file."""
    state_file = make_state_file(tmp_path)

    result = run_cli(
        "state", "set",
        "--path", str(state_file),
        "--field", "status",
        "--value", "COMPLETE",
    )
    assert result.returncode == 0, f"stderr: {result.stderr}"

    # Verify via show
    result2 = run_cli("state", "show", "--path", str(state_file), "--field", "status")
    assert result2.stdout.strip() == "COMPLETE"


def test_set_field_with_iso_updated_at_in_file(tmp_path: Path) -> None:
    """REGRESSION: end-to-end repro of example-pipeline Wave 7 item 6.

    Before fix: state set --field pr_number on a file whose updated_at is an
    ISO timestamp errored with `Input should be a valid string` pointing at
    the untouched updated_at field, because yaml.safe_load coerced the ISO
    string into a datetime that then failed the schema's Optional[str].

    After fix: the set succeeds, both fields are preserved.
    """
    state_file = tmp_path / "target-state.md"
    state_file.write_text(
        "---\n"
        "status: IN_PROGRESS\n"
        "iteration: 1\n"
        "updated_at: 2026-05-21T00:00:00Z\n"
        "---\n"
        "# Body\n"
    )

    result = run_cli(
        "state", "set",
        "--path", str(state_file),
        "--field", "iteration",
        "--value", "2",
    )
    assert result.returncode == 0, (
        f"set should succeed; got exit {result.returncode}\n"
        f"stdout: {result.stdout}\nstderr: {result.stderr}"
    )

    data, _body = _read_fm(state_file)
    assert data["iteration"] == 2
    assert data["updated_at"] == "2026-05-21T00:00:00Z"


def test_ac2_hp_set_body_unchanged(tmp_path: Path) -> None:
    """AC2-HP: state set preserves the file body."""
    state_file = make_state_file(tmp_path)
    original_data, original_body = _read_fm(state_file)

    run_cli(
        "state", "set",
        "--path", str(state_file),
        "--field", "iteration",
        "--value", "99",
    )

    _, new_body = _read_fm(state_file)
    assert original_body == new_body


def _read_fm(path: Path):
    from fno.state.io import read_frontmatter
    return read_frontmatter(path)


# -- AC3-ERR: state set rejects invalid status value --

def test_ac3_err_set_invalid_status(tmp_path: Path) -> None:
    """AC3-ERR: state set with invalid status exits 1 with clear error."""
    state_file = make_state_file(tmp_path)
    result = run_cli(
        "state", "set",
        "--path", str(state_file),
        "--field", "status",
        "--value", "GARBAGE",
    )
    assert result.returncode == 1, f"expected exit 1, got {result.returncode}"
    # stderr must mention the error
    combined = result.stdout + result.stderr
    assert "status" in combined.lower() or "invalid" in combined.lower() or "GARBAGE" in combined


# -- AC4-HP: state validate exits 0 on valid file --

def test_ac4_hp_validate_valid_file(tmp_path: Path) -> None:
    """AC4-HP: state validate exits 0 and outputs valid JSON for a correct file."""
    state_file = make_state_file(tmp_path, VALID_TARGET_STATE)
    result = run_cli("--json", "state", "validate", "--path", str(state_file), "--type", "target")
    assert result.returncode == 0, f"stderr: {result.stderr}"
    data = json.loads(result.stdout)
    assert data.get("valid") is True


def test_ac4_hp_validate_text_mode(tmp_path: Path) -> None:
    """AC4-HP: state validate without --json prints 'valid'."""
    state_file = make_state_file(tmp_path, VALID_TARGET_STATE)
    result = run_cli("state", "validate", "--path", str(state_file), "--type", "target")
    assert result.returncode == 0
    assert "valid" in result.stdout.lower()


def test_ac4_err_validate_invalid_file(tmp_path: Path) -> None:
    """AC4-ERR: state validate exits 1 with --json {valid: false} on invalid file."""
    bad_state = tmp_path / "bad-state.md"
    bad_state.write_text("---\nstatus: NOT_A_VALID_STATUS\n---\n# body\n")
    result = run_cli("--json", "state", "validate", "--path", str(bad_state), "--type", "target")
    assert result.returncode == 1
    data = json.loads(result.stdout)
    assert data.get("valid") is False
    assert "errors" in data


# -- AC5-HP: state init creates a fresh state file --

def test_ac5_hp_init_creates_file(tmp_path: Path) -> None:
    """AC5-HP: state init creates a state file with defaults."""
    output = tmp_path / "new-state.md"
    result = run_cli(
        "state", "init",
        "--type", "target",
        "--output", str(output),
    )
    assert result.returncode == 0, f"stderr: {result.stderr}"
    assert output.exists()
    data, _ = _read_fm(output)
    assert data.get("status") == "IN_PROGRESS"


def test_ac5_err_init_file_exists(tmp_path: Path) -> None:
    """AC5-ERR: state init exits 1 if file already exists (without --force)."""
    output = tmp_path / "existing.md"
    output.write_text("---\nstatus: COMPLETE\n---\n")
    result = run_cli(
        "state", "init",
        "--type", "target",
        "--output", str(output),
    )
    assert result.returncode == 1


# -- archive --

def test_archive_creates_backup(tmp_path: Path) -> None:
    """HP: state archive moves the state file to a backup path."""
    state_file = make_state_file(tmp_path)
    result = run_cli("state", "archive", "--path", str(state_file))
    assert result.returncode == 0, f"stderr: {result.stderr}"
    # Original is gone or a backup exists
    archived = list(tmp_path.glob("*.archived*")) + list(tmp_path.glob("*.bak*"))
    assert not state_file.exists() or len(archived) >= 1


# -- list-fields --

def test_list_fields_returns_field_names(tmp_path: Path) -> None:
    """HP: state list-fields --type target returns field names."""
    result = run_cli("--json", "state", "list-fields", "--type", "target")
    assert result.returncode == 0, f"stderr: {result.stderr}"
    data = json.loads(result.stdout)
    assert isinstance(data, list)
    assert "status" in data
    assert "session_id" in data


# -- path --

def test_path_ledger_prints_resolved_path() -> None:
    """HP: state path ledger prints str(ledger_json()) on stdout."""
    from fno.paths import ledger_json

    result = run_cli("state", "path", "ledger")
    assert result.returncode == 0, f"stderr: {result.stderr}"
    assert result.stdout.strip() == str(ledger_json())


def test_path_unknown_name_exits_1() -> None:
    """ERR: an unknown state file name exits 1."""
    result = run_cli("state", "path", "nope")
    assert result.returncode == 1
