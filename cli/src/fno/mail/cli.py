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
import time
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

    Nine keys; field names are part of the CLI surface that downstream
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
    sent_unclaimed: int


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


def _sent_unclaimed_count() -> int:
    """Count of THIS session's sent mail unclaimed past config.inbox.unclaimed_ttl.

    Session-scoped (keyed on my canonical handle), so a no-identity surface
    honestly reports 0 rather than a project-wide figure. Shares the notify-self
    predicate; never raises (a broken read degrades to 0).
    """
    from fno.config import load_settings
    from fno.harness_identity import canonical_handle, resolve_harness_identity

    ident = resolve_harness_identity()
    if not ident.harness or not ident.session_id:
        return 0
    try:
        handle = canonical_handle(ident.session_id)
        n, _ = _sent_unclaimed(handle, load_settings().inbox.unclaimed_ttl)
        return n
    except Exception as exc:  # noqa: BLE001 - status is advisory; never crash on it
        # Advisory-degrade to 0, but leave a breadcrumb (matches _active_session)
        # so a structural break doesn't render `sent unclaimed: 0` forever silently.
        print(
            f"warning: sent-unclaimed count failed: {type(exc).__name__}: {exc}",
            file=sys.stderr,
        )
        return 0


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
        sent_unclaimed=_sent_unclaimed_count(),
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

def _reply_to_name_handle(
    body_text: str, *, from_project: Optional[str], target: str, to_msg: str
) -> None:
    """Send a name-lane reply to ``target`` (a canonical handle): resolve it live
    and inject, else durable-floor to it. Shared by the bus-record reply path and
    the US3 transcript-recovered live-sender path.

    ``from_name=from_project`` stays None by default so stamp_from auto-stamps
    THIS session's canonical bare short-id -- the handle the original sender
    replies back to and that drain-self scans, NOT a project name."""
    from fno.agents import discover as discover_mod

    resolved, _ = discover_mod.resolve_or_suggest(target)
    if resolved is not None:
        _name_lane_send(
            body_text, from_name=from_project, resolved=resolved, reply_to=to_msg
        )
    else:
        # AC1-FR: the original sender is no longer live -> durable floor addressed
        # to their canonical handle, still drainable. No provider: it is only
        # consulted on the live-inject path, which a None `resolved` already skips.
        _name_lane_send(
            body_text,
            from_name=from_project,
            resolved=None,
            recipient=target,
            reply_to=to_msg,
        )


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

    from fno.harness_identity import LEGACY_HANDLE_RE

    orig = next((m for m in iter_messages() if m.id == to_msg), None)
    if orig is not None and orig.to_kind == "name":
        # A stored sender predating the address flip carries the retired
        # `<harness>-<short8>` form. That is a fact about an old RECORD, not a
        # mistake by whoever is replying, and the address it would carry today is
        # a substring - so migrate it and deliver. Refusing here would invent a
        # wall at a knowledge boundary: making a human perform a translation the
        # code can do is how a resumable peer gets treated as voicemail.
        # (Not the harness-parsing this scheme forbids: the harness is discarded,
        # the short-id is what routes, and routing is still a roster lookup.)
        target = orig.from_ or ""
        if LEGACY_HANDLE_RE.match(target):
            migrated = target.split("-", 1)[1][:8]
            print(
                f"note: stored sender {target!r} is a retired address form "
                f"(pre-flip record); replying to {migrated!r}.",
                file=sys.stderr,
            )
            target = migrated

        _reply_to_name_handle(body_text, from_project=from_project, target=target, to_msg=to_msg)
        return
    if orig is None:
        # US3: a live-confirmed delivery writes no durable thread (LD11a), so the
        # id is not on the bus. Before erroring, recover the sender off THIS
        # session's transcript -- where the injected <fno_mail id=...> envelope
        # already carries `from` -- and reply to that handle by identity. A miss
        # (id genuinely absent everywhere) falls through to the hard error below.
        from fno.mail.reply_resolve import resolve_live_sender

        live_sender = resolve_live_sender(to_msg)
        if live_sender:
            _reply_to_name_handle(
                body_text, from_project=from_project, target=live_sender, to_msg=to_msg
            )
            return
        # AC1-ERR / LD4: the name lane cannot invent a target from nothing. An id
        # absent from BOTH the bus and the transcript is genuinely unknown -- hard
        # error, never a silent self-note.
        print(f"msg-id {to_msg!r} not in the bus log or this session's transcript", file=sys.stderr)
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


