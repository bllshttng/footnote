"""`fno agents harness probe` - the harness capability-table instruments.

Moved verbatim out of ``fno/agents/cli.py`` (file budget): the composition
registers this app on the agents app there, and the logic lives here.
"""

from __future__ import annotations

import typer

harness_app = typer.Typer(
    help="Instruments over the harness capability table.",
    no_args_is_help=True,
)


@harness_app.command("probe")
def harness_probe(
    harness: str = typer.Argument(..., help="Harness name to probe."),
    live: bool = typer.Option(
        False, "--live", help="Allow behavioral probes (they spawn a scratch session)."
    ),
    write: bool = typer.Option(
        False, "--write", help="Emit the config stanza for each disagreement, evidence + date beside it."
    ),
    as_json: bool = typer.Option(False, "--json", "-J", help="Machine-readable report."),
) -> None:
    import json as _json

    from fno.agents.capability_probe import probe_harness

    report = probe_harness(harness, live=live, write=write)
    if as_json:
        typer.echo(_json.dumps(report, indent=2))
    else:
        if "error" in report:
            typer.secho(f"probe refused: {report['error']}", err=True)
            raise typer.Exit(code=1)
        typer.echo(f"probe {harness} (map_version {report['map_version']})")
        for field in report["fields"]:
            typer.echo(
                f"{field['verdict']:<11} {field['field']}: {field['detail']}"
            )
        for warning in report["warnings"]:
            typer.secho(f"override warning: {warning}", fg=typer.colors.YELLOW, err=True)
        if report["stanza"]:
            typer.echo("")
            typer.echo(report["stanza"])
    if any(field["verdict"] == "DISAGREES" for field in report["fields"]):
        raise typer.Exit(code=1)
