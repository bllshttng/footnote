"""`fno do research "X"` - retrieve + store + ship the doc deliverable.

Group 1 (US1+US2) made this the `scout` backbone: ddgs -> self-fetch ->
cache `sources.jsonl`. Group 2 (US3) adds the *ship* step: turn the cache into
a `doc` deliverable - a cited brief `<slug>.md` plus its evidence sidecar
`<slug>.sources.jsonl`, written to `config.research.output_dir` and terminating
`DoneAdvisory` (the non-PR completion state).

The deliverable is the default. `--no-deliver` keeps the Group-1 retrieve-only
behavior (cache write only); useful in CI / when no output_dir is configured.
With `--deliver` (default) and an unset `output_dir`, the ship step fails loud
(exit 5) - it never guesses a landing path (AC5).
"""
from __future__ import annotations

import os
from pathlib import Path

import typer

from fno.research.core import (
    DEFAULT_MAX_RESULTS,
    DdgsUnavailable,
    EmptyQuery,
    run_round,
)
from fno.research.deliverable import OutputDirUnset, deliver, emit_done_advisory


def _output_dir() -> "str | None":
    """Read config.research.output_dir (None when unset / on any load error)."""
    try:
        from fno.config import load_settings

        return load_settings().research.output_dir
    except Exception:
        return None


def research_command(
    topic: str = typer.Argument(..., help='Topic to research, e.g. "CA CCLD financials".'),
    max_results: int = typer.Option(
        DEFAULT_MAX_RESULTS, "--max-results", "-m", help="Max sources to retrieve this round."
    ),
    no_claim: bool = typer.Option(
        False, "--no-claim", help="Skip the per-topic single-writer claim (testing/CI)."
    ),
    deliver_doc: bool = typer.Option(
        True, "--deliver/--no-deliver",
        help="Ship the doc deliverable to config.research.output_dir (default on).",
    ),
    stopped: str = typer.Option(
        "declared", "--stopped",
        help='Why the investigation stopped, stamped in the brief ("declared" or "cap N").',
    ),
    node: "str | None" = typer.Option(
        None, "--node",
        help="Graph node this deliverable finishes; binds the shipped brief as its "
        "plan_path (best-effort). Falls back to $FNO_NODE when omitted.",
    ),
) -> None:
    """Retrieve sources for TOPIC, store them, and ship a cited brief.

    Searches via the ddgs backbone, self-fetches each result, writes one row per
    source to ~/.fno/notes/research/<slug>.sources.jsonl, then (unless
    --no-deliver) ships <slug>.md + <slug>.sources.jsonl to
    config.research.output_dir and reports DoneAdvisory.
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
    else:
        typer.echo(
            f'research: "{result.topic}" -> {result.sources_path} '
            f"({result.found} found, {result.verified} verified, {result.failed} failed)"
        )

    if not deliver_doc:
        return

    # Ship step: turn the cache into the doc deliverable (AC1/AC3/AC5).
    # An explicit --node wins (the reliable path: /ship doc's own bash
    # session holds $CLAIMS_ID and can pass it through directly, the same
    # way /blueprint passes --node "$CLAIMS_ID"). $FNO_NODE, the spawn-time
    # provenance env var (mux_spawn.py PROVENANCE_KEYS, read the same way at
    # graph/cli.py:860), is the fallback for a spawned worker's own process
    # environment. Neither is CLAIMS_ID: that is a shell-LOCAL variable a
    # skill's own bash session sets via `eval "$(parse-claims-arg.sh ...)"`;
    # it never reaches a child process environment on its own.
    resolved_node = (node or "").strip() or (os.environ.get("FNO_NODE") or "").strip() or None
    try:
        delivered = deliver(
            result.topic,
            sources_path=Path(result.sources_path),
            stopped=stopped,
            output_dir=_output_dir(),
            node=resolved_node,
        )
    except OutputDirUnset as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(5)
    except OSError as e:
        typer.echo(f"research: failed to write deliverable: {e}", err=True)
        raise typer.Exit(6)

    try:
        from fno.paths import resolve_repo_root

        events_path = resolve_repo_root() / ".fno" / "events.jsonl"
    except Exception:
        events_path = Path(".fno/events.jsonl")  # best-effort fallback; emit is non-fatal
    emit_done_advisory(events_path, slug=delivered.slug)
    typer.echo(
        f"research: shipped {delivered.brief_path} "
        f"(+{delivered.slug}.sources.jsonl, {delivered.verified} cited) -> DoneAdvisory"
    )
    if delivered.bind_warning:
        # Non-fatal (x-953b): the deliverable already landed above, so this
        # warns rather than exits - a lost doc would be worse than an unbound one.
        typer.echo(f"research: {delivered.bind_warning}", err=True)


research_app = research_command  # registered as an individual command in cli.py


if __name__ == "__main__":  # pragma: no cover
    typer.run(research_command)
