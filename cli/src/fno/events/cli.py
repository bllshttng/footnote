"""fno event subcommands - emit and audit."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional

import typer

cli = typer.Typer(name="event", help="emit and audit events", no_args_is_help=True)


@cli.callback()
def _event_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag
    merge_json_flag(ctx, json_output)


def _detect_source(state_path: Path) -> str:
    """Resolve the event source from a state file's presence.

    Returns ``"target"`` when ``state_path`` exists and the file body contains
    a ``session_id:`` key (the marker for a real target session). Otherwise
    returns ``"test"`` (an allowed source enum value for ad-hoc CLI use).

    The detection is intentionally lightweight (substring match, not YAML
    parse) so the CLI does not pay PyYAML startup cost on every invocation.
    Callers that want a different source for non-target contexts pass
    ``--source`` explicitly.
    """
    try:
        if state_path.is_file():
            text = state_path.read_text(encoding="utf-8")
            if "session_id:" in text:
                return "target"
    except OSError:
        pass
    return "test"


@cli.command()
def emit(
    ctx: typer.Context,
    type_: str = typer.Option(..., "--type", "-t", help="canonical event type (must appear in events-schema.yaml)"),
    data: Optional[str] = typer.Option(
        None,
        "--data",
        "-d",
        help="JSON object string for the event's data envelope",
    ),
    payload: Optional[str] = typer.Option(
        None,
        "--payload",
        help="[DEPRECATED] alias for --data; will be removed in a future release",
    ),
    source: Optional[str] = typer.Option(
        None,
        "--source",
        "-s",
        help="event source enum (default: 'target' if state file present, else 'test')",
    ),
    state_path: Optional[Path] = typer.Option(
        None, "--state", help="path to target-state.md (for source auto-detection)"
    ),
    events_path: Optional[Path] = typer.Option(
        None, "--events", help="path to events.jsonl"
    ),
) -> None:
    """Emit a single canonical event to events.jsonl.

    The envelope is ``{ts, type, source, data}`` (see
    ``cli/src/fno/events/schema.yaml``). Validation runs before the
    file lock is acquired so a malformed call cannot block writers.

    Source defaults to ``target`` when a target state file is present and
    ``test`` otherwise. Override with ``--source``.
    """
    # Lazy imports keep top-level `fno --help` cold-path fast and avoid
    # paying PyYAML schema-load cost when the user is invoking an
    # unrelated subcommand.
    from fno.events import _build, append_event, ValidationError

    if data is not None and payload is not None:
        typer.echo(
            "error: pass either --data or --payload, not both",
            err=True,
        )
        raise typer.Exit(code=1)

    if payload is not None:
        typer.echo(
            "warning: --payload is deprecated; use --data instead. "
            "The alias will be removed in a future release.",
            err=True,
        )
        data_str = payload
    else:
        data_str = data if data is not None else "{}"

    try:
        data_dict = json.loads(data_str)
    except json.JSONDecodeError as exc:
        typer.echo(f"error: invalid JSON in --data: {exc}", err=True)
        raise typer.Exit(code=1)

    if not isinstance(data_dict, dict):
        typer.echo("error: --data must be a JSON object", err=True)
        raise typer.Exit(code=1)

    # Anchor default state + events paths to the repo root so `fno event emit`
    # produces consistent results regardless of which subdirectory the user
    # invokes from. Gemini review on PR #270 caught the previous relative-path
    # default that silently routed events to a per-subdir .fno/ folder
    # and missed the central state file. Fall back to the relative path only
    # if repo discovery fails entirely (e.g. invoked outside any git repo).
    try:
        from fno.paths import resolve_repo_root
        repo_root = resolve_repo_root()
        default_state = repo_root / ".fno" / "target-state.md"
        default_events = repo_root / ".fno" / "events.jsonl"
    except Exception:
        default_state = Path(".fno/target-state.md")
        default_events = Path(".fno/events.jsonl")

    resolved_state = state_path if state_path is not None else default_state
    resolved_source = source if source is not None else _detect_source(resolved_state)

    try:
        event = _build(type_, resolved_source, data_dict)
    except ValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    resolved_events = events_path if events_path is not None else default_events

    try:
        append_event(event, events_path=resolved_events)
    except Exception as exc:
        typer.echo(f"error: failed to append event: {exc}", err=True)
        raise typer.Exit(code=1)

    json_mode = bool(ctx.obj and ctx.obj.get("json", False))
    if json_mode:
        typer.echo(json.dumps(event))
    else:
        # Non-JSON success output: print a stable success token so shell
        # callers using ``$(fno event emit ...)`` or piped automation
        # receive a non-empty value. Codex review on PR #270 caught the
        # previous silent-on-success path (legacy ``emit_event`` returned
        # a freshly-minted nonce on stdout). Prefer ``data.nonce`` when
        # the event type carries one (phase_transition, child_promise),
        # fall back to the canonical timestamp so the token is always
        # populated.
        success_token = event["data"].get("nonce") or event["ts"]
        typer.echo(success_token)


@cli.command("gate-escape")
def gate_escape(
    ctx: typer.Context,
    reason: str = typer.Argument(
        ...,
        help="intervention class: dead-bot | flake | stale-base | wedge | spawn-cap | other",
    ),
    pr: Optional[int] = typer.Option(
        None, "--pr", help="PR the escape rode on (becomes the dedup key when set)"
    ),
    node: Optional[str] = typer.Option(
        None, "--node", help="graph node the escape rode on (attribution)"
    ),
    detail: Optional[str] = typer.Option(
        None, "--detail", help="free-text context (retro flags an empty detail as low-signal)"
    ),
    dedup_key: Optional[str] = typer.Option(
        None,
        "--dedup-key",
        help="explicit PR-less dedup bucket; defaults to reason:session:day",
    ),
    events_path: Optional[Path] = typer.Option(
        None, "--events", help="path to events.jsonl (default: canonical root)"
    ),
) -> None:
    """Tag a human intervention the loop should have handled (x-91b5, Tier-2).

    Low-friction manual sugar for the reasons with no clean auto chokepoint
    (flake / stale-base / wedge): an operator runs this at the moment they
    intervene, so retro's autonomy-debt ranking sees all five reason buckets,
    not just the auto-emitted dead-bot (reconcile) and spawn-cap (spawn gates).
    Fail-closed on an unknown reason (loud non-zero exit, emits nothing).
    Deduped so a looped intervention counts once.
    """
    from fno.events import ValidationError
    from fno.events.gate_escape import default_dedup_key, emit_gate_escape

    # A PR-bearing escape dedups on (reason, pr); a PR-less one on an explicit
    # or default (reason, session, day) bucket. Never both - PR wins.
    key = None if pr else (dedup_key or default_dedup_key(reason))
    try:
        out = emit_gate_escape(
            reason,
            pr=pr,
            node_id=node,
            detail=detail,
            dedup_key=key,
            source="backlog",
            events_path=events_path,
        )
    except ValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    json_mode = bool(ctx.obj and ctx.obj.get("json", False))
    if out is None:
        # Dedup-skip or a swallowed fail-open error: report, do not fail the caller.
        if json_mode:
            typer.echo(json.dumps({"emitted": False, "reason": reason}))
        else:
            typer.echo(f"gate_escape[{reason}] not emitted (already counted or fail-open)")
        return
    if json_mode:
        typer.echo(json.dumps({"emitted": True, "reason": reason, "events": str(out)}))
    else:
        typer.echo(str(out))


@cli.command()
def audit(
    ctx: typer.Context,
    session_id: str = typer.Option(..., "--session-id", help="session ID to audit"),
    strict: bool = typer.Option(False, "--strict", help="check for required event sequences"),
    events_path: Optional[Path] = typer.Option(
        None, "--events", help="path to events.jsonl"
    ),
) -> None:
    """Audit events for a session. Use --strict to check for gaps."""
    from fno.events.log import audit_session

    try:
        result = audit_session(
            events_path=events_path,
            session_id=session_id,
            strict=strict,
        )
    except ValueError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    typer.echo(json.dumps(result))

    if not result["ok"]:
        raise typer.Exit(code=1)


@cli.command("verify-evidence")
def verify_evidence(
    session_id: str = typer.Argument(..., help="SESSION_ID passed to verify_event_evidence"),
    nonce: str = typer.Argument(..., help="NONCE passed to verify_event_evidence"),
    events_file: str = typer.Argument(..., help="EVENTS_FILE path passed to verify_event_evidence"),
    artifact_path: str = typer.Argument(..., help="ARTIFACT_PATH passed to verify_event_evidence"),
) -> None:
    """Verify event evidence via the bundled fno-agents binary.

    Folded from scripts/lib/verify-event-evidence.sh into fno-agents (US1,
    ab-58645f63), so the verb runs on a bare `pip install fno`. Forwards
    stdout/stderr and exit code verbatim.

    Exit codes mirror the former function:
      0 - all pairs satisfied; gate passed.
      1 - specific failure (diagnostic on stdout).
      2 - events.jsonl absent/unreadable or binary missing.
    """
    from fno.agents.rust_runtime import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno event verify-evidence: the fno-agents binary was not found. It "
            "ships in the `pip install fno` wheel and with the plugin; reinstall "
            "fno or run `fno update --rust`, or set FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=2)

    cmd = [
        str(binary),
        "verify-evidence",
        "event",
        session_id,
        nonce,
        events_file,
        artifact_path,
    ]
    result = subprocess.run(cmd, check=False)
    from fno._subprocess_util import propagate_returncode
    raise typer.Exit(code=propagate_returncode(result.returncode))
