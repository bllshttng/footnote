"""fno self-introspection commands: whoami + status.

Two read-only top-level commands (registered in ``fno.cli`` as
``fno whoami`` / ``fno status``). They were formerly ``fno agent whoami`` /
``fno agent status``; the ``fno agent`` (singular) namespace was retired
(ab-12dd2a5d) once ``suggest`` / ``capabilities`` were trimmed - neither was
auto-invoked anywhere. ``fno agents`` (plural, the dispatch mesh) is unrelated
and untouched.

    whoami   - one-line operating-stack summary (project + fleet + walker + session)
    status   - gate satisfaction + bounded events tail + inconsistencies

Both are read-only (no state mutations, no events emitted); tests assert
paired-state hash invariance. Shared options on each command:

    --json / -J    emit JSON to stdout (command-specific shape)
    --state-file   override session state-file detection
    --no-walker    suppress walker layer in output
    --no-fleet     suppress fleet layer in output
"""
from __future__ import annotations

import json
import os
from dataclasses import asdict, is_dataclass, replace
from datetime import date, datetime
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional

import typer

from fno.agent.state import (
    AgentContext,
    AgentOptions,
    MalformedStateError,
    MissingStateFileOverrideError,
    load_agent_context,
)
from fno.harness_identity import canonical_handle, resolve_harness_identity


def _mail_handle() -> tuple[Optional[str], Optional[str]]:
    """(canonical reply handle, harness_session_id) from ambient identity, else
    (None, None). Same derivation `stamp_from(None)` uses, so whoami advertises
    the exact string a name-lane send self-stamps. Read-only: env + a string
    slice, no discovery scan (whoami's md5-invariance contract)."""
    ident = resolve_harness_identity()
    if ident.session_id and ident.harness:
        return canonical_handle(ident.harness, ident.session_id), ident.session_id
    return None, None


def _mail_addresses(
    handle: Optional[str], agent_self: str, project_root: Path
) -> list[Optional[str]]:
    """The durable addresses this session answers to: canonical reply handle,
    mesh name (registered-agent send lane), and project inbox (project lane).
    Project resolution is guarded - an unresolvable project just omits that lane
    rather than breaking whoami."""
    addrs: list[Optional[str]] = [handle]
    if agent_self:
        addrs.append(agent_self)
    try:
        from fno.inbox.store import resolve_project

        addrs.append(resolve_project(cwd=project_root))
    except Exception:
        pass
    return addrs


def _mail_unread_count(addresses: Iterable[Optional[str]]) -> int:
    """Total unread bus messages across every durable address this session
    answers to (canonical handle, mesh name, project inbox) - the same cursor
    scan ``fno mail unread`` runs, deduped. The durable-floor lanes this exposes
    address by handle, by mesh name (registered-agent send), and by project, so
    counting only the handle would leave the other dead-letter lanes invisible.
    Each address is guarded independently so one unreadable lane never zeroes the
    others; whoami is the confused-agent recovery verb and must never gain a
    failure mode."""
    total = 0
    seen: set[str] = set()
    for addr in addresses:
        if not addr or addr in seen:
            continue
        seen.add(addr)
        try:
            from fno.bus.cursor import scan_unread

            total += len(scan_unread(addr, warn=False))
        except Exception:
            continue
    return total


def _derive_status_from_events(project_root: Path, session_id: Optional[str]) -> str:
    """Derive session status from the latest termination event in events.jsonl.

    Returns a human-readable string. Called when target-state.md has no status
    field (immutable manifest after control-plane collapse, ab-d0337fbc).
    Minimal read: scan the tail of events.jsonl for a termination event matching
    the session_id. Falls back to 'active' if no termination found.
    """
    events_path = project_root / ".fno" / "events.jsonl"
    if not events_path.exists():
        return "active"
    try:
        tail = _tail_events(events_path, max_bytes=131072, max_lines=50)
    except OSError:
        return "active"
    # Scan in reverse for a termination event
    for ev in reversed(tail):
        ev_type = str(ev.get("type") or ev.get("kind") or ev.get("event") or "")
        if ev_type != "termination":
            continue
        data = ev.get("data") or {}
        if session_id and str(data.get("session_id") or "") != session_id:
            continue
        reason = str(data.get("termination_reason") or data.get("reason") or "")
        if reason:
            return f"terminated ({reason})"
        return "terminated"
    return "active"