@mail_app.command("migrate-bus", hidden=True)
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
    typer.echo(f"sent unclaimed: {snapshot['sent_unclaimed']}")


@mail_app.command("lint", hidden=True)
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


def _warn_deferred(target: str, *, project: bool = False) -> None:
    """Fail loud on a dead-letter miss: the envelope hit only the durable floor
    with no live inject path, so the sender learns delivery deferred instead of
    the message vanishing silently until the recipient's next SessionStart drain.

    The durable copy is RECOVERY, not delivery - it waits on a drain the
    recipient may never run. So this names the recovery ladder rather than
    leaving the sender to wait: a session that is merely idle can be brought
    back and re-sent to immediately, which beats waiting on a drain every time.

    It leads with `peek`, not `resume`, because the fallback fires on an
    UNCONFIRMED live inject, not a proven failure: a busy recipient can record
    the injected turn past the confirm budget and receive it anyway, so a blind
    re-send is the documented double-delivery edge rather than a fix.

    Warning only - the durable enqueue succeeded, so exit stays 0."""
    if project:
        msg = (
            f"mail: project inbox {target} has no live drain; queued durably - "
            "delivery waits for a drain\n"
            "  this is NOT delivery. Address a live session instead: "
            "`fno agents top` to find one, then `fno mail send <short-id>`"
        )
    else:
        msg = (
            f"mail: {target} has no live pane; queued durably - "
            "delivery waits for its next SessionStart drain\n"
            "  live delivery NOT confirmed - do not wait for a reply, recover:\n"
            f"    fno agents peek {target}     # did it land? a busy peer may have queued it\n"
            f"    fno agents resume {target}   # idle session -> live, then re-send\n"
            f"    fno agents attach {target}   # drive it yourself (claude)"
        )
    print(msg, file=sys.stderr)


class AmbiguousTokenError(Exception):
    """A token matched two stored sessions. Never guess which one to wake."""

    def __init__(self, candidates: list[str]) -> None:
        super().__init__("ambiguous session token")
        self.candidates = candidates


class UnreachableTokenError(Exception):
    """Every rung missed AND no durable store knows the token.

    Distinct from a failed delivery: a failed delivery still has a real
    recipient and earns a durable copy, while this is a token that names
    nothing at all -- almost always a typo. Queuing for it would strand an
    envelope nobody will ever drain, so it exits 16 having sent nothing.
    """


def _is_self_send(recipient: Optional[str]) -> bool:
    """True when the sender is addressing its own session."""
    own = os.environ.get("CLAUDE_CODE_SESSION_ID") or ""
    if not own or not recipient:
        return False
    from fno.harness_identity import canonical_handle

    return canonical_handle(own) == recipient


def _resolve_token(token: str):
    """Resolve ``token`` to a reachable session BEFORE the envelope is addressed.

    Resolution has to precede wrapping. The durable recipient is the resolved
    session's canonical handle, and deriving it from the raw token instead would
    misaddress every alias: ``canonical_handle`` takes the first 8 characters, so
    a friendly ``footnote-9a063cd3`` becomes the recipient ``footnote`` and the
    real session never drains its own mail.

    Returns ``(reachable_or_None, lane_note_or_None)``. A ``None`` reachable with
    no note means every store was read cleanly and knows nothing -- the only case
    that still earns exit 16.
    """
    from fno.agents import discover as discover_mod

    try:
        reachable, ambiguous = discover_mod.resolve_reachable(token)
    except discover_mod.StoreReadError as exc:
        # Unreadable is not proof of absence, and exit 16 queues nothing. Keep
        # the mail: demote durably, addressed to the lone candidate when the
        # resolver found one, and name the stores that could not be read.
        return exc.resolved, f"wake=stores-unreadable({','.join(exc.failed)})"
    if ambiguous:
        raise AmbiguousTokenError(ambiguous)
    return reachable, None


