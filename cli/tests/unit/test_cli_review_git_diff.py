"""Regression tests for the `fno review` git-diff failure path.

Tracks AC5-HP / AC5-ERR / AC5-EDGE from the events + test hygiene cleanup
spec (ab-a1118224). The previous review command body silently substituted
an empty diff when ``git diff HEAD~1`` failed (no parent commit, detached
HEAD, permission error). The panel then reviewed an empty diff and
reported zero findings - a "clean" review indistinguishable from a real
one.

The fix: ``git diff HEAD~1`` failures now exit 2 with the git stderr
surfaced to the caller, so silent-clean reviews on broken inputs are
impossible.
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _completed(rc: int, stdout: str = "", stderr: str = "") -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(
        args=["git", "diff", "HEAD~1"],
        returncode=rc,
        stdout=stdout,
        stderr=stderr,
    )


# ---------------------------------------------------------------------------
# AC5-ERR: git diff returns non-zero -> exit 2 with stderr surfaced
# ---------------------------------------------------------------------------


def test_ac5_err_git_diff_failure_exits_2(
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """AC5-ERR: on first-commit branch (HEAD~1 absent), fno review exits 2."""
    import subprocess as _subprocess
    from fno import cli as cli_module

    # Capture but block the subprocess call so the test is hermetic.
    captured: list[list[str]] = []

    def fake_run(cmd: list[str], **kwargs: Any) -> subprocess.CompletedProcess:
        captured.append(list(cmd))
        return _completed(
            rc=128,
            stdout="",
            stderr="fatal: ambiguous argument 'HEAD~1': unknown revision\n",
        )

    monkeypatch.setattr(_subprocess, "run", fake_run)
    # cli.py imports subprocess at the module level, so patch that binding too.
    monkeypatch.setattr(cli_module, "subprocess", _subprocess, raising=False)

    # Patch the panel so a stray success path can't hide the bug.
    def must_not_run(**kwargs: Any) -> dict[str, Any]:
        raise AssertionError(
            "review panel should not have been invoked when git diff failed"
        )

    monkeypatch.setattr(
        "fno.worker.review.review",
        must_not_run,
    )

    result = runner.invoke(cli_module.app, ["review"])

    assert result.exit_code == 2, (
        f"expected exit 2 on git diff failure, got {result.exit_code}: "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    combined = (result.stderr or "") + (result.output or "")
    assert "git diff HEAD~1 failed" in combined, (
        f"expected 'git diff HEAD~1 failed' in output; got "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    assert "ambiguous argument" in combined, (
        f"expected git stderr substring in output; got "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    assert captured, "subprocess.run should have been called for `git diff HEAD~1`"
    assert captured[0][:3] == ["git", "diff", "HEAD~1"]


# ---------------------------------------------------------------------------
# AC5-HP: git diff returns 0 -> review proceeds, panel is dispatched
# ---------------------------------------------------------------------------


def test_ac5_hp_git_diff_success_proceeds(
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """AC5-HP: when git diff succeeds (rc=0), review proceeds against the diff."""
    import subprocess as _subprocess
    from fno import cli as cli_module

    diff_text = "diff --git a/foo.py b/foo.py\n+x\n"

    def fake_run(cmd: list[str], **kwargs: Any) -> subprocess.CompletedProcess:
        return _completed(rc=0, stdout=diff_text, stderr="")

    monkeypatch.setattr(_subprocess, "run", fake_run)
    monkeypatch.setattr(cli_module, "subprocess", _subprocess, raising=False)

    captured_diff: dict[str, str] = {}

    def stub_review(
        *,
        diff_context: str,
        state_path: Path,
        artifacts_dir: Path | None,
        session_id: str | None,
        no_cache: bool,
    ) -> dict[str, Any]:
        captured_diff["value"] = diff_context
        return {
            "action": "reviewed",
            "verdict": "ready-to-merge",
            "findings": 0,
        }

    monkeypatch.setattr(
        "fno.worker.review.review",
        stub_review,
    )

    result = runner.invoke(cli_module.app, ["review"])
    assert result.exit_code == 0, (
        f"expected exit 0 on git diff success, got {result.exit_code}: "
        f"output={result.output!r}"
    )
    assert captured_diff.get("value") == diff_text


# ---------------------------------------------------------------------------
# AC5-UI: --diff path override is unchanged by the rc-check
# ---------------------------------------------------------------------------


def test_ac5_ui_diff_override_unchanged(
    runner: CliRunner,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """AC5-UI: --diff path bypasses the rc-checked git path entirely."""
    import subprocess as _subprocess
    from fno import cli as cli_module

    diff_file = tmp_path / "patch.diff"
    diff_file.write_text("diff --git a/x b/x\n+y\n", encoding="utf-8")

    # subprocess.run should NOT be called at all in this path.
    def must_not_run(*args: Any, **kwargs: Any) -> subprocess.CompletedProcess:
        raise AssertionError(
            "subprocess.run should not run when --diff is provided"
        )

    monkeypatch.setattr(_subprocess, "run", must_not_run)
    monkeypatch.setattr(cli_module, "subprocess", _subprocess, raising=False)

    captured_diff: dict[str, str] = {}

    def stub_review(
        *,
        diff_context: str,
        state_path: Path,
        artifacts_dir: Path | None,
        session_id: str | None,
        no_cache: bool,
    ) -> dict[str, Any]:
        captured_diff["value"] = diff_context
        return {
            "action": "reviewed",
            "verdict": "ready-to-merge",
            "findings": 0,
        }

    monkeypatch.setattr(
        "fno.worker.review.review",
        stub_review,
    )

    result = runner.invoke(cli_module.app, ["review", "--diff", str(diff_file)])
    assert result.exit_code == 0
    assert captured_diff["value"] == diff_file.read_text(encoding="utf-8")
