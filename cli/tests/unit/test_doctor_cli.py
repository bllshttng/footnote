from __future__ import annotations

import click
import typer
import typer.main
from typer.testing import CliRunner


runner = CliRunner()


def test_doctor_lists_direct_actions() -> None:
    from fno.doctor_cli import doctor_app

    command = typer.main.get_command(doctor_app)
    assert isinstance(command, click.Group)
    context = click.Context(command, info_name="doctor")
    assert set(command.list_commands(context)) == {
        "bash-census",
        "bundle",
        "codemap",
        "evals",
        "event",
        "footprint",
        "harness",
        "harness-matrix",
        "intel",
        "lanes",
        "lint",
        "observer",
        "plugin-file",
        "reclaim",
        "route",
        "scratch",
        "skill-diff",
        "test",
        "update",
    }


def test_nested_doctor_actions_resolve() -> None:
    from fno.cli import app

    for argv in (
        ["doctor", "bundle", "check", "--help"],
        ["doctor", "codemap", "--help"],
        ["doctor", "evals", "grade", "--help"],
        ["doctor", "event", "fanout", "tick", "--help"],
        ["doctor", "lint", "menu-caps", "--help"],
        ["doctor", "observer", "sweep", "--help"],
        ["doctor", "plugin-file", "--help"],
        ["doctor", "skill-diff", "tick", "--help"],
        ["doctor", "test", "--help"],
        ["doctor", "update", "--help"],
    ):
        result = runner.invoke(app, argv)
        assert result.exit_code == 0, (argv, result.output)


def test_old_doctor_fold_spellings_forward_and_teach() -> None:
    from fno.cli import app

    for old, destination in (
        ("bundle", "doctor bundle"),
        ("codemap", "doctor codemap"),
        ("evals", "doctor evals"),
        ("event", "doctor event"),
        ("lint", "doctor lint"),
        ("observer", "doctor observer"),
        ("skill-diff", "doctor skill-diff"),
        ("status-fanout", "doctor event fanout"),
    ):
        result = runner.invoke(app, [old, "--help"])
        assert result.exit_code == 0, (old, result.output)
        assert f"fno {old} is now fno {destination}" in (result.stderr or "")


def test_harness_and_harness_matrix_leaves_delegate_to_the_binary(
    monkeypatch,
) -> None:
    """Both leaves keep their spellings and exec the fno-agents door: the
    reader and renderer live in the crate."""
    import fno.agents.rust_runtime as rust_runtime
    from fno.cli import app

    seen: list[list[str]] = []

    def fake_route(args, *, binary, env_pin=None, _exec=None, _resolve=None, _stderr=None):
        seen.append(list(args))

    monkeypatch.setattr(rust_runtime, "route_to_rust", fake_route)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_installed_binary", lambda: __import__("pathlib").Path("/usr/bin/true")
    )
    result = runner.invoke(app, ["doctor", "harness", "codex", "--live"])
    assert result.exit_code == 0, result.output
    assert seen[-1][0] == "harness-probe" and seen[-1][1] == "rubric", seen[-1]

    result = runner.invoke(app, ["doctor", "harness-matrix", "--write"])
    assert result.exit_code == 0, result.output
    assert seen[-1] == ["harness-matrix", "--write"], seen[-1]


def test_harness_and_harness_matrix_refuse_without_the_binary(
    monkeypatch,
) -> None:
    """No binary, no fallback: the leaf refuses with the named reason and
    exit 127, the same shape `fno doctor scratch` has."""
    from fno.cli import app

    monkeypatch.setattr("fno.rust_binary.resolve_installed_binary", lambda: None)
    for argv in (
        ["doctor", "harness", "codex"],
        ["doctor", "harness-matrix"],
    ):
        result = runner.invoke(app, argv)
        assert result.exit_code == 127, (argv, result.output, result.stderr)
        assert "fno-agents" in (result.output + (result.stderr or "")), argv
