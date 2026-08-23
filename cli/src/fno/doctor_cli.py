"""``fno doctor`` diagnostics and verification command group.

The moved actions reuse their existing command objects. Nesting changes only
routing, while the group callback preserves the bare diagnostic command.
"""

from __future__ import annotations

import typer
import typer.core

from fno.bundle import bundle_app
from fno.codemap_cli import app as codemap_app
from fno.doctor import doctor_command
from fno.doctor_footprint import footprint_command
from fno.evals.cli import evals_app
from fno.events.cli import cli as event_app
from fno.lint_cli import lint
from fno.observer.cli import observer_app
from fno.skill_diff.cli import skill_diff_app
from fno.status_fanout import status_fanout_app
from fno.test_cmd import test_command
from fno.update import update_command


class DoctorGroup(typer.core.TyperGroup):
    """Typer group that preserves the existing Click-based test command."""

    def __init__(self, *args, **kwargs) -> None:
        super().__init__(*args, **kwargs)
        self.add_command(test_command, name="test")


doctor_app = typer.Typer(
    name="doctor",
    help="Diagnose and verify the installed fno environment.",
    cls=DoctorGroup,
    invoke_without_command=True,
    no_args_is_help=False,
)
doctor_app.callback(invoke_without_command=True)(doctor_command)

# Add fanout to a copy of the event registrations. The compatibility event app
# keeps its original shape while the new path gains the nested action.
doctor_event_app = typer.Typer(
    name="event",
    help=event_app.info.help,
    no_args_is_help=True,
)
doctor_event_app.registered_callback = event_app.registered_callback
doctor_event_app.registered_commands.extend(event_app.registered_commands)
doctor_event_app.registered_groups.extend(event_app.registered_groups)
doctor_event_app.add_typer(status_fanout_app, name="fanout")

doctor_app.add_typer(bundle_app, name="bundle")
doctor_app.add_typer(codemap_app, name="codemap")
doctor_app.add_typer(evals_app, name="evals")
doctor_app.add_typer(doctor_event_app, name="event")
doctor_app.command("lint")(lint)
doctor_app.command("footprint", hidden=True)(footprint_command)
doctor_app.add_typer(observer_app, name="observer")
doctor_app.add_typer(skill_diff_app, name="skill-diff")
# test and update resolve the SAME command objects as the root spellings
# `fno test` / `fno update`, which are canonical by the 2026-08-22 operator
# ruling on the reorg node; these registrations are the kept silent aliases.
doctor_app.command("update")(update_command)
