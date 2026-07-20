"""fno event subcommands - emit and audit."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional

import typer

cli = typer.Typer(name="event", help="emit and audit events", no_args_is_help=True)

# Documented cap for truncatable x-dbaf family data strings (title/reason/evidence/
# termination_reason). Mirrors the Rust RUN_SUMMARY_DATA_CAP; keeps a runaway
# reason from bloating an events.jsonl line while still landing the event.
_PROTOCOL_DATA_STR_CAP = 500


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


def _read_manifest_fields(state_path: Path) -> dict[str, str]:
    """Best-effort parse of ``session_id`` + ``graph_node_id`` from a
    target-state.md. Substring scan (not a YAML parse) to stay cheap; a missing
    or unreadable manifest yields ``{}`` so the caller falls back to flags only.
    """
    fields: dict[str, str] = {}
    try:
        text = state_path.read_text(encoding="utf-8")
    except OSError:
        return fields
    for line in text.splitlines():
        stripped = line.strip()
        for key in ("session_id", "graph_node_id"):
            prefix = f"{key}:"
            if stripped.startswith(prefix):
                val = stripped[len(prefix):].strip().strip('"').strip()
                if val and val != "null":
                    fields.setdefault(key, val)
    return fields


def _stamp_protocol_envelope(
    state_path: Path,
    *,
    node: Optional[str],
    task: Optional[str],
    run: Optional[str],
    parent: Optional[str],
    outcome: Optional[str],
    project: Optional[str],
) -> dict:
    """Assemble the extended-envelope fields for an x-dbaf status-breakpoint
    event. Work coordinates fall back to the manifest; identity (from/model) is
    stamped ONLY for a real session producer and omitted entirely otherwise
    (a bare-shell producer never fakes an empty handle). ``None`` values are
    dropped by ``_build`` so 'omit' means absent, not null.
    """
    from fno.events import PROTOCOL_FAMILY_VERSION

    manifest = _read_manifest_fields(state_path)
    env: dict = {
        "v": PROTOCOL_FAMILY_VERSION,
        "run": run or manifest.get("session_id"),
        "node": node or manifest.get("graph_node_id"),
        "task": task,
        # Resolve lineage HERE so `parent` lands in the durable envelope for every
        # family event (the contract's "parent when spawned" field), not just on
        # the push path - pull observers route/filter on it too (codex P2). The
        # push then reuses event["parent"] rather than re-resolving.
        "parent": _resolve_parent_handle(parent),
        "outcome": outcome,
        "project": project,
    }
    # Identity: session producers only. resolve_harness_identity() returns no
    # session for cron / CI / a bare shell, so from/model stay unset there.
    try:
        from fno.agents.self_stamp import resolve_self_model
        from fno.harness_identity import canonical_handle, resolve_harness_identity

        ident = resolve_harness_identity()
        if ident.session_id and ident.harness:
            env["from"] = canonical_handle(ident.harness, ident.session_id)
            env["model"] = resolve_self_model()
    except Exception:
        pass
    try:
        import socket

        env["host"] = socket.gethostname() or None
    except Exception:
        pass
    return env


def _resolve_parent_handle(explicit: Optional[str]) -> Optional[str]:
    """Resolve the parent spawn-lineage handle, or None (no lineage -> no push).

    An explicit ``--parent`` wins. Otherwise best-effort: find this session's
    own registry row and read its ``spawned_by_*`` edge. Any miss (no ambient
    identity, no row, no edge, malformed registry) returns None so the push
    silently skips - a top-level target has no parent and must not push.
    """
    if explicit:
        return explicit
    try:
        from fno.agents.registry import HARNESS_SESSION_ID_FIELDS, load_registry
        from fno.harness_identity import canonical_handle, handle_aliases, resolve_harness_identity

        ident = resolve_harness_identity()
        if not (ident.session_id and ident.harness):
            return None
        # Match this session's row by STORED IDENTITY, not by name==handle: a
        # spawned row usually carries a caller-provided display name (e.g.
        # tgt-<node>-<harness>-gN), so a handle-equality check would miss it and
        # the push would silently skip (codex P1). The per-harness session field
        # may hold the full id or its first-8 (claude stores the short), so both
        # variants are accepted; a canonically-named row still matches too.
        my_names = set(handle_aliases(ident.harness, ident.session_id))
        session_field = HARNESS_SESSION_ID_FIELDS.get(ident.harness)
        sid_variants = {ident.session_id, ident.session_id[:8]}
        for entry in load_registry():
            same_session = (
                entry.harness == ident.harness
                and session_field is not None
                and getattr(entry, session_field, None) in sid_variants
            )
            if entry.name in my_names or same_session:
                if entry.spawned_by_session and entry.spawned_by_harness:
                    return canonical_handle(entry.spawned_by_harness, entry.spawned_by_session)
                return None
    except Exception:
        return None
    return None


def _push_to_parent(
    parent: str,
    *,
    event_type: str,
    run: Optional[str],
    node: Optional[str],
    reason: Optional[str],
) -> bool:
    """Push a blocked/run_summary notice to the parent via ``fno mail send``.

    `fno mail send` writes the envelope durably BEFORE attempting live delivery,
    so the push is at-least-once for free (AC1-FR); the events.jsonl record was
    already written independently by the caller. Non-fatal: any failure logs one
    stderr note and returns False.
    """
    msg = f"[fno:{event_type}] run={run or '?'}"
    if node:
        msg += f" node={node}"
    if reason:
        msg += f": {reason}"
    try:
        result = subprocess.run(
            ["fno", "mail", "send", parent, msg],
            check=False, capture_output=True, timeout=20,
        )
    except FileNotFoundError:
        typer.echo("push: note: fno unavailable, skipped parent push", err=True)
        return False
    except Exception as exc:  # noqa: BLE001 - push must never wedge the emit
        typer.echo(f"push: note: parent push failed (non-fatal): {exc}", err=True)
        return False
    if result.returncode != 0:
        typer.echo(
            f"push: note: parent push failed (non-fatal): "
            f"{result.stderr.decode('utf-8', 'replace').strip()}",
            err=True,
        )
        return False
    return True


@cli.command("push-parent")
def push_parent(
    event_type: str = typer.Option(..., "--type", "-t", help="blocked | run_summary"),
    run: Optional[str] = typer.Option(None, "--run", help="target-run id referenced in the notice"),
    node: Optional[str] = typer.Option(None, "--node", help="backlog node id"),
    reason: Optional[str] = typer.Option(None, "--reason", "-R", help="one-line reason / termination"),
    parent: Optional[str] = typer.Option(None, "--parent", help="explicit parent handle (else registry-resolved)"),
) -> None:
    """Push a status-breakpoint notice to the parent handle (x-dbaf push leg).

    The Rust ``finalize`` shells this for ``run_summary`` (it emits the
    events.jsonl line natively, so it cannot ride the emit-CLI auto-push). No
    spawn lineage -> silent skip. Always exits 0: the push is non-fatal and the
    pull leg (events.jsonl) never depends on it.
    """
    handle = _resolve_parent_handle(parent)
    if not handle:
        typer.echo("push: no parent lineage; skipped")
        raise typer.Exit(code=0)
    ok = _push_to_parent(handle, event_type=event_type, run=run, node=node, reason=reason)
    typer.echo("pushed" if ok else "push-skipped")
    raise typer.Exit(code=0)


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
    node: Optional[str] = typer.Option(
        None, "--node", help="backlog node id (x-dbaf family; envelope coordinate)"
    ),
    task: Optional[str] = typer.Option(
        None, "--task", help="task id within the plan (x-dbaf family; envelope coordinate)"
    ),
    run: Optional[str] = typer.Option(
        None, "--run", help="target-run id, the dedup identity (x-dbaf family; manifest fallback)"
    ),
    parent: Optional[str] = typer.Option(
        None, "--parent", help="parent spawn-lineage handle (x-dbaf family; when spawned)"
    ),
    outcome: Optional[str] = typer.Option(
        None, "--outcome", help="return-contract outcome (x-dbaf family; task_done/run_summary only)"
    ),
    project: Optional[str] = typer.Option(
        None, "--project", help="project the work belongs to (x-dbaf family; envelope coordinate)"
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
    from fno.events import _build, append_event, PROTOCOL_FAMILY_TYPES, ValidationError

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

    envelope = None
    if type_ in PROTOCOL_FAMILY_TYPES:
        # Truncate the free-text data strings to the documented cap (AC2-EDGE,
        # Boundaries) so an oversized reason/title lands truncated rather than
        # rejected; envelope fields are bounded by construction.
        for _k in ("title", "reason", "evidence", "termination_reason"):
            _v = data_dict.get(_k)
            if isinstance(_v, str) and len(_v) > _PROTOCOL_DATA_STR_CAP:
                data_dict[_k] = _v[:_PROTOCOL_DATA_STR_CAP]
        envelope = _stamp_protocol_envelope(
            resolved_state,
            node=node,
            task=task,
            run=run,
            parent=parent,
            outcome=outcome,
            project=project,
        )

    try:
        event = _build(type_, resolved_source, data_dict, envelope=envelope)
    except ValidationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)

    resolved_events = events_path if events_path is not None else default_events

    try:
        append_event(event, events_path=resolved_events)
    except Exception as exc:
        typer.echo(f"error: failed to append event: {exc}", err=True)
        raise typer.Exit(code=1)

    # Push leg (x-dbaf): blocked + run_summary notify the parent when spawn
    # lineage exists. Fired AFTER the durable append so the events.jsonl record
    # is independent of the push (AC1-FR). No lineage -> silent skip.
    # (run_summary is normally pushed by Rust finalize's native emit; a
    # CLI-emitted one pushes here too for uniformity.)
    if type_ in ("blocked", "run_summary"):
        _parent = event.get("parent")  # already resolved into the envelope above
        if _parent:
            _push_to_parent(
                _parent,
                event_type=type_,
                run=event.get("run"),
                node=event.get("node"),
                reason=data_dict.get("reason") or data_dict.get("termination_reason"),
            )

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
        None,
        "--pr-number",
        "--pr",
        help="PR the escape rode on (becomes the dedup key when set)",
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
    # or default (reason, session, day) bucket. Reject both explicitly rather
    # than silently dropping --dedup-key (gemini review on #241).
    effective_pr = pr if (pr is not None and pr > 0) else None
    if effective_pr is not None and dedup_key is not None:
        typer.echo("error: pass --pr-number XOR --dedup-key, not both", err=True)
        raise typer.Exit(code=1)
    key = None if effective_pr else (dedup_key or default_dedup_key(reason))
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
