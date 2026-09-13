"""Tests for the checked-in law command-form registry gate."""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
GATE = REPO_ROOT / "scripts/ci/check-law-command-forms.sh"


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    (tmp_path / "scripts/ci/fixtures").mkdir(parents=True)
    (tmp_path / "skills/reign").mkdir(parents=True)
    shutil.copy(GATE, tmp_path / "scripts/ci/check-law-command-forms.sh")
    (tmp_path / "scripts/ci/law-command-forms.txt").write_text(
        "# form|law ids|skill paths\n"
        "--substrate thread|d-b1a7afe2|skills/reign/court.md\n"
        "glm-5.3-flash[1m]|d-20293d74 d-94853e86|skills/reign/court.md\n"
        "-|d-f2d9cfa7|exempt: no reusable form\n"
    )
    (tmp_path / "scripts/ci/fixtures/law-command-form-canary.md").write_text(
        "--substrate thread\n"
    )
    (tmp_path / "skills/reign/court.md").write_text(
        "fno agents spawn --substrate thread\n"
        "glm-5.3-flash[1m]\n"
    )
    return tmp_path


def _run(repo: Path, *args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    merged = os.environ.copy()
    if env:
        merged.update(env)
    return subprocess.run(
        ["bash", "scripts/ci/check-law-command-forms.sh", *args],
        cwd=repo,
        env=merged,
        capture_output=True,
        text=True,
    )


def test_all_registered_forms_pass(repo: Path) -> None:
    result = _run(repo)
    assert result.returncode == 0, result.stderr
    assert "checked 2 form(s)" in result.stdout


def test_missing_form_names_laws_and_path(repo: Path) -> None:
    (repo / "skills/reign/court.md").write_text("fno agents spawn --substrate thread\n")
    result = _run(repo)
    assert result.returncode == 1
    assert "glm-5.3-flash[1m]" in result.stderr
    assert "d-20293d74 d-94853e86" in result.stderr
    assert "skills/reign/court.md" in result.stderr


def test_malformed_registry_row_fails_closed(repo: Path) -> None:
    (repo / "scripts/ci/law-command-forms.txt").write_text("bad|row\n")
    result = _run(repo)
    assert result.returncode == 1
    assert "malformed" in result.stderr


def test_canary_control_fails_closed(repo: Path) -> None:
    (repo / "scripts/ci/fixtures/law-command-form-canary.md").write_text("")
    result = _run(repo)
    assert result.returncode == 1
    assert "CONTROL FAILED" in result.stderr


def test_live_read_flags_unregistered_form_and_unreadable_store(repo: Path) -> None:
    bin_dir = repo / "bin"
    bin_dir.mkdir()
    fno = bin_dir / "fno"
    fno.write_text(
        "#!/usr/bin/env bash\n"
        "printf '%s' '{\"decisions\":[{\"id\":\"d-aaaaaaaa\",\"subject\":\"x\",\"decision\":\"run --unregistered\"}]}'\n"
    )
    fno.chmod(0o755)
    result = _run(repo, "--live", env={"PATH": f"{bin_dir}:{os.environ['PATH']}"})
    assert result.returncode == 1
    assert "UNREGISTERED LAW FORM: d-aaaaaaaa x" in result.stderr

    fno.write_text("#!/usr/bin/env bash\nexit 7\n")
    fno.chmod(0o755)
    result = _run(repo, "--live", env={"PATH": f"{bin_dir}:{os.environ['PATH']}"})
    assert result.returncode == 2
    assert "could not read live law" in result.stderr
