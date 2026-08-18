"""fno event subcommands - emit and audit."""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Optional

import typer

cli = typer.Typer(name="event", help="emit and audit events", no_args_is_help=True)

# Documented cap for truncatable x-dbaf family data strings (title/reason/evidence/
# termination_reason). Mirrors the Rust RUN_SUMMARY_DATA_CAP; keeps a runaway
# reason from bloating an events.jsonl line while still landing the event.
_PROTOCOL_DATA_STR_CAP = 500

# Event types written to the global log as well as the project one, so a reader
# in ANOTHER checkout of the same repo can see them.
#
# NO READER CONSUMES THE MIRRORED `review_attestation` TODAY, AND THAT IS
# DELIBERATE. Say it plainly here, because dead code that reads as load-bearing
# is its own trap. The gate chain runs attestation -> loopcheck -> coverage:
# `unattested_reviewers_scan` (loopcheck.rs) reads attestations from the PROJECT
# log alone, and `fno pr merge` reads `review_coverage`, never an attestation.
# This entry was originally added on a misdiagnosis - the block it was meant to
# fix was coverage never being produced at all, not an attestation missing from
# canonical. It stays for symmetry with the coverage path, which loopcheck.rs
# already dual-writes for exactly this reason, and because x-3a3f moves a reader
# to the global log and will need it. Named intent, not dead code.
#
# Small on purpose. Membership is earned by a cross-checkout reader, current or
# named-and-coming. `review_coverage` needs no entry: loopcheck.rs emits it to
# both logs itself, and a second writer here would double every row the merge
# gate counts.
GLOBAL_MIRROR_TYPES = frozenset({"review_attestation"})

# Event types whose emit ALSO mirrors the verdict to GitHub as the reviewer
# lane's bot identity (x-93ea). Deliberately a SEPARATE set from
# GLOBAL_MIRROR_TYPES: global-log membership answers "which events earn a
# cross-checkout copy" and will drift for its own reasons; joining it must
# not silently gain a GitHub posting call whose data may carry no verdict.
PUBLISH_REVIEW_TYPES = frozenset({"review_attestation"})