def _wake_rung(reachable, wrapped: str) -> tuple[bool, Optional[str], Optional[str]]:
    """Wake an already-resolved asleep session.

    Returns ``(delivered, revived_short_id, lane_note)``.
    """
    from fno.agents.dispatch import _mail_inject_claude, wake_and_deliver

    if reachable.agent != "claude":
        # Wake is claude-only: the revive substrate resumes a claude session, so
        # handing it a codex/opencode id would resume the wrong thing entirely.
        return False, None, f"wake=unsupported-harness({reachable.agent})"

    # Claude resume is cwd-scoped, so a recipient in another repo must be woken
    # from ITS directory, not the sender's. None means no store recorded one and
    # wake_and_deliver falls back.
    wake_cwd = None
    if isinstance(reachable.cwd, str) and reachable.cwd:
        try:
            wake_cwd = Path(reachable.cwd)
        except (TypeError, ValueError):
            wake_cwd = None

    delivered, detail = wake_and_deliver(
        reachable.session_id, wrapped, cwd=wake_cwd
    )
    if delivered:
        return True, detail, None

    # The asleep->live race: the session woke on its own between the probe and
    # the wake, so the wake correctly refused rather than opening a second
    # writer. Retry the socket ONCE -- it is now the right lane.
    if detail in ("writer-possibly-live", "wake-already-in-flight"):
        if _mail_inject_claude(reachable.session_id, wrapped):
            return True, None, None

    return False, None, f"wake={detail}"


