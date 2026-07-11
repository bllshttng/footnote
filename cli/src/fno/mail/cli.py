"""fno mail: durable polled mailbox CLI (ab-cee91152).

One namespace over the jsonl-canon bus log. Publish appends a durable envelope;
consume is a per-recipient cursor scan over the log; the per-recipient markdown
thread is a derived render (see ``fno.inbox.store`` for the data model).
This replaces the retired ``inbox`` namespace and the messaging half of
``agents`` (whose send/inbox/ack verbs moved here).

Commands:
    send           - publish a message to a peer or project (durable-first)
    unread         - list bus messages addressed to me past my cursor
    ack            - advance my read cursor
    reply          - answer a message by id; name-lane -> back to its sender
    list           - list threads in own render (default: unread only)
    triage         - run LLM triage on a heads-up thread
    drain          - drain unread threads (per-kind dispatch)
    status         - one-screen health snapshot for own mailbox
    view           - render the jsonl bus as an inbox projection
    lint           - check thread render files for malformed shape
    rebuild-render - regenerate a recipient's render from the bus log

Exit codes:
    0  success
    1  user error (invalid input, deprecated kind, typo in recipient)
    2  runtime error
"""
from __future__ import annotations

import dataclasses
import json
import os
import re
import sys
from datetime import datetime, timedelta, timezone
from enum import Enum
from pathlib import Path
from typing import Optional, TypedDict

import typer

from fno.inbox.store import (
    DEPRECATED_KINDS,
    ProjectIdentificationError,
    ThreadHandle,
    VALID_KINDS,
    append_to_thread,
    find_thread_by_msg_id,
    inbox_dir_for,
    log_inbox_error,
    read_all_threads,
    read_thread,
    resolve_project,
    write_new_thread,
)
from fno import paths


class DaemonState(str, Enum):
    """Result of probing launchctl for the per-project watch daemon."""

    LOADED = "loaded"
    NOT_INSTALLED = "not_installed"
    UNKNOWN_TIMEOUT = "unknown:timeout"


class StatusSnapshot(TypedDict):
    """Public --json contract returned by `fno mail status`.

    Eight keys; field names are part of the CLI surface that downstream
    tooling reads by name, so additions/removals are breaking changes.
    """

    daemon: str
    inbox_path: str
    unread: int
    acked_24h: int
    last_drain: str
    active_session: str
    wake_signals: int
    errors_24h: int


