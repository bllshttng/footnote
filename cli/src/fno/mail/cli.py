"""fno agents mail: durable polled mailbox CLI (ab-cee91152).

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

Call shape (one rule, because the verbs used to disagree):
    The message BODY is uniform across ``send`` and ``reply``. Both take it
    positionally, or via ``--body``, or via ``--body-file`` -- exactly one of the
    three, with two-at-once an explicit refusal rather than a precedence rule.

    ``reply`` was flag-only until 2026-08-10, so the positional form that the
    skills taught (and that ``send`` accepts) exited 2 while click echoed the
    body back. An echoed body resembles a delivery receipt closely enough that
    two agents independently concluded the verb was broken, and one of them
    reported mail as sent that had never left. Keep the two verbs symmetric: a
    surface whose call shape depends on which verb you happen to be calling is
    the defect, and correcting the docs alone leaves it live for the next doc
    written from memory.

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
    """Public --json contract returned by `fno agents mail status`.

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


def _read_body(
    body: Optional[str],
    body_file: Optional[Path],
    positional: Optional[str] = None,
) -> str:
    """The reply body, from whichever of the three forms was used.

    ``positional`` exists so ``reply`` accepts a bare body like ``send`` does.
    Without it the two verbs disagreed about their own call shape, and the
    failure was quiet in the worst way: click rejected the stray argument with
    exit 2 and echoed the body back, which reads like a delivery receipt rather
    than a refusal.
    """
    supplied = [x for x in (positional, body, body_file) if x is not None]
    if len(supplied) > 1:
        typer.echo(
            "error: provide the body once - as a positional argument, --body, or --body-file",
            err=True,
        )
        raise typer.Exit(code=1)
    if body_file is not None:
        return body_file.read_text(encoding="utf-8")
    if body is not None:
        return body
    if positional is not None:
        return positional
    typer.echo(
        "error: provide a body - as a positional argument, --body, or --body-file",
        err=True,
    )
    raise typer.Exit(code=1)


def _cap_env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        return int(raw)
    except ValueError:
        return default


# Brevity gate for authored relay bodies. Mail is re-read every turn by every
# recipient, so a body that duplicates a node or doc is paid many times over.
# Thresholds target the verbose tail (measured p90 ~3390 B over 354 real
# messages), not the median (p50 ~1182 B); a 1200 B cap would hit half of live
# coordination. Override via env; set a knob to 0 to disable that tier.
_BODY_WARN_BYTES = _cap_env_int("FNO_MAIL_BODY_WARN", 3000)
_BODY_REFUSE_BYTES = _cap_env_int("FNO_MAIL_BODY_REFUSE", 5000)


def classify_origin(explicit_origin: str | None = None) -> str:
    """Classify the sender once, before a mail lane can narrow behavior."""
    from fno.agents.self_stamp import resolve_self_identity
    from fno.decide import MAIL_ORIGINS, enforce_origin_floor

    ident = resolve_self_identity()
    agent_identity = bool(ident.session_id and ident.harness)
    if explicit_origin is not None:
        if explicit_origin not in MAIL_ORIGINS:
            raise ValueError(
                f"unknown mail origin {explicit_origin!r}; expected one of "
                f"{', '.join(MAIL_ORIGINS)}"
            )
        # An origin above peer is a claim about the channel, and an ambient
        # agent identity cannot make that claim. Downgrade to peer rather
        # than honoring the flag: least authority by default cannot be
        # forged, and a process with no session identity (a real scheduler,
        # a recovery sweep) still declares its origin honestly.
        floored = enforce_origin_floor(explicit_origin)
        if floored != explicit_origin:
            typer.echo(
                f"mail origin {explicit_origin!r} downgraded to 'peer': an "
                "agent session cannot declare an origin above peer",
                err=True,
            )
        return floored

    if agent_identity:
        return "peer"
    try:
        attended = bool(sys.stdin.isatty())
    except (AttributeError, OSError, ValueError):
        attended = False
    return "operator" if attended else "unknown"


def _record_mail_origin(
    *,
    origin: str,
    lane: str,
    sender: str | None = None,
    target_session: str | None = None,
) -> None:
    """Best-effort positive measurement of the classified send origin."""
    try:
        from fno.agents import events
        from fno.events import append_event, mail_origin_classified

        append_event(
            mail_origin_classified(
                origin=origin,
                lane=lane,
                presumed_human=origin == "operator",
                sender=sender,
                target_session=target_session,
            ),
            events.daemon_lifecycle_log(),
            lock_timeout_seconds=2,
        )
    except Exception:
        pass


def _enforce_body_cap(body: str, *, usage: bool = False) -> None:
    """Warn over WARN bytes, refuse over REFUSE bytes.

    Fail-open: a disabled tier (0) or an unset body never blocks coordination.
    The refusal teaches the rule: put the detail in a node or doc and send a
    short pointer, since the mail is re-read far more often than the node.
    ``usage=True`` exits 2: under ``--raw --check`` an over-cap payload is a
    malformed CALL, and exit 1 there would read as a not-injectable verdict
    about a session the run never measured.
    """
    warn, refuse = _BODY_WARN_BYTES, _BODY_REFUSE_BYTES
    if warn <= 0 and refuse <= 0:
        return
    n = len(body.encode("utf-8"))
    if refuse > 0 and n > refuse:
        print(
            f"error: mail body is {n} bytes (cap {refuse}). Relay mail is re-read "
            f"every turn; put the detail in a node or doc and send a short pointer. "
            f"Disable with FNO_MAIL_BODY_REFUSE=0 (warn-only) or both knobs 0.",
            file=sys.stderr,
        )
        raise typer.Exit(code=2 if usage else 1)
    if warn > 0 and n > warn:
        print(
            f"note: mail body is {n} bytes (over the {warn}-byte brevity guide); "
            f"prefer a short pointer with the detail in a node/doc.",
            file=sys.stderr,
        )


#: Matches either end of the peer-follow-up container, case-insensitively, for
#: the same reason `contains_fno_mail_tag` is case-insensitive: an exact-case
#: check is bypassed by one capital letter.
_CROSS_SESSION_TAG_RE = re.compile(r"</?cross-session-message", re.IGNORECASE)


def _refuse_forged_envelope(body: str) -> None:
    """Refuse a body containing an ``<fno_mail`` open tag or ``</fno_mail>`` close
    tag (x-4ce4), with a CLI-friendly error before the body ever reaches
    ``wrap_fno_mail`` (which enforces the same invariant as the backstop for
    every producer, not only these CLI entry points).

    The envelope's trailer (``wrap_fno_mail``) is only trustworthy if a peer
    cannot forge one: a body containing a close tag followed by a fabricated
    trailer would render as two envelopes to a reader, and the second could say
    the opposite of the first. Refuse at send time and name the reason, rather
    than silently stripping or escaping - the body is prose a human reads, and a
    mangled body is worse than a refused send.
    """
    from fno.mail.envelope import ForgedEnvelopeError, refuse_if_forged

    try:
        refuse_if_forged(body)
    except ForgedEnvelopeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise typer.Exit(code=1) from exc


def _enforce_style(body: str, *, allow_reason: str | None = None) -> None:
    """Refuse a body that breaks the seven style rules.

    Fail-open: an empty body, the kill switch (``FNO_STYLE_ENFORCE=0``), a
    ``style-exception:`` line, or a non-empty ``--style-exception`` reason skips
    the check. The refusal names each broken rule and the offending sentence,
    and the message itself passes rules 1 to 6. The refusal is stderr, not a
    mail body, so it is exempt from rule 7.
    """
    if os.environ.get("FNO_STYLE_ENFORCE") == "0" or not body:
        return
    if allow_reason and allow_reason.strip():
        return
    from fno import style

    if style.has_exception(body):
        return
    violations = style.check(body, surface="mail")
    if violations:
        _emit_style_refusal(violations)
        print(style.format_violations(violations), file=sys.stderr)
        raise typer.Exit(code=1)


def _reserve_budget(
    *,
    sender: str,
    recipient: str,
    body: str,
    msg_id: str,
    allow_reason: str | None = None,
    sender_key: str | None = None,
    recipient_key: str | None = None,
):
    """Reserve the authored count after both pair identities are canonical."""
    from fno import style
    from fno.mail import budget

    words = style.word_count(body)
    exempt = not _budget_enforced(body, allow_reason=allow_reason)
    try:
        reservation = budget.reserve(
            sender=sender,
            recipient=recipient,
            words=words,
            msg_id=msg_id,
            enforce=not exempt,
            sender_key=sender_key,
            recipient_key=recipient_key,
        )
    except budget.BudgetRefused as exc:
        print(
            f"refused: rolling word budget for {exc.pair}: {exc.marker()}",
            file=sys.stderr,
        )
        raise typer.Exit(code=1) from exc
    except budget.BudgetUnavailable as exc:
        print(f"refused: {exc}", file=sys.stderr)
        raise typer.Exit(code=1) from exc
    return reservation, words


def _budget_enforced(body: str, *, allow_reason: str | None = None) -> bool:
    """Whether this send refuses over-budget; all sends still reserve."""
    from fno import style

    return not (
        os.environ.get("FNO_STYLE_ENFORCE") == "0"
        or bool(allow_reason and allow_reason.strip())
        or style.has_exception(body)
    )


def _release_budget(reservation) -> None:
    """Release only after the caller proves no outward lane accepted the body."""
    from fno.mail import budget

    budget.release(reservation)


def _emit_style_refusal(violations: list) -> None:
    """Record one style_refusal event so the retry rate is a query over events.jsonl.

    Best-effort and never blocks a refusal: a measurement hook that wedged the
    gate it measures would silence the distress channel. Carries the rule ids
    that fired and the ambient session id, so a refusal paired with a later
    passing send in the same session is the retry signal.
    """
    try:
        from fno.events import _build, append_event
        from fno.agents.self_stamp import resolve_self_identity

        data: dict = {
            "surface": "mail",
            "rule_ids": sorted({v.rule for v in violations}),
            "violation_count": len(violations),
        }
        ident = resolve_self_identity()
        if ident.session_id:
            data["session_id"] = ident.session_id
            source = "target"
        else:
            source = "test"
        append_event(_build("style_refusal", source, data))
    except Exception:
        pass




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
    from fno.agents.self_stamp import resolve_self_identity
    from fno.config import load_settings
    from fno.harness_identity import canonical_handle

    ident = resolve_self_identity()
    if not ident.harness or not ident.session_id:
        return 0
    try:
        handle = canonical_handle(ident.session_id)
        return len(_sent_unclaimed(handle, load_settings().inbox.unclaimed_ttl))
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

    log_path = repo_root / ".fno" / "fno-watch.log"
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

def _is_job_name(name: Optional[str]) -> bool:
    """True when ``name`` is a ``node:<id>`` / ``pr:<n>`` job address."""
    if not name:
        return False
    from fno.mail.job_address import is_job_token

    return bool(is_job_token(name))


def _refuse_unsafe_short_address(
    token: Optional[str], *, self_addressed: bool = False
) -> None:
    """Refuse a codex head-8 supplied as a mail ADDRESS, naming what to use.

    Registry names and claude handles pass through untouched. A codex head-8 is
    refused on SHAPE, not on whether it happens to resolve uniquely this second:
    a codex session id is UUIDv7, so those eight characters are a ~65.536-second
    clock bucket, and the collision arrives silently the moment a sibling spawns
    in the same minute. Refusing only after a collision exists is refusing after
    the reply has already become impossible.
    """
    if not token:
        return
    if self_addressed:
        # A self-address resolves to exactly one session by construction: the
        # one running this process. The ambiguity this rule guards against
        # cannot arise, so refusing here only blocks a caller from reaching
        # itself.
        return
    from fno.agents.registry import load_registry
    from fno.harness_identity import (
        CODEX_SHORT_ADDRESS_RULE,
        canonical_handle,
        is_unsafe_short_address,
    )

    # Shape first: it is cheap, and it is what makes the registry read worth
    # doing. A registry name or a full session id never reaches the read below.
    if not is_unsafe_short_address(token, "codex"):
        return
    try:
        entries = load_registry()
    except Exception:  # noqa: BLE001 - an unreadable registry proves nothing
        return

    # The registry is what proves the head-8 is CODEX's. The same eight hex are
    # a fine claude address (UUIDv4, 32 random bits), so refusing on shape alone
    # would break the harness this rule does not apply to.
    wanted = token.strip().lower()
    matches = []
    for entry in entries:
        if getattr(entry, "harness", None) != "codex":
            continue
        session_id = (
            getattr(entry, "harness_session_id", None)
            or getattr(entry, "session_id", None)
            or ""
        )
        if session_id and canonical_handle(session_id).lower() == wanted:
            matches.append((entry, session_id))
    if not matches:
        return

    lines = [f"error: {token!r} addresses codex: {CODEX_SHORT_ADDRESS_RULE}."]
    for entry, session_id in matches:
        mux = getattr(entry, "mux", None) or {}
        pane = mux.get("pane_id")
        pane_note = f"  (pane {pane})" if pane is not None else ""
        lines.append(f"  {session_id}{pane_note}")
    print("\n".join(lines), file=sys.stderr)
    raise typer.Exit(code=2)


def _reply_to_name_handle(
    body_text: str,
    *,
    from_project: Optional[str],
    target: str,
    to_msg: str,
    require_resolution: bool = False,
    style_exception: Optional[str] = None,
    sender_session: Optional[str] = None,
    origin: Optional[str] = None,
) -> None:
    """Send a name-lane reply to ``target`` (a canonical handle): resolve it live
    and inject, else durable-floor to it. Shared by the bus-record reply path and
    the US3 transcript-recovered live-sender path.

    ``from_name=from_project`` stays None by default so stamp_from auto-stamps
    THIS session's canonical bare short-id -- the handle the original sender
    replies back to and that drain-self scans, NOT a project name."""
    from fno.agents import discover as discover_mod

    # `sender_session` is deliberately NOT consulted here. It is validated
    # against the real candidate set in `cmd_reply`, which is the only place
    # that has one: discovery knows which sessions a handle can name, and this
    # function does not. An attempt to honor it here compared its head-8 against
    # the target's, which every candidate in a collision shares BY CONSTRUCTION,
    # so it accepted any string with the right first eight characters (a
    # one-character tail typo included) and then ran the ladder on it, raising
    # UnreachableTokenError out of the command with the budget still reserved.
    # A guard that cannot fail is worse than no guard: it reads as validation.
    resolved, suggestions = discover_mod.resolve_or_suggest(target)
    if resolved is not None:
        _name_lane_send(
            body_text,
            from_name=from_project,
            resolved=resolved,
            reply_to=to_msg,
            style_exception=style_exception,
            origin=origin,
        )
    else:
        try:
            reachable, ambiguous = discover_mod.resolve_reachable(target)
        except discover_mod.StoreReadError as exc:
            # The uniqueness check exists to disambiguate a TYPED name. On this
            # path the handle came off the answered message and was never typed,
            # so an unreadable store is not evidence against it -- and refusing
            # here defeats the one verb built so a worker never re-types a
            # handle. The cost is total, not degraded: a worker whose thread
            # arrived live has no id on the durable bus, so this refusal removed
            # its only way to answer anyone.
            #
            # ``require_resolution`` marks the two cases where the handle IS a
            # guess: a mutable alias on a session-addressed record, and a
            # migrated legacy token. Those keep refusing, because there proven
            # uniqueness is the only thing vouching for the address. Do not
            # unify the two paths -- they differ in where the handle came from.
            # ``exc.resolved`` is the discriminator, and it refuses the OPPOSITE
            # of what it looks like. A visible candidate whose uniqueness went
            # unproven is exactly the wake-a-stranger risk this guard exists for:
            # an unreadable store may hold a session colliding on the same short
            # id, so choosing the one row we can see is the guess. Keep refusing.
            #
            # No candidate anywhere is a different situation. There is nothing to
            # choose between, so the reply falls to the durable floor addressed to
            # the handle the answered message recorded. That wakes nobody and
            # invents no address.
            if require_resolution or exc.resolved is not None:
                raise typer.BadParameter(
                    f"sender handle {target!r} cannot be checked uniquely; unreadable "
                    f"stores: {', '.join(exc.failed)}"
                ) from exc
            print(
                f"note: reachability stores unreadable "
                f"({', '.join(exc.failed)}); replying to {target!r} as recorded "
                "on the answered message.",
                file=sys.stderr,
            )
            reachable, ambiguous = None, []
        if ambiguous:
            # The collision escape hatch, and the ONLY place `--sender-session`
            # is read. A threaded reply that cannot be sent is worse than an
            # unthreaded one, and before this the verb offered no disambiguator
            # at all: the only route left was `send <name>`, which discards the
            # thread. `--sender-session` keeps `reply_to`, so the correlation
            # survives.
            #
            # It belongs here because `ambiguous` IS the candidate set. Validate
            # membership anywhere else and there is nothing real to check
            # against, which is how a version of this compared head-8s that
            # every candidate shares by construction and validated nothing.
            if sender_session:
                from fno.harness_identity import session_identity_key

                candidates = {session_identity_key(c): c for c in ambiguous}
                chosen = candidates.get(session_identity_key(sender_session))
                if chosen is None:
                    raise typer.BadParameter(
                        f"--sender-session {sender_session!r} is not one of the "
                        f"sessions {target!r} could name: {', '.join(ambiguous)}. "
                        f"Nothing was sent."
                    )
                _name_lane_send(
                    body_text,
                    from_name=from_project,
                    resolved=None,
                    token=chosen,
                    reply_to=to_msg,
                    style_exception=style_exception,
                    origin=origin,
                )
                return
            raise typer.BadParameter(
                f"sender handle {target!r} is ambiguous across sessions: "
                f"{', '.join(ambiguous)}. Re-run with "
                f"--sender-session <full-session-id> to answer one and keep the "
                f"thread."
            )
        if reachable is not None:
            _name_lane_send(
                body_text,
                from_name=from_project,
                resolved=None,
                token=target,
                reply_to=to_msg,
                style_exception=style_exception,
                origin=origin,
            )
            return
        if require_resolution:
            detail = f"; candidates: {', '.join(suggestions)}" if suggestions else ""
            raise typer.BadParameter(
                f"retired sender handle {target!r} cannot be resolved uniquely{detail}"
            )
        # AC1-FR: the original sender is no longer live -> durable floor addressed
        # to their canonical handle, still drainable. No provider: it is only
        # consulted on the live-inject path, which a None `resolved` already skips.
        _name_lane_send(
            body_text,
            from_name=from_project,
            resolved=None,
            recipient=target,
            reply_to=to_msg,
            style_exception=style_exception,
            origin=origin,
        )


