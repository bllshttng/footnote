"""``fno doctor`` diagnostics and verification command group.

The moved actions reuse their existing command objects. Nesting changes only
routing, while the group callback preserves the bare diagnostic command.
"""

from __future__ import annotations

import typer
import typer.core

from fno.bundle import bundle_app
from fno.codemap_cli import app as codemap_app
from fno.doctor import doctor_command, plugin_file_command
from fno.doctor_bash_census import bash_census_command
from fno.doctor_footprint import footprint_command
from fno.doctor_graph import graph_app
from fno.doctor_lanes import lanes_command
from fno.doctor_reclaim import reclaim_command
from fno.evals.cli import evals_app
from fno.events.cli import cli as event_app
from fno.lint_cli import lint
from fno.observer.cli import observer_app
from fno.route_cli import inventory_cmd
from fno.scratch_cli import scratch_app
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
doctor_app.add_typer(graph_app, name="graph")
doctor_app.command("lint")(lint)
# Machine janitor for dev-built disk bloat; hidden, `fno help doctor --all`.
doctor_app.command("reclaim", hidden=True)(reclaim_command)
doctor_app.command("footprint", hidden=True)(footprint_command)
# Bash-call shape over this project's transcripts; hidden, `fno help doctor --all`.
doctor_app.command("bash-census", hidden=True)(bash_census_command)


# `doctor intel` shells to the Rust fold; hidden per the new-verb convention.
@doctor_app.command("intel", hidden=True)
def intel_command() -> None:
    """Who typed: per-session provenance counters, tool calls, commits, relay facets.

    Runs the fold at its defaults (14 days, this project). The flag surface
    is the binary's, never Python's: `fno-agents intel --help`.
    """
    import subprocess
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("fno doctor intel: the fno-agents binary was not found; run `fno doctor update --rust`.", err=True)
        raise typer.Exit(code=2)
    result = subprocess.run([str(binary), "intel"], check=False)
    raise typer.Exit(code=propagate_returncode(result.returncode))
# `doctor lanes` is the whole-machine lane advisor: one number and its
# reasoning, or a refusal naming every dark sensor. Hidden per the new-verb
# convention; `fno help doctor --all`.
doctor_app.command("lanes", hidden=True)(lanes_command)


def _route_harness(verb: str, argv: list[str]) -> None:
    """Exec the fno-agents door: the Rust runtime is the only implementation."""
    from fno.agents.rust_runtime import refuse_without_binary, route_to_rust
    from fno.rust_binary import resolve_installed_binary

    binary = resolve_installed_binary()
    if binary is None:
        refuse_without_binary(verb)
    route_to_rust(argv, binary=binary)


@doctor_app.command("harness", hidden=True)
def harness_command(
    harness: str = typer.Argument(..., help="Harness name to probe."),
    live: bool = typer.Option(False, "--live", help="Run real pane and state probes."),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit machine-readable JSON."),
) -> None:
    """The executable support rubric for one harness, served by the crate."""
    argv = ["harness-probe", "rubric", harness]
    if live:
        argv.append("--live")
    if json_out:
        argv.append("--json")
    _route_harness("harness", argv)


# `doctor harness-matrix` regenerates both matrix docs from the table. The
# renderer lives in the fno-agents binary; the leaf refuses without it,
# the same shape `fno doctor scratch` has.
@doctor_app.command("harness-matrix", hidden=True)
def harness_matrix_command(
    write: bool = typer.Option(False, "--write"),
) -> None:
    """Render the features and verb matrices from the capability table."""
    argv = ["harness-matrix"]
    if write:
        argv.append("--write")
    _route_harness("harness-matrix", argv)
doctor_app.command("plugin-file", hidden=True)(plugin_file_command)
# `doctor route` is the reachability read: what this installation's declared
# routing inventory can actually reach (absorbs the old "no surface answers
# this" gap). Hidden per the new-verb convention; `fno help doctor --all`.
doctor_app.command("route", hidden=True)(inventory_cmd)
doctor_app.add_typer(observer_app, name="observer")
# `doctor scratch` is the scratch-shape sweep; the Rust binary is
# the only implementation and the leaf refuses without it.
doctor_app.add_typer(scratch_app, name="scratch")
doctor_app.add_typer(skill_diff_app, name="skill-diff")
# `doctor test` is the canonical spelling (d-df6c29a6): the root
# `fno test` is a VERB_MOVES shim. `doctor update` resolves the same command
# object as the root `fno update`, which stays a root verb.
doctor_app.command("update")(update_command)
