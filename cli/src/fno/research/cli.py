"""`fno research "X"` - the retrieve+store entry point (Group 1, US1+US2).

A thin command that runs the `scout` backbone (ddgs -> self-fetch ->
sources.jsonl) for a topic and prints a one-line summary. The full alias over
`target` (presetting executor=research deliverable=doc) plus the `doc`
DoneAdvisory terminal is Group 2; this command is the standalone primitive
those build on, and the surface that exercises retrieval+store today.
"""
from __future__ import annotations

import sys

import typer

from fno.research.core import (
    DEFAULT_MAX_RESULTS,
    DdgsUnavailable,
    EmptyQuery,
    run_round,
)


def research_command(
    topic: str = typer.Argument(..., help='Topic to research, e.g. "CA CCLD financials".'),
    max_results: int = typer.Option(
        DEFAULT_MAX_RESULTS, "--max-results", "-m", help="Max sources to retrieve this round."
    ),
    no_claim: bool = typer.Option(
        False, "--no-claim", help="Skip the per-topic single-writer claim (testing/CI)."
    ),
) -> None:
    """Retrieve sources for TOPIC and store them as an evidence sidecar.

    Searches via the ddgs backbone, self-fetches each result, and writes one
    row per source (url, fetched_at, hash, extract, verified) to
    ~/.fno/notes/research/<slug>.sources.jsonl.
    """
    try:
        result = run_round(topic, max_results=max_results, claim=not no_claim)
    except EmptyQuery as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(2)
    except DdgsUnavailable as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(3)

    if result.note == "topic claimed by another writer":
        typer.echo(f"research: {result.note} ({result.slug}); nothing written.", err=True)
        raise typer.Exit(4)

    if result.note == "no sources found":
        typer.echo(f'research: no sources found for "{result.topic}" -> {result.sources_path}')
        return

    typer.echo(
        f'research: "{result.topic}" -> {result.sources_path} '
        f"({result.found} found, {result.verified} verified, {result.failed} failed)"
    )


research_app = research_command  # registered as an individual command in cli.py


if __name__ == "__main__":  # pragma: no cover
    typer.run(research_command)