mail_app = typer.Typer(
    help="Durable polled mailbox: send/unread/ack/reply/list/drain/status/view.",
    no_args_is_help=True,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_OLD_PATH_WARNED = False


def _maybe_warn_old_path() -> None:
    global _OLD_PATH_WARNED
    if _OLD_PATH_WARNED:
        return
    # The pre-2026-05 flat layout only ever existed under an Obsidian vault
    # (``<vault>/agents/inbox``). A neutral, vault-less install never had it,
    # so there is nothing to warn about there.
    vault = paths.vault_root()
    if vault is None:
        return
    old = vault / "agents" / "inbox"
    if old.exists():
        # migrate-inbox-path.sh now takes explicit roots (no hardcoded vault),
        # so spell out the full command rather than the bare script name, which
        # would exit immediately on the required-env guard.
        new = paths.inbox_agents_root()
        print(
            f"warning: old inbox path {old} exists. Run:\n"
            f"  FNO_INBOX_OLD_ROOT={old} FNO_INBOX_NEW_ROOT={new} "
            f"scripts/migrate-inbox-path.sh",
            file=sys.stderr,
        )
        _OLD_PATH_WARNED = True


def _project_root() -> Path:
    """Per-project base under the inbox root, e.g. ``.../{project}/``."""
    override = os.environ.get("FNO_INBOX_ROOT")
    if override:
        return Path(override)
    _maybe_warn_old_path()
    return paths.inbox_agents_root()


def _legacy_inbox_md(project: str) -> Path:
    """Path to the pre-migration ``inbox.md`` file (for status/lint hints)."""
    return _project_root() / project / "inbox.md"


def _resolve_from(from_project: Optional[str]) -> str:
    try:
        return resolve_project(override=from_project)
    except ProjectIdentificationError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(code=1)


def _read_body(body: Optional[str], body_file: Optional[Path]) -> str:
    if body is not None and body_file is not None:
        typer.echo("error: provide --body or --body-file, not both", err=True)
        raise typer.Exit(code=1)
    if body_file is not None:
        return body_file.read_text(encoding="utf-8")
    if body is not None:
        return body
    typer.echo("error: provide --body or --body-file", err=True)
    raise typer.Exit(code=1)


def _validate_kind(kind: str) -> str:
    """Validate a CLI ``--kind`` value. Hint at replacement for deprecated kinds."""
    if kind in VALID_KINDS:
        return kind
    if kind in DEPRECATED_KINDS:
        replacement = DEPRECATED_KINDS[kind]
        valid = ", ".join(sorted(VALID_KINDS))
        typer.echo(
            f"error: kind {kind!r} was removed in the 2026-05 inbox redesign. "
            f"Use --kind {replacement} instead. Valid kinds: {valid}",
            err=True,
        )
        raise typer.Exit(code=1)
    valid = ", ".join(sorted(VALID_KINDS))
    typer.echo(
        f"error: unknown kind {kind!r}. Valid kinds: {valid}",
        err=True,
    )
    raise typer.Exit(code=1)


# ---------------------------------------------------------------------------
# Notification helpers
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Status helpers
# ---------------------------------------------------------------------------

def _daemon_loaded(project: str) -> DaemonState:
    import subprocess

    try:
        res = subprocess.run(
            ["launchctl", "list", f"com.fno.watch.{project}"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except subprocess.TimeoutExpired:
        print(
            f"warning: launchctl list timed out after 5s for project={project!r}",
            file=sys.stderr,
        )
        return DaemonState.UNKNOWN_TIMEOUT
    except FileNotFoundError:
        return DaemonState.NOT_INSTALLED
    return DaemonState.LOADED if res.returncode == 0 else DaemonState.NOT_INSTALLED


_LOG_TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z) drain complete")


def _last_drain_relative(log_path: Path) -> str:
    if not log_path.exists():
        return "never"
    last_ts: Optional[datetime] = None
    try:
        with log_path.open("r", encoding="utf-8") as f:
            for line in f:
                m = _LOG_TS_RE.match(line)
                if m:
                    try:
                        last_ts = datetime.strptime(
                            m.group(1), "%Y-%m-%dT%H:%M:%SZ"
                        ).replace(tzinfo=timezone.utc)
                    except ValueError:
                        continue
    except OSError as exc:
        print(f"warning: cannot read {log_path}: {exc}", file=sys.stderr)
        return "never"
    if last_ts is None:
        return "never"
    return _humanize_age(datetime.now(tz=timezone.utc) - last_ts)


def _humanize_age(delta: timedelta) -> str:
    total = int(delta.total_seconds())
    if total < 0:
        total = 0
    if total < 60:
        return f"{total}s ago"
    if total < 3600:
        return f"{total // 60}m ago"
    if total < 86400:
        return f"{total // 3600}h ago"
    return f"{total // 86400}d ago"


def _count_acked_24h(threads: list[ThreadHandle]) -> int:
    cutoff = datetime.now(tz=timezone.utc).timestamp() - 86400
    n = 0
    for h in threads:
        if h.read_at is None:
            continue
        if h.read_at.timestamp() >= cutoff:
            n += 1
    return n


def _count_errors_24h(repo_root: Path) -> int:
    from fno.paths import project_log

    errors_path = project_log("inbox-errors.jsonl", project_root=repo_root)
    if not errors_path.exists():
        return 0
    cutoff = datetime.now(tz=timezone.utc).timestamp() - 86400
    n = 0
    try:
        with errors_path.open("r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                ts_str = entry.get("ts")
                if not ts_str:
                    continue
                try:
                    ts = datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
                except ValueError:
                    continue
                if ts.timestamp() >= cutoff:
                    n += 1
    except OSError as exc:
        print(f"warning: cannot read {errors_path}: {exc}", file=sys.stderr)
        return 0
    return n


def _count_wake_signals(repo_root: Path) -> int:
    wake_dir = repo_root / ".fno" / "wake-signals"
    if not wake_dir.is_dir():
        return 0
    return sum(1 for p in wake_dir.glob("wake-*.json") if p.is_file())


def _active_session(repo_root: Path) -> str:
    try:
        from fno.wake.detect import detect_session_state

        return detect_session_state(repo_root).value
    except Exception as exc:  # noqa: BLE001
        print(
            f"warning: detect_session_state failed for {repo_root}: "
            f"{type(exc).__name__}: {exc}",
            file=sys.stderr,
        )
        return "unknown"


def _collect_status(project: str, repo_root: Path) -> StatusSnapshot:
    threads = read_all_threads(project)
    unread = sum(1 for h in threads if h.is_unread)
    inbox = inbox_dir_for(project)

    log_path = repo_root / ".fno" / "abi-watch.log"
    return StatusSnapshot(
        daemon=_daemon_loaded(project).value,
        inbox_path=str(inbox),
        unread=unread,
        acked_24h=_count_acked_24h(threads),
        last_drain=_last_drain_relative(log_path),
        active_session=_active_session(repo_root),
        wake_signals=_count_wake_signals(repo_root),
        errors_24h=_count_errors_24h(repo_root),
    )


# ---------------------------------------------------------------------------
# Refs collection
# ---------------------------------------------------------------------------

def _collect_refs(
    ref_pr: Optional[int],
    ref_node: Optional[str],
    ref_gate: Optional[str],
    ref_mission: Optional[str],
    source_mission: Optional[str],
    cascade_of: Optional[str],
) -> dict[str, str]:
    refs: dict[str, str] = {}
    if ref_pr is not None:
        refs["ref_pr"] = str(ref_pr)
    if ref_node is not None:
        refs["ref_node"] = ref_node
    if ref_gate is not None:
        refs["ref_gate"] = ref_gate
    if ref_mission is not None:
        refs["mission_id"] = ref_mission
    if source_mission is not None:
        refs["source_mission"] = source_mission
    if cascade_of is not None:
        refs["cascade_of"] = cascade_of
    return refs


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------

@mail_app.command("reply")
def cmd_reply(
    to_msg: str = typer.Option(..., "--to", help="msg-id to reply to"),
    kind: str = typer.Option("fyi", "--kind", help="Reply kind (default: fyi)"),
    body: Optional[str] = typer.Option(None, "--body", help="Reply body"),
    body_file: Optional[Path] = typer.Option(None, "--body-file", help="Read body from file"),
    ref_mission: Optional[str] = typer.Option(None, "--ref-mission", help="Mission id (megatron)"),
    source_mission: Optional[str] = typer.Option(None, "--source-mission", help="Originating mission for cascades"),
    cascade_of: Optional[str] = typer.Option(None, "--cascade-of", help="Originating msg-id for cascades"),
    from_project: Optional[str] = typer.Option(None, "--from", help="Sender project (overrides settings.yaml)"),
    json_out: bool = typer.Option(False, "--json", "-J", help="Print {msg_id, thread_path} as JSON"),
) -> None:
    """Reply to a message, routed by the answered message's lane.

    Looks ``to_msg`` up on the durable bus. A name-lane message (``to_kind ==
    "name"``) is answered by sending back to its original sender -- no re-typed
    handle -- with the correlation threaded via ``in_reply_to`` (and the wire
    ``reply_to`` attr). Any other target falls through to the thread-store reply
    (append to the existing thread, or a ``replies_to``-linked new thread). A
    ``to_msg`` absent from the bus is a hard error.
    """
    kind = _validate_kind(kind)
    body_text = _read_body(body, body_file)

    # Name-lane routing (x-8045): look the --to msg-id up on the durable bus and
    # branch on its addressing. A name-lane message is answered by sending back to
    # its original sender (no re-typed handle) with the correlation threaded via
    # in_reply_to. Anything else falls through to the thread-store reply below.
    from fno.bus.log import iter_messages

    orig = next((m for m in iter_messages() if m.id == to_msg), None)
    if orig is not None and orig.to_kind == "name":
        from fno.agents import discover as discover_mod

        # from_name defaults to None so stamp_from auto-stamps THIS session's
        # canonical handle (claude-/codex-<short>) -- the handle the original
        # sender replies back to and that drain-self scans, NOT a project name.
        resolved, _ = discover_mod.resolve_or_suggest(orig.from_)
        if resolved is not None:
            _name_lane_send(
                body_text, from_name=from_project, resolved=resolved, reply_to=to_msg
            )
        else:
            # AC1-FR: the original sender is no longer live -> durable floor
            # addressed to their canonical handle (orig.from_), still drainable.
            prov = orig.from_.split("-", 1)[0] if "-" in orig.from_ else None
            # Keep the wire `to` attr the 8-hex short even if from_ carries a full
            # uuid (`claude-<uuid>`): take the first dash-segment after the harness.
            short = (
                orig.from_.split("-", 1)[1].split("-")[0]
                if "-" in orig.from_
                else orig.from_
            )
            _name_lane_send(
                body_text,
                from_name=from_project,
                resolved=None,
                recipient=orig.from_,
                provider=prov,
                to_short=short,
                reply_to=to_msg,
            )
        return
    if orig is None:
        # AC1-ERR / LD4: the name lane cannot invent a target from nothing. Every
        # real message is durable-first (on the bus), so an id absent from the bus
        # is genuinely unknown -- hard error, never a silent self-note.
        print(f"msg-id {to_msg!r} not in the bus log", file=sys.stderr)
        raise typer.Exit(code=1)

    # Thread-store reply path (non-name-lane): resolve the sender project here so
    # the name-lane path above is never forced through project identification.
    sender = _resolve_from(from_project)

    own_handle = find_thread_by_msg_id(sender, to_msg)
    if own_handle is None:
        typer.echo(
            f"warning: msg-id {to_msg!r} not found in own inbox; "
            f"writing a self-note thread (likely orphan reply)",
            err=True,
        )
        recipient = sender
    else:
        recipient = own_handle.from_project

    refs = _collect_refs(None, None, None, ref_mission, source_mission, cascade_of)

    existing = find_thread_by_msg_id(recipient, to_msg)
    if existing is not None:
        new_id = append_to_thread(existing.path, sender, body_text)
        payload = {"msg_id": new_id, "thread_path": str(existing.path), "appended": True}
        if json_out:
            typer.echo(json.dumps(payload))
        else:
            typer.echo(f"appended reply {new_id} to {existing.path.name} (in {recipient})")
        return

    handle = write_new_thread(
        recipient, sender, kind, body_text,
        replies_to=to_msg, refs=refs,
    )
    payload = {
        "msg_id": handle.thread_id,
        "thread_path": str(handle.path),
        "appended": False,
        "orphan": True,
    }
    if json_out:
        typer.echo(json.dumps(payload))
    else:
        typer.echo(
            f"sent orphan reply {handle.thread_id} to {recipient} "
            f"({handle.path.name}; replies_to:{to_msg})"
        )


@mail_app.command("list")
def cmd_list(
    all_msgs: bool = typer.Option(False, "--all", "-A", help="Show all threads (default: unread only)"),
    json_out: bool = typer.Option(False, "--json", "-J", help="Output as JSON"),
    from_project: Optional[str] = typer.Option(None, "--from", help="Project to read inbox for"),
) -> None:
    """List threads in own inbox (default: unread only)."""
    project = _resolve_from(from_project)
    threads = read_all_threads(project)
    if not all_msgs:
        threads = [h for h in threads if h.is_unread]

    if json_out:
        typer.echo(json.dumps([_thread_to_dict(h) for h in threads]))
        return

    if not threads:
        label = "threads" if all_msgs else "unread threads"
        typer.echo(f"no {label}")
        return

    for h in threads:
        _print_thread_summary(h)


@mail_app.command("triage")
def cmd_triage(
    msg_id: str = typer.Argument(..., help="Any msg-id contained in the thread to triage"),
    json_out: bool = typer.Option(False, "--json", "-J", help="Output plan as JSON"),
    from_project: Optional[str] = typer.Option(None, "--from", help="Project (overrides settings.yaml)"),
) -> None:
    """Run LLM triage on a heads-up thread; output a JSON action plan."""
    from fno.inbox.triage import (
        TriageFailedError,
        read_triage_settings,
        triage_thread,
    )

    project = _resolve_from(from_project)
    handle = find_thread_by_msg_id(project, msg_id)
    if handle is None:
        typer.echo(f"error: msg-id not found in {project!r} inbox: {msg_id}", err=True)
        raise typer.Exit(code=1)

    settings = read_triage_settings()
    try:
        plan = triage_thread(handle, settings=settings, project_override=project)
    except TriageFailedError:
        typer.echo("error: triage failed twice; see .fno/inbox-errors.jsonl", err=True)
        raise typer.Exit(code=2)

    plan_dict = dataclasses.asdict(plan)
    typer.echo(json.dumps(plan_dict))


@mail_app.command("drain")
def cmd_drain(
    json_out: bool = typer.Option(False, "--json", "-J", help="Output DrainResults as JSON"),
    max_messages: int = typer.Option(10, "--max", help="Cap on threads drained per call"),
    from_project: Optional[str] = typer.Option(None, "--from", help="Project (overrides settings.yaml)"),
) -> None:
    """Drain unread threads. Per-kind dispatch:
    heads-up -> triage + create graph node; question -> drop wake-signal;
    fyi -> dismiss or write a memory file (when persist_to_memory)."""
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import _git_root

    project = _resolve_from(from_project)
    repo_root = _git_root()

    results = drain_inbox(repo_root, project, max_threads=max_messages)

    if json_out:
        typer.echo(json.dumps([dataclasses.asdict(r) for r in results]))
        return
    for r in results:
        typer.echo(f"{r.thread_id}  kind:{r.kind}  action:{r.action}")


@mail_app.command("migrate-bus")
def cmd_migrate_bus(
    json_out: bool = typer.Option(False, "--json", "-J", help="Output as JSON"),
) -> None:
    """Backfill pre-bus markdown threads into the canonical bus log.

    Group 3 cutover (US8 AC8-EDGE): markdown threads written before the bus log
    existed live only on disk. This imports any message not already in the log
    so a cursor scan / agent inbox never strands unread legacy mail. Idempotent
    (dedup by message-id); safe to re-run.
    """
    from fno.inbox.store import migrate_md_threads_to_bus

    res = migrate_md_threads_to_bus()
    if json_out:
        typer.echo(json.dumps({
            "migrated": res.migrated,
            "threads_scanned": res.threads_scanned,
            "recipients": res.recipients,
        }))
        return
    typer.echo(
        f"migrated {res.migrated} message(s) from {res.threads_scanned} thread(s) "
        f"across {len(res.recipients)} recipient(s)"
    )


def _envelope_to_dict(env) -> dict:
    """Project a bus envelope to a JSON-able dict.

    Enriched address fields are included only when present, so the projection
    stays clean and is forward-compatible: unknown future fields on a line are
    simply not surfaced (LD11 additive read), never echoed or crashed on.
    """
    out = {
        "id": env.id, "ts": env.ts, "thread": env.thread,
        "from": env.from_, "to": env.to, "kind": env.kind, "body": env.body,
    }
    for key, val in (
        ("provider_from", env.provider_from), ("provider_to", env.provider_to),
        ("from_session", env.from_session), ("from_model", env.from_model),
        ("to_kind", env.to_kind), ("in_reply_to", env.in_reply_to),
    ):
        if val:
            out[key] = val
    return out


def _names_in_project(project: str) -> set[str]:
    """Registry names whose cwd resolves to ``project`` (best-effort scoping)."""
    try:
        from fno.agents.registry import load_registry
        from fno.agents.discover import resolve_project_for_cwd
        return {
            e.name for e in load_registry()
            if resolve_project_for_cwd(e.cwd) == project
        }
    except Exception:  # noqa: BLE001 - scoping is best-effort; fall back to project name
        return set()


@mail_app.command("view")
def cmd_view(
    all_projects: bool = typer.Option(
        False, "--all", "-A", help="Operator view: messages across all projects"
    ),
    limit: int = typer.Option(50, "--limit", "-n", help="Show the most recent N messages"),
    json_out: bool = typer.Option(False, "--json", "-J", help="Output as JSON"),
    from_project: Optional[str] = typer.Option(
        None, "--from", help="Project to view (overrides settings.yaml)"
    ),
) -> None:
    """Render the JSONL bus (the source of record) as an inbox view.

    The bus log is the source of truth; this is a read-only projection. Default
    scope is this project's traffic (to/from the project or an agent in it) so a
    cross-project body is not leaked; ``--all`` is the explicit operator view.
    """
    from fno.bus.log import iter_messages

    project = None if all_projects else _resolve_from(from_project)
    msgs = list(iter_messages())
    if project is not None:
        names = _names_in_project(project) | {project}
        msgs = [m for m in msgs if m.to in names or m.from_ in names]
    if limit and limit > 0:
        msgs = msgs[-limit:]

    if json_out:
        typer.echo(json.dumps([_envelope_to_dict(m) for m in msgs]))
        return
    if not msgs:
        typer.echo("no messages" if all_projects else f"no messages for {project}")
        return
    for m in msgs:
        who_from = m.from_ + (f"/{m.from_model}" if m.from_model else "")
        kindtag = f" [{m.to_kind}]" if m.to_kind else ""
        body1 = (m.body or "").strip().replace("\n", " ")
        if len(body1) > 80:
            body1 = body1[:77] + "..."
        typer.echo(f"{m.ts}  {who_from} -> {m.to}{kindtag} ({m.kind}): {body1}")


@mail_app.command("status")
def cmd_status(
    json_out: bool = typer.Option(False, "--json", "-J", help="Output as JSON"),
    from_project: Optional[str] = typer.Option(None, "--from", help="Project (overrides settings.yaml)"),
) -> None:
    """One-screen health snapshot for the current project's inbox."""
    from fno.inbox.store import _git_root

    project = _resolve_from(from_project)
    repo_root = _git_root()

    try:
        snapshot = _collect_status(project, repo_root)
    except OSError as exc:
        typer.echo(
            f"error: cannot read inbox state for project {project!r}: "
            f"{type(exc).__name__}: {exc}",
            err=True,
        )
        raise typer.Exit(code=2)

    if json_out:
        typer.echo(json.dumps(dict(snapshot)))
        return

    typer.echo(f"project: {project}")
    typer.echo(f"daemon: {snapshot['daemon']}")
    typer.echo(f"inbox path: {snapshot['inbox_path']}")
    typer.echo(f"unread: {snapshot['unread']}")
    typer.echo(f"acked_24h: {snapshot['acked_24h']}")
    typer.echo(f"last drain: {snapshot['last_drain']}")
    typer.echo(f"active session: {snapshot['active_session']}")
    typer.echo(f"wake signals: {snapshot['wake_signals']}")
    typer.echo(f"errors_24h: {snapshot['errors_24h']}")


@mail_app.command("lint")
def cmd_lint(
    project: Optional[str] = typer.Argument(None, help="Project to lint (default: own)"),
) -> None:
    """Check thread files for malformed shape."""
    if project is None:
        try:
            project = resolve_project()
        except ProjectIdentificationError as exc:
            typer.echo(f"error: {exc}", err=True)
            raise typer.Exit(code=1)

    inbox = inbox_dir_for(project)
    legacy = _legacy_inbox_md(project)

    if not inbox.exists():
        if legacy.exists():
            typer.echo(
                f"warning: {project!r} still on the pre-2026-05 flat layout "
                f"({legacy}). Run scripts/migrate-inbox-flat-to-threads.py.",
            )
            raise typer.Exit(code=1)
        typer.echo(f"no inbox/ for {project}")
        return

    bad: list[Path] = []
    good = 0
    for p in sorted(inbox.glob("*.md")):
        h = read_thread(p)
        if h is None:
            bad.append(p)
            log_inbox_error("thread parse failure", path=str(p), project=project)
        else:
            good += 1

    if bad:
        typer.echo(f"lint: {project} found {len(bad)} malformed thread file(s):")
        for p in bad:
            typer.echo(f"  {p}")
        raise typer.Exit(code=1)

    typer.echo(f"lint: {project} OK ({good} thread(s))")


# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------

def _thread_to_dict(h: ThreadHandle) -> dict:
    return {
        "thread_id": h.thread_id,
        "path": str(h.path),
        "from": h.from_project,
        "to": h.to_project,
        "kind": h.kind,
        "created": h.created.isoformat(),
        "read_at": h.read_at.isoformat() if h.read_at else None,
        "replies_to": h.replies_to,
        "persist_to_memory": h.persist_to_memory,
        "refs": h.refs,
        "messages": [
            {
                "msg_id": m.msg_id,
                "timestamp": m.timestamp.isoformat(),
                "from": m.from_project,
                "body": m.body,
            }
            for m in h.messages
        ],
    }


def _print_thread_summary(h: ThreadHandle) -> None:
    created = h.created.strftime("%Y-%m-%d %H:%M")
    read_marker = "" if h.is_unread else " [read]"
    typer.echo(
        f"{h.thread_id}  {created}  from:{h.from_project}  kind:{h.kind}"
        f"  msgs:{len(h.messages)}{read_marker}"
    )
    if h.messages:
        first_line = h.messages[0].body.split("\n")[0].strip()
        if first_line:
            typer.echo(f"  {first_line[:80]}")
    typer.echo(f"  {h.path}")


# ---------------------------------------------------------------------------
# Publish + cursor-consume (relocated from `fno agents`, ab-cee91152 Move B)
# ---------------------------------------------------------------------------
# `fno mail send` is the durable-first publish (the envelope lands on the bus
# log before any live delivery is attempted). `fno mail unread`/`ack` are the
# cursor-based consume over that log: unread lists messages addressed to me
# after my cursor; ack advances it. These supersede the retired agents
# send/inbox/ack and inbox unread/ack verbs (the one messaging namespace).


def _name_lane_send(
    message: str,
    *,
    from_name: Optional[str],
    resolved,
    recipient: Optional[str] = None,
    provider: Optional[str] = None,
    to_short: Optional[str] = None,
    reply_to: Optional[str] = None,
) -> None:
    """Name-lane delivery core, shared by ``mail send <name>`` and a name-lane
    ``mail reply``. When ``resolved`` (a live ``DiscoveredSession``) is set:
    live-inject-first, durable floor on miss, addressed to its canonical handle.
    When ``resolved`` is None: durable-only, addressed to ``recipient`` (a reply
    to an offline sender). ``reply_to`` stamps BOTH the wire ``reply_to`` attr and
    the bus ``in_reply_to`` from ONE msg-id -- never one set, the other null.
    Exits 12 on a durable-floor write failure."""
    from fno.agents.dispatch import _mail_inject_claude, _mail_inject_codex
    from fno.agents.provider_resolve import infer_invoking_harness
    from fno.agents.self_stamp import resolve_self_model, stamp_from
    from fno.harness_identity import canonical_handle
    from fno.inbox.store import write_new_thread
    from fno.mail.envelope import wrap_fno_mail

    if resolved is not None:
        recipient = canonical_handle(resolved.agent, resolved.session_id)
        provider = resolved.agent
        to_short = resolved.short_id

    wrapped = wrap_fno_mail(
        message,
        from_=stamp_from(from_name),
        harness=infer_invoking_harness() or "cli",
        model=resolve_self_model(),
        to=to_short,
        reply_to=reply_to,
    )

    injected = False
    if resolved is not None:
        if provider == "claude":
            injected = _mail_inject_claude(resolved.session_id, wrapped)
        elif provider == "codex":
            injected = _mail_inject_codex(resolved.session_id, wrapped)

    live = f" [live {resolved.agent} session {resolved.handle}]" if resolved is not None else ""
    corr = f" re:{reply_to}" if reply_to else ""
    if injected:
        print(f"delivered (hosted) to {recipient}{live}{corr}")
        return

    try:
        th = write_new_thread(
            recipient=recipient,
            sender=stamp_from(from_name),
            kind="send",
            body=wrapped,
            to_kind="name",
            provider_to=provider,
            replies_to=reply_to,
        )
    except (OSError, ValueError, RuntimeError) as exc2:
        print(f"durable envelope write failed for {recipient!r}: {exc2}", file=sys.stderr)
        raise typer.Exit(code=12) from exc2
    print(f"{th.thread_id} queued (durable) for {recipient}{live}{corr}")


@mail_app.command("send")
def cmd_send(
    name: str | None = typer.Argument(
        None, help="Agent name. Omit when using --to-project."
    ),
    message: str | None = typer.Argument(
        None, help="Message to send (async, fire-and-forget)."
    ),
    provider: str | None = typer.Option(
        None, "--provider", "-p", help="claude | codex | gemini (optional; used for mismatch check)."
    ),
    cwd: str | None = typer.Option(
        None, "--cwd", "-c", help="Working directory context."
    ),
    from_name: str | None = typer.Option(
        None, "--from-name",
        help=(
            "Identity advertised in the envelope (must be XML-attribute-safe). "
            "Unset defaults to 'fno' for an agent send, or the working "
            "dir's project for an inbox-kind send."
        ),
    ),
    to_project: str | None = typer.Option(
        None, "--to-project",
        help=(
            "Anycast: deliver to whoever works on this project (live if exactly "
            "one peer, durable queue if none). Use instead of <name>."
        ),
    ),
    any_live: bool = typer.Option(
        False, "--any",
        help="With --to-project, break a multi-live-peer tie (most recent activity wins).",
    ),
    kind: str | None = typer.Option(
        None, "--kind", "-k",
        help=(
            "Inbox kind (heads-up | question | fyi). When set, the message is an "
            "inbox-style durable note the recipient's drain dispatches on; omit "
            "for a default agent-to-agent send (live if a peer is hosted)."
        ),
    ),
    reply_to: str | None = typer.Option(
        None, "--reply-to",
        help="With --kind: msg-id being replied to (appends to the existing thread).",
    ),
    persist: str | None = typer.Option(
        None, "--persist",
        help="With --kind fyi: 'memory' writes a recipient memory file.",
    ),
    body: str | None = typer.Option(
        None, "--body", "-b",
        help="With --kind: message body (alternative to the positional arg).",
    ),
    body_file: Path | None = typer.Option(
        None, "--body-file",
        help="With --kind: read the message body from a file.",
    ),
    ref_pr: int | None = typer.Option(
        None, "--ref-pr", help="With --kind: PR number reference for triage."
    ),
    ref_node: str | None = typer.Option(
        None, "--ref-node", help="With --kind: graph node id reference."
    ),
    ref_gate: str | None = typer.Option(
        None, "--ref-gate", help="With --kind: named gate/milestone reference."
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J",
        help="With --kind: print {msg_id, thread_path, appended} as JSON.",
    ),
) -> None:
    """Send a message asynchronously to a registered agent or a project.

    Name mode (``send <name> <message>``): requires the agent to already exist;
    unknown names exit 16. Project mode (``send --to-project <X> <message>``):
    resolves over the registry - one live peer delivers live, none queues
    durable for project X, many errors with the candidate list unless ``--any``.

    The envelope is written durably BEFORE delivery is attempted so it survives
    every failure path.

    Stdout contract (US3 AC3-UI / US6 AC6-UI): exactly one line, either
    ``msg-<id> delivered (hosted)`` or ``msg-<id> queued (durable)``.
    Exit 0 for both outcomes. Failures surface on stderr with nonzero exit.
    """
    from fno.agents.dispatch import (
        DispatchAskError,
        dispatch_send,
        dispatch_send_to_project,
    )
    from fno.agents.self_stamp import resolve_self_model, stamp_from

    workdir = Path(cwd).resolve() if cwd else Path(os.getcwd())

    # Inbox-kind mode: heads-up / question / fyi are inbox-style durable notes
    # the recipient's drain dispatches on (heads-up -> triage, question ->
    # wake-signal, fyi / fyi+persist). They ALWAYS queue durable - never live
    # PTY delivery - so this is the durable project-note path (--kind <kind>)
    # that keeps the triage/wake/fyi pipeline unchanged.
    if kind is not None:
        from fno.inbox.store import (
            DEPRECATED_KINDS,
            Kind,
            ProjectIdentificationError,
            post_inbox_message,
            resolve_project,
        )

        inbox_kinds = {Kind.HEADS_UP.value, Kind.QUESTION.value, Kind.FYI.value}
        if kind not in inbox_kinds:
            if kind in DEPRECATED_KINDS:
                # Preserve the migration hint for retired
                # kinds (notification -> fyi, lesson -> fyi --persist memory, ...).
                print(
                    f"error: kind {kind!r} was removed in the 2026-05 inbox "
                    f"redesign. Use --kind {DEPRECATED_KINDS[kind]} instead.",
                    file=sys.stderr,
                )
            else:
                print(
                    f"error: --kind must be one of "
                    f"{', '.join(sorted(inbox_kinds))} (got {kind!r})",
                    file=sys.stderr,
                )
            raise typer.Exit(code=2)

        recipient = to_project or name
        # Body: --body-file wins, then --body, then the positional (which
        # parks in `name` under --to-project, or in `message` in name mode).
        if body is not None and body_file is not None:
            print("error: provide --body or --body-file, not both", file=sys.stderr)
            raise typer.Exit(code=2)
        if body_file is not None:
            content: str | None = body_file.read_text(encoding="utf-8")
        elif body is not None:
            content = body
        elif to_project:
            content = message if message is not None else name
        else:
            content = message
        if not recipient or content is None:
            print(
                "usage: fno mail send --to-project <project> --kind <kind> "
                "<message>",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)

        persist_to_memory = False
        if persist is not None:
            if persist != "memory":
                print(
                    f"error: --persist only accepts 'memory' (got {persist!r})",
                    file=sys.stderr,
                )
                raise typer.Exit(code=2)
            persist_to_memory = True

        # Sender identity: an explicit --from-name wins; otherwise resolve the
        # current project from settings (the project-note sender default, so a
        # /think|/blueprint send still advertises its own project as the sender).
        try:
            # An unset --from-name resolves the sender from the working dir's
            # settings; any explicit --from-name (including the literal
            # "fno") wins verbatim - None is the unambiguous "unset"
            # sentinel.
            sender = (
                from_name
                if from_name is not None
                else resolve_project(cwd=workdir)
            )
        except ProjectIdentificationError as exc:
            print(f"error: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc

        refs: dict[str, str] = {}
        if ref_pr is not None:
            refs["ref_pr"] = str(ref_pr)
        if ref_node is not None:
            refs["ref_node"] = ref_node
        if ref_gate is not None:
            refs["ref_gate"] = ref_gate

        try:
            res = post_inbox_message(
                recipient=recipient,
                sender=sender,
                kind=kind,
                body=content,
                persist_to_memory=persist_to_memory,
                reply_to=reply_to,
                refs=refs or None,
            )
        except ValueError as exc:
            print(f"error: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc

        if json_out:
            import json as _json

            print(_json.dumps({
                "msg_id": res.msg_id,
                "thread_path": str(res.thread_path),
                "appended": res.appended,
            }))
        else:
            verb = "appended (durable) to" if res.appended else "queued (durable) for"
            print(f"{res.msg_id} {verb} {recipient} [{kind}]")
        return

    # Project mode: the message is the sole positional, so `send --to-project X
    # "msg"` parks "msg" in the `name` slot - accept it from either slot.
    if to_project:
        content = message if message is not None else name
        if not content:
            print(
                "usage: fno mail send --to-project <project> <message>",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        try:
            result = dispatch_send_to_project(
                to_project,
                content,
                provider=provider,
                cwd=workdir,
                from_name=stamp_from(from_name),
                any_=any_live,
            )
        except DispatchAskError as exc:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc

        if result.delivery == "hosted":
            print(
                f"{result.msg_id} delivered (hosted) to {result.recipient} "
                f"[project {to_project}]"
            )
        elif result.recipient is not None:
            # A live peer was resolved but injection demoted to durable: the
            # envelope is addressed to that peer (its at-least-once copy), NOT
            # the project. Report it as such so the line is not a peer/project
            # mismatch (codex P2) - the resolved peer's own drain picks it up.
            print(
                f"{result.msg_id} queued (durable) for {result.recipient} "
                f"[project {to_project}]"
            )
        else:
            print(f"{result.msg_id} queued (durable) for project {to_project}")
        return

    # Name mode.
    if not name or message is None:
        print(
            "usage: fno mail send <name> <message>  "
            "(or --to-project <project> <message>)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    try:
        result = dispatch_send(
            name=name,
            message=message,
            provider=provider,
            cwd=workdir,
            from_name=stamp_from(from_name),
        )
    except DispatchAskError as exc:
        from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

        # US2 (ab-098967b4): a bare <name> that is not a registered agent may be
        # a discovered live-session handle (friendly alias or hex short-id).
        # Resolve it to a project and ride the existing --to-project durable bus
        # (Locked Decision 2: live-to-live comms is async over the bus, never a
        # live injection). Only the unknown-agent error falls through; a real
        # delivery/provider error re-raises unchanged.
        if exc.exit_code != UNKNOWN_AGENT_EXIT_CODE:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc

        from fno.agents import discover as discover_mod

        resolved, suggestions = discover_mod.resolve_or_suggest(name)

        # x-605c US3: ANY handle-resolved session is delivered TO THAT SESSION,
        # live-inject first with a durable floor addressed to its canonical handle
        # -- that handle is exactly what the recipient's `drain-self` reads, so a
        # resolved send is always drainable by construction. Claude injects over
        # control.sock (`mail-inject`); codex over the app-server daemon (US8). The
        # old claude->project re-route is gone; project anycast stays explicit via
        # --to-project. The body is <fno_mail>-wrapped with a truthful from/model
        # so the recipient can reply by handle (`fno mail send <from>`) for a live
        # message, or `fno mail reply --to <id>` when answering a drained one.
        if resolved is not None:
            # Live-inject-first with a durable floor addressed to the resolved
            # session's canonical handle. Shared with the name-lane reply path.
            _name_lane_send(message, from_name=from_name, resolved=resolved)
            return

        # AC2-ERR: not a registered agent, not a discovered handle. Error with
        # the closest live-session handles, sending nothing.
        hint = ""
        if suggestions:
            hint = f" Closest live sessions: {', '.join(suggestions)}."
        print(
            f"unknown agent or live-session handle: {name!r}.{hint}",
            file=sys.stderr,
        )
        raise typer.Exit(code=exc.exit_code) from exc

    # AC3-UI: distinguish delivered vs queued on stdout.
    if result.delivery == "hosted":
        label = "delivered (hosted)"
    else:
        label = "queued (durable)"
    print(f"{result.msg_id} {label}")


@mail_app.command("unread")
def cmd_unread(
    name: str = typer.Option(
        "fno", "--name", "-n",
        help="Whose inbox to read (registry name or project).",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON regardless of TTY."
    ),
) -> None:
    """Show unread bus messages addressed to <name> (cursor-filtered).

    "My inbox" is a cursor-bounded scan of the one global bus log filtered to
    ``to == name``: only messages after the consumer's cursor are shown,
    regardless of which provider sent them. ``fno mail ack`` advances the
    cursor. JSON when stdout is not a TTY or ``--json`` is passed.
    """
    from fno.bus.cursor import scan_unread

    msgs = scan_unread(name)
    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())
    if json_out or not is_tty:
        payload = [
            {
                "id": m.id, "thread": m.thread, "from": m.from_, "to": m.to,
                "kind": m.kind, "ts": m.ts, "in_reply_to": m.in_reply_to,
                "body": m.body,
            }
            for m in msgs
        ]
        print(json.dumps(payload, ensure_ascii=False))
        return
    if not msgs:
        print(f"inbox empty for {name!r} (no unread bus messages)")
        return
    for m in msgs:
        excerpt = m.body.replace("\n", " ")[:100]
        print(f"{m.id}  {m.from_} -> {m.to}  [{m.kind}]  {excerpt}")
    print('\nto answer one: fno mail reply --to <id> --body "..."')


@mail_app.command("ack")
def cmd_bus_ack(
    msg_id: str = typer.Argument(..., help="Message id to acknowledge up through."),
    name: str = typer.Option(
        "fno", "--name", "-n", help="Whose read cursor to advance."
    ),
) -> None:
    """Advance <name>'s read cursor to <msg_id> (marks everything up to it seen)."""
    from fno.bus.cursor import advance_cursor
    from fno.bus.log import iter_messages

    # The ack target must be a retained message addressed to `name`. Two failure
    # modes this guards (both would silently corrupt the read position because
    # the cursor is a single global-log position and scan_unread returns to==name
    # AFTER it):
    #   - an id not in the log -> scan_unread can't find it -> re-surfaces ALL mail;
    #   - an id addressed to ANOTHER recipient but positioned after my unread ->
    #     advances me past my own earlier unread, hiding it.
    target = next((m for m in iter_messages() if m.id == msg_id), None)
    if target is None:
        print(
            f"unknown message id {msg_id!r}: not found in the retained bus log; "
            f"cursor not advanced (run `fno mail unread --name {name}` to see ids)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if target.to != name:
        print(
            f"message {msg_id!r} is addressed to {target.to!r}, not {name!r}; "
            f"cursor not advanced (ack only your own messages)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    if advance_cursor(name, msg_id):
        print(f"cursor for {name!r} advanced to {msg_id}")
    else:
        # Forward-only: the id is at or before the current cursor (re-ack / older
        # message). Idempotent no-op, not an error - the cursor never rewinds.
        print(f"cursor for {name!r} already at or past {msg_id}; unchanged")


@mail_app.command("drain-self")
def cmd_drain_self(
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON regardless of TTY."
    ),
) -> None:
    """Drain THIS session's own cross-harness inbox and mark it seen (US5).

    The receive side of the a2a relay: a session computes its own handle from
    the ambient harness env markers (``canonical_handle(harness, session-id)``,
    the SAME string a sender resolves and the registry registers under), reads
    its unread bus mail, prints it for injection into the session, then advances
    its own cursor so nothing re-surfaces next wake. Wired into each harness's
    SessionStart hook, this is what makes a codex/gemini session actually
    RECEIVE mail addressed to ``<harness>-<id>`` -- addressability already
    existed, drainage did not.

    Forward-only + inject-before-ack: a crash between print and ack re-surfaces
    the message next SessionStart (a harmless repeat), never a loss. No harness
    identity in env -> silent no-op (nothing to drain), never an error, so the
    hook is safe on any surface.
    """
    from fno.bus.cursor import advance_cursor, scan_unread
    from fno.harness_identity import canonical_handle, resolve_harness_identity

    ident = resolve_harness_identity()
    if not ident.harness or not ident.session_id:
        if json_out:
            print(json.dumps([]))
        return

    handle = canonical_handle(ident.harness, ident.session_id)
    msgs = scan_unread(handle)

    if json_out:
        print(
            json.dumps(
                [
                    {
                        "id": m.id, "from": m.from_, "to": m.to,
                        "kind": m.kind, "ts": m.ts, "body": m.body,
                    }
                    for m in msgs
                ],
                ensure_ascii=False,
            )
        )
    elif msgs:
        print(f"[fno mail] {len(msgs)} message(s) for {handle}:")
        for m in msgs:
            print(f"\n--- from {m.from_} ({m.ts})  id:{m.id} ---")
            print(m.body.rstrip("\n"))
        # This render is what a session sees on receive, so surface the id (which
        # `reply --to` correlates against) and the how-to. Replying is optional --
        # an FYI/broadcast needs none.
        print(
            '\n[fno mail] to answer one: fno mail reply --to <id> --body "..."'
        )

    # Inject-before-ack: advance the cursor to the last drained id only after
    # the bodies are out, so a crash re-surfaces rather than drops.
    if msgs:
        advance_cursor(handle, msgs[-1].id)


@mail_app.command("rebuild-render")
def cmd_rebuild_render(
    recipient: Optional[str] = typer.Argument(
        None, help="Recipient whose render to rebuild (default: own project)."
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Print {recipient, threads} as JSON."
    ),
) -> None:
    """Regenerate a recipient's markdown render from the canonical bus log.

    LD2 (ab-cee91152): the jsonl bus log is the source of truth; the per-recipient
    markdown is a derived, throwaway view. This rebuilds it from the log so a
    deleted or corrupted render is recovered with no message lost. Idempotent.
    """
    from fno.inbox.store import rebuild_render

    target = recipient if recipient is not None else _resolve_from(None)
    n = rebuild_render(target)
    if json_out:
        typer.echo(json.dumps({"recipient": target, "threads": n}))
        return
    typer.echo(f"rebuilt {n} thread render(s) for {target!r} from the bus log")
