"""Unit tests for the no-unwrapped-lib-scripts lint (hard-fail mode).

After PR 2 of the CLI promotion, the lint hard-fails (exit 1) on any
scripts/lib/<name>.{sh,py} that has no fno wrapper and is not in the
allowlist. The tests run the canonical lint with a synthetic state by
swapping the repo's scripts/lib/ and cli/src/fno/ directories
under a tmp_path and pointing the lint at it via the standard
FNO_REPO_ROOT mechanism.

The lint resolves its repo root via `git rev-parse --show-toplevel`, so
the tests fan out an actual `git init` in tmp_path and copy the lint
script into the fake repo before invoking it.
"""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


# Resolve the canonical lint script in the real repo so we can copy it
# into each test workspace without depending on installed binaries.
_REAL_REPO_ROOT = Path(__file__).resolve().parents[3]
_LINT_SCRIPT = _REAL_REPO_ROOT / "scripts" / "lint" / "no-unwrapped-lib-scripts.sh"


def _make_fake_repo(tmp_path: Path) -> Path:
    """Initialize a minimal git repo with the lint plumbing in place."""
    root = tmp_path / "repo"
    root.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    (root / "scripts" / "lib").mkdir(parents=True)
    (root / "scripts" / "lint").mkdir(parents=True)
    (root / "cli" / "src" / "fno").mkdir(parents=True)
    shutil.copy2(_LINT_SCRIPT, root / "scripts" / "lint" / "no-unwrapped-lib-scripts.sh")
    # Empty allowlist by default; tests can override.
    (root / "scripts" / "lint" / ".unwrapped-lib-allowlist.txt").write_text(
        "# test allowlist\n", encoding="utf-8"
    )
    return root


def _run_lint(repo_root: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", "scripts/lint/no-unwrapped-lib-scripts.sh"],
        cwd=repo_root,
        capture_output=True,
        text=True,
    )


def test_no_unwrapped_lib_scripts_passes_when_wrapper_exists(tmp_path: Path) -> None:
    """AC2-ERR: lint passes when every lib script has a wrapper."""
    repo = _make_fake_repo(tmp_path)
    (repo / "scripts" / "lib" / "foo-helper.sh").write_text("#!/usr/bin/env bash\n")
    (repo / "cli" / "src" / "fno" / "wrapper.py").write_text(
        "# wraps foo-helper.sh\n"
    )
    result = _run_lint(repo)
    assert result.returncode == 0, f"expected rc=0, got rc={result.returncode}\nstdout={result.stdout}\nstderr={result.stderr}"
    assert "0 missing wrappers" in result.stdout


def test_no_unwrapped_lib_scripts_flags_new_script(tmp_path: Path) -> None:
    """AC1-HP: lint hard-fails on a planted unwrapped script."""
    repo = _make_fake_repo(tmp_path)
    (repo / "scripts" / "lib" / "test-fake-helper.sh").write_text("#!/usr/bin/env bash\n")
    # No wrapper in cli/src/fno/ - this should hard-fail.
    result = _run_lint(repo)
    assert result.returncode == 1, f"expected rc=1, got rc={result.returncode}\nstdout={result.stdout}\nstderr={result.stderr}"
    assert "test-fake-helper.sh" in result.stderr
    assert "cli/src/fno/" in result.stderr
    assert "allowlist" in result.stderr.lower()


def test_no_unwrapped_lib_scripts_respects_allowlist(tmp_path: Path) -> None:
    """AC3-EDGE: allowlisted entries are not flagged."""
    repo = _make_fake_repo(tmp_path)
    (repo / "scripts" / "lib" / "common.sh").write_text("#!/usr/bin/env bash\n")
    (repo / "scripts" / "lint" / ".unwrapped-lib-allowlist.txt").write_text(
        "# pure library\ncommon.sh\n", encoding="utf-8"
    )
    result = _run_lint(repo)
    assert result.returncode == 0, f"expected rc=0, got rc={result.returncode}\nstdout={result.stdout}\nstderr={result.stderr}"
    assert "0 missing wrappers" in result.stdout


def test_no_unwrapped_lib_scripts_real_repo_state_is_clean() -> None:
    """AC2-ERR: the real repo's current state passes the (now hard) lint."""
    result = subprocess.run(
        ["bash", "scripts/lint/no-unwrapped-lib-scripts.sh"],
        cwd=_REAL_REPO_ROOT,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, (
        f"real-repo lint should be clean (rc=0), got rc={result.returncode}.\n"
        f"stdout={result.stdout}\nstderr={result.stderr}"
    )