@mail_app.command("reply")
def cmd_reply(
    body_arg: Optional[str] = typer.Argument(
        None, help="Reply body (alternative to --body, matching `send`)."
    ),
    to_msg: str = typer.Option(..., "--to", help="msg-id to reply to"),
    kind: str = typer.Option("fyi", "--kind", help="Reply kind (default: fyi)"),
    body: Optional[str] = typer.Option(None, "--body", help="Reply body"),
    body_file: Optional[Path] = typer.Option(None, "--body-file", help="Read body from file"),
    ref_mission: Optional[str] = typer.Option(None, "--ref-mission", help="Mission id (megatron)"),
    source_mission: Optional[str] = typer.Option(None, "--source-mission", help="Originating mission for cascades"),
    cascade_of: Optional[str] = typer.Option(None, "--cascade-of", help="Originating msg-id for cascades"),
    from_project: Optional[str] = typer.Option(None, "--from", help="Sender project (overrides settings.yaml)"),
    json_out: bool = typer.Option(False, "--json", "-J", help="Print {msg_id, thread_path} as JSON"),
    sender_session: str | None = typer.Option(
        None, "--sender-session",
        help=(
            "Full session id to answer when the stored sender handle is "
            "ambiguous. A legacy message carries only a head-8 handle, and under "
            "UUIDv7 that is a ~65.536-second clock bucket, so two workers started "
            "in one minute share it. Naming the full id here keeps the thread: "
            "the reply still carries the original in_reply_to. A value that is "
            "not one of the candidates sends nothing."
        ),
    ),
    style_exception: str | None = typer.Option(
        None, "--style-exception",
        help="Bypass the style check for this body with a stated reason.",
    ),
) -> None:
    """Reply to a message, routed by the answered message's lane.

    The id is resolved against the durable bus FIRST. A directed message (to_kind
    is name, session, or node) goes back to its original sender without you
    re-typing the handle, correlated via in_reply_to. A ``node``-addressed job
    message is answered at its sender too (the job address is the routing key on
    the way IN; the reply goes back to who sent it). Any other target falls
    through to the thread-store reply.

    If the id is not on the bus, this session's own TRANSCRIPT is searched next.
    That path is the common one, not a fallback for odd cases: a live-confirmed
    delivery writes no durable thread, so an id that arrived live is absent from
    the bus by design, and resolve_live_sender recovers the sender from the
    injected <fno_mail id=...> envelope instead.

    Only an id absent from BOTH is a hard error. This text used to describe the
    bus step alone, and two agents read that as proof the verb could not answer
    live mail at all.

    The body is positional, or --body, or --body-file. Exactly one of the three;
    giving two is refused rather than resolved by precedence.
    """
    kind = _validate_kind(kind)
    body_text = _read_body(body, body_file, body_arg)
    _refuse_forged_envelope(body_text)
    _enforce_body_cap(body_text)
    _enforce_style(body_text, allow_reason=style_exception)
    classified_origin = classify_origin()
    _record_mail_origin(origin=classified_origin, lane="reply", sender=from_project)
    mail_origin: str | None = (
        None if classified_origin == "unknown" else classified_origin
    )

    # Directed-lane routing (x-8045): look the --to msg-id up on the durable bus
    # and answer name/session/node mail back to its original sender. Anything else
    # falls through to the thread-store reply below.
    from fno.bus.log import iter_messages

    from fno.harness_identity import LEGACY_HANDLE_RE, canonical_handle

    orig = next((m for m in iter_messages() if m.id == to_msg), None)
    if orig is not None and orig.to_kind in {"name", "session", "node"}:
        # A stored sender predating the address flip carries the retired
        # `<harness>-<short8>` form. That is a fact about an old RECORD, not a
        # mistake by whoever is replying, and the address it would carry today is
        # a substring - so migrate it and deliver. Refusing here would invent a
        # wall at a knowledge boundary: making a human perform a translation the
        # code can do is how a resumable peer gets treated as voicemail.
        # (Not the harness-parsing this scheme forbids: the harness is discarded,
        # the short-id is what routes, and routing is still a roster lookup.)
        from fno.agents.store_fallback import is_full_session_id
        from fno.harness_identity import canonical_handle

        target = orig.from_ or ""
        require_resolution = False
        # Full sender provenance wins on EVERY lane, not only the
        # session-addressed one (node x-3a64). The name lane is the lane that
        # writes `from_session`, and it stamps `to_kind="name"` on all three of
        # its records, so a check gated on `to_kind == "session"` never read the
        # value it had just been taught to store. The head-8 the row also
        # carries is a display handle: under UUIDv7 its first eight hex are a
        # truncated millisecond timestamp, so two workers from one ~65-second
        # bucket share it and the reply refuses as ambiguous. The record already
        # held the address that works.
        if orig.from_session and is_full_session_id(orig.from_session):
            target = orig.from_session
        elif orig.to_kind == "session":
            # A session-addressed record without full sender provenance may use
            # its stored sender only when current discovery proves that token
            # uniquely. Never demote an unverified mutable alias.
            require_resolution = True
        elif LEGACY_HANDLE_RE.match(target):
            migrated = canonical_handle(target.split("-", 1)[1])
            print(
                f"note: stored sender {target!r} is a retired address form "
                f"(pre-flip record); resolving legacy token {migrated!r}.",
                file=sys.stderr,
            )
            target = migrated
            require_resolution = True

        _reply_to_name_handle(
            body_text,
            from_project=from_project,
            target=target,
            to_msg=to_msg,
            require_resolution=require_resolution,
            style_exception=style_exception,
            sender_session=sender_session,
            origin=mail_origin,
        )
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
                body_text,
                from_project=from_project,
                target=live_sender,
                to_msg=to_msg,
                style_exception=style_exception,
                sender_session=sender_session,
                origin=mail_origin,
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

    from fno.inbox.store import generate_msg_id

    reply_id = generate_msg_id()
    reservation, authored_words = _reserve_budget(
        sender=sender,
        recipient=recipient,
        body=body_text,
        msg_id=reply_id,
        allow_reason=style_exception,
    )
    existing = find_thread_by_msg_id(recipient, to_msg)
    if existing is not None:
        try:
            new_id = append_to_thread(
                existing.path,
                sender,
                body_text,
                msg_id=reply_id,
                word_count=authored_words,
                origin=mail_origin,
            )
        except Exception:
            _release_budget(reservation)
            raise
        payload = {"msg_id": new_id, "thread_path": str(existing.path), "appended": True}
        if json_out:
            typer.echo(json.dumps(payload))
        else:
            typer.echo(f"appended reply {new_id} to {existing.path.name} (in {recipient})")
        return

    try:
        handle = write_new_thread(
            recipient,
            sender,
            kind,
            body_text,
            msg_id=reply_id,
            replies_to=to_msg,
            refs=refs,
            word_count=authored_words,
            origin=mail_origin,
        )
    except Exception:
        _release_budget(reservation)
        raise
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
        ("delivery", env.delivery),
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
        delivery = f"/{m.delivery}" if m.delivery else ""
        typer.echo(
            f"{m.ts}  {who_from} -> {m.to}{kindtag} "
            f"({m.kind}{delivery}): {body1}"
        )


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


@mail_app.command("seed-provenance", hidden=True)
def cmd_seed_provenance() -> None:
    """Render this session's spawn-seed provenance envelope on stdout.

    Reads the ``FNO_SEED_PROV_*`` fields the launcher exported and renders them
    through the sole ``<fno_mail>`` renderer. Called by
    ``hooks/spawn-seed-provenance-session-start.sh``, which owns the
    ``source=startup`` gate and the hook JSON.

    Exit 1 with no output when there is nothing to attribute: a hand-started
    session, an operator spawn, or an unusable sidecar. The hook stays silent on
    that, because a session with no peer sender has nothing to attribute.
    """
    from fno.mail.seed_provenance import render_from_env

    rendered = render_from_env()
    if rendered is None:
        raise typer.Exit(code=1)
    print(rendered)