def _name_lane_send(
    message: str,
    *,
    from_name: Optional[str],
    resolved,
    recipient: Optional[str] = None,
    provider: Optional[str] = None,
    reply_to: Optional[str] = None,
    token: Optional[str] = None,
) -> None:
    """Name-lane delivery core, shared by ``mail send <name>`` and a name-lane
    ``mail reply`` -- the ONE choke point every delivery ladder rung lives in.

    Three modes, by which of ``resolved`` / ``token`` is set:

    - ``resolved`` (a live ``DiscoveredSession``): live-inject first, mux pane
      next, durable floor on miss, addressed to its canonical handle.
    - ``token`` (discovery MISSED, but a miss from a liveness-gated listing is
      not a verdict on reachability): the full ladder -- inject-as-probe, then
      asleep resolution, then wake-and-deliver, then a durable demotion naming
      each failed lane. Raises ``UnreachableTokenError`` when no store knows the
      token at all, so the caller can exit 16 having queued nothing, and
      ``AmbiguousTokenError`` rather than guessing between two sessions.
    - neither: durable-only, addressed to ``recipient`` (a reply to an offline
      sender).

    ``reply_to`` stamps BOTH the wire ``reply_to`` attr and the bus
    ``in_reply_to`` from ONE msg-id -- never one set, the other null. Exits 12 on
    a durable-floor write failure."""
    from fno.agents.dispatch import _mail_inject_claude, _mail_inject_codex, _mux_pane_send
    from fno.agents.provider_resolve import infer_invoking_harness
    from fno.agents.registry import AgentResolutionError, resolve_agent
    from fno.agents.self_stamp import resolve_self_model, stamp_from
    from fno.harness_identity import canonical_handle
    from fno.inbox.store import (
        classify_durable_owner,
        generate_msg_id,
        write_new_thread,
    )
    from fno.mail.envelope import harness_for_provider, wrap_fno_mail

    if resolved is not None:
        recipient = canonical_handle(resolved.session_id)
        provider = resolved.agent
    elif token is not None:
        # Resolve BEFORE addressing. The durable copy must be addressed to the
        # resolved session's canonical handle -- deriving it from the raw token
        # would misaddress every alias (canonical_handle takes the first 8
        # chars, so `footnote-9a063cd3` would queue to `footnote`, which nothing
        # drains). Falls back to the token when nothing resolved, which is the
        # unregistered/exited-row case the socket probe below still covers.
        if _is_self_send(canonical_handle(token)):
            token_reachable, token_lane = None, "self-send"
        else:
            token_reachable, token_lane = _resolve_token(token)
        recipient = canonical_handle(
            token_reachable.session_id if token_reachable is not None else token
        )
        provider = (
            token_reachable.agent if token_reachable is not None else provider
        ) or "claude"

    # Mint the msg-id ONCE, before wrapping (Locked Decision 2): the same id
    # rides the live-injected envelope AND any durable fallback, so a recipient
    # can reply --to it whether or not a durable thread was written, and the
    # drain dedups a bounded-duplicate on that one id. Passing it to
    # write_new_thread below reuses it instead of minting a second.
    msg_id = generate_msg_id()

    # Wire `to` carries the canonical handle, matching the durable-bus recipient
    # exactly -- `from` is already a handle via stamp_from, so both attrs agree.
    wrapped = wrap_fno_mail(
        message,
        from_=stamp_from(from_name),
        # Through harness_for_provider like every other send path: the wire
        # vocabulary is claude-code, and stamping a raw "claude" here made the
        # name lane the one producer disagreeing with dispatch, the relay, and
        # the Rust contract. "cli" survives as the honest no-harness value: the
        # mapper defaults a MISSING provider to claude-code, a guess we avoid.
        harness=harness_for_provider(h) if (h := infer_invoking_harness()) else "cli",
        model=resolve_self_model(),
        to=recipient,
        id=msg_id,
        reply_to=reply_to,
    )

    injected = False
    woken_as: Optional[str] = None
    lanes: list[str] = []

    if resolved is None and token is not None:
        # The ladder below the discovery miss. Discovery is a liveness-gated
        # LISTING, so a miss means "not listed", never "not reachable" -- and
        # demoting here without attempting a live rung is the wall this whole
        # node exists to remove.
        if token_lane == "self-send":
            # A session can neither inject into nor wake itself; attempting it
            # deadlocks a live session and revives a second writer on an asleep
            # one. Durable is the only honest lane.
            lanes.append("self-send")
        else:
            # Rung 3: inject-as-probe. The socket is its own source of truth --
            # a confirmed delivery IS the receipt, so no roster query is needed
            # and a miss costs one cheap, side-effect-free call. Probe the
            # resolved session id when we have one; otherwise the raw token,
            # which is how an unregistered session with no store record is still
            # reached.
            probe_target = (
                token_reachable.session_id if token_reachable is not None else token
            )
            # Probe the harness we resolved; when nothing resolved, try both,
            # because an unregistered live session of either harness is exactly
            # the case with no store record to read the harness off. Both
            # injectors are cheap and side-effect-free on a miss.
            probe_agent = token_reachable.agent if token_reachable is not None else None
            if probe_agent == "codex":
                injected = _mail_inject_codex(probe_target, wrapped)
            else:
                injected = _mail_inject_claude(probe_target, wrapped)
                if not injected and probe_agent is None:
                    injected = _mail_inject_codex(probe_target, wrapped)
            if not injected:
                lanes.append("inject=not-delivered")
                if token_reachable is None:
                    if token_lane:
                        # A store was unreadable, so absence is unproven: keep
                        # the mail via the durable floor instead of exit 16.
                        lanes.append(token_lane)
                    else:
                        raise UnreachableTokenError(token)
                elif token_lane:
                    # Resolved, but uniqueness was unprovable. Do not wake a
                    # possible stranger; demote durably to this candidate.
                    lanes.append(token_lane)
                else:
                    injected, woken_as, wake_lane = _wake_rung(
                        token_reachable, wrapped
                    )
                    if wake_lane:
                        lanes.append(wake_lane)

    if resolved is not None:
        if provider == "claude":
            injected = _mail_inject_claude(resolved.session_id, wrapped)
        elif provider == "codex":
            injected = _mail_inject_codex(resolved.session_id, wrapped)
        if not injected:
            # A send addressed by session id never consults the roster, so a
            # mux-hosted session of any provider would demote to durable with a
            # live pane right there. Not-found means "not mux-hosted", not an error.
            try:
                entry = resolve_agent(resolved.session_id).entry
            except (AgentResolutionError, OSError):
                pass
            else:
                # Gate on live status like the registered-name path does. An
                # exited row keeps its mux ref, and pane ids are reused across a
                # mux restart, so sending on a stale ref types into an unrelated
                # pane and reports hosted -- suppressing the durable copy the
                # real recipient still needs.
                if entry.status == "live":
                    injected = _mux_pane_send(entry, wrapped)

    live = f" [live {resolved.agent} session {resolved.handle}]" if resolved is not None else ""
    corr = f" re:{reply_to}" if reply_to else ""
    # Surface the minted id so the sender can quote it and the recipient (who
    # also sees it in the injected <fno_mail id=...>) can reply --to it even
    # though a live-confirmed delivery writes no durable thread (US3).
    idtag = f" id:{msg_id}"
    if injected and woken_as:
        print(f"delivered (woken) to {recipient}{idtag}{corr} [revived as bg thread {woken_as}]")
        return
    if injected:
        print(f"delivered (hosted) to {recipient}{idtag}{live}{corr}")
        return

    # Every live rung that applied has now been attempted and missed. Durable is
    # a demotion, so the receipt names WHY each lane failed -- a delivery bug has
    # to be diagnosable from the sender's own terminal, without a daemon log.
    if lanes:
        print(f"lanes tried: {', '.join(lanes)}", file=sys.stderr)

    # Terminal classification (US6): a name lane reaches the durable floor only
    # after every live rung missed. A self-send lands in the sender's own inbox
    # and a live-listed-but-wedged recipient still has a turn-boundary drain, so
    # both own as live-drain; every other miss (asleep, offline, unprovable) is
    # optimistically resumable and owns as wake-daemon. The dead-letter verdict is
    # the sweep's to make once a wake-daemon thread sits unread past its TTL - at
    # birth we never know a recipient is gone for good (a token no store knows
    # already exits 16 upstream), so the durable floor never escalates non-zero.
    self_send = resolved is None and token is not None and token_lane == "self-send"
    recipient_live = self_send or resolved is not None
    owner = classify_durable_owner(
        param_forced=False,
        recipient_live=recipient_live,
        recipient_resumable=not recipient_live,
    )

    assert recipient is not None  # resolved by the name-lane logic before the durable write
    try:
        th = write_new_thread(
            recipient=recipient,
            sender=stamp_from(from_name),
            kind="send",
            body=wrapped,
            msg_id=msg_id,
            to_kind="name",
            provider_to=provider,
            replies_to=reply_to,
            owner=owner.value,
        )
    except (OSError, ValueError, RuntimeError) as exc2:
        print(f"durable envelope write failed for {recipient!r}: {exc2}", file=sys.stderr)
        raise typer.Exit(code=12) from exc2
    _warn_deferred(recipient)
    # Routing-reason disclosure (US10): name WHY this is durable so a delivery
    # bug is diagnosable from the sender's own terminal. A self-send can never
    # inject itself; everything else here is a live miss.
    reason = "self-send" if self_send else "live-miss"
    print(f"{th.thread_id} queued (durable) for {recipient}{live}{corr} [{reason}]")


