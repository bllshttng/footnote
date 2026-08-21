from __future__ import annotations

import click
import typer.main
from typer.testing import CliRunner


runner = CliRunner()


def test_do_advertises_exactly_eleven_direct_children() -> None:
    from fno.do_cli import do_app

    command = typer.main.get_command(do_app)
    assert isinstance(command, click.Group)
    context = click.Context(command, info_name="do")
    assert set(command.list_commands(context)) == {
        "delivery",
        "loops",
        "phase",
        "plan",
        "pr",
        "research",
        "resume",
        "review",
        "state",
        "target",
        "think",
    }


def test_nested_target_init_and_pr_children_resolve() -> None:
    from fno.cli import app

    for argv in (
        ["do", "target", "init", "--help"],
        ["do", "pr", "watch", "tick", "--help"],
        ["do", "pr", "stub-manifest", "write", "--help"],
    ):
        result = runner.invoke(app, argv)
        assert result.exit_code == 0, (argv, result.output)


def test_old_spelling_forwards_and_teaches_new_path() -> None:
    from fno.cli import app

    for argv, destination in (
        (["target", "init", "--help"], "fno do target"),
        (["pr-watch", "tick", "--help"], "fno do pr watch"),
        (["stub-manifest", "write", "--help"], "fno do pr stub-manifest"),
    ):
        result = runner.invoke(app, argv)
        assert result.exit_code == 0, (argv, result.output)
        assert destination in (result.stderr or "")


def test_pr_hot_leaves_remain_silent_and_cold_leaf_teaches() -> None:
    from fno.cli import app

    for leaf in ("status", "merge", "rebase"):
        result = runner.invoke(app, ["pr", leaf, "--help"])
        assert result.exit_code == 0, (leaf, result.output)
        assert "is now" not in (result.stderr or "")

    result = runner.invoke(app, ["pr", "verify", "--help"])
    assert result.exit_code == 0, result.output
    assert "fno pr verify is now fno do pr verify" in (result.stderr or "")
