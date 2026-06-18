"""Tests for fno.reality_check.gh - gh PR check with timeout + error semantics."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest


# -- AC1-HP: gh check confirms PR is open --

def test_ac1_hp_gh_check_pr_open(tmp_path: Path) -> None:
    """AC1-HP: gh check returns ok:true with evidence when PR state matches expected."""
    from fno.reality_check.gh import check_gh

    gh_output = json.dumps({"state": "OPEN", "mergeable": "MERGEABLE", "number": 42})

    mock_result = MagicMock()
    mock_result.returncode = 0
    mock_result.stdout = gh_output
    mock_result.stderr = ""

    with patch("subprocess.run", return_value=mock_result) as mock_run:
        result = check_gh(pr_number=42, expect="open", timeout=5)

    assert result["ok"] is True
    assert "evidence" in result
    assert result["evidence"]["state"] == "OPEN"

    # Verify subprocess was called with the right args
    mock_run.assert_called_once()
    call_args = mock_run.call_args
    cmd = call_args[0][0]
    assert "gh" in cmd
    assert "pr" in cmd
    assert "view" in cmd
    assert "42" in [str(a) for a in cmd]


def test_ac1_hp_gh_check_pr_state_case_insensitive(tmp_path: Path) -> None:
    """AC1-HP: state comparison is case-insensitive (OPEN matches open)."""
    from fno.reality_check.gh import check_gh

    gh_output = json.dumps({"state": "OPEN", "number": 42})
    mock_result = MagicMock()
    mock_result.returncode = 0
    mock_result.stdout = gh_output
    mock_result.stderr = ""

    with patch("subprocess.run", return_value=mock_result):
        result = check_gh(pr_number=42, expect="OPEN", timeout=5)

    assert result["ok"] is True


def test_ac1_hp_gh_check_pr_state_mismatch(tmp_path: Path) -> None:
    """AC1-HP: returns ok:false when actual state does not match expected."""
    from fno.reality_check.gh import check_gh

    gh_output = json.dumps({"state": "MERGED", "number": 42})
    mock_result = MagicMock()
    mock_result.returncode = 0
    mock_result.stdout = gh_output
    mock_result.stderr = ""

    with patch("subprocess.run", return_value=mock_result):
        result = check_gh(pr_number=42, expect="open", timeout=5)

    assert result["ok"] is False
    assert result["error"]["kind"] == "state_mismatch"
    assert result["error"]["actual"] == "MERGED"
    assert result["error"]["expected"] == "open"


# -- AC2-ERR: gh check detects missing PR --

def test_ac2_err_pr_not_found(tmp_path: Path) -> None:
    """AC2-ERR: non-zero exit from gh returns ok:false with pr_not_found."""
    from fno.reality_check.gh import check_gh

    mock_result = MagicMock()
    mock_result.returncode = 1
    mock_result.stdout = ""
    mock_result.stderr = "no pull requests found for #99"

    with patch("subprocess.run", return_value=mock_result):
        result = check_gh(pr_number=99, expect="open", timeout=5)

    assert result["ok"] is False
    assert result["error"]["kind"] == "pr_not_found"


# -- AC3-EDGE: gh check times out gracefully --

def test_ac3_edge_timeout_returns_error(tmp_path: Path) -> None:
    """AC3-EDGE: subprocess timeout raises TimeoutExpired -> ok:false, kind:timeout."""
    from fno.reality_check.gh import check_gh

    with patch("subprocess.run", side_effect=subprocess.TimeoutExpired(cmd=["gh"], timeout=5)):
        result = check_gh(pr_number=42, expect="open", timeout=5)

    assert result["ok"] is False
    assert result["error"]["kind"] == "timeout"


def test_ac3_edge_timeout_never_downgrade_to_ok(tmp_path: Path) -> None:
    """AC3-EDGE: timeout MUST return ok:false, never ok:true."""
    from fno.reality_check.gh import check_gh

    with patch("subprocess.run", side_effect=subprocess.TimeoutExpired(cmd=["gh"], timeout=5)):
        result = check_gh(pr_number=42, expect="open", timeout=5)

    # The key guarantee: ok is NEVER True on timeout
    assert result.get("ok") is not True


# -- Edge: invalid JSON from gh --

def test_edge_invalid_json_from_gh(tmp_path: Path) -> None:
    """EDGE: invalid JSON output from gh returns ok:false with parse_error."""
    from fno.reality_check.gh import check_gh

    mock_result = MagicMock()
    mock_result.returncode = 0
    mock_result.stdout = "this is not json"
    mock_result.stderr = ""

    with patch("subprocess.run", return_value=mock_result):
        result = check_gh(pr_number=42, expect="open", timeout=5)

    assert result["ok"] is False
    assert result["error"]["kind"] == "parse_error"


# -- Edge: gh not installed / FileNotFoundError --

def test_edge_gh_not_installed(tmp_path: Path) -> None:
    """EDGE: FileNotFoundError (gh not installed) returns ok:false with gh_not_found."""
    from fno.reality_check.gh import check_gh

    with patch("subprocess.run", side_effect=FileNotFoundError("gh not found")):
        result = check_gh(pr_number=42, expect="open", timeout=5)

    assert result["ok"] is False
    assert result["error"]["kind"] == "gh_not_found"
