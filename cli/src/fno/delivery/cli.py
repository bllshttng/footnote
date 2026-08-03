"""Hidden generic delivery inspection surface."""

from __future__ import annotations

import json
from pathlib import Path

import typer

from fno.delivery.reader import evaluate_plan_delivery


delivery_app = typer.Typer(name="delivery", help="Evaluate generic delivery evidence.")


@delivery_app.command("evaluate")
def evaluate_command(
    plan_path: Path = typer.Option(..., "--plan-path"),
    events: Path = typer.Option(Path(".fno/events.jsonl"), "--events"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    response = evaluate_plan_delivery(plan_path, events)
    payload = response.model_dump(mode="json")
    if json_output:
        typer.echo(json.dumps(payload, separators=(",", ":")))
        return
    typer.echo(f"delivery: {response.status}")
    for diagnostic in response.diagnostics:
        typer.echo(f"- {diagnostic}")
