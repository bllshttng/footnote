"""Unit tests for `fno do plan stamp`, `graduate`, and `set-expected`.

The wrappers are clients of the keeper-served plan-doc writer (via
``fno.plan._project``). Tests verify:
1. Help text renders without error.
2. Args + flags reach the client functions, parsed.
3. Exit codes propagate from the writer.
"""
from __future__ import annotations

import sys

from typer.testing import CliRunner

from fno.cli import app
from fno.plan import cli as plan_cli_module

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


def test_plan_stamp_forwards_args_verbatim(tmp_path, monkeypatch):
    """AC1-HP: every flag the user passes reaches the client function, parsed.

    Stubs the client functions in fno.plan._project so we can capture the
    arguments without contacting a keeper.
    """
    import fno.plan._project as project_client

    captured: dict = {}

    def _stub_stamp(plan_path, session_id, urls, expected_url_count, dry_run):
        captured["stamp"] = (plan_path, session_id, urls, expected_url_count, dry_run)
        return 0

    monkeypatch.setattr(project_client, "stamp_plan", _stub_stamp)

    result = runner.invoke(
        app,
        [
            "do", "plan", "stamp",
            "--plan-path", "/tmp/some-plan.md",
            "--session-id", "abc-123",
            "--url", "https://example.com/pr/42",
            "--expected-url-count", "1",
        ],
    )
    assert result.exit_code == 0
    assert captured["stamp"] == (
        "/tmp/some-plan.md",
        "abc-123",
        ["https://example.com/pr/42"],
        1,
        False,
    )


def test_plan_graduate_forwards_args_verbatim(tmp_path, monkeypatch):
    """Same as stamp-forward but for graduate verb."""
    import fno.plan._project as project_client

    captured: dict = {}

    def _stub_grad(plan_path, dry_run):
        captured["grad"] = (plan_path, dry_run)
        return 0

    monkeypatch.setattr(project_client, "graduate_plan", _stub_grad)

    result = runner.invoke(
        app, ["do", "plan", "graduate", "--plan-path", "/tmp/some-plan.md"],
    )
    assert result.exit_code == 0
    assert captured["grad"] == ("/tmp/some-plan.md", False)


def test_plan_set_expected_forwards_args_verbatim(tmp_path, monkeypatch):
    """`fno do plan set-expected` forwards to the client with a parsed count."""
    import fno.plan._project as project_client

    captured: dict = {}

    def _stub_set(plan_path, count, dry_run):
        captured["set"] = (plan_path, count, dry_run)
        return 0, ""

    monkeypatch.setattr(project_client, "set_expected_count", _stub_set)

    result = runner.invoke(
        app,
        ["do", "plan", "set-expected", "--plan-path", "/tmp/some-plan.md", "--count", "3"],
    )
    assert result.exit_code == 0
    assert captured["set"] == ("/tmp/some-plan.md", 3, False)


# ---------------------------------------------------------------------------
# fno do plan path (config.plans_filename renderer)
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


def test_plan_doc_filename_refuses_rendered_node_mismatch(monkeypatch):
    import datetime
    import pytest

    from fno import paths

    class _S:
        plans_filename = "%Y-%m-%d-fixed-x-bbbb.md"

    monkeypatch.setattr(paths, "_settings", lambda: _S())
    with pytest.raises(ValueError, match="x-bbbb.*x-aaaa|x-aaaa.*x-bbbb"):
        paths.plan_doc_filename("feature", "x-aaaa", now=datetime.datetime(2026, 7, 11))


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
    result = runner.invoke(app, ["do", "plan", "path", "--slug", "dark-mode", "--node", "x-8af8", "--name-only"])
    assert result.exit_code == 0
    out = result.stdout.strip()
    assert out.endswith("-dark-mode-x-8af8.md")
    assert "/" not in out


# ---------------------------------------------------------------------------
# `fno do plan rung` - the shell-facing readiness authority (x-3571)
# ---------------------------------------------------------------------------


def _rung(tmp_path, body: str, name: str = "p.md"):
    p = tmp_path / name
    p.write_text(body)
    return CliRunner().invoke(app, ["do", "plan", "rung", str(p)])


def test_rung_is_hidden_from_the_plan_menu():
    """`fno do plan` sits at its menu-caps ceiling; new verbs default hidden."""
    out = CliRunner().invoke(app, ["do", "plan", "--help"]).output
    assert "rung" not in out


def test_AC1_HP_cli_verdict_matches_the_python_verdict(tmp_path):
    """The CLI is a face on plan_rung, not a second classifier."""
    from fno.graph.ladder import is_dispatchable, plan_rung

    for status in ("idea", "stub", "design", "ready", "in_review", "done"):
        p = tmp_path / f"{status}.md"
        p.write_text(f"---\nstatus: {status}\n---\n")
        entry = {"id": "-", "plan_path": str(p)}
        res = CliRunner().invoke(app, ["do", "plan", "rung", str(p)])

        assert f"rung={plan_rung(entry).value}" in res.output
        assert "selectable=" not in res.output
        assert f"dispatchable={'true' if is_dispatchable(entry) else 'false'}" in res.output
        assert res.exit_code == (0 if is_dispatchable(entry) else 1)


def test_AC1_HP_absent_and_unreadable_plans_agree_too(tmp_path):
    from fno.graph.ladder import plan_rung

    missing = tmp_path / "gone.md"
    assert plan_rung({"id": "-", "plan_path": str(missing)}).value == "unreadable"
    res = CliRunner().invoke(app, ["do", "plan", "rung", str(missing)])
    assert "rung=unreadable" in res.output
    assert res.exit_code == 1

    binary = tmp_path / "b.md"
    binary.write_bytes(b"\xff\xfe\x00\x80")
    res = CliRunner().invoke(app, ["do", "plan", "rung", str(binary)])
    assert "rung=unreadable" in res.output
    assert res.exit_code == 1


def test_exit_code_carries_the_dispatchable_verdict(tmp_path):
    """So each shell caller stays a one-liner with no parsing at all."""
    assert _rung(tmp_path, "---\nstatus: ready\n---\n").exit_code == 0
    assert _rung(tmp_path, "---\nstatus: idea\n---\n", "b.md").exit_code == 1


def test_a_relative_path_resolves_against_the_shell_cwd(tmp_path, monkeypatch):
    """plan_rung anchors on the ENTRY's cwd; a typed path means the shell's.

    Without the explicit cwd the resolver refuses to guess and reports
    UNREADABLE, which would park every relative-path caller.
    """
    (tmp_path / "p.md").write_text("---\nstatus: ready\n---\n")
    monkeypatch.chdir(tmp_path)
    res = CliRunner().invoke(app, ["do", "plan", "rung", "p.md"])
    assert "rung=ready" in res.output
    assert res.exit_code == 0