EVENTS_TAIL_BYTES = 65536
EVENTS_TAIL_LINES = 10


def _load_or_exit(opts: AgentOptions) -> AgentContext:
    """Centralized state loader; converts state-file errors -> rc=2."""
    try:
        return load_agent_context(state_file_override=opts.state_file)
    except MalformedStateError as exc:
        typer.echo(f"error: malformed state file: {exc.path}: {exc.original}", err=True)
        raise typer.Exit(code=2)
    except MissingStateFileOverrideError as exc:
        typer.echo(f"error: --state-file path does not exist: {exc.path}", err=True)
        raise typer.Exit(code=2)


def _drop_layers(ctx: AgentContext, opts: AgentOptions) -> AgentContext:
    """Apply --no-walker / --no-fleet suppressions. Returns a NEW context;
    leaves the caller's reference intact so concurrent reads stay safe."""
    if not opts.no_walker and not opts.no_fleet:
        return ctx
    return replace(
        ctx,
        walker=None if opts.no_walker else ctx.walker,
        fleet=None if opts.no_fleet else ctx.fleet,
    )


def _ctx_to_jsonable(ctx: AgentContext) -> Dict[str, Any]:
    """AgentContext -> dict suitable for json.dumps. Paths -> str."""
    def _normalize(value: Any) -> Any:
        if isinstance(value, Path):
            return str(value)
        if isinstance(value, (datetime, date)):
            # yaml.safe_load turns unquoted ISO timestamps in the manifest
            # frontmatter (e.g. `created_at: 2026-06-11T13:27:56Z`) into
            # datetime objects, which json.dumps cannot serialize. Emit
            # isoformat so `--json` never crashes on a real manifest.
            return value.isoformat()
        if is_dataclass(value) and not isinstance(value, type):
            return {k: _normalize(v) for k, v in asdict(value).items()}
        if isinstance(value, dict):
            return {k: _normalize(v) for k, v in value.items()}
        if isinstance(value, list):
            return [_normalize(v) for v in value]
        return value

    return {
        "project_root": str(ctx.project_root),
        "provider": ctx.provider,
        "fleet": _normalize(ctx.fleet) if ctx.fleet else None,
        "walker": _normalize(ctx.walker) if ctx.walker else None,
        "session": _normalize(ctx.session) if ctx.session else None,
        "detected_paths": [str(p) for p in ctx.detected_paths],
        "warnings": list(ctx.warnings),
    }


def _emit_warnings(ctx: AgentContext) -> None:
    for w in ctx.warnings:
        typer.echo(f"warn: {w}", err=True)


def _global_json(ctx: typer.Context) -> bool:
    """Honor the root callback's global -J/--json flag (`fno -J whoami`).

    The root `fno` callback stores --json/-J in ctx.obj["json"]; an individual
    top-level command must OR it with its own command-local flag so both
    `fno -J whoami` (global) and `fno whoami -J` (local) emit JSON. Mirrors the
    `fno review` command's merge pattern. Defensive: ctx.obj may be unset or a
    non-dict when the command is invoked outside the root app (e.g. a bare
    single-command test harness)."""
    obj = getattr(ctx, "obj", None)
    return bool(obj.get("json")) if isinstance(obj, dict) else False


# --- whoami --------------------------------------------------------------


