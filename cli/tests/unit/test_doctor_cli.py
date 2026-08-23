from __future__ import annotations

import click
import typer
import typer.main
from typer.testing import CliRunner


runner = CliRunner()


def test_doctor_advertises_ten_direct_actions() -> None:
    from fno.doctor_cli import doctor_app

    command = typer.main.get_command(doctor_app)
    assert isinstance(command, click.Group)
    context = click.Context(command, info_name="doctor")
    assert set(command.list_commands(context)) == {
        "bundle",
        "codemap",
        "evals",
        "event",
        "footprint",
        "lint",
        "observer",
        "skill-diff",
        "test",
        "update",
    }


def test_bare_doctor_matches_the_pre_group_command(
    monkeypatch,
) -> None:
    from fno import doctor
    from fno.cli import app

    monkeypatch.setattr(
        doctor,
        "build_report",
        lambda source: {"rust_binary": None, "status": "fresh"},
    )
    monkeypatch.setattr(doctor, "_blockers", lambda result: [])
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: None)
    monkeypatch.setattr(doctor, "_cargo_bin_present", lambda: False)
    monkeypatch.setattr(
        doctor,
        "_emit_human",
        lambda result, source, rust, *, err, cargo_present: typer.echo(
            "doctor-output", err=err
        ),
    )
    monkeypatch.setattr(doctor, "_preamble_budget_line", lambda source: None)

    legacy = typer.Typer(add_completion=False)
    legacy.command("doctor")(doctor.doctor_command)
    before = runner.invoke(legacy, [])
    after = runner.invoke(app, ["doctor"])

    assert after.exit_code == before.exit_code == 0
    assert after.stdout_bytes == before.stdout_bytes
    assert after.stderr_bytes == before.stderr_bytes


def test_nested_doctor_actions_resolve() -> None:
    from fno.cli import app

    for argv in (
        ["doctor", "bundle", "check", "--help"],
        ["doctor", "codemap", "--help"],
        ["doctor", "evals", "grade", "--help"],
        ["doctor", "event", "fanout", "tick", "--help"],
        ["doctor", "lint", "menu-caps", "--help"],
        ["doctor", "observer", "sweep", "--help"],
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


def test_restored_test_and_update_resolve_at_doctor_as_silent_aliases() -> None:
    """`fno test` / `fno update` are canonical (2026-08-22 operator ruling),
    so doctor keeps the nested spellings as silent aliases - registered,
    quiet, no teaching line."""
    from fno.cli import app

    for argv in (["doctor", "test", "--help"], ["doctor", "update", "--help"]):
        result = runner.invoke(app, argv)
        assert result.exit_code == 0, (argv, result.output)
        assert "is now" not in (result.stderr or ""), argv
