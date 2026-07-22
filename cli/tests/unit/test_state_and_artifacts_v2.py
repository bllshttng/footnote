"""Phase 07: `fno state show --v2` honors v2 layout.

Covers:
- `fno state show --v2` reads `.fno/v2/target-state.md` when present
- `fno state show --v2` falls back to v1 with a stderr note when v2 absent
- `fno state show` without --v2 is unchanged

Note: gate artifact tests (artifact_path, check_gate_safe) removed in
Task 3.2 (control-plane collapse ab-d0337fbc) along with fno.gates.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import yaml


SESSION_ID = "state-v2-session-001"


def _write_state(base: Path, session_id: str = SESSION_ID, status: str = "IN_PROGRESS") -> Path:
    base.mkdir(parents=True, exist_ok=True)
    data = {
        "status": status,
        "session_id": session_id,
        "current_phase": "blueprint",
    }
    path = base / "target-state.md"
    path.write_text("---\n" + yaml.dump(data, default_flow_style=False) + "---\n")
    return path


def _run_fno(args: list[str], cwd: Path) -> subprocess.CompletedProcess:
    cli_dir = Path(__file__).parent.parent.parent
    return subprocess.run(
        ["uv", "run", "fno-py", *args],
        cwd=cli_dir,
        capture_output=True,
        text=True,
        env={**os.environ, "FNO_REPO_ROOT": str(cwd)},
    )


# ---- AC1-V2: fno state show --v2 reads v2 path ------------------------

def test_state_show_v2_reads_v2_when_present(tmp_path):
    _write_state(tmp_path / ".fno" / "v2", session_id="v2-session")

    completed = _run_fno(
        ["state", "show", "--v2", "--path", str(tmp_path / ".fno" / "v2" / "target-state.md")],
        cwd=tmp_path,
    )

    assert completed.returncode == 0, completed.stderr
    assert "v2-session" in completed.stdout


def test_state_show_v2_default_path_picks_v2(tmp_path):
    """Without --path, --v2 auto-resolves to .fno/v2/target-state.md."""
    _write_state(tmp_path / ".fno" / "v2", session_id="default-v2")

    completed = _run_fno(["state", "show", "--v2"], cwd=tmp_path)

    assert completed.returncode == 0, f"stderr: {completed.stderr}"
    assert "default-v2" in completed.stdout


# ---- AC1-FALLBACK: --v2 with no v2 file falls back to v1 -------------

def test_state_show_v2_falls_back_to_v1_with_stderr_note(tmp_path):
    _write_state(tmp_path / ".fno", session_id="v1-fallback")
    # v2 dir does NOT exist
    assert not (tmp_path / ".fno" / "v2").exists()

    completed = _run_fno(["state", "show", "--v2"], cwd=tmp_path)

    assert completed.returncode == 0
    assert "v1-fallback" in completed.stdout
    assert "v2 not found" in completed.stderr.lower()


# ---- AC1-DEFAULT: no --v2 keeps v1-only behavior ---------------------

def test_state_show_without_v2_uses_v1(tmp_path):
    _write_state(tmp_path / ".fno", session_id="pure-v1")
    # v2 state also exists; without --v2 we must NOT read it
    _write_state(tmp_path / ".fno" / "v2", session_id="should-not-be-read")

    completed = _run_fno(["state", "show"], cwd=tmp_path)

    assert completed.returncode == 0
    assert "pure-v1" in completed.stdout
    assert "should-not-be-read" not in completed.stdout
    # No fallback note when --v2 isn't set
    assert "v2 not found" not in completed.stderr.lower()