@mail_app.command("pane-prepare", hidden=True)
def cmd_pane_prepare(
    session: Optional[str] = typer.Option(
        None, "--session-id", help="mux session name."
    ),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    pane: int = typer.Option(..., "--pane", help="mux pane id."),
    harness: Optional[str] = typer.Option(
        None, "--harness",
        help="Harness hosting the pane (default: resolved from the agent registry).",
    ),
    style_exception: Optional[str] = typer.Option(
        None, "--style-exception",
        help="Reasoned one-send exception to the style and word-budget gates "
        "on enveloped prose (a --raw send never enters them).",
    ),
) -> None:
    """Gate and envelope a pane payload read from stdin; print it on stdout.

    The transport-agnostic half of an enveloped ``fno mux pane send``. The Rust
    verb shells here rather than mirroring the renderer (node x-1904 deleted the
    Rust mirror; ``fno.mail.envelope`` is the sole renderer) and fails closed on
    any non-zero exit.

    Exit 0 prints the bytes to type. Exit 3 refuses and names why on stderr: the
    pane is showing an option prompt, hosts no registered agent, or the body
    cannot be attributed. Exit 1 refuses on the style, body-cap, or word-budget
    gates the mail verbs enforce: this is the sole renderer every non-raw pane
    send passes through, so a sender refused by mail must not deliver the
    identical prose here instead.
    """
    from fno._flag_aliases import merge_deprecated_alias
    from fno.mail.pane_transport import PaneSendRefused, prepare, resolve_pane_identity

    session = merge_deprecated_alias(
        session, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    if not session:
        print("error: --session-id is required", file=sys.stderr)
        raise typer.Exit(code=2)

    # Read BYTES and decode once, explicitly. The Rust caller pipes a payload it
    # never re-encodes, and text-mode stdin does two things to it: universal
    # newlines rewrite a lone CR to LF, so a body carrying one is typed
    # differently from the body that was sent, and a non-UTF-8 byte raises
    # UnicodeDecodeError out of a read that has no handler, which the caller
    # reports as a renderer fault rather than as the bad input it is.
    raw = sys.stdin.buffer.read()
    # The UNTRUSTED boundary. Internal callers import `prepare` directly, so
    # everything arriving here came off a command line, and `wrap`'s
    # already-wrapped passthrough is a PREFIX test: a body opening with a
    # handcrafted `<fno_mail from="king" ...>` is returned verbatim, typed at a
    # pane, and read as peer mail from whoever it names. The audit row is
    # skipped too, because that check uses the same prefix test. Every other
    # producer refuses a body carrying the tag; this one has to as well.
    try:
        body = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        print(
            f"error: pane payload is not valid UTF-8 ({exc}); the envelope "
            f"quotes text and cannot frame arbitrary bytes.",
            file=sys.stderr,
        )
        raise typer.Exit(code=3) from exc
    _refuse_forged_envelope(body)
    # BOTH attribution containers, because `_already_wrapped` passes both. The
    # guard above knows only `<fno_mail`, so a handcrafted
    # `<cross-session-message from-name="king">` sailed through it, matched the
    # passthrough, and was typed at a worker's pane as an attributed peer order
    # with no envelope of ours and no audit row. Refusing one tag and not its
    # sibling is not a boundary. `claude_ask.rs` already refuses both.
    if _CROSS_SESSION_TAG_RE.search(body):
        print(
            "error: body carries a <cross-session-message> container. That tag "
            "marks a peer's attributed turn, and a body cannot supply its own. "
            "Send it as mail, or use --raw to type it as keystrokes.",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    # The relay contract's two gates, at the one choke point every non-raw pane
    # send fails closed through. A `--raw` send never reaches this process, so
    # its exemption is structural, and `--style-exception` keeps the mail
    # verbs' one escape shape.
    _enforce_body_cap(body)
    _enforce_style(body, allow_reason=style_exception)
    # ONE registry snapshot drives everything downstream: the envelope's `to`,
    # the identity the prompt gate pins, and the budget key. Resolving the
    # recipient here and letting `prepare` re-resolve it was a TOCTOU - a pane
    # reassigned between the two reads rendered an envelope addressed to the
    # old occupant while the gate validated the new one. The captured
    # name/fno_id now pin the gate, so a reassigned pane refuses instead.
    from fno.agents.self_stamp import resolve_self_session_id, stamp_from
    from fno.inbox.store import generate_msg_id

    sender = stamp_from(None)
    identity = resolve_pane_identity(session, pane)
    msg_id = generate_msg_id()
    try:
        rendered = prepare(
            body,
            session=session,
            pane_id=pane,
            harness=harness,
            sender=sender,
            to=identity.handle if identity else None,
            msg_id=msg_id,
            expected_name=identity.name if identity else None,
            expected_fno_id=identity.fno_id if identity else None,
        )
    except PaneSendRefused as exc:
        print(f"pane send refused: {exc}", file=sys.stderr)
        raise typer.Exit(code=3) from exc
    # Reserved only after attribution, prompt gating, and envelope construction
    # succeeded, so a refused send charges nothing. A later Rust transport
    # failure can leave a charge that expires with the window; that bounded
    # overcharge is the safe direction, because this renderer cannot learn
    # whether its parent delivered.
    #
    # The LEDGER keys on full session ids, both ends: an eight-hex handle
    # collides for codex siblings spawned inside one ~65s bucket, which fused
    # two distinct workers into one pair and refused normal parallel fanout.
    # The inbound-reset lookup keeps the display handles (bus envelopes carry
    # handles). A row without a session id still gets a budget, keyed on the
    # pane address rather than skipped.
    pane_address = f"pane {session}:{pane}"
    recipient = identity.handle if identity and identity.handle else pane_address
    _reserve_budget(
        sender=sender,
        recipient=recipient,
        body=body,
        msg_id=msg_id,
        allow_reason=style_exception,
        sender_key=resolve_self_session_id() or sender,
        recipient_key=(
            identity.session_id if identity and identity.session_id else recipient
        ),
    )
    print(rendered, end="")


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
# `fno agents mail send` is the durable-first publish (the envelope lands on the bus
# log before any live delivery is attempted). `fno agents mail unread`/`ack` are the
# cursor-based consume over that log: unread lists messages addressed to me
# after my cursor; ack advances it. These supersede the retired agents
# send/inbox/ack and inbox unread/ack verbs (the one messaging namespace).


# Live-lane failures where the recipient WAS live and reachable but the inject
# did not confirm (node x-1904). For these the durable preamble must NOT say
# "is not live" -- the recipient was live, so that wording read as a liveness
# lie and cost a wrong hypothesis on measured evidence. The receipt names the
# real cause instead.
_LIVE_LANE_FAILURE_REASONS = frozenset(
    {"not-confirmed", "attach-failed", "io-error", "mux-send-failed", "unsafe-text"}
)


def _is_live_lane_failure(reason: Optional[str]) -> bool:
    if not reason:
        return False
    return any(
        token in _LIVE_LANE_FAILURE_REASONS or token.startswith("mux-send-failed-")
        for token in reason.split(";")
    )


def _warn_deferred(target: str, *, project: bool = False, reason: Optional[str] = None) -> None:
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

    ``reason`` is the live lane's own cause (node x-1904). When it names a
    live-lane failure (see :data:`_LIVE_LANE_FAILURE_REASONS`) the recipient WAS
    live and reachable, so the preamble says so and names the cause rather than
    claiming "is not live" -- a receipt naming the wrong cause is worse than one
    naming none, because it sends the reader to diagnose a recipient that was
    never the problem. A None/unreachable reason keeps the honest not-live line.

    A lock timeout gets its own arm for the same reason. The per-agent flock is
    shared by every verb that touches the agent (send, ask, spawn, stop, rm), so
    a timeout says nothing about the recipient's liveness in EITHER direction.
    The not-live copy would send the reader to resurrect a session that is
    working fine; naming the holder a peer sender would tell the reader a
    just-stopped session is fine. The arm names neither and points at `peek`.

    Warning only - the durable enqueue succeeded, so exit stays 0."""
    from fno.agents.dispatch import LOCK_TIMEOUT_REASON

    if project:
        msg = (
            f"mail: project inbox {target} has no live drain; queued durably as "
            "recovery only - a session must drain the project inbox to read this, "
            "and may never do so\n"
            "  this is NOT delivery. Address a live session instead: "
            "`fno agents top` to find one, then `fno agents mail send <short-id>`"
        )
    elif reason == LOCK_TIMEOUT_REASON:
        msg = (
            f"mail: live delivery to {target} was not attempted (another verb "
            f"held {target}'s agent lock past the wait); queued durably. That "
            "holder is any verb on this agent - a send, an ask, a spawn, a "
            "stop, an rm - so the token proves nothing about the recipient in "
            "either direction. Do not resurrect it on this evidence, and do "
            "not read it as healthy either: check it.\n"
            "  a busy peer may not drain soon, so the rungs that stay open,\n"
            "  in this order - a bare re-send DOUBLE-DELIVERS, since the queued\n"
            "  copy still lands at the recipient's next drain:\n"
            f"    fno agents peek {target}     # still taking turns, or just stopped?\n"
            "    fno agents mail withdraw <id>      # retract the queued copy FIRST\n"
            f"    fno agents mail send {target} '<message>'  # then retry live\n"
            "  a withdraw that refuses because the recipient already claimed\n"
            "  the message is telling you it LANDED. Stop there: re-sending on\n"
            "  top of that is the double delivery this ladder exists to avoid."
        )
    elif _is_live_lane_failure(reason):
        msg = (
            f"mail: live delivery to {target} not confirmed ({reason}); queued "
            "durably as recovery only - the recipient was live and reachable, so "
            "the message may still land past the confirm window or sit until the "
            "recipient drains its inbox\n"
            "  live delivery NOT confirmed - do not wait for a reply, recover:\n"
            f"    fno agents peek {target}     # did it land? a busy peer may have queued it\n"
            f"    fno agents resume {target}   # wakes it (claude) or resumes it (other harnesses), then re-send\n"
            f"    fno agents attach {target}   # drive it yourself (claude)\n"
            # The rung that was missing. Every option above tries to reach the
            # recipient; when none of them can, the sender was left holding a
            # message that nagged every turn and could not be taken back.
            "    fno agents mail withdraw <id>      # none of the above? retract it"
        )
    else:
        msg = (
            f"mail: {target} is not live; queued durably as recovery only - the "
            "recipient must drain its inbox to read this, and may never do so\n"
            "  live delivery NOT confirmed - do not wait for a reply, recover:\n"
            f"    fno agents peek {target}     # did it land? a busy peer may have queued it\n"
            f"    fno agents resume {target}   # wakes it (claude) or resumes it (other harnesses), then re-send\n"
            f"    fno agents attach {target}   # drive it yourself (claude)\n"
            # The rung that was missing. Every option above tries to reach the
            # recipient; when none of them can, the sender was left holding a
            # message that nagged every turn and could not be taken back.
            "    fno agents mail withdraw <id>      # none of the above? retract it"
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


class UnavailableTokenError(Exception):
    """A short token could not be proven unique because stores were unreadable."""

    def __init__(self, failed: list[str], candidates: list[str]) -> None:
        super().__init__("session token resolution unavailable")
        self.failed = failed
        self.candidates = candidates


_RAW_SELF_TOKEN = object()


def _self_recipient(
    token: str,
    *,
    resolved_session_id: object = _RAW_SELF_TOKEN,
    full_only: bool = False,
) -> Optional[str]:
    """Canonical own address after resolution, or on a clean raw-token miss."""
    from fno.harness_identity import (
        canonical_handle,
        current_session_id,
        session_handle_tier,
    )

    own = current_session_id() or ""
    if not own:
        return None
    candidate = token if resolved_session_id is _RAW_SELF_TOKEN else resolved_session_id
    if not isinstance(candidate, str):
        return None
    tier = session_handle_tier(candidate, own)
    if (
        tier is None
        or (resolved_session_id is not _RAW_SELF_TOKEN and tier != 0)
        or (full_only and tier != 0)
    ):
        return None
    return canonical_handle(own)


def _is_self_send(recipient: Optional[str]) -> bool:
    """True when an already-addressed token is this ambient session."""
    return bool(recipient and _self_recipient(recipient))


def _resolve_token(token: str):
    """Resolve ``token`` to a reachable session BEFORE the envelope is addressed.

    Resolution has to precede wrapping. The durable recipient is the resolved
    session's canonical handle, and deriving it from the raw token instead would
    misaddress aliases because an alias is not a session identifier. Friendly
    names must resolve to a full session before the canonical handle is derived.

    Returns ``(reachable_or_None, lane_note_or_None)``. A ``None`` reachable with
    no note means every store was read cleanly and knows nothing -- the only case
    that still earns exit 16.
    """
    from fno.agents import discover as discover_mod

    try:
        reachable, ambiguous = discover_mod.resolve_reachable(token)
    except discover_mod.StoreReadError as exc:
        from fno.agents.store_fallback import is_full_session_id

        # Full ids are collision-free and remain safe to address directly. A
        # short token is different: the unreadable store may contain another
        # match, so even a durable write to the lone visible candidate is a
        # wrong-recipient side effect. Refuse before minting or writing mail.
        if not is_full_session_id(token):
            candidates = [exc.resolved.session_id] if exc.resolved is not None else []
            raise UnavailableTokenError(exc.failed, candidates) from exc
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


def _codex_daemon_socket_absent() -> bool:
    """True when the codex app-server control socket is absent (no daemon).

    Mirrors ``codex_app_server_socket_path`` in codex_inject.rs: the socket at
    ``$CODEX_HOME/app-server-control/app-server-control.sock`` exists only while
    a codex app-server daemon runs (``codex app-server daemon start``). A live
    mail send to a codex peer demotes to durable when it is absent, so the demote
    line names the fix rather than only the reason.

    Delegates to the doctor report so the send-time demote line and `fno doctor`
    can never disagree about where the socket lives (lazy import: doctor is a
    heavy module and this is one line on a cold path)."""
    from fno.doctor import _codex_app_server_report

    return not _codex_app_server_report().get("present", False)


def _resolve_pane_entry(resolved, recipient: Optional[str], token: Optional[str]):
    """The registry row behind a name-lane address, or None.

    Tries the resolved session id first (the strongest address), then the token
    the caller typed, then the canonical recipient handle. None means no row
    claims this address, which for ``--force`` is a refusal, not a fallback.
    """
    from fno.agents.registry import AgentResolutionError, resolve_agent

    candidates = [
        getattr(resolved, "session_id", None) if resolved is not None else None,
        token,
        recipient,
    ]
    for candidate in candidates:
        if not candidate:
            continue
        try:
            return resolve_agent(candidate).entry
        except (AgentResolutionError, OSError):
            continue
    return None


def _forced_pane_send(
    wrapped: str,
    *,
    entry,
    recipient: str,
    sender: str,
    msg_id: str,
    reply_to: Optional[str],
    provider: Optional[str],
    sender_harness: Optional[str],
    sender_session: Optional[str],
    sender_model: str,
    authored_words: Optional[int],
    reservation,
) -> bool:
    """``mail send --force``: type the wrapped body into the recipient's pane.

    ``--force`` changes only the TRANSPORT. Every mail semantic is kept: the same
    minted ``msg_id`` rides the envelope, the reply handle and authority footer
    travel with it, and an outbox row records the send. Before this existed, a
    live-miss forced the sender to switch verbs, and switching verbs is what lost
    all four.

    The gate runs. ``_mux_pane_send`` is called non-raw, so it reads the pane
    first and refuses one showing an option prompt -- a submit there would
    dismiss the payload and select the highlighted default, which is how a
    king's option-3 ruling once became the worker's option 1.

    The receipt says ``typed``, never ``delivered``. Bytes written to a PTY is
    not delivery and is certainly not action; the recipient's own transcript is
    the only thing that could confirm consumption, and this path does not read
    it. Returns True when the bytes were written.
    """
    from fno.agents.dispatch import _mux_pane_send
    from fno.bus.log import record_typed_delivery

    mux = getattr(entry, "mux", None) or {}
    pane_id = mux.get("pane_id")
    mux_session = mux.get("session")
    # Liveness, on the same rule the resolved lane states: an exited row keeps
    # its mux ref, and pane ids are reused across a mux restart, so a stale ref
    # types into whatever pane now holds that number. --force needs this MORE
    # than the ladder does, because it is reached after the ladder missed, which
    # is exactly the shape a dead recipient makes. --force overrides the
    # transport, never the fact that nobody is there.
    # NOT-TERMINAL, never `== "live"`. `live` is one of six non-terminal
    # statuses: `dispatch_spawn_pane` writes `spawning` until the SessionStart
    # restamp and `register_agent` defaults to `idle`, so an equality test
    # refused a pane that was alive and told the sender its id might belong to
    # somebody else. `TERMINAL_STATUSES` is shared for exactly this reason, and
    # a hand-rolled copy of the vocabulary is the drift its comment warns about.
    from fno.agents.dispatch import BUS_ONLY_POLICY, _delivery_policy_refusal
    from fno.agents.registry import TERMINAL_STATUSES

    # The ROW is the truth about which harness occupies this pane. `provider`
    # arrives from the caller's optional -H flag, which is normally unset, and
    # the name lane then floors it to a literal "claude". For a full codex
    # session id whose discovery listing missed -- the case --force exists for
    # -- that labelled a codex pane as claude on the one row whose entire
    # purpose is auditability.
    provider = getattr(entry, "harness", None) or provider

    # Bus-only FIRST, and up front, because it is a policy on the row rather
    # than an outcome of trying. `_mux_pane_send` returns False for it without
    # printing anything, which every other caller reads correctly as "demote to
    # durable". This one does not demote, so that silent False surfaced as an
    # exit 1 blaming a refusal that was never printed and a composer that was
    # never typed into, with no durable row written. The same send without
    # --force queues durable and reports a turn-boundary delivery.
    if _delivery_policy_refusal(entry) == BUS_ONLY_POLICY:
        _release_budget(reservation)
        print(
            f"error: {recipient} is bus-only by policy, so no pane paste is "
            f"allowed and --force has nothing it may do. Send without --force: "
            f"that queues durable and the recipient drains it.",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)

    status = getattr(entry, "status", None)
    if status in TERMINAL_STATUSES:
        _release_budget(reservation)
        print(
            f"error: --force types at a live prompt and {recipient} is "
            f"{status}. Its pane id may already belong to another session. "
            f"Send without --force to reach the durable bus.",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)
    if pane_id is None:
        _release_budget(reservation)
        print(
            f"error: --force types into a mux pane and {recipient} has none. "
            f"Send without --force (it queues durable), or spawn the recipient "
            f"with --substrate pane.",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)

    if not _mux_pane_send(entry, wrapped, guarded=False):
        _release_budget(reservation)
        # NOT "nothing was sent". One bool covers two worlds here: a refusal
        # before any bytes moved, and a paste that landed whose submit key then
        # failed, which leaves the envelope sitting in the recipient's composer.
        # No outbox row exists either way, so a sender told "nothing was sent"
        # retries and pastes it twice. The raw lane already says this honestly.
        print(
            f"error: --force did not confirm a send into pane {pane_id}, and no "
            f"row claims otherwise. The payload may still be sitting unsent in "
            f"the recipient's composer, so check the pane before retrying. The "
            f"refusal is on stderr above.",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)

    try:
        record_typed_delivery(
            msg_id=msg_id,
            sender=sender,
            recipient=recipient,
            body=wrapped,
            pane_id=str(pane_id),
            mux_session=str(mux_session) if mux_session else None,
            provider_from=sender_harness,
            provider_to=provider,
            in_reply_to=reply_to,
            from_session=sender_session,
            from_model=sender_model,
            to_kind="name",
            word_count=authored_words,
        )
    except Exception as exc:  # noqa: BLE001 - the bytes are already typed
        print(
            f"typed; outbox record failed; do not retry: {exc}",
            file=sys.stderr,
        )
    corr = f" re:{reply_to}" if reply_to else ""
    print(f"typed (pane {pane_id}) to {recipient} id:{msg_id}{corr}")
    return True


def _reply_session_for(from_name: Optional[str]) -> Optional[str]:
    """This session's full id, or None when ``--from-name`` set another address.

    `cmd_reply` prefers a record's `from_session` over its compact `from` handle
    on every lane, so stamping the ambient id unconditionally let it OUTRANK an
    explicit override: `--from-name web` sent the answer back to this session
    instead of to web, and the flag went quiet rather than erroring.

    `--from-self` is not that case. It derives `from_name` from this very
    session, so the handle matches here and the collision-safe id still rides,
    which is the whole point of the flag.
    """
    from fno.agents.self_stamp import resolve_self_session_id

    session = resolve_self_session_id()
    if not session or from_name is None:
        return session
    from fno.harness_identity import session_identity_key

    key = session_identity_key(session)
    # A caller may name itself by short handle or by full id; both are this
    # session, and only a THIRD address means "route the reply elsewhere".
    named = from_name.strip()
    exact = (key[:8], key, session)
    if named in exact:
        return session
    # Fold case ONLY for a hex address, where case carries no meaning and an
    # uppercase self-address would otherwise drop `from_session` without a word.
    # An opencode `ses_` id is case-SENSITIVE, so folding it could accept a
    # DIFFERENT session as this one, which is the same silent misroute in the
    # other direction. Blanket lowercasing here did exactly that.
    if all(c in "0123456789abcdefABCDEF-" for c in named) and named:
        lowered = named.lower()
        if any(lowered == candidate.lower() for candidate in exact):
            return session
    return None


def _name_lane_send(
    message: str,
    *,
    from_name: Optional[str],
    resolved,
    recipient: Optional[str] = None,
    provider: Optional[str] = None,
    reply_to: Optional[str] = None,
    token: Optional[str] = None,
    style_exception: Optional[str] = None,
    force: bool = False,
    origin: Optional[str] = None,
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
    from fno.agents.dispatch import (
        BUS_ONLY_POLICY,
        _mail_inject_claude,
        _mail_inject_codex,
        _mux_pane_send,
    )
    from fno.agents.registry import AgentResolutionError, resolve_agent
    from fno.agents.self_stamp import resolve_self_model, stamp_from
    from fno.agents.store_fallback import is_full_session_id, is_session_shaped
    from fno.dispatch_flags import infer_invoking_harness
    from fno.harness_identity import canonical_handle, session_identity_key
    from fno.inbox.store import (
        classify_durable_owner,
        generate_msg_id,
        write_new_thread,
    )
    from fno.mail.envelope import harness_for_provider, wrap_fno_mail

    self_send = False
    if resolved is not None:
        recipient = canonical_handle(resolved.session_id)
        provider = resolved.agent
        self_send = _self_recipient(
            recipient, resolved_session_id=resolved.session_id
        ) is not None
    elif token is not None:
        # Resolve BEFORE addressing. The durable copy must be addressed to the
        # resolved session's canonical handle -- deriving it from the raw token
        # would misaddress every alias. A clean miss may still be this ambient
        # session's full, canonical, or legacy identity; all three drain under
        # the canonical recipient without attempting to inject into self.
        self_recipient = None
        token_reachable, token_lane = _resolve_token(token)
        if token_reachable is None:
            self_recipient = _self_recipient(
                token, full_only=token_lane is not None
            )
            if self_recipient is not None:
                token_lane = "self-send"
        if token_reachable is not None:
            self_recipient = _self_recipient(
                token, resolved_session_id=token_reachable.session_id
            )
            if self_recipient is not None:
                token_reachable, token_lane = None, "self-send"
        self_send = token_lane == "self-send"
        if self_recipient is not None:
            recipient = self_recipient
        elif is_full_session_id(token):
            # The full id is the collision escape hatch: address the durable copy
            # by the full id (distinct), not canonical_handle. Two same-window
            # codex sessions share first-8, so canonicalizing a full id would
            # collapse both onto one durable key. drain-self reads the full id.
            recipient = session_identity_key(token)
        elif token_reachable is not None:
            recipient = canonical_handle(token_reachable.session_id)
        else:
            # A non-id token (a --name like blueprint-x-ce6e-glm) is not a mail
            # address, and writing it as a durable recipient strands the message:
            # the drain is handle-keyed, so a name never matches a session's
            # handle. Refuse rather than queue a message nobody can drain. A
            # bare hex short-handle of a session no store currently knows still
            # earns a durable write (it may yet drain if that session revives).
            if not is_session_shaped(token):
                # --force is the exception, and the refusal above says why it is
                # one: a name cannot be a DURABLE recipient because the drain is
                # handle-keyed. Forcing writes no such row. It types at a pane
                # the registry names, and the registry is exactly what resolves
                # a friendly name to the session behind it. Discovery is
                # liveness-gated, so this arm IS the situation --force exists
                # for, and refusing here made the flag unusable for the address
                # its own error text tells you to use.
                forced_entry = _resolve_pane_entry(None, None, token) if force else None
                # `harness_session_id` FIRST, matching `_pane_recipient_handle`
                # and `resolve_pane_recipient`. `AgentEntry.session_id` is a
                # property that returns claude's short_id (an attach jobId) when
                # one is present, and that is not a mail address. Bounded today
                # by the mux-XOR-bg invariant so it only ever reached a refusal
                # string, but three places deriving one address must not derive
                # it three ways.
                forced_session = (
                    getattr(forced_entry, "harness_session_id", None)
                    or getattr(forced_entry, "session_id", None)
                ) if forced_entry is not None else None
                if not forced_session:
                    raise UnreachableTokenError(token)
                recipient = canonical_handle(forced_session)
                provider = getattr(forced_entry, "harness", None) or provider
            else:
                recipient = token
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
    assert recipient is not None  # every routing branch above resolves the address
    sender = stamp_from(from_name)
    reservation, authored_words = _reserve_budget(
        sender=sender,
        recipient=recipient,
        body=message,
        msg_id=msg_id,
        allow_reason=style_exception,
    )
    sender_harness = infer_invoking_harness()
    sender_model = resolve_self_model()
    # The collision-safe reply address (node x-3a64). `from` stays the compact
    # display handle; `from_session` is the full id, and it is the one a
    # recipient can answer when two workers share a head-8 clock bucket. None
    # when this session's identity is unprovable, and then the attribute is
    # omitted rather than guessed.
    sender_session = _reply_session_for(from_name)
    wrapped = wrap_fno_mail(
        message,
        from_=sender,
        # Through harness_for_provider like every other send path: the wire
        # vocabulary is claude-code, and stamping a raw "claude" here made the
        # name lane the one producer disagreeing with dispatch, the relay, and
        # the Rust contract. "cli" survives as the honest no-harness value: the
        # mapper renders a MISSING provider as "unknown", never a vendor guess.
        harness=harness_for_provider(sender_harness) if sender_harness else "cli",
        model=sender_model,
        to=recipient,
        id=msg_id,
        reply_to=reply_to,
        from_session=sender_session,
        origin=origin,
    )

    # --force (node x-3a64): change the TRANSPORT, keep every mail semantic. The
    # branch sits here, after the envelope and the msg-id, and before the live
    # ladder: forcing means "type it into the pane", not "try the ladder first".
    # An automatic fallback is deliberately absent -- the pane path asks
    # permission from nothing, so a caller must opt into a transport that cannot
    # refuse.
    if force:
        if self_send:
            # The ladder parks a self-send durable because a self-inject is a
            # deadlock, and --force returned before ever reaching that. Typing a
            # wrapped envelope into this session's own composer (guarded=False,
            # so the turn interlock is skipped too) is what that bar exists to
            # stop. Self-injection has its own supported lane, and it is the raw
            # one: it carries a `self_ok` of its own and never wraps.
            _release_budget(reservation)
            print(
                "error: --force cannot type into this session's own prompt. "
                "Use --to-self --raw to self-inject a verb, or send without "
                "--force to leave yourself a durable note.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        entry = _resolve_pane_entry(resolved, recipient, token)
        if entry is None:
            _release_budget(reservation)
            print(
                f"error: --force needs a registry row naming the recipient's "
                f"pane, and none resolves for {recipient!r}. Send without "
                f"--force to reach the durable bus.",
                file=sys.stderr,
            )
            raise typer.Exit(code=1)
        _forced_pane_send(
            wrapped,
            entry=entry,
            recipient=recipient,
            sender=sender,
            msg_id=msg_id,
            reply_to=reply_to,
            provider=provider,
            sender_harness=sender_harness,
            sender_session=sender_session,
            sender_model=sender_model,
            authored_words=authored_words,
            reservation=reservation,
        )
        return

    injected = False
    woken_as: Optional[str] = None
    lanes: list[str] = []
    # node x-1904: the live lane's own cause when a claude inject misses, so the
    # durable receipt names it (e.g. not-confirmed) instead of a bare live-miss.
    live_reason: Optional[str] = None

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
                _codex_probe_reason: list = []
                injected = _mail_inject_codex(
                    probe_target, wrapped, reason_out=_codex_probe_reason
                )
                if not injected:
                    live_reason = (
                        _codex_probe_reason[0] if _codex_probe_reason else None
                    )
            else:
                _probe_reason: list = []
                injected = _mail_inject_claude(probe_target, wrapped, reason_out=_probe_reason)
                if not injected:
                    live_reason = _probe_reason[0] if _probe_reason else None
                if not injected and probe_agent is None:
                    _both_reason: list = []
                    injected = _mail_inject_codex(
                        probe_target, wrapped, reason_out=_both_reason
                    )
                    if not injected and _both_reason:
                        live_reason = _both_reason[0]
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
                elif live_reason == BUS_ONLY_POLICY:
                    # x-e21e: bus-only also declines the wake rung. Waking
                    # revives a second writer on the recipient; the policy
                    # named the durable bus as this recipient's ONE lane, so
                    # hold to it rather than spawning a bg copy of a session
                    # that declared itself bus-only.
                    pass
                else:
                    injected, woken_as, wake_lane = _wake_rung(
                        token_reachable, wrapped
                    )
                    if wake_lane:
                        lanes.append(wake_lane)

    if resolved is not None and self_send:
        lanes.append("self-send")
    elif resolved is not None:
        if provider == "claude":
            _resolved_reason: list = []
            injected = _mail_inject_claude(
                resolved.session_id, wrapped, reason_out=_resolved_reason
            )
            if not injected:
                live_reason = _resolved_reason[0] if _resolved_reason else None
        elif provider == "codex":
            _resolved_codex_reason: list = []
            injected = _mail_inject_codex(
                resolved.session_id, wrapped, reason_out=_resolved_codex_reason
            )
            if not injected:
                live_reason = (
                    _resolved_codex_reason[0] if _resolved_codex_reason else None
                )
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
                    # The mail lane holds the boolean contract; only the review
                    # lane consumes the widened "started"/"queued"/"unconfirmed".
                    # Name the failure values, never bool(): bool("unconfirmed")
                    # would read an unclassified frame as delivered.
                    delivered = _mux_pane_send(
                        entry, wrapped, guarded=False, confirm=True
                    )
                    injected = delivered not in (False, "unconfirmed")

    live = f" [live {resolved.agent} session {resolved.handle}]" if resolved is not None else ""
    corr = f" re:{reply_to}" if reply_to else ""
    # Surface the minted id so the sender can quote it and the recipient (who
    # also sees it in the injected <fno_mail id=...>) can reply --to it even
    # though a live-confirmed delivery writes no durable thread (US3).
    idtag = f" id:{msg_id}"
    if injected:
        from fno.bus.log import record_hosted_delivery

        try:
            record_hosted_delivery(
                msg_id=msg_id,
                sender=sender,
                recipient=recipient,
                body=wrapped,
                provider_from=sender_harness,
                provider_to=provider,
                in_reply_to=reply_to,
                from_session=sender_session,
                from_model=sender_model,
                to_kind="name",
                word_count=authored_words,
            )
        except Exception as exc:  # noqa: BLE001 - delivery already succeeded
            print(
                "delivery succeeded; outbox record failed; "
                f"do not retry: {exc}",
                file=sys.stderr,
            )
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
    # A live rung was actually attempted (and missed): the resolved-session
    # inject, or the token ladder run below discovery. A self-send and a
    # durable-only reply (neither resolved nor token) had no live attempt, so
    # the attended lane must not fire for them (Locked Decision 3: live-miss).
    live_attempted = (resolved is not None or token is not None) and not self_send
    # Resolving a session proves its IDENTITY, never that it can be reached: the
    # resolver deliberately accepts a row whose recorded pid is dead, so a handle
    # stays addressable. Asking `is_reachable` (the shared derivation, falsifiers
    # applied) is what separates the two. Reading `resolved is not None` alone
    # classed a provably dead recipient as live, which sent the durable fallback
    # to `live-drain` -- a turn-boundary drain on a worker that has no turns
    # left -- instead of `wake-daemon`, stranding the message.
    recipient_live = self_send or (
        resolved is not None and getattr(resolved, "is_reachable", True)
    )
    owner = classify_durable_owner(
        param_forced=False,
        recipient_live=recipient_live,
        recipient_resumable=not recipient_live,
    )

    # x-e21e: a bus-only queue is DESIGNED, not stranded. The recipient polls
    # the durable bus at each turn boundary (notify-self), so this is delivery
    # on the recipient's terms -- no recovery warning, no escalation, and a
    # receipt that says so instead of reading as a live-miss.
    bus_only = live_reason == BUS_ONLY_POLICY
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
            # The durable floor carries the same full sender id the live
            # envelope does, so a drained reply resolves the collision-safe
            # address exactly as a live one does (node x-3a64).
            from_session=sender_session,
            word_count=authored_words,
            origin=origin,
        )
    except (OSError, ValueError, RuntimeError) as exc2:
        if not injected:
            _release_budget(reservation)
        print(f"durable envelope write failed for {recipient!r}: {exc2}", file=sys.stderr)
        raise typer.Exit(code=12) from exc2
    if not bus_only:
        _warn_deferred(recipient, reason=live_reason)
    # Routing-reason disclosure (US10): name WHY this is durable so a delivery
    # bug is diagnosable from the sender's own terminal. A self-send can never
    # inject itself; everything else here is a live miss. When the live lane
    # named its own cause (node x-1904), carry that token so a miss to a LIVE
    # recipient reads as its real cause (e.g. not-confirmed), not a bare
    # live-miss that reads as a dead recipient. A bus-only queue is neither: it
    # is the recipient's declared delivery policy, and its receipt says the
    # message WILL surface at the recipient's turn boundary.
    # x-481e: a busy-mode hold is a bus-only flag with a clock on it, and the
    # generic receipt above would promise a turn boundary that is not coming.
    # Say what is actually true: it is held, here is when it lands.
    hold_note = None
    if bus_only:
        from fno.mail import hold as _hold

        hold_note = _hold.bounce_reason(recipient)
    if bus_only:
        reason = hold_note or "bus-only: recipient polls the bus at each turn boundary"
    else:
        reason = "self-send" if self_send else (live_reason or "live-miss")
    hint = ""
    if hold_note:
        hint = f" `fno agents mail withdraw {msg_id}` retracts it."
    if not self_send and not bus_only and provider == "codex" and _codex_daemon_socket_absent():
        hint = (
            " codex app-server daemon not running: run "
            "`codex app-server daemon start`, then restart the session "
            "(the socket must exist before the codex TUI starts)"
        )
    print(f"{th.thread_id} queued (durable) for {recipient}{live}{corr} [{reason}]{hint}")
    # Live-miss escalation lane (node x-1904 widened this from attended-only). A
    # miss to an operator-attended session is the stranded case: the human is not
    # watching the drain, so nothing else surfaces it. A miss to a worker the
    # resolver reports reachable is the same case from the other side: worker
    # rows (origin=None) never matched the attended lane, so a live miss to a
    # busy worker sat silently until a 30-min unclaimed hook surfaced it
    # (msg-133d96). Both escalate through the existing _escalate_to_human ->
    # _sent_unclaimed / `fno agents mail status` path; no new pending indicator. The
    # change is which misses reach the send-time nudge, not a second mechanism.
    # Best-effort, never affects the send's exit code or receipt.
    attended = _recipient_is_attended(recipient)
    # Reuse the `recipient_live` verdict computed above rather than asking
    # `is_reachable` a second time: one derivation, one default, and the two
    # cannot disagree about the same recipient.
    resolved_reachable = resolved is not None and recipient_live
    if live_attempted and not bus_only and (attended or resolved_reachable):
        esc_reason = "attended-miss" if attended else "reachable-miss"
        if (
            _escalate_to_human(
                stamp_from(from_name), recipient, message, reason=esc_reason, msg_id=msg_id
            )
            == "escalated"
        ):
            print(f"escalated to human ({recipient}) [{esc_reason}]", file=sys.stderr)


def _job_lane_send(
    message: str,
    token: str,
    *,
    from_name: Optional[str],
    style_exception: Optional[str] = None,
    origin: Optional[str] = None,
) -> None:
    """Deliver to a JOB address (``node:<id>`` / ``pr:<n>``), resolved to whoever
    holds the claim RIGHT NOW (x-8f8c part 2).

    A job address names the work, not the process: it survives the holder's death
    because the durable copy is addressed to ``node:<id>`` and a successor drains
    it at SessionStart. This is the structural fix for the dead-handle strand --
    a session handle expired faster than the message, so mail to it accumulated on
    a queue the dead session never drained.

    Two outcomes, matching the name-lane one-line stdout contract (no separate
    delivery-verification receipt -- the existing receipt is the send-time inject
    confirmation, and a second receipt claiming delivery happened is the shape
    that has lied four times):

    - holder exists (claim live/suspect): live-inject to the holder's session. On
      a confirmed inject, that IS delivery (no durable copy, same as a name-lane
      hosted send). On a live miss, durable-floor to ``node:<id>`` so a successor
      drains it.
    - no holder (free/stale/corrupted/no-node): REFUSE, exit 16, queue nothing.
      Queueing would strand the message at the job address -- the defect again,
      one address over.

    ``pr:<n>`` is normalized to ``node:<id>`` by the resolver (graph lookup), so
    the durable envelope always carries the canonical node address and the drain
    consumes one address space.
    """
    from fno.agents.dispatch import (
        BUS_ONLY_POLICY,
        _mail_inject_claude,
        _mail_inject_codex,
    )
    from fno.agents.self_stamp import resolve_self_model, stamp_from
    from fno.dispatch_flags import infer_invoking_harness
    from fno.inbox.store import DurableOwner, generate_msg_id, write_new_thread
    from fno.mail.envelope import harness_for_provider, wrap_fno_mail
    from fno.mail.job_address import resolve_job_address

    job = resolve_job_address(token)
    if job is None:
        # Not a job address -- should not reach here (cmd_send gates on the
        # prefix), but fail closed rather than misaddress.
        print(f"error: not a job address: {token!r}", file=sys.stderr)
        raise typer.Exit(code=2)

    if not job.has_holder:
        note = f" ({job.note})" if job.note else ""
        print(
            f"mail: {token}{note} has no live holder "
            f"(claim state: {job.state}); not queued.\n"
            f"  a job address with no holder would strand. "
            f"Retry when a /target session holds {job.address}.",
            file=sys.stderr,
        )
        raise typer.Exit(code=16)
    # Bound to a local so the type-checker narrows Optional -> str past the
    # has_holder guard (a property it cannot track across).
    session_id = job.session_id
    assert session_id is not None  # has_holder is True iff session_id is set

    msg_id = generate_msg_id()
    # The durable recipient is the JOB (node:<id>), not the holder's session
    # handle: this is what makes the address outlive the session. pr:<n> was
    # already normalized to node:<id> by the resolver.
    recipient = job.address
    sender = stamp_from(from_name)
    _reservation, authored_words = _reserve_budget(
        sender=sender,
        recipient=recipient,
        body=message,
        msg_id=msg_id,
        allow_reason=style_exception,
    )
    sender_harness = infer_invoking_harness()
    sender_model = resolve_self_model()
    # Same collision-safe reply address the name lane carries: a job address
    # routes the message IN, and the holder still answers the sender, so the
    # sender needs an id that survives a head-8 collision. Bound to a local
    # because the wire is not the only place it has to land -- both durable
    # records below read it too, and a reply consults THOSE, not the envelope.
    sender_session = _reply_session_for(from_name)
    wrapped = wrap_fno_mail(
        message,
        from_=sender,
        harness=harness_for_provider(sender_harness) if sender_harness else "cli",
        model=sender_model,
        to=recipient,
        node=job.node_id,
        id=msg_id,
        from_session=sender_session,
        origin=origin,
    )

    # Live-inject to the current holder's session. Inject targets the session id
    # (control.sock / codex daemon are keyed by it, cwd-independent), so a holder
    # in another worktree is reachable from this sender's cwd. A bus-only holder
    # (x-e21e) is refused inside the injector, so the receipt below names the
    # policy rather than a miss.
    provider = job.harness or "claude"
    _job_reason: list = []
    if provider == "codex":
        injected = _mail_inject_codex(session_id, wrapped, reason_out=_job_reason)
    else:
        injected = _mail_inject_claude(session_id, wrapped, reason_out=_job_reason)
    bus_only = not injected and BUS_ONLY_POLICY in _job_reason

    holder_tag = f" [holder {provider} {session_id[:8]}]"
    if injected:
        from fno.bus.log import record_hosted_delivery

        try:
            record_hosted_delivery(
                msg_id=msg_id,
                sender=sender,
                recipient=recipient,
                body=wrapped,
                provider_from=sender_harness,
                provider_to=provider,
                from_session=sender_session,
                from_model=sender_model,
                to_kind="node",
                word_count=authored_words,
            )
        except Exception as exc:  # noqa: BLE001 - delivery already succeeded
            print(
                "delivery succeeded; outbox record failed; "
                f"do not retry: {exc}",
                file=sys.stderr,
            )
        print(f"delivered (hosted) to {recipient}{holder_tag} id:{msg_id}")
        return

    # Live miss: durable floor addressed to the JOB, written through the SAME
    # write_new_thread the name lane uses (node:<id> is a first-class recipient
    # now that inbox_dir_for admits ':'). A successor (or this holder's next
    # drain) surfaces it via scan_unread(node:<id>). Owner is wake-daemon: the
    # holder exists but the inject missed, so the message waits for a drain
    # (resumable), not a turn boundary -- live-drain's 1h "drains next turn"
    # assumption does not hold for a job address whose only drain is SessionStart.
    owner = DurableOwner.WAKE_DAEMON
    try:
        th = write_new_thread(
            recipient=recipient,
            sender=stamp_from(from_name),
            kind="send",
            body=wrapped,
            msg_id=msg_id,
            provider_to=provider,
            to_kind="node",
            owner=owner.value,
            from_session=sender_session,
            origin=origin,
            word_count=authored_words,
        )
    except (OSError, ValueError, RuntimeError) as exc:
        _release_budget(_reservation)
        print(
            f"durable envelope write failed for {recipient!r}: {exc}",
            file=sys.stderr,
        )
        raise typer.Exit(code=12) from exc
    # One stdout line (the receipt contract) + an advisory on stderr naming the
    # recovery: the message waits for the holder's next drain, not a reply window.
    # A bus-only holder skipped the inject by policy (x-e21e), so the receipt
    # says the policy, not a miss. A node:<id> thread is NOT surfaced by the
    # holder's turn-boundary notify-self (that scan reads only the session's
    # own handle): it drains at a holder's SessionStart scan, so the receipt
    # must not promise turn-boundary visibility.
    if bus_only:
        print(
            "mail: holder is bus-only by delivery policy; queued durable "
            "until a holder drains",
            file=sys.stderr,
        )
        print(
            f"{th.thread_id} queued (durable) for {recipient} "
            f"[bus-only: a holder drains it by policy]{holder_tag}"
        )
        return
    print(f"mail: {recipient} live-inject missed; durable until a holder drains",
          file=sys.stderr)
    print(f"{th.thread_id} queued (durable) for {recipient} [job-live-miss]{holder_tag}")


# Send-time human escalation for a question, per (sender, recipient). A burst
# re-nudges every window rather than once forever (marker refreshed only on an
# actual escalation, so the window runs from the last nudge, not the first send).
_ESCALATION_DEBOUNCE_S = 300


def _recipient_is_attended(recipient: str) -> bool:
    """True iff ``recipient``'s registry row was stamped ``origin=operator`` at
    a hand-start (SessionStart register hook / ``fno agents register``).

    Attendance is declared at registration, never inferred at send time, so a
    row missing the field (a spawn/host worker, or a pre-change row) reads as
    not-attended -- fail toward silence. Never raises: an unreadable registry or
    an unresolved recipient escalates nothing, so the send still succeeds.
    """
    try:
        from fno.agents.registry import load_registry, resolve_agent_in

        entry = resolve_agent_in(load_registry(), recipient).entry
    except Exception:  # noqa: BLE001 - a registry read failure never breaks the send
        return False
    return getattr(entry, "origin", None) == "operator"


def _escalate_to_human(
    sender: str,
    recipient: str,
    summary: str,
    reason: str,
    msg_id: str | None = None,
) -> str:
    """Notify the human at send time that mail needs them, and surface it in the
    needs-me mux overlay.

    ``reason`` is ``"question"`` (a --kind question send; Locked Decision 7: a
    question NEVER autonomous-responds - only the human answers it) or
    ``"attended-miss"`` (a send to an operator-attended session that fell to the
    durable floor) or ``"reachable-miss"`` (the same miss to a worker the
    resolver reports reachable). Every reason flows through this ONE helper so
    the overlay event is emitted from a single place; a second emit site would
    leave one reason un-surfaced (the silent-eat this exists to close). A reason
    added here must also be added to :data:`fno.events.MAIL_ESCALATION_REASONS`
    and the schema enum, or the overlay emit raises and is swallowed by the
    best-effort guard below - the nudge then reaches the notifier only.

    Debounced per (sender, recipient) so a chatty peer cannot spam the queue, and
    the debounce gates BOTH the notifier and the event (one event per
    non-debounced escalation, zero on debounced). The caller writes the durable
    thread regardless, so the ambient unread count stays truthful even when this
    nudge is debounced. Best-effort throughout: a notifier, events-write, or
    filesystem failure never breaks the send. Returns ``"escalated"`` (the human
    was notified), ``"debounced"`` (a recent nudge for this pair suppressed it),
    or ``"notifier-unavailable"`` (no OS notifier on this host, so nothing
    displayed - the caller must not claim escalation; the overlay event still
    fired).
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
    # Debounce gate passed: this is a real escalation. Emit the overlay event
    # BEFORE the notifier verdict - the overlay is an independent surface that
    # must render even on a headless host where the notifier is unavailable (the
    # whole point of surfacing in the mux). Best-effort: an events-write failure
    # never breaks the notifier or the send.
    try:
        from fno.events import append_event, mail_escalation

        append_event(
            mail_escalation(
                reason=reason,
                sender=sender,
                recipient=recipient,
                summary=summary.split("\n", 1)[0][:120],
                msg_id=msg_id,
            )
        )
    except Exception:  # noqa: BLE001 - an overlay miss never breaks the send
        pass
    # Only report escalation when the notification actually displayed:
    # send_notification returns (code, err) and a nonzero code means no OS
    # notifier (a headless host), so the human was NOT notified.
    try:
        from fno.notify._impl import send_notification

        one_line = summary.split("\n", 1)[0][:120]
        label = "missed you" if reason in ("attended-miss", "reachable-miss") else "question"
        code, _err = send_notification(
            f"fno agents mail: {label} from {sender}",
            f"{one_line} - run `fno agents mail drain-self`",
        )
    except Exception:  # noqa: BLE001 - a notifier failure never breaks the send
        code = 1
    return "escalated" if code == 0 else "notifier-unavailable"


_CODEX_REVIEW_VERBS = frozenset({"/review", "/code-review"})
_COMMIT_SHA = re.compile(r"[0-9a-fA-F]{7,64}")
_EXPLICIT_PR_REVIEW = re.compile(
    r"^HEAD (?P<head>[0-9a-fA-F]{7,64}) of PR (?P<pr>[1-9][0-9]*) "
    r"against origin/(?P<base>[A-Za-z0-9][A-Za-z0-9._/-]*)$"
)


def _codex_default_review_base(cwd: str | None) -> str | None:
    """Return the repository-declared origin default branch, never a guessed name."""
    if not cwd:
        return None
    import subprocess

    try:
        proc = subprocess.run(
            [
                "git",
                "-C",
                cwd,
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    ref = proc.stdout.strip()
    return ref if proc.returncode == 0 and ref else None


def _codex_review_target(
    payload: str, *, default_base: str | None = None
) -> tuple[str | None, bool]:
    """Resolve the structured review target without inventing custom instructions."""
    parts = payload.split(maxsplit=1)
    if len(parts) == 1:
        target = f"baseBranch:{default_base}" if default_base else None
        return target, False
    remainder = parts[1].strip()
    explicit_pr = _EXPLICIT_PR_REVIEW.fullmatch(remainder)
    if explicit_pr:
        # The PR/HEAD identity remains in the raw payload for the author and
        # audit trail. Codex review/start receives the PR's explicit base scope,
        # which is the diff it must inspect rather than its ambient cwd.
        return f"baseBranch:origin/{explicit_pr.group('base')}", False
    if remainder.startswith("HEAD "):
        # A malformed explicit target must not fall through to
        # uncommittedChanges, which would review a different diff.
        return None, False
    base = remainder.split()
    if base[0] == "--base":
        # A named base is an explicit scope request: a malformed form (dangling
        # flag, a flag-like value, trailing tokens) must refuse rather than
        # fall through to uncommittedChanges, which silently reviews a
        # different diff than the one the operator asked for.
        if len(base) == 2 and not base[1].startswith("--"):
            return f"baseBranch:{base[1]}", False
        return None, False
    if remainder == "--uncommitted":
        return "uncommittedChanges", False
    if _COMMIT_SHA.fullmatch(remainder):
        return f"commit:{remainder}", False
    if remainder.startswith("custom:") and remainder != "custom:":
        return remainder, False
    return "uncommittedChanges", True


def _raw_send(
    name,
    payload,
    *,
    self_ok: bool,
    check: bool = False,
    style_exception: Optional[str] = None,
    review_request: bool = False,
    origin: Optional[str] = None,
) -> None:
    """``fno agents mail send --raw``: fire a verb in a peer by injecting ``payload``
    UNWRAPPED at the recipient's prompt line (no ``<fno_mail>`` envelope), so the
    REPL's slash parser runs it before the model sees it.

    This is the only way to make a verb the model is barred from invoking
    actually run (a harness built-in like ``/compact``, or a skill the model may
    not self-invoke). Ordinary wrapped mail already works for model-invocable
    verbs. See node x-c24d and ``internal/fno/plans/20260806-bare-verb-injection.md``.

    Never queues durable on any transport result: a not-confirmed raw inject may
    still land, and re-queueing it is how a verb fires twice at the wrong moment.

    ``check`` runs every precondition and INJECTS NOTHING, printing ``injectable``
    or ``not-injectable: <reason>``. It exists because advice is worse than
    useless when the mechanism it prescribes cannot fire: a Stop hook that told
    every session to self-inject ``/compact`` sent one session round the loop
    twice before it gave up. A caller gates on this rather than guessing from the
    session's shape, and gets the same resolution the real send would run.
    """
    from fno.agents.dispatch import (
        _mail_inject_claude,
        _mux_pane_send,
        _review_start_codex,
        keystroke_lane,
        mail_inject_probe,
    )
    from fno.agents.registry import AgentResolutionError, resolve_agent

    def _refused(reason: str, *, usage: bool = False) -> None:
        # Under --check a refusal about the SESSION is an ANSWER, not an error:
        # "no injection path, and here is why" on stdout, so one caller reads one
        # shape whether the miss was an unregistered session, a wrong lane, or an
        # absent socket. A refusal about the CALL (`usage`: a malformed payload) is
        # NOT: printing it as not-injectable would state a verdict about a session
        # this run never measured, which is the exact failure --check exists to
        # prevent. Those stay a usage error on stderr at exit 2.
        if check and not usage:
            print(f"not-injectable: {reason}")
            raise typer.Exit(code=1)
        print(f"refused: {reason}", file=sys.stderr)
        raise typer.Exit(code=2)

    def _unmeasurable(reason: str) -> None:
        # --check's THIRD answer: the evidence needed to decide could not be read.
        # An unreadable registry says nothing about whether the session has a
        # path, so it must not be reported as not-injectable (same reason
        # probe-unavailable is separate). Without --check it is an ordinary refusal.
        if check:
            print(f"unmeasurable: {reason}")
            raise typer.Exit(code=3)
        print(f"refused: {reason}", file=sys.stderr)
        raise typer.Exit(code=2)

    # 1. Leading slash after stripping; refuse bare "/" and whitespace-only. The
    #    leading slash is the marker a human skimming a transcript reads as
    #    "invocation, not a typed ruling".
    stripped = payload.strip()
    if not stripped.startswith("/"):
        _refused(
            "payload must start with / (a verb invocation); free prose belongs "
            "in an ordinary wrapped send",
            usage=True,
        )
    if stripped == "/":
        _refused("payload is just '/'; nothing to invoke", usage=True)

    # 2. Single line: the transport is one bracketed paste plus one CR, so a
    #    second line would ride in as trailing content on the same turn.
    if "\n" in payload or "\r" in payload:
        _refused(
            "payload must be a single line (a second line rides in as trailing "
            "content on the same submitted turn)",
            usage=True,
        )


    # Raw sends bypass the ordinary wrapped-mail entry points, so enforce their
    # shared size ceiling and structure gate here before any of the reachable
    # transports can fire. Under --check the cap refusal is a usage error
    # (exit 2), never a session verdict: exit 1 is the not-injectable code.
    _enforce_body_cap(stripped, usage=check)
    _enforce_style(stripped, allow_reason=style_exception)

    # 2b. Forged envelope: a raw payload starts with "/", so it cannot itself
    #     be a `<fno_mail>` tag, but it can still smuggle one mid-line. The mux
    #     lane (`_mux_pane_send` below) pastes this string directly and never
    #     reaches the Rust mail-inject binary's own check, so this is the only
    #     door for that lane.
    from fno.mail.envelope import contains_fno_mail_tag

    if contains_fno_mail_tag(stripped):
        _refused(
            "payload contains an <fno_mail> tag. The envelope frames peer mail; "
            "a payload cannot contain one",
            usage=True,
        )

    # 3. Resolve name -> registry row. The lane lives on the row. An UNAVAILABLE
    #    resolution (a registry this fno cannot read) is not a miss: it is the
    #    unmeasurable answer, kept apart from "resolved, and there is no path".
    lookup_name = name
    if self_ok:
        from fno.agents.self_stamp import resolve_self_session_id

        lookup_name = resolve_self_session_id() or name
    try:
        entry = resolve_agent(lookup_name).entry
    except AgentResolutionError as exc:
        if exc.unavailable:
            _unmeasurable(f"registry unreadable, so {name!r} could not be resolved")
        _refused(f"could not resolve {name!r} to a registered agent")
    except OSError:
        _unmeasurable(f"registry unreadable, so {name!r} could not be resolved")

    session_id = getattr(entry, "harness_session_id", None) or ""

    # 5. Self-send redirect. The transport is send-keys: an unwrapped payload is
    #    text typed at a prompt line, so nothing here can be a capability
    #    boundary - downstream of the keystroke nothing distinguishes an operator
    #    typing a slash from an agent injecting it. The check below is a usability
    #    redirect: a caller who addressed this own session positionally almost
    #    certainly meant --to-self (the opt-in that parks the payload in
    #    positional #1 and derives the recipient). Fail closed on an unknown
    #    session id: _self_recipient compares the RESOLVED id, and
    #    session_handle_tier returns None for an empty token, so a row carrying
    #    no harness_session_id (legacy/unmigrated) would sail past the self-check
    #    even when it IS this session. No id, no soundness.
    # --check is exempt from both self-address guards below. They are a usability
    # redirect and a soundness floor for an ACTUAL keystroke; a check injects
    # nothing, so there is no boundary to hold, and refusing here would answer
    # "no path" to a caller that has one. It also lets a caller ask about itself by
    # the session id it was HANDED (a Stop hook gets one in its payload) instead of
    # reconstructing its own identity from the environment.
    if not session_id and not (self_ok or check):
        _refused(
            f"{name!r} resolves to a registry row with no harness_session_id, so "
            "the self-send check cannot be decided; re-register the row "
            "(`fno agents register`), or if this IS your session address it with "
            "--to-self instead of a positional id"
        )
    # Order matters: under --check the self test is decided again below, so gate on
    # the cheap booleans first rather than resolving ambient identity twice.
    if not (self_ok or check) and _self_recipient(name, resolved_session_id=session_id):
        _refused(
            "you addressed this session. For a plain self-note the envelope is "
            "fine:\n    fno agents mail send <own-id> \"text\"\n"
            "To fire a verb at your own prompt line (no envelope, so the slash "
            "parses):\n    fno agents mail send '<payload>' --to-self --raw"
        )

    # 3b. Bus-only delivery policy (x-e21e): this recipient's mail belongs on
    #     the durable bus, and the raw lane never queues durable -- so a raw
    #     send here can do nothing and must refuse loud rather than silently
    #     not-deliver. Under --check this refusal is an ANSWER about the
    #     session, the same not-injectable shape as any other no-path verdict.
    from fno.agents.dispatch import BUS_ONLY_POLICY

    if getattr(entry, "delivery_policy", None) == BUS_ONLY_POLICY:
        _refused(
            f"{name!r} has delivery-policy bus-only: prompt-line injection is "
            "forbidden for this recipient. Send wrapped mail instead - it "
            "queues durable and surfaces at their turn boundary"
        )

    # Derive provenance before routing: daemon review/start returns before the
    # keystroke transports below, but its unwrapped invocation needs the same
    # actor record.
    from fno.agents.self_stamp import resolve_self_handle, stamp_from
    from fno.harness_identity import canonical_handle
    from fno.inbox.store import generate_msg_id

    transport_sender = resolve_self_handle()
    sender = stamp_from(transport_sender)
    raw_recipient = canonical_handle(session_id)

    def _reserve_raw():
        raw_msg_id = generate_msg_id()
        reservation, authored_words = _reserve_budget(
            sender=sender,
            recipient=raw_recipient,
            body=stripped,
            msg_id=raw_msg_id,
            allow_reason=style_exception,
        )
        return raw_msg_id, reservation, authored_words

    def _record_raw(raw_msg_id: str, authored_words: int) -> None:
        from fno.bus.log import record_hosted_delivery

        try:
            record_hosted_delivery(
                msg_id=raw_msg_id,
                sender=sender,
                recipient=raw_recipient,
                body=stripped,
                provider_to=entry.harness,
                to_kind="session",
                word_count=authored_words,
            )
        except Exception as exc:  # noqa: BLE001 - delivery already succeeded
            print(
                "delivery succeeded; outbox record failed; "
                f"do not retry: {exc}",
                file=sys.stderr,
            )

    # 4. Route by the actual lane. Mux-hosted Codex is a keystroke lane like any
    #    other mux pane; only a Codex app-server thread uses structured review/start.
    lane, is_keystroke = keystroke_lane(entry)
    if not is_keystroke:
        verb = stripped.split(maxsplit=1)[0]
        if lane == "codex-daemon":
            # --check answers before the RPC fires: a review verb HAS a path on
            # this lane (the structured RPC), everything else has none. The
            # probe claims path-existence only, same as the keystroke branches -
            # but only for preconditions it cannot cheaply decide; a missing
            # binary or an unresolvable target WOULD refuse the send, so the
            # check answers them rather than promising a path the send lacks.
            if check and verb not in _CODEX_REVIEW_VERBS:
                print(
                    "not-injectable: codex-daemon has no prompt line; only "
                    "/review and /code-review map to its review/start RPC"
                )
                raise typer.Exit(code=1)
            if verb not in _CODEX_REVIEW_VERBS:
                _refused(
                    f"{name!r} is a codex app-server thread, which has no prompt "
                    "line - a slash payload cannot parse there. The app-server "
                    "exposes turn/start (text to the model, no slash parsing) and "
                    f"review/start (the reviewer); {verb!r} maps to neither.\n"
                    "  - to have the codex model READ this, drop --raw (a wrapped "
                    "send delivers it as text, which is all any codex lane can do "
                    "with it)\n"
                    f"  - if {verb!r} is a codex TUI built-in (/compact and "
                    "friends), no fno lane can fire it on a daemon thread; host "
                    "the session in a mux pane, where --raw pastes at the real "
                    "prompt line and the TUI parser runs it"
                )
            if check:
                from fno import rust_binary

                if rust_binary.resolve_installed_binary() is None:
                    print(
                        "not-injectable: the fno-agents binary is absent or too "
                        "old (run `fno doctor`), so review/start has no transport"
                    )
                    raise typer.Exit(code=1)
            default_base = (
                _codex_default_review_base(getattr(entry, "cwd", None))
                if stripped in _CODEX_REVIEW_VERBS
                else None
            )
            target, ignored_remainder = _codex_review_target(
                stripped, default_base=default_base
            )
            if check:
                if target is None:
                    print(
                        "not-injectable: no resolvable review target (bare verb "
                        "with no origin default branch, or unparsable arguments); "
                        "retry with '/review --base <branch>' or "
                        "'/review --uncommitted'"
                    )
                    raise typer.Exit(code=1)
                print("injectable: codex-daemon review/start RPC")
                raise typer.Exit(code=0)
            if target is None:
                _refused(
                    f"{name!r} {verb} has no resolvable review target - a bare "
                    "verb with no origin default branch, or arguments after the "
                    "verb do not parse; retry with '/review --base <branch>' or "
                    "explicitly request '/review --uncommitted'"
                )
            assert target is not None
            raw_msg_id, reservation, authored_words = _reserve_raw()
            review_kwargs = {
                "audit_payload": stripped[:512],
                "audit_sender": transport_sender,
                "audit_target_cwd": getattr(entry, "cwd", None),
            }
            if origin is not None:
                review_kwargs["origin"] = origin
            receipt = _review_start_codex(session_id, target, **review_kwargs)
            if receipt.get("delivered"):
                _record_raw(raw_msg_id, authored_words)
                note = " (unrecognized remainder ignored)" if ignored_remainder else ""
                print(
                    f"review/start target={target} delivery=inline "
                    f"turn={receipt.get('turn_id', '')} "
                    f"review_thread={receipt.get('review_thread_id', '')}{note}"
                )
                raise typer.Exit(code=0)
            reason = str(receipt.get("reason") or "rpc-error")
            if reason == "not-confirmed":
                print(
                    f"warning: {name!r} codex review/start was not-confirmed after "
                    "the request was sent; the review may already be running. "
                    "Inspect the thread before deciding what happened; do not retry "
                    "blindly",
                    file=sys.stderr,
                )
                raise typer.Exit(code=0)
            _release_budget(reservation)
            if reason == "no-daemon":
                _refused(
                    f"{name!r} codex review/start failed: no-daemon; run "
                    "`codex app-server daemon start` and retry"
                )
            if reason in ("stale-binary", "binary-not-found"):
                _refused(
                    f"{name!r} codex review/start failed: {reason}; the deployed "
                    "fno-agents binary is absent or rejected the invocation - "
                    "run `fno doctor --fix` and retry"
                )
            _refused(f"{name!r} codex review/start failed: {reason}")
        if check:
            print(f"not-injectable: {lane} is not a prompt-line keystroke lane")
            raise typer.Exit(code=1)
        _refused(
            f"{name!r} resolves to the {lane} lane, which is not a prompt-line "
            "keystroke path; a raw slash payload would reach the model as text, "
            "not fire"
        )

    # --check stops here, one step short of the keystroke. Each lane is asked the
    # strongest question it can answer cheaply, and neither answer is a promise the
    # turn lands: no probe can see whether the prompt line is idle, so an idle-only
    # refusal (a mid-turn pane, a busy control.sock) is invisible to both branches.
    # "A path exists" is the whole claim.
    if check:
        if entry.mux:
            # SELF used to have no path on this lane, structurally: `_raw_send`
            # pasted with `guarded=True`, which rides the server-side turn-taken
            # interlock and refuses EXIT_TARGET_NOT_IDLE while the recipient is
            # mid-turn, and a session asking about ITSELF is mid-turn by
            # construction (running this command inside its own turn). Node
            # x-1904 removed that veto: the guard was `rerun_allowed`, borrowed
            # from the rerun verb, refusing a delivery the transport can
            # actually make -- a busy claude session enqueues an injected paste
            # rather than corrupting its composer (measured, not inferred; see
            # `crates/fno/src/server.rs`). `_raw_send` now pastes unguarded and
            # confirms by content against the recipient's own transcript
            # (`_mux_pane_send(..., confirm=True)`), landing even mid-turn --
            # the same property the control.sock lane already had, which is why
            # that lane never carried a self/peer split. Self and peer are no
            # longer a structurally different question on this lane either: the
            # row recording a mux pane IS the path, for both. Not verified
            # against the mux server here: a pane that has since exited still
            # reads injectable, and the send answers that in about a second
            # rather than a second subprocess answering it now.
            print("injectable: mux-pane (a paste still needs the confirm to land)")
            raise typer.Exit(code=0)
        if not session_id:
            # Reachable only under --check, which is exempt from the
            # no-harness_session_id guard above. Answer with the real reason: a
            # probe on an empty id resolves nothing and would print the opaque
            # `not-injectable: not-injectable`, hiding what a caller has to explain.
            print(
                f"not-injectable: the registry row for {name!r} carries no "
                "harness_session_id, so the control.sock lane has no session to "
                "address; re-register it with `fno agents register`"
            )
            raise typer.Exit(code=1)
        injectable, reason = mail_inject_probe(session_id)
        if injectable:
            print("injectable: control.sock (a paste can still refuse a busy prompt)")
            raise typer.Exit(code=0)
        if reason == "probe-unavailable":
            # THIRD answer, not folded into the second. "I resolved and found no
            # path" and "I could not resolve" are different claims, and a caller
            # that gates advice on this must be able to tell them apart: the fno
            # -agents binary being absent or stale (no --probe yet) says nothing
            # about the session. Collapsing it into not-injectable would make this
            # verb assert a verdict it never established.
            print(
                "unmeasurable: probe-unavailable (the fno-agents binary is absent, "
                "too old to carry --probe, or did not answer; run `fno doctor`)"
            )
            raise typer.Exit(code=3)
        print(f"not-injectable: {reason}")
        raise typer.Exit(code=1)

    # 6 + 7. Inject UNWRAPPED (no _MailCtx -> none of the four wrap sites fire).
    #        Never durable on any result. Inject `stripped`, the string every
    #        validation above ran against: a leading space passes the slash check
    #        after stripping but defeats the REPL slash parser when injected raw,
    #        and the receipt would still print `injected`.
    #        The audit event's `sender` is only populated here: an unwrapped
    #        payload has no `from` attribute in the recipient transcript, so the
    #        ledger is the ONLY place that can say who fired the verb. Absent
    #        ambient identity it stays absent rather than guessing.
    raw_msg_id, _reservation, authored_words = _reserve_raw()
    if entry.mux:
        delivered = _mux_pane_send(
            entry,
            stripped,
            guarded=False,
            confirm=True,
            sender=transport_sender,
            origin=origin,
            # raw: this lane exists to make the REPL slash parser fire, which an
            # envelope defeats.
            raw=True,
            # But GATED. This is the middle case, not the keystroke case. The
            # keystroke exemption exists for a payload that ANSWERS a showing
            # prompt (a digit, a control key), where the prompt has to be there.
            # This lane fires a verb, which a showing prompt swallows exactly
            # the way it swallows mail: at a codex auth wall the CR takes the
            # wall's default, the verb never runs, and codex has no transcript
            # confirm, so the receipt still reads `injected`.
            gate=True,
            review=review_request,
        )
    else:  # claude control.sock - the only other keystroke lane
        delivered = _mail_inject_claude(
            session_id,
            stripped,
            sender=transport_sender,
            origin=origin,
        )

    # 8. Four-state receipt (never a boolean; never a durable write).
    # The note used to read as a refusal: it named a defect in the argument and
    # prescribed a different one, with no word saying the verb had already
    # fired. A caller re-sent with a handoff path and queued a SECOND /compact.
    # The operator watched both sit in one prompt line on 2026-08-21, and
    # /compact is not idempotent -- the second summarises the summary.
    #
    # A bare /compact stays legal, because a self-send of it is a reachable
    # prescribed path, so this cannot become a precondition refusal without
    # breaking that caller (six tests in test_mail_raw.py say so). Instead the
    # receipt leads with the completed action and states do-not-re-send BEFORE
    # the advice, the same shape the unconfirmed branch below already uses. The
    # advice survives as retrospective ("would have"), which is what it is:
    # nothing here is actionable for the send that just happened.
    if delivered is True or delivered in {"started", "queued"}:
        _record_raw(raw_msg_id, authored_words)
        if review_request and isinstance(delivered, str):
            print(delivered)
            raise typer.Exit(code=0)
        note = ""
        if stripped == "/compact":
            note = (
                " ALREADY SENT - do not re-send. A handoff path (/compact "
                "<path>) would have produced a better summary: a bare /compact "
                "at ~100% context has no headroom left to summarize."
            )
        print(f"injected.{note}" if note else "injected")
        raise typer.Exit(code=0)
    if review_request and delivered == "unconfirmed":
        print(
            "unconfirmed (review request was not positively classified; do not retry blindly)",
            file=sys.stderr,
        )
        raise typer.Exit(code=0)
    # not-confirmed: the transport returns one bool for two different worlds --
    # poll-budget exhaustion on a paste that DID land, and a clean send failure
    # (binary absent, pane stalled on EXIT_TARGET_NOT_IDLE, socket refused) where
    # nothing was sent. Do NOT claim "sent"; both readings keep the same standing
    # order, because the one we cannot rule out is the landed one and re-queueing
    # it is how a verb fires twice. Exit 0, never durable.
    print(
        "unconfirmed (not confirmed: either the confirm budget expired on a "
        "payload that landed, or the transport refused and nothing was sent - "
        "check the recipient before assuming either; never re-queue)"
    )
    raise typer.Exit(code=0)


@mail_app.command("send")
def cmd_send(
    name: str | None = typer.Argument(
        None,
        help=(
            "Agent name, short-id (first 8 of the session id), or full session "
            "id. Codex: use the full session_id or pane; never head-8. A codex "
            "session id is UUIDv7, so its first 8 characters are a "
            "65.536-second timestamp bucket rather than random - siblings "
            "spawned in one minute share them - and a head-8 aimed at a codex "
            "row is refused outright, not merely when it happens to be "
            "ambiguous today. Claude ids are UUIDv4, so either form works there."
        ),
    ),
    message: str | None = typer.Argument(
        None, help="Message to send (async, fire-and-forget)."
    ),
    harness: str | None = typer.Option(
        None, "--harness", "-H",
        help="claude | codex | gemini (optional; used for mismatch check).",
    ),
    _provider_tombstone: str | None = typer.Option(
        None, "--provider", hidden=True,
        help="Retired: the harness axis is --harness/-H.",
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
    origin: str | None = typer.Option(
        None,
        "--origin",
        hidden=True,
        help="Stamped machine mail origin: operator, peer, scheduler, or recovery.",
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
    raw: bool = typer.Option(
        False, "--raw",
        help=(
            "Inject the payload UNWRAPPED at the recipient's prompt line so the "
            "REPL slash parser fires it - the only way to make a verb the model "
            "is barred from invoking actually run. One axis binds it: an actor "
            "OTHER than the model must supply the trigger (cross-session, the "
            "king-mediated path; self-injection is barred unless --to-self). Keeping "
            "the reviewer off the author is the aim of this lane, not a second "
            "axis it enforces: a self-attested review counts as coverage and "
            "merges. Payload must start with / and be "
            "a single line. Never queues durable. A payload-varying retry is a "
            "two-variable experiment - report any refusal verbatim and stop."
        ),
    ),
    check: bool = typer.Option(
        False, "--check",
        help=(
            "With --raw: report whether an injection path EXISTS and inject "
            "nothing. Prints 'injectable: <lane>' (exit 0), 'not-injectable: "
            "<reason>' (exit 1), or 'unmeasurable: <reason>' (exit 3) when it "
            "could not resolve at all - that third answer is separate on purpose, "
            "since an absent probe binary or an unreadable registry says nothing "
            "about the session. A malformed payload stays a usage error (exit 2), "
            "never a verdict about the session. Gate on "
            "this before you TELL anyone to "
            "self-inject: a session with no registry row, a non-keystroke lane, or "
            "no control socket has no path at all, and advice naming a mechanism "
            "that cannot fire is worse than no advice. It resolves through the "
            "same path the real send uses, so it cannot say yes where the send "
            "says no. It reports a PATH, never a landing: no probe can see whether "
            "the prompt line is idle."
        ),
    ),
    to_self: bool = typer.Option(
        False, "--to-self",
        help=(
            "Address this session as the recipient (no <id> needed). With --raw "
            "the envelope is stripped so a slash command parses at your own "
            "prompt line - this is how an agent reaches a verb the harness serves "
            "to a typed invocation. The audit event records the sender, since an "
            "unwrapped payload carries no `from`."
        ),
    ),
    force: bool = typer.Option(
        False, "--force", "-F",
        help=(
            "Deliver over the PANE transport: type the wrapped body at the "
            "recipient's prompt instead of running the live-inject ladder. Every "
            "mail semantic is kept - same envelope, same msg-id, same reply "
            "handle, same outbox row - and only the transport changes, so a "
            "live-miss no longer forces you to switch verbs and lose all four. "
            "The receipt says `typed (pane <id>)`, NEVER `delivered`: bytes "
            "written to a PTY is not delivery and is certainly not action. Opt-in "
            "on purpose - the pane path asks permission from nothing, so it can "
            "also select a showing prompt's default; it reads the pane first and "
            "refuses one."
        ),
    ),
    style_exception: str | None = typer.Option(
        None, "--style-exception",
        help="Bypass the style check for this body with a stated reason.",
    ),
) -> None:
    """Send a message asynchronously to a registered agent or a project.

    Name mode (``send <name> <message>``): requires the agent to already exist;
    unknown names exit 16. Project mode (``send --to-project <X> <message>``):
    resolves over the registry - one live peer delivers live, none queues
    durable for project X, many errors with the candidate list unless ``--any``.

    Delivery is live-inject-FIRST; the durable envelope is the fallback tier,
    written when the live lane misses or never runs. Sustained agent-lock
    contention writes nothing and exits 11 - it says so on stderr rather than
    implying a receipt.

    Address it by the ADDRESS column of ``fno agents list`` (or the full session
    id). The NAME column is a spawn label and the discovered lane's LABEL is a
    friendly alias; neither is a mailbox, and a durable write keyed to one
    queues under a key no drain reads. If a send does strand, ``fno agents mail sent
    --unclaimed`` finds it and ``fno agents mail withdraw <id>`` retracts it.

    Stdout contract (US3 AC3-UI / US6 AC6-UI): exactly one line, either
    ``msg-<id> delivered (hosted)`` or ``msg-<id> queued (durable) [<reason>]``,
    where ``<reason>`` is the live lane's own cause (node x-1904).
    Exit 0 for both outcomes. Failures surface on stderr with nonzero exit.
    """
    from fno.agents.dispatch import (
        DispatchAskError,
        dispatch_send,
        dispatch_send_to_project,
    )
    from fno.agents.self_stamp import stamp_from
    from fno._flag_aliases import refuse_retired_provider

    refuse_retired_provider(_provider_tombstone)

    workdir = Path(cwd).resolve() if cwd else Path(os.getcwd())

    try:
        classified_origin = classify_origin(origin)
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise typer.Exit(code=2) from exc
    _record_mail_origin(
        origin=classified_origin,
        lane="raw" if raw else "inbox" if kind is not None else "project" if to_project else "peer",
        sender=from_name,
        # Under --to-self the positional parks the payload, so `name` is not
        # a handle at this point; recording it wrote the payload into the
        # audit row. The self target resolves below.
        target_session=name if raw and not to_self else None,
    )
    # Unknown is an explicit audit result, not a wire authority. Legacy
    # carriers omit the attribute so a law gate cannot mistake silence for an
    # operator origin.
    mail_origin: str | None = (
        None if classified_origin == "unknown" else classified_origin
    )

    # --to-self: the recipient is THIS session, derived from ambient identity, so
    # the positional parks the payload (positional #1) exactly as under
    # --to-project. Fail loud without identity - never a silent floor - and refuse
    # alongside --to-project or a second positional (which reads as a named
    # recipient, contradicting a self address). After this the rest of cmd_send
    # sees name=<self handle>, message=<payload>, indistinguishable from a typed
    # `send <own-id> <payload>`.
    if to_self:
        if to_project is not None:
            print(
                "error: --to-self and --to-project are mutually exclusive",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        if message is not None:
            print(
                "error: --to-self derives the recipient from this session; drop "
                "the positional <id>/<name>",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        from fno.agents.self_stamp import (
            IdentityAmbiguousError,
            require_self_identity,
        )
        from fno.harness_identity import canonical_handle

        try:
            ident = require_self_identity()
        except IdentityAmbiguousError as exc:
            print(f"error: --to-self: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc
        if not (ident.session_id and ident.harness):
            print(
                "error: --to-self: no ambient harness identity - cannot self-address",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        message = name
        name = canonical_handle(ident.session_id)

    # The codex head-8 refusal belongs up here for the same reason the --force
    # guard below does: it sat under the --raw, --to-project, --kind and
    # job-address returns, so `send 01a025f8 '/verb' --raw` still fired a verb at
    # whichever colliding session discovery happened to list. An address rule
    # that only covers the lanes reached last is not an address rule.
    #
    # Never for --to-self. The rule exists because a head-8 cannot pick between
    # two sessions in one clock bucket, and self-addressing has nothing to pick
    # between: --to-self DERIVED this handle from the running session twelve
    # lines up. Refusing it killed `mail send --to-self --raw '/<verb>'` on
    # codex, which is the documented self-invocation path and the one the
    # --force refusal text recommends, and the caller cannot even comply because
    # --to-self rejects a positional address.
    # Not on the --to-project lane, where `name` holds the message BODY rather
    # than an address (the project is the address, and the positional parks the
    # text). Hoisting the call without that exclusion made an eight-hex body
    # refuse as an address whenever a live codex row happened to share those
    # characters, which is a refusal aimed at content nobody was addressing.
    if not to_project:
        _refuse_unsafe_short_address(name, self_addressed=to_self)

    # --force ABOVE every lane that returns without reading it. Four lanes below
    # end the command on their own, so a guard placed after any of them leaves
    # the flag silently dropped there -- which is the defect this guard exists to
    # close, reintroduced one line at a time. A dropped transport flag is worse
    # than a refused one, because the receipt reads like a success: the send
    # prints `queued (durable)` while the sender believes it was typed at a
    # prompt. Only the name lane can force, because only it names one pane.
    if force:
        why = None
        if raw:
            why = "--raw already types at the prompt line, unwrapped"
        elif kind is not None:
            why = f"--kind {kind} is a durable inbox note by design"
        elif to_project:
            why = "a project addresses a repo, not a pane"
        elif name and _is_job_name(name):
            why = "a job address names work that outlives any one session"
        if why is not None:
            print(
                f"error: --force types into one named pane and {why}. Send to "
                f"the agent by name to force a pane, or drop --force.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)

    # --raw: fire a verb in a peer by injecting the payload UNWRAPPED at the
    # prompt line (no <fno_mail> envelope, so the REPL slash parser runs it).
    # Separate flow: never wraps, never queues durable, four-state receipt.
    if raw:
        if from_self:
            print(
                "error: --raw strips the envelope, so --from-self has no `from` "
                "attribute to stamp; the ledger records the sender instead",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        if message is None:
            print("error: --raw needs a payload (the verb invocation)", file=sys.stderr)
            raise typer.Exit(code=2)
        _raw_send(
            name,
            message,
            self_ok=to_self,
            check=check,
            style_exception=style_exception,
            origin=mail_origin,
        )
        return
    if check:
        # Only the --raw lane has a keystroke path to have or lack; a wrapped send
        # always has the durable floor, so there is nothing to gate on.
        print(
            "error: --check applies to --raw (the keystroke lane); a wrapped send "
            "always has the durable fallback",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # --from-self resolves this session's own canonical handle and threads it as
    # from_name, so every lane below (project-note / --to-project / name) stamps a
    # reachable reply address instead of the project. It fails LOUD without ambient
    # identity - the silent "fno" floor stamp_from uses is the exact bug this kills.
    if from_self:
        if from_name is not None:
            print("error: --from-self and --from-name are mutually exclusive", file=sys.stderr)
            raise typer.Exit(code=2)
        from fno.agents.self_stamp import IdentityAmbiguousError, require_self_identity
        from fno.harness_identity import canonical_handle

        try:
            ident = require_self_identity()
        except IdentityAmbiguousError as exc:
            print(f"error: --from-self: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc
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
                "usage: fno agents mail send --to-project <project> --kind <kind> "
                "<message>",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        _refuse_forged_envelope(content)
        _enforce_body_cap(content)
        _enforce_style(content, allow_reason=style_exception)

        # US10 kind-scoped guard: question/fyi are project-inbox drain contracts
        # (question -> wake-signal, fyi -> memory). Addressed to an agent they
        # queue durable to an inbox nothing drains, so refuse and name the two
        # real intents. Agent heads-up stays accepted.
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

        # An agent-scoped heads-up must use the same canonical session handle
        # that notify-self and drain-self consume. Resolve registered names and
        # every supported handle form before the durable append; an unresolved
        # token exits without creating a stranded inbox.
        if to_project is None:
            from fno.agents import discover as discover_mod
            from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE
            from fno.harness_identity import canonical_handle, session_identity_key

            resolved, suggestions = discover_mod.resolve_or_suggest(
                recipient, require_alive=False
            )
            if resolved is None:
                hint = (
                    f" Closest sessions: {', '.join(suggestions)}."
                    if suggestions
                    else ""
                )
                print(
                    f"unknown agent or live-session handle: {recipient!r}.{hint}",
                    file=sys.stderr,
                )
                raise typer.Exit(code=UNKNOWN_AGENT_EXIT_CODE)
            if resolved.identity_provisional:
                print(
                    f"cannot resolve agent heads-up uniquely: {recipient!r} has "
                    "no canonical session identity",
                    file=sys.stderr,
                )
                raise typer.Exit(code=UNKNOWN_AGENT_EXIT_CODE)
            try:
                durable, ambiguous = discover_mod.resolve_reachable(
                    resolved.session_id
                )
            except discover_mod.StoreReadError as exc:
                print(
                    f"cannot resolve agent heads-up uniquely: {recipient!r}; "
                    f"unreadable stores: {', '.join(exc.failed)}",
                    file=sys.stderr,
                )
                raise typer.Exit(code=UNKNOWN_AGENT_EXIT_CODE) from exc
            if (
                durable is None
                or ambiguous
                or session_identity_key(durable.session_id)
                != session_identity_key(resolved.session_id)
            ):
                detail = f"; candidates: {', '.join(ambiguous)}" if ambiguous else ""
                print(
                    f"cannot resolve agent heads-up uniquely: {recipient!r}{detail}",
                    file=sys.stderr,
                )
                raise typer.Exit(code=UNKNOWN_AGENT_EXIT_CODE)
            recipient = canonical_handle(durable.session_id)

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

        from fno.inbox.store import generate_msg_id

        msg_id = generate_msg_id()
        reservation, authored_words = _reserve_budget(
            sender=sender,
            recipient=recipient,
            body=content,
            msg_id=msg_id,
            allow_reason=style_exception,
        )
        try:
            res = post_inbox_message(
                recipient=recipient,
                sender=sender,
                kind=kind,
                body=content,
                persist_to_memory=persist_to_memory,
                reply_to=reply_to,
                refs=refs or None,
                msg_id=msg_id,
                word_count=authored_words,
                origin=mail_origin,
            )
        except ValueError as exc:
            _release_budget(reservation)
            print(f"error: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc
        except (OSError, RuntimeError):
            _release_budget(reservation)
            raise

        # A question never gets an autonomous responder (US9 wakes only heads-up);
        # it escalates to the human at send time instead, debounced per pair.
        if kind == Kind.QUESTION.value:
            esc = _escalate_to_human(
                sender, recipient, content, reason="question", msg_id=res.msg_id
            )
            if esc == "escalated":
                print(f"escalated to human ({recipient})", file=sys.stderr)
        # A heads-up to a resumable-but-asleep claude session is woken at send
        # time to drain it: the per-project watch daemon drains project inboxes,
        # never a session-handle inbox, so send time is the reachable trigger
        # (US9). The durable note is already written, so a wake miss loses nothing.
        # A bus-only recipient (x-e21e) declines the wake too: waking revives a
        # second writer on a session that declared the durable bus its one lane.
        elif kind == Kind.HEADS_UP.value:
            from fno.agents.dispatch import wake_if_asleep_claude

            # A bus-only recipient declines the wake inside
            # wake_if_asleep_claude: the note surfaces at its turn boundary.
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
                "usage: fno agents mail send --to-project <project> <message>",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        _refuse_forged_envelope(content)
        _enforce_body_cap(content)
        _enforce_style(content, allow_reason=style_exception)
        try:
            result = dispatch_send_to_project(
                to_project,
                content,
                provider=harness,
                cwd=workdir,
                from_name=stamp_from(from_name),
                origin=mail_origin,
                any_=any_live,
                budget_enforce=_budget_enforced(
                    content, allow_reason=style_exception
                ),
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
            # A bus-only peer demoted by policy gets the designed-queue
            # receipt, not a recovery warning.
            from fno.agents.dispatch import BUS_ONLY_POLICY

            if result.reason == BUS_ONLY_POLICY:
                from fno.mail import hold as _hold

                _note = _hold.bounce_reason(result.recipient)
                print(
                    f"{result.msg_id} queued (durable) for {result.recipient} "
                    f"[project {to_project}] "
                    f"[{_note or 'bus-only: recipient polls the bus at each turn boundary'}]"
                    + (f" `fno agents mail withdraw {result.msg_id}` retracts it." if _note else "")
                )
            else:
                # The anycast lane reaches the SAME dispatch_send as the by-name
                # lane, so it must carry the same cause. Dropping the reason
                # here printed "is not live ... fno agents resume" over an
                # agent-lock timeout, which says nothing about the recipient,
                # and stamped [live-miss] when no live attempt ever ran.
                _warn_deferred(result.recipient, reason=result.reason)
                print(
                    f"{result.msg_id} queued (durable) for {result.recipient} "
                    f"[project {to_project}] [{result.reason or 'live-miss'}]"
                )
        else:
            _warn_deferred(to_project, project=True)
            print(
                f"{result.msg_id} queued (durable) for project {to_project} "
                f"[param-forced: --to-project]"
            )
        return

    # Job-address mode (x-8f8c part 2): node:<id> / pr:<n> names the work, not a
    # process. It resolves to the current claim holder and outlives any session, so
    # mail survives the holder's death. Intercept before name-mode resolution: a
    # job token is neither a registered agent nor a session handle, so the normal
    # dispatch_send -> handle path would only refuse it.
    if name:
        from fno.mail.job_address import is_job_token

        if is_job_token(name):
            if message is None:
                print(f"usage: fno agents mail send {name} <message>", file=sys.stderr)
                raise typer.Exit(code=2)
            _refuse_forged_envelope(message)
            _enforce_body_cap(message)
            _enforce_style(message, allow_reason=style_exception)
            _job_lane_send(
                message,
                name,
                from_name=stamp_from(from_name),
                style_exception=style_exception,
                origin=mail_origin,
            )
            return

    # Name mode.
    if not name or message is None:
        print(
            "usage: fno agents mail send <name> <message>  "
            "(or --to-project <project> <message>)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    _refuse_forged_envelope(message)
    _enforce_body_cap(message)
    _enforce_style(message, allow_reason=style_exception)

    # --force routes into the shared name-lane choke point with the ladder
    # skipped. Intercepted here rather than threaded through `dispatch_send`
    # because forcing is a transport CHOICE, not a rung: running the ladder
    # first and forcing on its miss would be the automatic fallback this flag
    # exists to keep out of every send.
    if force:
        from fno.agents import discover as discover_mod
        from fno.agents.dispatch import UNKNOWN_AGENT_EXIT_CODE

        forced_resolved, forced_suggestions = discover_mod.resolve_or_suggest(name)
        try:
            _name_lane_send(
                message,
                from_name=from_name,
                resolved=forced_resolved,
                token=None if forced_resolved is not None else name,
                # The recipient's harness decides `provider_to` on the row whose
                # whole purpose is auditability. Dropping it let the token arm
                # fall to a literal "claude" and label a codex recipient wrong.
                provider=harness,
                style_exception=style_exception,
                force=True,
                origin=mail_origin,
            )
        except AmbiguousTokenError as amb:
            # Discovery is liveness-gated, so a registered worker whose listing
            # misses lands on the token rung - which is exactly the situation
            # --force exists for. These three refusals belong here for the same
            # reason they belong on the ordinary lane below: without them the
            # verb exits non-zero with an empty terminal and a raw traceback.
            print(
                f"ambiguous session token {name!r}: matches "
                f"{', '.join(amb.candidates)}. Send to a full session id.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2) from amb
        except UnreachableTokenError:
            hint = (
                f" Closest live sessions: {', '.join(forced_suggestions)}."
                if forced_suggestions
                else ""
            )
            print(
                f"unknown agent or live-session handle: {name!r}.{hint}",
                file=sys.stderr,
            )
            raise typer.Exit(code=UNKNOWN_AGENT_EXIT_CODE)
        except UnavailableTokenError as unavailable:
            stores = ", ".join(unavailable.failed)
            visible = (
                f" Visible candidates: {', '.join(unavailable.candidates)}."
                if unavailable.candidates
                else ""
            )
            print(
                f"cannot resolve short session token {name!r}: unreadable stores: "
                f"{stores}.{visible} Send to a full session id.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2) from unavailable
        return

    try:
        result = dispatch_send(
            name=name,
            message=message,
            provider=harness,
            cwd=workdir,
            from_name=stamp_from(from_name),
            origin=mail_origin,
            budget_enforce=_budget_enforced(
                message, allow_reason=style_exception
            ),
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
        # so the recipient can reply by handle (`fno agents mail send <from>`) for a live
        # message, or `fno agents mail reply --to <id>` when answering a drained one.
        if resolved is not None:
            # Live-inject-first with a durable floor addressed to the resolved
            # session's canonical handle. Shared with the name-lane reply path.
            _name_lane_send(
                message,
                from_name=from_name,
                resolved=resolved,
                style_exception=style_exception,
                origin=mail_origin,
            )
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
            _name_lane_send(
                message,
                from_name=from_name,
                resolved=None,
                token=name,
                style_exception=style_exception,
                origin=mail_origin,
            )
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
        except UnavailableTokenError as unavailable:
            stores = ", ".join(unavailable.failed)
            visible = (
                f" Visible candidates: {', '.join(unavailable.candidates)}."
                if unavailable.candidates
                else ""
            )
            print(
                f"cannot resolve short session token {name!r}: unreadable stores: "
                f"{stores}.{visible} Send to a full session id.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2) from unavailable
        return

    # AC3-UI: distinguish delivered vs queued on stdout. A durable demotion
    # carries the live lane's own reason (node x-1904), so a miss to a LIVE
    # recipient names its cause (e.g. not-confirmed) instead of reading as a
    # dead recipient. A receipt naming the wrong cause is worse than one naming
    # none: it sends the reader to diagnose a recipient that was never the
    # problem.
    if result.delivery == "hosted":
        print(f"{result.msg_id} delivered (hosted)")
    elif result.reason == "bus-only":
        # x-e21e: the registered-agent lane's gate refused by policy; the
        # durable write already happened inside dispatch_send. Designed, not
        # stranded -- no recovery ladder.
        from fno.mail import hold as _hold

        _note = _hold.bounce_reason(name)
        print(
            f"{result.msg_id} queued (durable) "
            f"[{_note or 'bus-only: recipient polls the bus at each turn boundary'}]"
            + (f" `fno agents mail withdraw {result.msg_id}` retracts it." if _note else "")
        )
    else:
        reason_tok = result.reason or "live-miss"
        _warn_deferred(name, reason=result.reason)
        print(f"{result.msg_id} queued (durable) [{reason_tok}]")


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
    regardless of which provider sent them. ``fno agents mail ack`` advances the
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
    print('\nto answer one: fno agents mail reply --to <id> --body "..."')


@mail_app.command("sent")
def cmd_sent(
    unclaimed_only: bool = typer.Option(
        False, "--unclaimed", "-u",
        help="Only mail still past its recipient's cursor AND older than "
             "config.inbox.unclaimed_ttl - the ones the nag counts.",
    ),
    from_name: Optional[str] = typer.Option(
        None, "--from-name",
        help="List mail sent under this label instead of the ambient handle "
             "(the same label `mail send --from-name` stamps).",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON regardless of TTY."
    ),
) -> None:
    """List mail THIS session sent, with claim state.

    The outbound half of ``fno agents mail unread``. It exists because the sender could
    previously see only a tally: ``mail status`` said "sent unclaimed: 1" and the
    every-prompt nudge repeated it, with no way to learn which message, to whom,
    or how old - so a strand was something you were told about hourly and could
    do nothing about.

    ``--unclaimed`` applies exactly the predicate the nag applies, so what this
    prints is what that line is counting, never a differently-scoped set.
    """
    from fno.agents.self_stamp import stamp_from
    from fno.bus.log import HOSTED_DELIVERY, TYPED_DELIVERY
    from fno.config import load_settings

    # `stamp_from`, not the precedence-only resolver: this must be the SAME
    # handle the send path stamped into `from`, or the outbox lists mail this
    # session did not send and hides mail it did. A session that inherited a
    # foreign marker stamps "fno" and resolves to its spawner by precedence, so
    # the two answers genuinely differ. Passing `--from-name` through is what
    # makes the labelled send path listable at all: `stamp_from` returns an
    # explicit label verbatim, so mail sent under one is invisible here without
    # it -- an entire supported send path with no way to see its own strands.
    handle = stamp_from(from_name)

    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())
    if unclaimed_only:
        msgs = _sent_unclaimed(handle, load_settings().inbox.unclaimed_ttl)
        claimed_flag = {m.id: False for m in msgs}
    else:
        from fno.bus.log import iter_messages, withdrawn_ids

        all_msgs = list(iter_messages())
        retracted = withdrawn_ids(all_msgs)
        # TTL of -1, not 0: `_age_exceeds` is strict `>` and bus timestamps are
        # whole seconds, so a message sent this second has age 0.0 and a TTL of 0
        # would classify it CLAIMED - reporting a just-sent, unread message as
        # picked up, which is the opposite of the truth this verb exists to tell.
        unclaimed_ids = {m.id for m in _sent_unclaimed(handle, -1)}
        msgs = [
            m for m in all_msgs if m.from_ == handle and m.id not in retracted
        ]
        # A typed row is excluded from the unclaimed scan the way a hosted row
        # is, so "not unclaimed" silently read as "claimed" for it. Consumption
        # is the one thing the pane transport can never assert: bytes at a
        # prompt can be discarded by that prompt. Neither renderer below may
        # infer it, so the ambiguity is resolved HERE, once, rather than in each.
        claimed_flag = {
            m.id: (m.delivery != TYPED_DELIVERY and m.id not in unclaimed_ids)
            for m in msgs
        }

    if json_out or not is_tty:
        print(
            json.dumps(
                [
                    {
                        "id": m.id, "to": m.to, "to_kind": m.to_kind,
                        "kind": m.kind, "ts": m.ts,
                        "delivery": m.delivery or "durable",
                        "claimed": claimed_flag[m.id],
                        # A typed row is neither claimed nor claimABLE: it is
                        # excluded from the unclaimed scan, withdraw refuses it,
                        # and the recipient cannot drain it. Reporting only
                        # `claimed: false` made it identical to an UNCLAIMED
                        # durable row that can still clear, so this renderer and
                        # `--unclaimed-only` disagreed about the same row.
                        "claimable": m.delivery != TYPED_DELIVERY,
                    }
                    for m in msgs
                ],
                ensure_ascii=False,
            )
        )
        return
    if not msgs:
        scope = "unclaimed " if unclaimed_only else ""
        print(f"no {scope}mail sent from {handle}")
        return
    for m in msgs:
        state = (
            "delivered" if m.delivery == HOSTED_DELIVERY
            # `typed` gets its own word. It is excluded from the unclaimed scan
            # like a hosted row, so it fell through to "claimed" here and told
            # the sender the recipient had consumed it. That is the one claim
            # this transport must never make: bytes written into a PTY can be
            # discarded by the prompt they land on.
            else "typed (unconfirmed)" if m.delivery == TYPED_DELIVERY
            else "claimed" if claimed_flag[m.id]
            else "UNCLAIMED"
        )
        lane = m.to_kind or "?"
        delivery = m.delivery or "durable"
        print(f"{m.id}  -> {m.to}  [{lane}/{delivery}]  {m.ts}  {state}")
    print("\nto retract one: fno agents mail withdraw <id>")


@mail_app.command("withdraw")
def cmd_withdraw(
    msg_id: str = typer.Argument(..., help="Message id to retract."),
    from_name: Optional[str] = typer.Option(
        None, "--from-name",
        help="Prove ownership as this label instead of the ambient handle "
             "(needed for mail sent with `mail send --from-name`).",
    ),
) -> None:
    """Retract a message you sent that the recipient has not picked up.

    The bus log is append-only and read state is a per-consumer cursor, so a
    withdrawal is neither a delete nor a cursor move. It cannot delete a line,
    and it must not advance the RECIPIENT's cursor: a cursor is a last-seen
    position rather than a per-message flag, so moving it would mark every other
    message queued for that recipient as seen - trading one strand for many.

    So this appends a tombstone naming the message, and every reader that
    decides delivery skips the pair.

    Refuses when the message is already past the recipient's cursor. They have
    read it; a tombstone then would only hide it from you.
    """
    from fno.bus.cursor import read_cursor
    from fno.bus.log import (
        HOSTED_DELIVERY,
        TYPED_DELIVERY,
        WITHDRAW_KIND,
        Envelope,
        append,
        iter_messages,
        withdrawn_ids,
    )
    from fno.agents.self_stamp import stamp_from

    # The ownership check is the ONLY authz gate on this verb, so it compares
    # against the same resolver that stamped the envelope's `from`. The
    # precedence-only resolver would answer differently for a session carrying
    # an inherited marker: it would refuse that session's own mail (stamped
    # "fno") and let it retract its spawner's. `--from-name` is passed through
    # for the same reason `mail sent` takes it: `stamp_from` returns an explicit
    # label verbatim, so mail sent under one could otherwise never be retracted
    # by anyone. This widens no authority a sender did not already have - the
    # label is unauthenticated on the send side too.
    handle = stamp_from(from_name)

    all_msgs = list(iter_messages())
    target = next((m for m in all_msgs if m.id == msg_id), None)
    if target is None:
        print(f"no such message on the bus: {msg_id}", file=sys.stderr)
        raise typer.Exit(code=1)
    if target.from_ != handle:
        print(
            f"{msg_id} was sent by {target.from_}, not you ({handle}); "
            "only its sender can withdraw it",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)
    if msg_id in withdrawn_ids(all_msgs):
        print(f"{msg_id} is already withdrawn")
        raise typer.Exit(code=0)
    if target.delivery == HOSTED_DELIVERY:
        print(
            f"{msg_id} was already delivered (hosted); it cannot be withdrawn",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)
    if target.delivery == TYPED_DELIVERY:
        # A typed row is not a durable message with a tombstone to write, it is
        # bytes already sitting at somebody's prompt. Withdrawing it wrote the
        # tombstone and printed success while retracting nothing, which is the
        # worst of the three outcomes: the sender believes the message is gone.
        print(
            f"{msg_id} was typed into a pane; the bytes are already at the "
            f"recipient's prompt and cannot be recalled. Send a correction.",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)
    cursor = read_cursor(target.to)
    if cursor is not None:
        pos = {m.id: i for i, m in enumerate(all_msgs)}
        cursor_pos = pos.get(cursor)
        if cursor_pos is not None and pos[msg_id] <= cursor_pos:
            print(
                f"{msg_id} was already claimed by {target.to}; "
                "a withdrawal now would hide it from you, not from them",
                file=sys.stderr,
            )
            raise typer.Exit(code=1)

    append(
        Envelope.new(
            from_=handle,
            to=target.to,
            kind=WITHDRAW_KIND,
            body=f"withdrawn: {msg_id}",
            thread=target.thread,
            meta={"withdraws": msg_id},
            to_kind=target.to_kind,
        )
    )
    # Deliberately NOT "it will not be delivered". The cursor read above and
    # this append are not one atomic step, and nothing locks the recipient out
    # in between: a concurrent drain can print and flush the body and only then
    # advance its cursor, so a withdrawal that observed an un-advanced cursor
    # can still land after delivery. The honest receipt names the boundary the
    # tombstone actually guarantees - no drain from here on - rather than
    # claiming an outcome this command cannot verify.
    print(
        f"withdrew {msg_id} (to {target.to}); no drain will deliver it from now on. "
        "A drain already in flight when this ran may have delivered it: "
        f"`fno agents peek {target.to}` to check."
    )


@mail_app.command("ack")
def cmd_bus_ack(
    msg_id: str = typer.Argument(..., help="Message id to acknowledge up through."),
    name: str = typer.Option(
        "fno", "--name", "-n", help="Whose read cursor to advance."
    ),
) -> None:
    """Advance a read cursor to ``msg_id`` (marks everything up to it seen).

    The id is the positional; the cursor's owner is ``--name``/``-n``, NOT a
    second positional::

        fno agents mail ack msg-a1b2c3 --name <handle>

    Spelled out because the old one-liner read as if ``<name>`` came first and
    cost a reader two failed invocations before they resorted to ``--help``.
    """
    from fno.bus.cursor import advance_cursor, scan_unread
    from fno.bus.log import is_deliverable, iter_messages

    # The ack target must be a retained message addressed to `name`. Two failure
    # modes this guards (both would silently corrupt the read position because
    # the cursor is a single global-log position and scan_unread returns to==name
    # AFTER it):
    #   - an id not in the log -> scan_unread can't find it -> re-surfaces ALL mail;
    #   - an id addressed to ANOTHER recipient but positioned after my unread ->
    #     advances me past my own earlier unread, hiding it.
    all_msgs = list(iter_messages())
    target = next((m for m in all_msgs if m.id == msg_id), None)
    if target is None:
        print(
            f"unknown message id {msg_id!r}: not found in the retained bus log; "
            f"cursor not advanced (run `fno agents mail unread --name {name}` to see ids)",
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
    if not is_deliverable(target):
        # Name the delivery this row actually carries. `is_deliverable` excludes
        # `typed` alongside `hosted`, so a forced pane message reported itself as
        # "delivered (hosted)" here, which is the one claim the pane transport
        # must never make: bytes at a prompt can be discarded by that prompt.
        from fno.bus.log import TYPED_DELIVERY

        how = (
            "typed into a pane (delivery unconfirmed)"
            if target.delivery == TYPED_DELIVERY
            else "already delivered (hosted)"
        )
        print(
            f"message {msg_id!r} was {how}; cursor not advanced",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # The advance consumes every message addressed to `name` up through msg_id.
    # Emit a receipt for each so a manual ack does not leave them unread with no
    # terminal event -- the same accounting gap drain-self closes. Captured
    # before the advance (after it, scan_unread no longer returns them).
    pos = {m.id: i for i, m in enumerate(all_msgs)}
    acked = [m for m in scan_unread(name) if m.id in pos and pos[m.id] <= pos[msg_id]]

    if advance_cursor(name, msg_id):
        print(f"cursor for {name!r} advanced to {msg_id}")
        for m in acked:
            _emit_drain_marker(m.id, name, name, m.from_, "acked")
    else:
        # Forward-only: the id is at or before the current cursor (re-ack / older
        # message). Idempotent no-op, not an error - the cursor never rewinds.
        print(f"cursor for {name!r} already at or past {msg_id}; unchanged")


def _manifest_fields(*names: str) -> dict[str, Optional[str]]:
    """Read named fields from this session's ``.fno/target-state.md`` (cwd-relative).

    The manifest is per-worktree (each target session owns one), so reading it
    from cwd is reading THIS session's own claim binding. Returns ``{}`` when no
    manifest is present (a non-target session has no job to drain)."""
    try:
        raw = (Path.cwd() / ".fno" / "target-state.md").read_text(
            encoding="utf-8", errors="replace"
        )
    except OSError:
        return {}
    out: dict[str, Optional[str]] = {}
    for name in names:
        m = re.search(rf"^{re.escape(name)}\s*:\s*(.*)$", raw, re.MULTILINE)
        if m is None:
            out[name] = None
            continue
        val = m.group(1).strip().strip("\"'")
        out[name] = val if val and val != "null" else None
    return out


def _scan_held_job_mail(ident) -> "tuple[Optional[str], list]":
    """Scan job-addressed mail for the node THIS session holds, verified live.

    The job address outlives any session, so a successor re-claiming the node
    drains mail here that the prior holder never read (x-8f8c part 2). The node
    comes from this session's own manifest (``target_claim_key``); the holder
    check reuses ``resolve_truth_status`` -- the same node->holder-session join
    ``fno agents list`` runs -- so this session drains only when IT is the live
    holder. A successor sees a different holder -> ``session_id`` None -> no
    drain, which is the security gate (a stale manifest must not drain another
    holder's mail).

    Returns ``(job_address, envelopes)``; ``(None, [])`` when this session holds
    no live node claim. Never raises: an unreadable manifest or claim degrades to
    no job mail, so the drain still surfaces handle mail.
    """
    from fno.agents.truth_status import resolve_truth_status
    from fno.bus.cursor import scan_unread
    from fno.mail.job_address import HOLDER_STATES

    key = _manifest_fields("target_claim_key").get("target_claim_key")
    if not key or not key.startswith("node:"):
        return None, []
    res = resolve_truth_status(
        key[len("node:"):], manifest_cwd=str(Path.cwd())
    )
    if res.get("claim_state") not in HOLDER_STATES:
        return None, []
    # resolve_truth_status returns the holder's session id only when the live
    # claim holder still matches this manifest's recorded holder; equalling
    # ident.session_id means THIS session is that holder.
    if not ident.session_id or res.get("session_id") != ident.session_id:
        return None, []
    return key, scan_unread(key)


def _self_handle_or_exit() -> "tuple[str, object]":
    """This session's canonical mail handle AND the identity it came from.

    Returns both so the caller never re-resolves. A second resolve can answer
    differently from the one this function validated, and then the row written
    is not the row checked.

    Fails closed on a contaminated env, for the same reason `--to-self` does
    and with worse consequences. An inherited marker from a parent harness
    makes a precedence-only resolve answer with the PARENT session. A
    misaddressed `--to-self` sends one message to the wrong place, which is
    visible and recoverable. A misaddressed hold stamps a DELIVERY POLICY on
    another agent's row and arms a timer against their handle, silently holding
    their mail.

    The refusal is the shared one now. `resolve_harness_identity` already
    refuses a mixed-family env, so nothing is laundered either way; what the
    owned path adds is that a mixed env the process tree CAN decide resolves
    instead of refusing, which is the difference between a real claude worker
    holding its own mail and being told it has no identity.
    """
    from fno.agents.self_stamp import IdentityAmbiguousError, require_self_identity
    from fno.harness_identity import canonical_handle

    try:
        ident = require_self_identity()
    except IdentityAmbiguousError as exc:
        sys.stderr.write(
            f"{exc}\na hold stamped on the wrong row holds another agent's mail\n"
        )
        raise typer.Exit(code=3) from exc
    if not ident.harness or not ident.session_id:
        sys.stderr.write(
            "no provable harness identity - there is no session to hold mail for\n"
        )
        raise typer.Exit(code=3)
    return canonical_handle(ident.session_id), ident


@mail_app.command("hold")
def cmd_hold(
    minutes: int = typer.Option(
        None,
        "--minutes",
        "-m",
        help="Idle minutes before the hold lifts by itself (default 5). The "
        "window restarts every time you submit a prompt.",
    ),
    off: bool = typer.Option(
        False, "--off", help="Lift the hold now and deliver what it held."
    ),
    status: bool = typer.Option(
        False, "--status", help="Report the current hold without changing it."
    ),
) -> None:
    """Busy mode: hold this session's incoming mail, and drain it on a timer.

    While the hold is on, mail addressed to this session never pastes into the
    prompt line. It queues durable and the sender gets a receipt saying so.
    The hold lifts by itself after ``--minutes`` of no prompt from you, and the
    lift DELIVERS - it does not wait for you to type. That is the whole point:
    a hold whose only drain trigger is the operator converts an interruption
    into a stall.

    The hold reuses the ``delivery_policy = "bus-only"`` flag that already
    exists on the agent row, so every injector lane refuses it before any
    transport call. This verb owns the clock, not the enforcement.
    """
    import shutil
    import subprocess

    from fno.mail import hold as hold_mod

    handle, ident = _self_handle_or_exit()

    if status:
        # Ask the delivery gate, not the clock. A flag stamped by
        # `fno agents register --delivery-policy bus-only` has no clock, and
        # reading the clock alone reported "mail delivers normally" for a
        # session whose mail was in fact being held indefinitely.
        from fno.agents.dispatch import BUS_ONLY_POLICY, _delivery_policy_refusal

        if _delivery_policy_refusal(handle) != BUS_ONLY_POLICY:
            print(f"{handle}: no hold - mail delivers normally")
            return
        label = hold_mod.dnd_label(handle)
        if label == "held":
            print(f"{handle}: holding mail, no expiry (hand-stamped bus-only)")
        elif label is None:
            # The gate says held and the clock says otherwise. Unreachable while
            # both derive from `lapsed`, and mypy is right that nothing across
            # the module boundary enforces that. Report the disagreement rather
            # than crash on it or pick a side: two readings differing is the
            # thing worth telling the operator.
            print(
                f"{handle}: holding mail, but the clock disagrees with the "
                "delivery gate - run `fno agents mail hold --off` to clear it"
            )
        else:
            print(f"{handle}: holding mail, lifts in {label.lstrip('~')}")
        return

    if off:
        result = hold_mod.release(handle, held_for_s=0)
        # Report the FLAG first. Both lines below describe delivery, and an
        # operator who asked for the hold to stop is asking about the flag. A
        # registry this could not write leaves mail held while the receipt says
        # "hold off", which is a lie about their own session.
        if not result["policy_cleared"]:
            sys.stderr.write(
                f"hold NOT off: the registry write failed, so {handle} still "
                "reads bus-only and mail is still held. Retry, or check "
                "`fno agents list` for the row.\n"
            )
            raise typer.Exit(code=1)
        if result["held_count"]:
            print(
                f"hold off: delivered {result['held_count']} held message(s) "
                f"({result['deduped_count']} deduped) - {result['outcome']}"
            )
        else:
            print("hold off: nothing was held")
        return

    window = hold_mod.DEFAULT_MINUTES if minutes is None else minutes
    if window < 1:
        sys.stderr.write("error: --minutes must be at least 1\n")
        raise typer.Exit(code=2)

    from fno.agents.registry import register_existing_session

    register_existing_session(
        provider=str(getattr(ident, "harness", "") or ""),
        session_id=str(getattr(ident, "session_id", "") or ""),
        cwd=os.getcwd(),
        delivery_policy="bus-only",
    )
    clock = hold_mod.arm(handle, window)

    # The third drain trigger. Detached on purpose: it must outlive this CLI
    # invocation, because the whole contract is that the drain happens with no
    # further input from the operator.
    #
    # Re-invoke THIS executable, not whatever `fno` is on PATH. A deployed
    # binary can be several merges behind the code that just armed the hold,
    # and one that predates this verb dies instantly on an unknown command -
    # the timer never runs, and the only symptom is a hold that never lifts.
    binary = sys.argv[0] if os.path.isfile(sys.argv[0]) else shutil.which("fno")
    armed = False
    if binary:
        try:
            subprocess.Popen(  # noqa: S603 - fixed argv, no shell
                [binary, "agents", "mail", "hold-release", "--handle", handle],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            armed = True
        except OSError:
            armed = False

    until = clock.until or datetime.now(timezone.utc)
    print(
        f"busy mode on for {handle}: mail holds until "
        f"{until.strftime('%H:%M:%S')} UTC ({window}m idle), then delivers itself."
    )
    if not armed:
        print(
            "note: the release timer did not start, so the hold lifts on the "
            "next send attempt or at your next prompt instead of on the clock."
        )


@mail_app.command("hold-release", hidden=True)
def cmd_hold_release(
    handle: str = typer.Option(..., "--handle", help="The held session's handle."),
    poll_s: int = typer.Option(
        15, "--poll-s", hidden=True, help="Seconds between clock re-reads."
    ),
) -> None:
    """Sleep until ``handle``'s hold expires, then release it.

    Re-reads the clock on every wake rather than sleeping once to the original
    deadline, so an idle re-arm (the operator typed again) extends the hold
    instead of being overrun by a timer that already committed to a time.

    Exits quietly when the clock disappears or turns permanent: both mean
    someone else took the hold off, and a second release would be a no-op that
    still emitted a release event.
    """
    from fno.mail import hold as hold_mod

    started = time.monotonic()
    while True:
        clock = hold_mod.read(handle)
        if clock is None or clock.until is None:
            return
        remaining = (clock.until - datetime.now(timezone.utc)).total_seconds()
        if remaining <= 0:
            break
        time.sleep(min(remaining, max(1, poll_s)))

    result = hold_mod.release(handle, held_for_s=int(time.monotonic() - started))
    print(json.dumps(result))


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
    from fno.agents.self_stamp import IdentityAmbiguousError, require_self_identity
    from fno.harness_identity import canonical_handle, legacy_suffix_handle, session_identity_key

    try:
        ident = require_self_identity()
    except IdentityAmbiguousError as exc:
        print(f"error: drain-self: {exc}", file=sys.stderr)
        return
    if not ident.harness or not ident.session_id:
        if json_out:
            print(json.dumps([]))
        return

    sid = ident.session_id
    handle = canonical_handle(sid)  # primary address, used in the render label
    # Drain every address form this session owns: the canonical first-eight (new
    # mail), the full id (the collision-escape send path), and the legacy
    # last-eight (pre-flip mail still on the bus). Each address has its own
    # cursor; a message has one `to`, so it matches exactly one form.
    last_by_form: dict[str, str] = {}
    form_by_id: dict[str, str] = {}
    all_msgs: list = []
    for _form in (handle, session_identity_key(sid), legacy_suffix_handle(sid)):
        _got = scan_unread(_form)
        if _got:
            last_by_form[_form] = _got[-1].id
            for _m in _got:
                form_by_id.setdefault(_m.id, _form)
            all_msgs.extend(_got)
    _seen: set[str] = set()
    msgs = []
    for _m in all_msgs:
        if _m.id not in _seen:
            _seen.add(_m.id)
            msgs.append(_m)
    # Render chronologically: the three forms are scanned in address-order, not
    # log-order, so without this a legacy (pre-flip) message would render after a
    # newer canonical one. Cursor advance below is per-form and unaffected.
    msgs.sort(key=lambda _m: _m.ts)
    # Job mail: also drain mail addressed to the node THIS session holds. The
    # address outlives any session, so this is where a successor picks up mail
    # the prior holder never read (x-8f8c part 2). Per-address cursor, so this is
    # independent of the handle/form cursors -- no double-delivery across them.
    job_addr, job_msgs = _scan_held_job_mail(ident)

    # W2 cross-delivery dedup: a message whose id already landed in THIS
    # session's transcript (a live inject that confirmed after the durable copy
    # was written) is not surfaced again. The transcript is the ledger -- no new
    # state -- built in one read per drain, not one per message. ``present`` is
    # None when the transcript could not be read; a read failure is not evidence
    # of absence, so we print everything rather than risk a drop (AC5-ERR).
    from fno.mail.reply_resolve import present_mail_ids

    present = present_mail_ids()

    def _already_landed(m) -> bool:
        return present is not None and getattr(m, "id", "") in present

    to_print = [m for m in msgs if not _already_landed(m)]
    skipped = [m for m in msgs if _already_landed(m)]
    job_to_print = [m for m in job_msgs if not _already_landed(m)]
    job_skipped = [m for m in job_msgs if _already_landed(m)]

    # A live-injected send already carries FNO_MAIL_TRAILER inside `wrap_fno_mail`'s
    # `<fno_mail>` envelope, but a durable inbox-kind send (heads-up/question/fyi)
    # never routes through that wrapper. Stamp the trailer here, the one
    # chokepoint every drained body passes through regardless of output shape,
    # so both the text render and `--json` carry the authority boundary
    # regardless of which lane produced the body.
    # A live-injected send stores the full paired envelope durably (body
    # ends `...trailer\n</fno_mail>`), so recognizing "already stamped"
    # needs both shapes: the bare trailer, and the trailer immediately
    # before a terminal close tag. The trailer comes from the record's own
    # origin field (d-b2dbf5ad): gated at write time by classify_origin,
    # trustworthy at drain time. A forged trailer in the body never
    # suppresses the stamp - only the record's exact trailer dedups, and a
    # mismatch gets the real one appended beneath it.
    from fno.mail.envelope import render_body_with_record_trailer

    def _render_body(m) -> str:
        return render_body_with_record_trailer(
            m.body, getattr(m, "origin", None)
        )

    if json_out:
        out = [
            {
                "id": m.id, "from": m.from_, "to": m.to,
                "kind": m.kind, "ts": m.ts, "body": _render_body(m),
            }
            for m in to_print
        ]
        for m in job_to_print:
            out.append(
                {
                    "id": m.id, "from": m.from_, "to": m.to,
                    "kind": m.kind, "ts": m.ts, "body": _render_body(m),
                    "job": job_addr or "",
                }
            )
        print(json.dumps(out, ensure_ascii=False))
    else:
        if to_print:
            print(f"[fno agents mail] {len(to_print)} message(s) for {handle}:")
            for m in to_print:
                print(f"\n--- from {m.from_} ({m.ts})  id:{m.id} ---")
                print(_render_body(m))
        if job_to_print:
            print(f"\n[fno agents mail] {len(job_to_print)} job message(s) for {job_addr}:")
            for m in job_to_print:
                print(f"\n--- from {m.from_} ({m.ts})  id:{m.id} ---")
                print(_render_body(m))
        # This render is what a session sees on receive, so surface the id (which
        # `reply --to` correlates against) and the how-to. Replying is optional --
        # an FYI/broadcast needs none.
        if to_print or job_to_print:
            print(
                '\n[fno agents mail] to answer one: fno agents mail reply --to <id> --body "..."'
            )

    # Inject-before-ack: advance the cursor to the last drained id only after
    # the bodies are out, so a crash re-surfaces rather than drops.
    #
    # "out" means flushed, not printed. The SessionStart drain hook reads this
    # through a command substitution, so stdout is a pipe and block-buffered:
    # without the flush, the bodies sit in this process's buffer while the cursor
    # is already advanced, and a SIGTERM from the hook's wall-clock bound
    # discards them with the mail marked consumed. Losing it permanently, which
    # is the opposite of what the paragraph above promises.
    if msgs or job_msgs:
        sys.stdout.flush()
        # Per-form receipt (W1.1): advance one address form, then emit that
        # form's drained markers before the next advance. A later form's cursor
        # failure cannot strand the markers for an already-acked form, because
        # each marker rides its own form's commit. The marker is emitted at the
        # ack point, never the print point: this function is inject-before-ack,
        # so a crash between print and here re-surfaces the message, and a marker
        # at print would claim delivery for one about to be delivered again.
        # Best-effort and swallowed (AC9-ERR): the message is already acked, so
        # an observability gap must never become a delivery failure. A skipped
        # duplicate still advances and still gets a marker (reason distinguishes
        # it) so the dedup is observable, never a silent swallow (W2).
        for _form, _last_id in last_by_form.items():
            advance_cursor(_form, _last_id)
            for m in to_print:
                if form_by_id.get(m.id, handle) == _form:
                    _emit_drain_marker(m.id, handle, _form, m.from_, "printed")
            for m in skipped:
                if form_by_id.get(m.id, handle) == _form:
                    _emit_drain_marker(m.id, handle, _form, m.from_, "skipped-duplicate")
        if job_addr and job_msgs:
            advance_cursor(job_addr, job_msgs[-1].id)
            for m in job_to_print:
                _emit_drain_marker(
                    m.id, job_addr or handle, job_addr or handle, m.from_, "printed"
                )
            for m in job_skipped:
                _emit_drain_marker(
                    m.id, job_addr or handle, job_addr or handle, m.from_, "skipped-duplicate"
                )


def _emit_drain_marker(
    msg_id: str,
    recipient: str,
    address_form: str,
    sender: "str | None",
    reason: str = "printed",
) -> None:
    """Best-effort ``agent_mail_drained`` receipt, one per drained message id (W1.1).

    Lets a sender join ``events.jsonl`` on ``msg_id`` to a terminal 'drained'
    state, and lets the dead-letter sweep prefer a positive marker over cursor
    inference. ``reason`` distinguishes a message that was printed from one
    skipped as a duplicate (W2), so the receipt never silently swallows a
    message. Swallowed on any failure: the caller has already printed and acked
    the message, so a missing receipt degrades to the cursor fallback rather than
    failing the drain (AC9-ERR).
    """
    from fno.agents import events

    try:
        events.emit(
            events.KIND_AGENT_MAIL_DRAINED,
            msg_id=msg_id,
            recipient=recipient,
            address_form=address_form,
            sender=sender or "",
            reason=reason,
        )
    except (OSError, ValueError, TypeError):
        pass


# ---------------------------------------------------------------------------
# Active-turn delivery helpers. The sent-unclaimed predicate remains stat-only
# and is shared with `fno agents mail status`.
# ---------------------------------------------------------------------------

# Mail text is embedded inside a hook-owned <system-reminder> wrapper, so a
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


def _sent_unclaimed(handle: str, ttl_seconds: int) -> list:
    """My sent mail still unclaimed past TTL, oldest -> newest.

    Unclaimed = still past the recipient's consume cursor AND strictly older than
    ``ttl_seconds``. Reads the bus ONCE (a single ``iter_messages`` snapshot) and
    compares each recipient's cursor position against that snapshot, so cost is
    ``O(bus + recipients)`` not ``O(recipients x bus)`` -- a per-recipient
    ``scan_unread`` reparse could cross the hook's 2s timeout and silently drop
    the nudge. Stat-only: recipient cursors are read fresh every call (never
    cached), so a just-consumed message stops being flagged immediately; no
    cursor is advanced.

    Returns the envelopes rather than a count because for its whole life this
    computed exactly the rows a sender needs to act -- id, recipient, age -- and
    threw all but the tally away, which is why the nag could say "1 unclaimed"
    every turn and offer no way to find or stop it. Callers that only want the
    tally take ``len()``.
    """
    from datetime import datetime as _dt
    from datetime import timezone as _tz

    from fno.bus.cursor import read_cursor
    from fno.bus.log import is_deliverable, iter_messages, withdrawn_ids

    now = _dt.now(tz=_tz.utc)
    all_msgs = list(iter_messages())
    # A withdrawn message stops being unclaimed the moment it is retracted; this
    # reader takes `iter_messages` directly and so is NOT covered by the filter
    # in `scan_unread`. Without this line the nag survives its own withdrawal.
    retracted = withdrawn_ids(all_msgs)
    sent = [
        m for m in all_msgs
        if m.from_ == handle and m.id not in retracted and is_deliverable(m)
    ]
    if not sent:
        return []
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
    out = []
    for m in sent:
        if pos[m.id] <= cursor_pos.get(m.to, len(all_msgs)):  # claimed / unresolvable
            continue
        if not _age_exceeds(m.ts, ttl_seconds, now):  # still fresh (strict >)
            continue
        out.append(m)
    return out


def _distinct_recipients(msgs: list) -> list[str]:
    """Recipients in first-seen order, one entry each."""
    seen: list[str] = []
    for m in msgs:
        if m.to not in seen:
            seen.append(m.to)
    return seen


@mail_app.command("notify-self", hidden=True)
def cmd_notify_self() -> None:
    """Write one atomic ``UserPromptSubmit`` mail payload, then acknowledge it.

    The CLI owns the complete hook envelope so no shell capture can advance the
    cursor before the JSON is ready. A write or flush failure leaves the cursor
    unchanged; the next SessionStart or active-turn boundary can retry.
    """
    from fno.agents.self_stamp import IdentityAmbiguousError, require_self_identity
    from fno.bus.cursor import advance_cursor, scan_unread
    from fno.config import load_settings
    from fno.harness_identity import canonical_handle

    try:
        ident = require_self_identity()
    except IdentityAmbiguousError as exc:
        print(f"error: notify-self: {exc}", file=sys.stderr)
        return
    if not ident.harness or not ident.session_id:
        return

    handle = canonical_handle(ident.session_id)

    # Busy mode (x-481e). This hook fires on every UserPromptSubmit, which is
    # precisely the "the operator is not idle" signal the hold window resets
    # on - so one hook is both the suppressor and the idle re-arm, and no new
    # wiring is needed. A live timed hold renders nothing and pushes its own
    # deadline out. A lapsed one is tidied here rather than on the send path,
    # where the gate stays a pure read to avoid a re-entrant registry lock.
    # Both calls WRITE, so both are wrapped: a hold that cannot be extended or
    # tidied must degrade to rendering the mail, never to swallowing this
    # turn's delivery. Busy mode is a convenience layered over the bus, and it
    # does not get to break the bus.
    try:
        from fno.mail import hold as hold_mod

        if hold_mod.extend(handle) is not None:
            return
        hold_mod.tidy_lapsed(handle)
    except Exception:  # noqa: BLE001 - a hold failure never costs a delivery
        pass

    lines: list[str] = []

    unread = scan_unread(handle)
    from fno.mail.reply_resolve import present_mail_ids

    present = present_mail_ids()

    def _dup(m: object) -> bool:
        return present is not None and getattr(m, "id", "") in present

    to_render = [m for m in unread if not _dup(m)]
    if to_render:
        lines.append(f"[fno agents mail] {len(to_render)} message(s) for {handle}:")
        for message in to_render:
            lines.extend(
                (
                    f"\n--- from {message.from_} ({message.ts})  id:{message.id} ---",
                    message.body.rstrip("\n"),
                )
            )
        lines.append(
            '\n[fno agents mail] to answer one: fno agents mail reply --to <id> --body "..."'
        )

    ttl = load_settings().inbox.unclaimed_ttl
    unclaimed = _sent_unclaimed(handle, ttl)
    if unclaimed:
        who = _bounded_names(_distinct_recipients(unclaimed))
        # Name the exits. This line fired every turn for hours with no way to
        # see which message it meant or to stop it, which is what made an
        # unclaimed message a standing tax on the sender rather than a notice.
        lines.append(
            f"{len(unclaimed)} sent fno agents mail unclaimed (to {who}, >{ttl // 60}m): "
            "recipient has not picked it up; "
            "`fno agents mail sent --unclaimed` to see them, "
            "`fno agents mail withdraw <id>` to retract one"
        )

    if not lines:
        if unread:
            advance_cursor(handle, unread[-1].id)
            for m in unread:
                _emit_drain_marker(m.id, handle, handle, m.from_, "skipped-duplicate")
        return

    try:
        context = (
            f"<system-reminder>\n"
            f"{_defang_reminder(chr(10).join(lines))}\n"
            f"</system-reminder>"
        )
        payload = json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": context,
                }
            },
            ensure_ascii=False,
        )
        sys.stdout.write(payload + "\n")
        sys.stdout.flush()
    except (OSError, TypeError, ValueError):
        return

    if unread:
        advance_cursor(handle, unread[-1].id)
        for m in unread:
            reason = "skipped-duplicate" if _dup(m) else "printed"
            _emit_drain_marker(m.id, handle, handle, m.from_, reason)


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