def whoami_command(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False, "--json", "-J", help="emit JSON to stdout"
    ),
    state_file: Optional[Path] = typer.Option(
        None, "--state-file", help="override session state-file detection"
    ),
    no_walker: bool = typer.Option(
        False, "--no-walker", help="suppress walker layer in output"
    ),
    no_fleet: bool = typer.Option(
        False, "--no-fleet", help="suppress fleet layer in output"
    ),
) -> None:
    """Print the agent's operating stack: project + fleet + walker + session."""
    opts = AgentOptions(
        json_output=json_output or _global_json(ctx),
        state_file=state_file,
        no_walker=no_walker,
        no_fleet=no_fleet,
    )
    state = _drop_layers(_load_or_exit(opts), opts)
    mail, harness_sid = _mail_handle()
    agent_self = (os.environ.get("FNO_AGENT_SELF") or "").strip()
    mail_unread = _mail_unread_count(_mail_addresses(mail, agent_self, state.project_root))
    if opts.json_output:
        payload = _ctx_to_jsonable(state)
        payload["mail_handle"] = mail
        payload["harness_session_id"] = harness_sid
        if mail_unread:
            payload["mail_unread"] = mail_unread
        typer.echo(json.dumps(payload, indent=2, sort_keys=True))
        _emit_warnings(state)
        return
    typer.echo(f"project:  {state.project_root}")
    if state.fleet:
        wave = ""
        if state.fleet.wave_current is not None and state.fleet.wave_total is not None:
            wave = f" - wave {state.fleet.wave_current}/{state.fleet.wave_total}"
        title = f" ({state.fleet.title})" if state.fleet.title else ""
        typer.echo(f"fleet:    {state.fleet.mission_id}{title}{wave}")
    if state.walker:
        progress = f" - {state.walker.in_flight} in-flight, {state.walker.done} done"
        phase = f" phase={state.walker.phase}" if state.walker.phase else ""
        typer.echo(f"walker:   {state.walker.session_id or '(unknown)'}{phase}{progress}")
    if state.session:
        # phase: n/a (collapsed) when current_phase absent in immutable manifest
        raw = state.session.raw
        if state.session.phase:
            phase = f" phase={state.session.phase}"
        elif "current_phase" not in raw and "phase" not in raw:
            phase = " phase=n/a (collapsed)"
        else:
            phase = ""
        status = f" status={state.session.status}" if state.session.status else ""
        # 'run:' not 'session:': the value is the run-scoped ledger id (spans
        # harness sessions across handoff/revival), not a session/mail handle -
        # the misnomer that got copied as a --from-name and stranded a reply.
        typer.echo(
            f"run:      {state.session.session_id or '(unknown)'} ({state.session.kind}){phase}{status} (ledger/run id - not a mail handle)"
        )
    if mail:
        typer.echo(f"mail:     {mail}  (reply handle - pass as --from-name, or omit to self-stamp)")
    if mail_unread:
        # Distinct label (not a second `mail:` line): the `mail:` line is the
        # reply handle to copy as --from-name, and this render is injected into
        # SessionStart context - a collidable prefix invites copying the count.
        typer.echo(f"mail_unread: {mail_unread}")
    typer.echo(f"provider: {state.provider}")
    # x-301a: opportunistic mesh-name pointer. `fno whoami` reports operating
    # CONTEXT and does not otherwise surface the registered mesh name; when this
    # process IS a mesh worker (the spawn path injected FNO_AGENT_SELF), echo it
    # as one extra line so a worker that ran the reflexive `fno whoami` sees its
    # own handle. Env-gated, so a human / non-mesh session is byte-for-byte
    # unchanged. The focused, complete answer remains `fno agents whoami`.
    if agent_self:
        typer.echo(f"agent:    {agent_self} (mesh)")
    _emit_warnings(state)


# --- status --------------------------------------------------------------


def _tail_events(path: Path, max_bytes: int = EVENTS_TAIL_BYTES,
                 max_lines: int = EVENTS_TAIL_LINES) -> List[Dict[str, Any]]:
    """Seek from end, read last max_bytes, parse complete JSONL lines."""
    if not path.exists() or path.stat().st_size == 0:
        return []
    size = path.stat().st_size
    with path.open("rb") as f:
        f.seek(max(0, size - max_bytes))
        data = f.read()
    text = data.decode("utf-8", errors="replace")
    lines = text.split("\n")
    # If we truncated mid-line at the start, drop the partial first line.
    if size > max_bytes:
        lines = lines[1:]
    parsed: List[Dict[str, Any]] = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
            if isinstance(obj, dict):
                parsed.append(obj)
        except json.JSONDecodeError:
            continue  # mid-append trailing line - ignore
    return parsed[-max_lines:]


def _gate_keys(raw: Dict[str, Any]) -> List[str]:
    suffixes = ("_passed", "_validated", "_shipped", "_generated", "_updated")
    return sorted(k for k in raw if any(k.endswith(s) for s in suffixes))