@cli.callback()
def _event_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
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
                val = stripped[len(prefix) :].strip().strip('"').strip()
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
        from fno.harness_identity import (
            canonical_handle,
            resolve_harness_identity,
        )

        ident = resolve_harness_identity()
        if ident.session_id and ident.harness:
            env["from"] = canonical_handle(ident.session_id)
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
        from fno.harness_identity import (
            canonical_handle,
            resolve_harness_identity,
        )

        ident = resolve_harness_identity()
        if not (ident.session_id and ident.harness):
            return None
        # Match this session's row by STORED IDENTITY, not by name==handle: a
        # spawned row usually carries a caller-provided display name (e.g.
        # tgt-<node>-<harness>-gN), so a handle-equality check would miss it and
        # the push would silently skip (codex P1). The per-harness session field
        # may hold the full id or its first-8 (claude stores the short), so both
        # variants are accepted; a canonically-named row still matches too.
        my_handle = canonical_handle(ident.session_id)
        session_field = HARNESS_SESSION_ID_FIELDS.get(ident.harness)
        sid_variants = {ident.session_id, canonical_handle(ident.session_id)}
        for entry in load_registry():
            same_session = (
                entry.harness == ident.harness
                and session_field is not None
                and getattr(entry, session_field, None) in sid_variants
            )
            if entry.name == my_handle or same_session:
                if entry.spawned_by_session and entry.spawned_by_harness:
                    return canonical_handle(entry.spawned_by_session)
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
            check=False,
            capture_output=True,
            timeout=20,
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
    reason: Optional[str] = typer.Option(
        None, "--reason", "-R", help="one-line reason / termination"
    ),
    parent: Optional[str] = typer.Option(
        None, "--parent", help="explicit parent handle (else registry-resolved)"
    ),
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
    type_: str = typer.Option(
        ..., "--type", "-t", help="canonical event type (must appear in events-schema.yaml)"
    ),
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
        None,
        "--outcome",
        help="return-contract outcome (x-dbaf family; task_done/run_summary only)",
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
    events_path: Optional[Path] = typer.Option(None, "--events", help="path to events.jsonl"),
) -> None:
    """Emit a single canonical event to events.jsonl.

    The envelope is ``{ts, type, source, data}`` (see
    ``cli/src/fno/events/schema.yaml``). Validation runs before the
    file lock is acquired so a malformed call cannot block writers.

    Source defaults to ``target`` when a target state file is present and
    ``test`` otherwise. Override with ``--source``.
    """
    if type_ == "verification_receipt":
        typer.echo(
            "error: verification_receipt is preflight-owned; use the preflight runner",
            err=True,
        )
        raise typer.Exit(code=1)
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
        # Bound either way: the global-log mirror below reads it, and leaving it
        # unbound here would turn a discovery failure into a NameError there.
        repo_root = None
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

    # CROSS-CHECKOUT TYPES ALSO REACH THE GLOBAL LOG.
    #
    # The project log is written wherever the emitter happens to run, which for
    # a review is a worktree. The global log is the one file every checkout of a
    # repo stands in, so it is where a cross-checkout reader looks. See
    # GLOBAL_MIRROR_TYPES for which types earn a place here and for the fact
    # that the mirrored attestation has no reader yet.
    #
    # `global_events_json()`, never `state_dir() / "events.jsonl"`. A RELATIVE
    # `config.state_dir` resolves into the repo checkout, while the global
    # journal deliberately falls back to `~/.fno` - which is also the path the
    # Rust writer hardcodes. Writing via state_dir() on exactly those configs
    # puts the row in a file nobody opens. `fno/pr/_reviews.py` carries the same
    # warning on the READ side; the two must name one path or the mirror is a
    # write with no reader by construction.
    #
    # Best-effort: the durable project append above already succeeded, so a
    # failure here must not fail the emit and lose that record.
    if type_ in GLOBAL_MIRROR_TYPES:
        try:
            from fno.paths import global_events_json, repo_identity

            global_events = global_events_json()
            if global_events.resolve() != Path(resolved_events).resolve():
                # Scope the row the way loopcheck scopes every coverage row it
                # writes here (x-f43c). The global log is cross-project and this
                # payload carries no repo of its own, so without it a reader can
                # only match on `head_sha` - and a fork shares those. Stamped on
                # the MIRRORED copy alone: the project log needs no scoping, and
                # the row already appended above must not change under it.
                # Omitted, never null, when no remote resolves.
                mirrored = dict(event)
                # `resolve_repo_root()`, the same root this emit already resolved
                # above - not `Path.cwd()`. It honours FNO_REPO_ROOT, and a
                # `--events` pointing into another checkout would otherwise stamp
                # the row with the CWD's identity: a silently mis-scoped row,
                # which is the one failure the scoping exists to prevent.
                slug = repo_identity(repo_root) if repo_root else None
                if slug:
                    mirrored["data"] = {**event.get("data", {}), "repo": slug}
                append_event(mirrored, events_path=global_events)
        except Exception as exc:  # noqa: BLE001 - never lose the project append
            typer.echo(
                f"warning: {type_} not mirrored to the global log: {exc}",
                err=True,
            )

    # BOT-REVIEW MIRROR (x-93ea): a review_attestation also posts to GitHub as
    # the reviewer lane's own identity (config.review.bot_identity), so a clean
    # pass can land APPROVE instead of the COMMENTED an author account is
    # structurally limited to. This emit is the ONE call every verdict already
    # funnels through - the skill script, a direct CLI emit, and a spawned
    # worker all land here - so the producer has exactly one reachable path
    # rather than sitting on one of N paths while the others silently skip it.
    #
    # Gated on PUBLISH_REVIEW_TYPES, NOT GLOBAL_MIRROR_TYPES: that set answers
    # "which events earn a global-log copy" and its membership will drift for
    # global-log reasons; a type joining it must not silently gain GitHub
    # posting (a data-less verdict would refuse on every emit).
    #
    # Best-effort, same posture as the global-log mirror above: the durable
    # append has already succeeded and a network failure must never fail the
    # emit. Unconfigured lane -> publish_review returns skipped without any
    # network call, so a stock install sees one stderr receipt line and nothing
    # else changes.
    if type_ in PUBLISH_REVIEW_TYPES:
        try:
            from fno.pr._publish_review import publish_review

            _data = event.get("data", {})
            result = publish_review(
                head_sha=str(_data.get("head_sha") or ""),
                verdict=str(_data.get("verdict") or ""),
                reviewer=str(_data.get("reviewer") or ""),
                cwd=str(repo_root) if repo_root else os.getcwd(),
            )
            typer.echo(result.receipt, err=True)
        except Exception as exc:  # noqa: BLE001 - never fail the emit
            typer.echo(f"bot-review: skipped (mirror error: {exc})", err=True)

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
def gc(
    ctx: typer.Context,
    events_path: Optional[Path] = typer.Option(
        None, "--events", help="path to events.jsonl (default: worktree root)"
    ),
    ttl_hours: Optional[int] = typer.Option(
        None, "--ttl-hours", min=1, help="ephemeral retention horizon in hours"
    ),
    dry_run: bool = typer.Option(False, "--dry-run", "-N", help="report without rewriting"),
) -> None:
    """Delete expired explicit-ephemeral rows while preserving all other rows."""
    from fno.events import RETENTION_MINIMUM_TTL_HOURS, SchemaUnavailableError
    from fno.events.gc import gc_events
    from fno.paths import resolve_repo_root

    resolved = events_path or (resolve_repo_root() / ".fno" / "events.jsonl")
    horizon = ttl_hours if ttl_hours is not None else RETENTION_MINIMUM_TTL_HOURS
    try:
        result = gc_events(resolved, ttl_hours=horizon, dry_run=dry_run)
    except (
        OSError,
        RuntimeError,
        TimeoutError,
        ValueError,
        SchemaUnavailableError,
    ) as exc:
        typer.echo(f"error: event gc failed: {exc}", err=True)
        raise typer.Exit(code=1)
    payload = {**result, "events": str(resolved), "ttl_hours": horizon, "dry_run": dry_run}
    if bool(ctx.obj and ctx.obj.get("json", False)):
        typer.echo(json.dumps(payload))
    else:
        typer.echo(
            "event gc: "
            f"scanned={result['scanned']} deleted={result['deleted']} "
            f"kept={result['kept']} malformed={result['malformed']}"
        )


@cli.command()
def audit(
    ctx: typer.Context,
    session_id: str = typer.Option(..., "--session-id", help="session ID to audit"),
    strict: bool = typer.Option(False, "--strict", help="check for required event sequences"),
    events_path: Optional[Path] = typer.Option(None, "--events", help="path to events.jsonl"),
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
