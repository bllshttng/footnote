"""Unit tests for `fno do plan stamp`, `graduate`, and `set-expected`.

The wrappers are clients of the keeper-served plan-doc writer (via
``fno.plan._project``). Tests verify:
1. Help text renders without error.
2. Args + flags reach the writer unchanged.
3. Exit codes propagate from the writer.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def test_plan_help_renders():
    result = runner.invoke(app, ["do", "plan", "--help"])
    assert result.exit_code == 0
    assert "stamp" in result.stdout
    assert "graduate" in result.stdout


def test_plan_validate_execution_refuses_post_gate_no_difficulty(tmp_path):
    """x-baef round-5: validate-plan.sh runs --execution, which never reaches
    PlanFrontmatter, so the difficulty gate must fire in THAT scope - a
    post-gate plan with no difficulty is refused at authoring time, quoting
    the bands; the on-gate twin passes the gate."""
    post_gate = tmp_path / "post-gate.md"
    post_gate.write_text(
        "---\nnode: x-baef\nstatus: ready\ncreated: 2026-08-27\n---\n# T\n\nBody.\n"
    )
    result = runner.invoke(app, ["do", "plan", "validate", str(post_gate), "--execution"])
    assert result.exit_code == 1, result.output
    assert "difficulty is required" in result.output
    assert "low, medium, high" in result.output

    # Round-12: undatable created is LENIENT at the authoring scope -
    # validate-plan.sh dates those plans itself (filename date, tolerant
    # reads), so the gate defers instead of refusing; the refusal the
    # minting lanes raise never appears here.
    no_created = tmp_path / "no-created.md"
    no_created.write_text("---\nnode: x-baef\nstatus: ready\n---\n# T\n\nBody.\n")
    import json as _json

    r3 = runner.invoke(
        app, ["do", "plan", "validate", str(no_created), "--execution", "--json"]
    )
    payload = _json.loads(r3.output)
    assert not any(
        "cannot read created" in v["message"] for v in payload["violations"]
    ), "authoring scope defers undatable created to validate-plan.sh's own dating"

    on_gate = tmp_path / "on-gate.md"
    on_gate.write_text(
        "---\nnode: x-baef\nstatus: ready\ncreated: 2026-08-26\n---\n"
        "# T\n\n## Execution Strategy\n\n```yaml\n"
        "execution_mode: sequential\n"
        "waves:\n  - wave: 1\n    mode: sequential\n    name: w\n    tasks: ['1.1']\n"
        "tasks:\n  - id: '1.1'\n    title: t\n    surface: ['cli/x.py']\n"
        "    verify: pytest cli/x.py -q\n"
        "    acceptance: [AC1-ERR]\n"
        "```\n"
    )
    result2 = runner.invoke(app, ["do", "plan", "validate", str(on_gate), "--execution"])
    assert result2.exit_code == 0, result2.output
    assert "difficulty is required" not in result2.output


def test_plan_stamp_help_renders():
    result = runner.invoke(app, ["do", "plan", "stamp", "--help"])
    assert result.exit_code == 0


def test_plan_graduate_help_renders():
    result = runner.invoke(app, ["do", "plan", "graduate", "--help"])
    assert result.exit_code == 0


def test_plan_stamp_forwards_args_and_propagates_error(tmp_path):
    """When the module returns non-zero, the wrapper propagates.

    The module is always importable in-package (run via ``-m``), so no
    repo-root resolution is needed; a non-existent plan path makes it exit 1.
    """
    result = runner.invoke(
        app,
        ["do", "plan", "stamp", "--plan-path", str(tmp_path / "no-such-plan.md"),
         "--session-id", "test-sid", "--url", "https://example.com/pr/1"],
    )
    # Module's exit code (non-zero) propagates.
    assert result.exit_code != 0


def test_plan_graduate_forwards_args(tmp_path):
    """Same as stamp but for graduate."""
    result = runner.invoke(
        app,
        ["do", "plan", "graduate", "--plan-path", str(tmp_path / "no-such-plan.md")],
    )
    # Either the module exits non-zero (no plan) or zero with a no-op message.
    # Either way: no Python exception should bubble up.
    assert result.exit_code in (0, 1, 2)


def _stub_plan_docs(monkeypatch, exit_code=0):
    """Capture what the verb hands the keeper client, without a keeper."""
    import fno.plan._project as project_client

    captured: list = []

    def _stub(op, **params):
        captured.append((op, params))
        return {"exit": exit_code}

    monkeypatch.setattr(project_client, "plan_docs", _stub)
    return captured


@pytest.mark.parametrize(
    "argv",
    [
        ["stamp", "--plan-path", "/tmp/some-plan.md", "--session-id", "abc-123",
         "--url", "https://example.com/pr/42", "--expected-url-count", "1"],
        ["graduate", "--plan-path", "/tmp/some-plan.md"],
        ["set-expected", "--plan-path", "/tmp/some-plan.md", "--count", "3"],
    ],
)
def test_plan_verbs_forward_args_verbatim(monkeypatch, argv):
    """AC1-HP: every flag reaches the writer unchanged; the keeper parses them."""
    captured = _stub_plan_docs(monkeypatch)
    result = runner.invoke(app, ["do", "plan", *argv])
    assert result.exit_code == 0
    assert captured == [("argv", {"verb": argv[0], "args": argv[1:]})]


def test_plan_verb_propagates_the_writer_exit_code(monkeypatch):
    _stub_plan_docs(monkeypatch, exit_code=2)
    result = runner.invoke(app, ["do", "plan", "graduate", "--plan-path", "/tmp/p.md"])
    assert result.exit_code == 2


# ---------------------------------------------------------------------------
# fno do plan path (config.plans_filename renderer): the renderer and the
# resolver chain now live in Rust (plans_path.rs). Python-side coverage is
# the plans-path parity goldens (crates/fno-agents/tests/golden/plans-path/).
# ---------------------------------------------------------------------------