def _detect_inconsistencies(raw: Dict[str, Any]) -> List[str]:
    """Flag known state-pair mismatches."""
    findings: List[str] = []
    pr_number = raw.get("pr_number")
    ext = raw.get("external_review_passed")
    if pr_number not in (None, "null") and ext is False:
        findings.append(
            f"pr_number={pr_number} but external_review_passed: false -> /pr check probably owed"
        )
    if raw.get("artifact_shipped") is True and raw.get("pr_number") in (None, "null"):
        findings.append(
            "artifact_shipped: true but pr_number is null -> /pr create likely incomplete"
        )
    return findings


def status_command(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False, "--json", "-J", help="emit JSON to stdout"
    ),
    state_file: Optional[Path] = typer.Option(
        None, "--state-file", help="override session state-file detection"
    ),
    no_walker: bool = typer.Option(
        False, "--no-walker", help="suppress walker layer in output"
    ),
    no_fleet: bool = typer.Option(
        False, "--no-fleet", help="suppress fleet layer in output"
    ),
) -> None:
    """Show gate satisfaction, bounded events tail, inconsistencies."""
    opts = AgentOptions(
        json_output=json_output or _global_json(ctx),
        state_file=state_file,
        no_walker=no_walker,
        no_fleet=no_fleet,
    )
    state = _drop_layers(_load_or_exit(opts), opts)
    events_path = state.project_root / ".fno" / "events.jsonl"
    try:
        tail = _tail_events(events_path)
        events_warning = None
    except OSError as exc:
        tail = []
        events_warning = f"events.jsonl unreadable: {exc}"
    inconsistencies = (
        _detect_inconsistencies(state.session.raw) if state.session else []
    )
    if opts.json_output:
        payload = _ctx_to_jsonable(state)
        payload["events_tail"] = tail
        payload["inconsistencies"] = inconsistencies
        if events_warning:
            payload["events_warning"] = events_warning
        typer.echo(json.dumps(payload, indent=2, sort_keys=True))
        _emit_warnings(state)
        if events_warning:
            typer.echo(f"warn: {events_warning}", err=True)
        return
    typer.echo(f"project:  {state.project_root}")
    if state.session:
        # phase: n/a (collapsed) when current_phase absent (immutable manifest post-wedge)
        raw = state.session.raw
        if "current_phase" in raw or "phase" in raw:
            phase_str = state.session.phase or "(unknown)"
        else:
            phase_str = "n/a (collapsed)"
        # status: derive from latest termination event when absent from manifest
        if "status" in raw:
            status_str = state.session.status or "?"
        else:
            status_str = _derive_status_from_events(state.project_root, raw.get("session_id"))
        sid = state.session.session_id or "(unknown)"
        typer.echo(f"session:  {sid} phase={phase_str} status={status_str}")
        gate_lines = []
        for key in _gate_keys(state.session.raw):
            value = state.session.raw[key]
            mark = "y" if value is True else ("n" if value is False else "?")
            gate_lines.append(f"  [{mark}] {key}: {value}")
        if gate_lines:
            typer.echo("gates:")
            for line in gate_lines:
                typer.echo(line)
    if state.walker:
        typer.echo(
            f"walker:   {state.walker.session_id or '(unknown)'} "
            f"in_flight={state.walker.in_flight} done={state.walker.done}"
        )
    if state.fleet:
        typer.echo(
            f"fleet:    {state.fleet.mission_id} status={state.fleet.status} "
            f"wave={state.fleet.wave_current}/{state.fleet.wave_total}"
        )
    if tail:
        typer.echo(f"events (last {len(tail)}):")
        for ev in tail:
            kind = ev.get("type") or ev.get("kind") or ev.get("event") or "?"
            ts = ev.get("ts") or ev.get("timestamp") or ""
            typer.echo(f"  - {ts} {kind}")
    if inconsistencies:
        typer.echo("inconsistencies:")
        for line in inconsistencies:
            typer.echo(f"  WARNING: {line}")
    if events_warning:
        typer.echo(f"warn: {events_warning}", err=True)
    _emit_warnings(state)