# Send-time human escalation for a question, per (sender, recipient). A burst
# re-nudges every window rather than once forever (marker refreshed only on an
# actual escalation, so the window runs from the last nudge, not the first send).
_ESCALATION_DEBOUNCE_S = 300


def _escalate_question(sender: str, recipient: str, summary: str) -> str:
    """Notify the human at send time that a question needs them (Locked Decision
    7: a question NEVER autonomous-responds - only the human answers it).

    Debounced per (sender, recipient) so a chatty peer cannot spam the queue.
    The caller writes the durable question thread regardless, so the ambient
    unread count stays truthful even when this nudge is debounced. Best-effort
    throughout: a notifier or filesystem failure never breaks the send. Returns
    ``"escalated"`` (the human was notified), ``"debounced"`` (a recent nudge for
    this pair suppressed it), or ``"notifier-unavailable"`` (no OS notifier on
    this host, so nothing displayed - the caller must not claim escalation).
    """
    import hashlib

    from fno.paths import state_dir

    pair = hashlib.sha256(f"{sender}\x00{recipient}".encode()).hexdigest()[:16]
    marker_dir = state_dir() / "mail-escalations"
    marker = marker_dir / pair
    try:
        marker_dir.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    # Atomically claim the debounce window via O_CREAT|O_EXCL: exactly one
    # concurrent sender wins a fresh escalation, the rest see the marker and
    # debounce. A check-then-touch here would let a concurrent burst from one
    # pair all notify at once, defeating the debounce during the exact spike it
    # exists to damp.
    try:
        fd = os.open(str(marker), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
        os.close(fd)
    except FileExistsError:
        try:
            last = marker.stat().st_mtime
        except OSError:
            last = 0.0
        if time.time() - last < _ESCALATION_DEBOUNCE_S:
            return "debounced"
        try:
            os.utime(marker, None)  # stale window: refresh so the next runs from now
        except OSError:
            pass
    except OSError:
        pass  # a missing marker just re-notifies; it never suppresses the durable write
    # Only report escalation when the notification actually displayed:
    # send_notification returns (code, err) and a nonzero code means no OS
    # notifier (a headless host), so the human was NOT notified.
    try:
        from fno.notify._impl import send_notification

        one_line = summary.split("\n", 1)[0][:120]
        code, _err = send_notification(
            f"fno mail: question from {sender}",
            f"{one_line} - run `fno mail drain-self`",
        )
    except Exception:  # noqa: BLE001 - a notifier failure never breaks the send
        code = 1
    return "escalated" if code == 0 else "notifier-unavailable"


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
    from_self: bool = typer.Option(
        False, "--from-self",
        help=(
            "Stamp the sender with this session's own canonical mail handle "
            "(the reply handle `fno whoami` shows) instead of the project. "
            "Use with --to-project when you will hold for the reply. Fails loud "
            "(exit 2) with no ambient harness identity - never a silent floor."
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
            "Inbox kind (heads-up | question | fyi). A project-inbox drain "
            "contract, so pair it with --to-project; question/fyi to a bare "
            "session handle is refused (a handle has no drain that reads them). "
            "Omit --kind for a default agent-to-agent send (live if a peer is hosted)."
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
    from fno.agents.self_stamp import stamp_from

    workdir = Path(cwd).resolve() if cwd else Path(os.getcwd())

    # --from-self resolves this session's own canonical handle and threads it as
    # from_name, so every lane below (project-note / --to-project / name) stamps a
    # reachable reply address instead of the project. It fails LOUD without ambient
    # identity - the silent "fno" floor stamp_from uses is the exact bug this kills.
    if from_self:
        if from_name is not None:
            print("error: --from-self and --from-name are mutually exclusive", file=sys.stderr)
            raise typer.Exit(code=2)
        from fno.harness_identity import canonical_handle, resolve_harness_identity

        ident = resolve_harness_identity()
        if not (ident.session_id and ident.harness):
            print(
                "error: --from-self: no ambient harness identity - cannot self-stamp",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        from_name = canonical_handle(ident.session_id)

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

        # US10 kind-scoped guard: question/fyi are project-inbox drain contracts
        # (question -> wake-signal, fyi -> memory). Addressed to a bare session
        # handle they queue durable to an inbox nothing drains, so refuse and name
        # the two real intents. heads-up to a handle stays accepted (the production
        # notification pattern; its emitters are programmatic, not enumerable).
        if to_project is None and kind in {Kind.QUESTION.value, Kind.FYI.value}:
            print(
                f"error: --kind {kind} to a session handle ({recipient}) has no "
                f"drain that reads it. Drop --kind to inject it live, or add "
                f"--to-project <project> to file it as a durable {kind} note.",
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
                else resolve_project(
                    cwd=workdir, flag_hint="--from-name/--from-self"
                )
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

        # A question never gets an autonomous responder (US9 wakes only heads-up);
        # it escalates to the human at send time instead, debounced per pair.
        if kind == Kind.QUESTION.value:
            reason = _escalate_question(sender, recipient, content)
            if reason == "escalated":
                print(f"escalated to human ({recipient})", file=sys.stderr)
        # A heads-up to a resumable-but-asleep claude session is woken at send
        # time to drain it: the per-project watch daemon drains project inboxes,
        # never a session-handle inbox, so send time is the reachable trigger
        # (US9). The durable note is already written, so a wake miss loses nothing.
        elif kind == Kind.HEADS_UP.value:
            from fno.agents.dispatch import wake_if_asleep_claude

            woke, short = wake_if_asleep_claude(recipient)
            if woke:
                print(f"woke {recipient} to drain (bg thread {short})", file=sys.stderr)

        if json_out:
            import json as _json

            print(_json.dumps({
                "msg_id": res.msg_id,
                "thread_path": str(res.thread_path),
                "appended": res.appended,
            }))
        else:
            verb = "appended (durable) to" if res.appended else "queued (durable) for"
            print(f"{res.msg_id} {verb} {recipient} [param-forced: --kind {kind}]")
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
            _warn_deferred(result.recipient)
            print(
                f"{result.msg_id} queued (durable) for {result.recipient} "
                f"[project {to_project}] [live-miss]"
            )
        else:
            _warn_deferred(to_project, project=True)
            print(
                f"{result.msg_id} queued (durable) for project {to_project} "
                f"[param-forced: --to-project]"
            )
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

        # A caller-TYPED retired <harness>-<short8> address is refused outright,
        # before any lane runs. Nothing mints that form any more, so a typed one
        # is a caller bug worth surfacing rather than silently translating. (A
        # retired form READ off a stored record is the opposite case: a data
        # artifact, migrated and delivered. Caller-error vs data-artifact is the
        # discriminator, and the two directions must never blur.)
        from fno.harness_identity import LEGACY_HANDLE_RE

        if LEGACY_HANDLE_RE.fullmatch(name or ""):
            hint = f" Use the bare id instead: {', '.join(suggestions)}." if suggestions else ""
            print(f"retired handle form: {name!r}.{hint}", file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc

        # Discovery missed -- but discovery is a liveness-gated LISTING, so this
        # means "not listed", NOT "not reachable". Fall INTO the shared choke
        # point carrying the raw token so the socket and the disk stores each
        # get their turn. Exit 16 now lives at the BOTTOM of that ladder, where
        # matrix cell 5 (resolves nowhere) actually belongs, instead of here
        # where it used to pre-empt every live rung.
        try:
            _name_lane_send(message, from_name=from_name, resolved=None, token=name)
        except AmbiguousTokenError as amb:
            print(
                f"ambiguous session token {name!r}: matches "
                f"{', '.join(amb.candidates)}. Send to a full session id.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2) from amb
        except UnreachableTokenError:
            # AC2-ERR: not a registered agent, not discoverable, and no durable
            # store knows it. Error with the closest live handles, sending nothing.
            hint = ""
            if suggestions:
                hint = f" Closest live sessions: {', '.join(suggestions)}."
            print(
                f"unknown agent or live-session handle: {name!r}.{hint}",
                file=sys.stderr,
            )
            raise typer.Exit(code=exc.exit_code) from exc
        return

    # AC3-UI: distinguish delivered vs queued on stdout.
    if result.delivery == "hosted":
        label = "delivered (hosted)"
    else:
        _warn_deferred(name)
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


@mail_app.command("drain-self", hidden=True)
def cmd_drain_self(
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON regardless of TTY."
    ),
) -> None:
    """Drain THIS session's own cross-harness inbox and mark it seen (US5).

    The receive side of the a2a relay: a session computes its own handle from
    the ambient harness env markers (``canonical_handle(session-id)``,
    the SAME string a sender resolves and the registry registers under), reads
    its unread bus mail, prints it for injection into the session, then advances
    its own cursor so nothing re-surfaces next wake. Wired into each harness's
    SessionStart hook, this is what makes a codex/gemini session actually
    RECEIVE its mail -- addressability already existed, drainage did not.

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

    handle = canonical_handle(ident.session_id)
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


# ---------------------------------------------------------------------------
# notify-self engine (x-39a4): stat-only turn-boundary nudge helpers, shared
# with `fno mail status` (the sent-unclaimed count).
# ---------------------------------------------------------------------------

# The nudge text is embedded inside a hook-owned <system-reminder> wrapper, so a
# sender/recipient handle carrying a literal </system-reminder> could break out
# and inject context. Defang the delimiter (open/close, case- + whitespace-
# insensitive) in every interpolated field, mirroring born-with-why-offer-inject.sh.
_REMINDER_TAG = re.compile(r"<\s*(/?)\s*system-reminder\s*>", re.IGNORECASE)


def _defang_reminder(s: str) -> str:
    return _REMINDER_TAG.sub(r"[\1system-reminder]", s)


def _bounded_names(names: list[str], cap: int = 3) -> str:
    """De-dupe (first-seen), defang, then cap at ``cap`` names + ``+K more``."""
    seen: list[str] = []
    for n in names:
        if n not in seen:
            seen.append(n)
    shown = [_defang_reminder(n) for n in seen[:cap]]
    extra = len(seen) - cap
    return ", ".join(shown) + (f", +{extra} more" if extra > 0 else "")


def _age_exceeds(ts: str, ttl_seconds: int, now: "datetime") -> bool:
    """True iff bus ISO ``ts`` (``...Z`` UTC) is strictly older than TTL.

    Unparseable ts -> False (never flag): degrade to quiet, never to a crash.
    ``fromisoformat`` is lock-free (unlike ``strptime``, which grabs a global
    locale lock) and pre-3.11-safe once the trailing ``Z`` is normalized -- it
    runs once per sent message on the every-turn hook path, so the lock matters.
    """
    from datetime import datetime as _dt

    try:
        sent_at = _dt.fromisoformat(ts.replace("Z", "+00:00") if ts.endswith("Z") else ts)
        return (now - sent_at).total_seconds() > ttl_seconds
    except (ValueError, TypeError, AttributeError):
        return False


def _sent_unclaimed(handle: str, ttl_seconds: int) -> tuple[int, list[str]]:
    """Count + distinct recipients (first-seen) of my sent mail unclaimed past TTL.


    Unclaimed = still past the recipient's consume cursor AND strictly older than
    ``ttl_seconds``. Reads the bus ONCE (a single ``iter_messages`` snapshot) and
    compares each recipient's cursor position against that snapshot, so cost is
    ``O(bus + recipients)`` not ``O(recipients x bus)`` -- a per-recipient
    ``scan_unread`` reparse could cross the hook's 2s timeout and silently drop
    the nudge. Stat-only: recipient cursors are read fresh every call (never
    cached), so a just-consumed message stops being flagged immediately; no
    cursor is advanced.
    """
    from datetime import datetime as _dt
    from datetime import timezone as _tz

    from fno.bus.cursor import read_cursor
    from fno.bus.log import iter_messages

    now = _dt.now(tz=_tz.utc)
    all_msgs = list(iter_messages())
    sent = [m for m in all_msgs if m.from_ == handle]
    if not sent:
        return 0, []
    pos = {m.id: i for i, m in enumerate(all_msgs)}
    # Per recipient, its consume-cursor position in the single snapshot. A
    # message to r is unread iff it sits AFTER that position; an absent, corrupt,
    # or rotated-out cursor means "nothing consumed" (-1 -> all unread), matching
    # scan_unread's fail-open. A recipient name read_cursor rejects (path-
    # traversal guard) or that errors -> sentinel len(all_msgs) so nothing is
    # "after" it -> fully claimed / skipped: fail-open to quiet, never a crash.
    cursor_pos: dict[str, int] = {}
    for r in {m.to for m in sent}:
        try:
            cid = read_cursor(r)
        except (ValueError, OSError):
            cursor_pos[r] = len(all_msgs)
            continue
        cursor_pos[r] = pos.get(cid, -1) if cid else -1
    count = 0
    recipients: list[str] = []
    for m in sent:
        if pos[m.id] <= cursor_pos.get(m.to, len(all_msgs)):  # claimed / unresolvable
            continue
        if not _age_exceeds(m.ts, ttl_seconds, now):  # still fresh (strict >)
            continue
        count += 1
        if m.to not in recipients:
            recipients.append(m.to)
    return count, recipients


@mail_app.command("notify-self", hidden=True)
def cmd_notify_self() -> None:
    """Stat-only turn-boundary nudge: unread inbound + unclaimed sent (x-39a4).

    The push half of push-first delivery, wired into every session's
    ``UserPromptSubmit`` hook. Unlike ``drain-self`` it NEVER advances the
    consume cursor -- a nudge is a notice, not a consume, so SessionStart's
    ``drain-self`` and the sender-side check still see un-acted mail (the
    load-bearing invariant: notify never eats delivery). Two stats over the one
    global bus:

      1. inbound: unread mail addressed to my handle -> one line "N unread fno
         mail from <senders>: run `fno mail drain-self`". It points at
         ``drain-self``, NOT ``fno mail unread``: only ``drain-self`` self-
         resolves this session's handle and advances its consume cursor; ``fno
         mail unread`` defaults ``--name`` to the project ("fno"), so it would
         read the wrong inbox and never clear the nudge. Persistent: the
         wrapping hook fires each turn, so the line re-injects while unread and
         clears the moment ``drain-self`` advances the consume cursor.
      2. sent-unclaimed: my own sent mail still past the recipient's cursor and
         strictly older than ``config.inbox.unclaimed_ttl`` -> "N sent fno mail
         unclaimed (to <recipients>, >Nm)". Closes the "queued (durable)" ==
         "delivered" silent gap.

    No harness identity in env -> silent no-op (mirror ``drain-self``).
    """
    from fno.bus.cursor import scan_unread
    from fno.config import load_settings
    from fno.harness_identity import canonical_handle, resolve_harness_identity

    ident = resolve_harness_identity()
    if not ident.harness or not ident.session_id:
        return

    handle = canonical_handle(ident.session_id)
    lines: list[str] = []

    unread = scan_unread(handle)
    if unread:
        senders = _bounded_names([m.from_ for m in unread])
        lines.append(
            f"{len(unread)} unread fno mail from {senders}: run `fno mail drain-self`"
        )

    ttl = load_settings().inbox.unclaimed_ttl
    n_sent, recipients = _sent_unclaimed(handle, ttl)
    if n_sent:
        who = _bounded_names(recipients)
        lines.append(
            f"{n_sent} sent fno mail unclaimed (to {who}, >{ttl // 60}m): "
            "recipient has not picked it up"
        )

    if lines:
        print("\n".join(lines))


@mail_app.command("rebuild-render", hidden=True)
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
