"""Unit tests for `fno plan stamp` and `fno plan graduate`.

The wrappers are forwarders over the in-package ``fno.plan._stamp`` module
(run via ``python3 -m fno.plan._stamp``). Tests verify:
1. Help text renders without error.
2. Args + flags forward verbatim to the module.
3. Exit codes propagate from the module.
"""
from __future__ import annotations

import sys

from typer.testing import CliRunner

from fno.cli import app
from fno import plan as plan_module

runner = CliRunner()


def test_plan_help_renders():
    result = runner.invoke(app, ["plan", "--help"])
    assert result.exit_code == 0
    assert "stamp" in result.stdout
    assert "graduate" in result.stdout


def test_plan_stamp_help_renders():
    result = runner.invoke(app, ["plan", "stamp", "--help"])
    assert result.exit_code == 0


def test_plan_graduate_help_renders():
    result = runner.invoke(app, ["plan", "graduate", "--help"])
    assert result.exit_code == 0


def test_plan_stamp_forwards_args_and_propagates_error(tmp_path):
    """When the module returns non-zero, the wrapper propagates.

    The module is always importable in-package (run via ``-m``), so no
    repo-root resolution is needed; a non-existent plan path makes it exit 1.
    """
    result = runner.invoke(
        app,
        ["plan", "stamp", "--plan-path", str(tmp_path / "no-such-plan.md"),
         "--session-id", "test-sid", "--url", "https://example.com/pr/1"],
    )
    # Module's exit code (non-zero) propagates.
    assert result.exit_code != 0


def test_plan_graduate_forwards_args(tmp_path):
    """Same as stamp but for graduate."""
    result = runner.invoke(
        app,
        ["plan", "graduate", "--plan-path", str(tmp_path / "no-such-plan.md")],
    )
    # Either the module exits non-zero (no plan) or zero with a no-op message.
    # Either way: no Python exception should bubble up.
    assert result.exit_code in (0, 1, 2)


def test_plan_stamp_forwards_args_verbatim(tmp_path, monkeypatch):
    """AC1-HP: every flag the user passes reaches the module,
    in the right order, with verb prefixed.

    Stubs subprocess.run inside the wrapper module so we can capture
    the exact cmd list without invoking the real module.
    """
    captured = {}

    class _StubResult:
        returncode = 0

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubResult()

    monkeypatch.setattr(plan_module.cli.subprocess, "run", _stub_run)

    result = runner.invoke(
        app,
        [
            "plan", "stamp",
            "--plan-path", "/tmp/some-plan.md",
            "--session-id", "abc-123",
            "--url", "https://example.com/pr/42",
            "--expected-url-count", "1",
        ],
    )
    assert result.exit_code == 0
    cmd = captured["cmd"]
    # Layout: [sys.executable, "-m", "fno.plan._stamp", "stamp", ...flags...]
    assert cmd[0] == sys.executable
    assert cmd[1:3] == ["-m", "fno.plan._stamp"]
    assert cmd[3] == "stamp"
    # All user-supplied args land at positions 4+, in order.
    assert cmd[4:] == [
        "--plan-path", "/tmp/some-plan.md",
        "--session-id", "abc-123",
        "--url", "https://example.com/pr/42",
        "--expected-url-count", "1",
    ]


def test_plan_graduate_forwards_args_verbatim(tmp_path, monkeypatch):
    """Same as stamp-forward but for graduate verb."""
    captured = {}

    class _StubResult:
        returncode = 0

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubResult()

    monkeypatch.setattr(plan_module.cli.subprocess, "run", _stub_run)

    result = runner.invoke(
        app, ["plan", "graduate", "--plan-path", "/tmp/some-plan.md"],
    )
    assert result.exit_code == 0
    cmd = captured["cmd"]
    assert cmd[1:3] == ["-m", "fno.plan._stamp"]
    assert cmd[3] == "graduate"
    assert cmd[4:] == ["--plan-path", "/tmp/some-plan.md"]


def test_plan_set_expected_forwards_args_verbatim(tmp_path, monkeypatch):
    """`fno plan set-expected` forwards to fno.plan._stamp set-expected verbatim."""
    captured = {}

    class _StubResult:
        returncode = 0

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubResult()

    monkeypatch.setattr(plan_module.cli.subprocess, "run", _stub_run)

    result = runner.invoke(
        app,
        ["plan", "set-expected", "--plan-path", "/tmp/some-plan.md", "--count", "3"],
    )
    assert result.exit_code == 0
    cmd = captured["cmd"]
    assert cmd[1:3] == ["-m", "fno.plan._stamp"]
    assert cmd[3] == "set-expected"
    assert cmd[4:] == ["--plan-path", "/tmp/some-plan.md", "--count", "3"]


# ---------------------------------------------------------------------------
# fno plan path (config.plans_filename renderer)
# ---------------------------------------------------------------------------


def test_plan_doc_filename_default_template(monkeypatch):
    import datetime

    from fno import paths

    name = paths.plan_doc_filename(
        "dark-mode", "x-8af8", now=datetime.datetime(2026, 7, 11)
    )
    assert name == "20260711-dark-mode-x-8af8.md"


def test_plan_doc_filename_collapses_empty_parts():
    import datetime

    from fno import paths

    stamp = datetime.datetime(2026, 7, 11)
    assert paths.plan_doc_filename("dark-mode", "", now=stamp) == "20260711-dark-mode.md"
    assert paths.plan_doc_filename("", "x-8af8", now=stamp) == "20260711-x-8af8.md"


def test_plan_doc_filename_honors_custom_template(monkeypatch):
    import datetime

    from fno import paths

    class _S:
        plans_filename = "%Y-%m-%d-{slug}-{node}.md"

    monkeypatch.setattr(paths, "_settings", lambda: _S())
    name = paths.plan_doc_filename("dark-mode", "x-8af8", now=datetime.datetime(2026, 7, 11))
    assert name == "2026-07-11-dark-mode-x-8af8.md"


def test_plan_doc_path_threads_now_for_stable_date(tmp_path, monkeypatch):
    import datetime

    from fno import paths

    # Anchor plans_content_dir to tmp_path so the assertion is on the filename.
    monkeypatch.setattr(paths, "plans_content_dir", lambda project_root=None: tmp_path)
    stamp = datetime.datetime(2026, 1, 2)
    p = paths.plan_doc_path("dark-mode", "x-8af8", project_root=tmp_path, now=stamp)
    # Date comes from `now`, not today - the whole re-decompose idempotency mechanism.
    assert p.name == "20260102-dark-mode-x-8af8.md"


def test_plans_filename_config_rejects_bad_template():
    import pytest
    from pydantic import ValidationError

    from fno.config import ConfigBlock

    with pytest.raises(ValidationError):
        ConfigBlock(plans_filename="{slug}/{node}.md")  # renders a path, not a name
    with pytest.raises(ValidationError):
        ConfigBlock(plans_filename="{slug}-{nodeid}.md")  # unknown placeholder
    assert ConfigBlock(plans_filename="%Y%m%d-{slug}-{node}.md")


def test_plan_path_verb_prints_rendered_path():
    result = runner.invoke(app, ["plan", "path", "--slug", "dark-mode", "--node", "x-8af8", "--name-only"])
    assert result.exit_code == 0
    out = result.stdout.strip()
    assert out.endswith("-dark-mode-x-8af8.md")
    assert "/" not in out
