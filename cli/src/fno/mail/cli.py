"""fno agents mail: durable polled mailbox CLI.

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
import subprocess
import sys
from fno.mail.receipts import _escalate_to_human, _recipient_is_attended
from datetime import datetime, timedelta, timezone
from enum import Enum
from pathlib import Path
from typing import Any, NoReturn, Optional, TypedDict

import typer

from fno.mail.receipts import (
    _live_miss_age_suffix,
    _warn_deferred,
    demotion_receipt,
    durable_leg_story,
    durable_window_clause,
    json_receipt,
    print_project_demotion,
)
from fno.inbox.store import (
    DEPRECATED_KINDS,
    DurableOwner,
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
from fno.mail.thread_force import resolve_pane_entry as _resolve_pane_entry


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

    ``positional`` exists so ``reply`` accepts a bare body like ``send``
    does: without it click rejected the stray argument with exit 2 and
    echoed the body back, which reads like a delivery receipt. File reads
    (and ``-`` for stdin) live in ``fno.text_or_file.read_text_arg``.
    """
    from fno.text_or_file import read_text_arg

    supplied = [x for x in (positional, body, body_file) if x is not None]
    if len(supplied) > 1:
        typer.echo(
            "error: provide the body once - as a positional argument, --body, or --body-file",
            err=True,
        )
        raise typer.Exit(code=1)
    text = read_text_arg(body, body_file, what="the body")
    if text is not None:
        return text
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
    from fno.mail.origins import MAIL_ORIGINS, enforce_origin_floor

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
    body: str | None = None,
    reply_to: str | None = None,
) -> None:
    """Best-effort origin row and ask/route spans, via the Rust mail-record leaf."""
    try:
        import shutil

        pairs = (("--origin", origin), ("--lane", lane), ("--sender", sender),
                 ("--target-session", target_session), ("--reply-to", reply_to))
        flags = [x for pair in pairs if pair[1] for x in pair if x is not None]
        binary = shutil.which("fno-agents")
        if binary is not None:
            subprocess.run([binary, "mail-record", *flags],
                           input=body or "", timeout=2, capture_output=True)
    except Exception:
        pass


def _vet_body(body: str, *, allow_reason: str | None = None) -> None:
    """The three checks every outgoing body runs, in reader order."""
    _refuse_forged_envelope(body)
    _enforce_body_cap(body)
    _enforce_style(body, allow_reason=allow_reason)


def _enforce_body_cap(body: str, *, usage: bool = False) -> None:
    """Warn over WARN bytes, refuse over REFUSE bytes.

    Fail-open: a disabled tier (0) or an unset body never blocks
    coordination. Under ``--raw --check`` an over-cap payload is a malformed
    CALL, hence exit 2.
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


def _refuse_forged_envelope(body: str) -> None:
    """Refuse a body carrying an ``<fno_mail`` open tag or ``</fno_mail>`` close
    tag, before it reaches ``wrap_fno_mail``.

    The envelope is only trustworthy if a peer cannot forge one: a close tag
    plus a fabricated envelope renders as two messages, and the second could
    say the opposite of the first. Refuse at send time; a mangled body is
    worse than a refused send.
    """
    from fno.mail.envelope import ForgedEnvelopeError, refuse_if_forged

    try:
        refuse_if_forged(body)
    except ForgedEnvelopeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise typer.Exit(code=1) from exc


def _enforce_style(body: str, *, allow_reason: str | None = None) -> None:
    """Refuse a body that breaks the style rules (1 to 8). Fail-open: an empty
    body, the kill switch, a ``style-exception:`` line, or a non-empty
    ``--style-exception`` reason skips the check. The refusal prints the
    offending rule and text; stderr is not a mail body, so it is exempt from
    the cap.
    """
    if os.environ.get("FNO_STYLE_ENFORCE") == "0" or not body:
        return
    if allow_reason and allow_reason.strip():
        return
    from fno import rust_binary
    from fno.config import load_settings
    from fno.mail import budget

    if budget.is_control(body):
        # A control body is terse operational fragments; refusing it re-creates the wall the lane removes.
        return

    err, receipt = rust_binary.style_receipt(body, "mail", load_settings().style.word_cap.mail)
    # A door error, a violation, or a silent binary (no dict) refuses: the gate never vanishes.
    clean = isinstance(receipt, dict) and (receipt.get("exception") or not receipt.get("violations"))
    if not clean:
        if isinstance(receipt, dict) and receipt.get("violations"):
            _emit_style_refusal(receipt["violations"])
        print(err or (receipt or {}).get("report") or "", file=sys.stderr)
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
    """Reserve a control send after both pair identities are canonical.

    An ordinary body reserves nothing: rule 7 is its only word gate, so the
    return is ``(None, words)`` and callers release unconditionally.
    """
    from fno import rust_binary
    from fno.mail import budget

    if not budget.is_control(body):
        return None, rust_binary.style_word_count(body)
    return _reserve_control_budget(
        sender=sender, recipient=recipient, body=body, msg_id=msg_id,
        sender_key=sender_key, recipient_key=recipient_key,
    )


def _reserve_control_budget(
    *,
    sender: str,
    recipient: str,
    body: str,
    msg_id: str,
    sender_key: str | None = None,
    recipient_key: str | None = None,
):
    """Reserve a control send against the control lane's own ledger.

    The stderr note is the receipt's lane marker, in one place, for every
    lane that routes through here; it reads RESERVED because delivery is
    proven later, by the lane's own receipt. The word count arrives from the
    checked door: the cap enforces, so a door failure refuses the send.
    """
    from fno import rust_binary
    from fno.mail import budget

    try:
        words = rust_binary.style_word_count_checked(body)
        reservation = budget.reserve_control(
            sender=sender,
            recipient=recipient,
            words=words,
            msg_id=msg_id,
            sender_key=sender_key,
            recipient_key=recipient_key,
        )
    except budget.BudgetRefused as exc:
        print(f"refused: control word budget for {exc.pair}: {exc.marker()}", file=sys.stderr)
        raise typer.Exit(code=1) from exc
    except (budget.BudgetUnavailable, budget.BudgetCountUnavailable) as exc:
        print(f"refused: {exc}", file=sys.stderr)
        raise typer.Exit(code=1) from exc
    print("control lane: reserved against its own 60-word rolling window", file=sys.stderr)
    return reservation, words


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
            "rule_ids": sorted({v["rule"] for v in violations}),
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


# Status helpers

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
        print(f"warning: launchctl list timed out after 5s for project={project!r}", file=sys.stderr)
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
    honestly reports 0 rather than a project-wide figure. Shares the status
    read's same predicate; never raises (a broken read degrades to 0).
    """
    from fno.agents.self_stamp import resolve_self_identity
    from fno.config import load_settings
    from fno.harness_identity import canonical_handle
    from fno.mail.landed import _sent_unclaimed

    ident = resolve_self_identity()
    if not ident.harness or not ident.session_id:
        return 0
    try:
        handle = canonical_handle(ident.session_id)
        return len(_sent_unclaimed(handle, load_settings().inbox.unclaimed_ttl))
    except Exception as exc:  # noqa: BLE001 - status is advisory; never crash on it
        # Advisory-degrade to 0, but leave a breadcrumb (matches _active_session)
        # so a structural break doesn't render `sent unclaimed: 0` forever silently.
        print(f"warning: sent-unclaimed count failed: {type(exc).__name__}: {exc}", file=sys.stderr)
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
            "Full session id when the stored sender handle is ambiguous; a "
            "non-candidate value sends nothing."
        ),
    ),
    style_exception: str | None = typer.Option(
        None, "--style-exception",
        help="Bypass the style check for this body with a stated reason.",
    ),
) -> None:
    """Reply to a message, routed by the answered message's lane.

    The id resolves against the durable bus FIRST (directed mail answers
    at its original sender), then in this session's transcript; only an
    id absent from BOTH is a hard error. Routing contract:
    docs/architecture/mail-reply-routing.md.

    The body is positional, or --body, or --body-file. Exactly one of the
    three; giving two is refused rather than resolved by precedence.
    """
    kind = _validate_kind(kind)
    body_text = _read_body(body, body_file, body_arg)
    _vet_body(body_text, allow_reason=style_exception)
    classified_origin = classify_origin()
    from fno.agents.self_stamp import stamp_from
    _record_mail_origin(origin=classified_origin, lane="reply", sender=stamp_from(from_project),
                        body=body_text, reply_to=to_msg)
    mail_origin: str | None = (
        None if classified_origin == "unknown" else classified_origin
    )

    # Directed-lane routing: look the --to msg-id up on the durable bus
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
        # session-addressed one (node). The name lane is the lane that
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


@mail_app.command("pane-prepare", hidden=True)
def cmd_pane_prepare_forward(
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
        help="Reasoned one-send exception to the style gates on enveloped "
        "prose (a --raw send never enters them).",
    ),
) -> None:
    """Forward the pane-send renderer to the bundled fno-agents binary.

    The renderer's body lives in the Rust mail front now (``fno-agents mail
    pane-prepare``); Python owns the command surface only, the same split as
    the hold forward. The Rust ``fno mux pane send`` verb calls that binary
    directly and never lands here.
    """
    argv = ["pane-prepare", "--pane", str(pane)]
    # Both session spellings ride verbatim when given; the renderer owns the
    # alias merge and its deprecation warning, so this leaf stays argument-silent.
    if session is not None:
        argv.extend(["--session-id", session])
    if session_legacy is not None:
        argv.extend(["--session", session_legacy])
    if harness is not None:
        argv.extend(["--harness", harness])
    if style_exception is not None:
        argv.extend(["--style-exception", style_exception])
    _forward_mail_pane_prepare(argv)


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


# Output helpers

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


# Publish + cursor-consume (relocated from `fno agents`, Move B)
# `fno agents mail send` is the durable-first publish (the envelope lands on the bus
# log before any live delivery is attempted). `fno agents mail unread`/`ack` are the
# cursor-based consume over that log: unread lists messages addressed to me
# after my cursor; ack advances it. These supersede the retired agents
# send/inbox/ack and inbox unread/ack verbs (the one messaging namespace).


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
    """True when no codex app-server daemon is listening on its control socket.

    Mirrors ``codex_app_server_socket_path`` in codex_inject.rs. The socket file
    at ``$CODEX_HOME/app-server-control/app-server-control.sock`` survives the
    daemon that created it, so liveness is a connect probe (the doctor report's),
    never the file's existence. A live mail send to a codex peer demotes to
    durable when no daemon answers, so the demote line names the fix rather than
    only the reason.

    Delegates to the doctor report so the send-time demote line and `fno doctor`
    can never disagree about where the socket lives (lazy import: doctor is a
    heavy module and this is one line on a cold path)."""
    from fno.doctor import _codex_app_server_report

    return not _codex_app_server_report().get("present", False)


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
    subject: Optional[str] = None,
    record_body: Optional[str] = None,
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
    lead's option-3 ruling once became the worker's option 1.

    The receipt says ``typed``, never ``delivered``. Bytes written to a PTY is
    not delivery and is certainly not action; the recipient's own transcript is
    the only thing that could confirm consumption, and this path does not read
    it. Returns True when the bytes were written.
    """
    from fno.agents.dispatch import _mux_pane_send

    from fno.mail.thread_force import prepare_forced_entry

    entry, mux_session, pane_id, thread_viewport = prepare_forced_entry(
        entry, recipient=recipient, reservation=reservation
    )
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
            f"error: {recipient} is DND (bus-only by policy), so no pane paste is "
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

    # One field set feeds both rows: the staged body row before the first
    # keystroke and the typed receipt after confirmation. The bus row is the
    # body's only home, so a staging failure types nothing -- typing into an
    # id whose row never landed is the dangling-Read-line bug this prevents.
    from fno.bus.log import record_typed_delivery, stage_before_live

    record: dict[str, Any] = dict(
        msg_id=msg_id,
        sender=sender,
        recipient=recipient,
        body=record_body or wrapped,
        from_harness=sender_harness,
        to_harness=provider,
        in_reply_to=reply_to,
        from_session=sender_session,
        from_model=sender_model,
        to_kind="name",
        word_count=authored_words,
        subject=subject,
    )
    try:
        stage_before_live(**record)
    except Exception as exc:  # noqa: BLE001 - nothing typed, nothing confirmed
        _release_budget(reservation)
        print(
            f"error: body row not staged before the forced send; nothing was "
            f"typed. Send without --force to queue durable. ({exc})",
            file=sys.stderr,
        )
        raise typer.Exit(code=1) from exc

    if _mux_pane_send(entry, wrapped, guarded=False) in (False, "unconfirmed"):
        _release_budget(reservation)
        # NOT "nothing was sent". One bool covers two worlds here: a refusal
        # before any bytes moved, and a paste that landed whose submit key then
        # failed, leaving the envelope in the recipient's composer. No typed
        # receipt exists either way, but the staged body row does, so a drain
        # delivers the message -- a sender told "nothing was sent" retries and
        # pastes it twice on top of the drain copy.
        print(
            f"error: --force did not confirm a send into pane {pane_id}. The "
            f"payload may still be sitting unsent in the recipient's composer, "
            f"and the staged body row will deliver by drain, so check the pane "
            f"before retrying. The refusal is on stderr above.",
            file=sys.stderr,
        )
        raise typer.Exit(code=1)

    try:
        record_typed_delivery(
            **record,
            pane_id=str(pane_id),
            mux_session=str(mux_session) if mux_session else None,
        )
    except Exception as exc:  # noqa: BLE001 - the bytes are already typed
        print(
            "typed; receipt row failed; the staged bus row holds the body; "
            f"do not retry: {exc}",
            file=sys.stderr,
        )
    corr = f" re:{reply_to}" if reply_to else ""
    label = f"thread viewport {mux_session}:{pane_id}" if thread_viewport else f"pane {pane_id}"
    print(json_receipt(
        msg_id, to=recipient, status=f"typed ({label})", subject=subject,
    ))
    if corr:
        print(corr.strip())
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


def _row_hosts_keeper_thread(session_id: str) -> bool:
    """Whether ``session_id``'s registry row carries a keeper thread socket.

    The same predicate the mail-inject verb resolves the row by: a
    ``messaging_socket_path`` under ``mux/threads/``. Anything else - a pane
    row, no row, an unreadable registry - reads False, and the caller keeps
    its other lanes. An unreadable registry must not raise here: a send to a
    keeper-lane recipient whose registry cannot be read still owes the ladder
    its fall-through rather than an error."""
    from fno.agents.registry import AgentResolutionError, resolve_agent

    try:
        entry = resolve_agent(session_id).entry
    except (AgentResolutionError, OSError):
        return False
    sock = getattr(entry, "messaging_socket_path", None) or ""
    return "mux/threads/" in sock


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
    subject: Optional[str] = None,
) -> None:
    """Name-lane delivery core, shared by ``mail send <name>`` and a name-lane
    ``mail reply`` -- the ONE choke point every delivery ladder rung lives in.
    Three modes, by which of ``resolved`` / ``token`` is set:

    - ``resolved`` (a live ``DiscoveredSession``): live-inject first, mux pane
      next, durable floor on miss, addressed to its canonical handle.
    - ``token`` (discovery MISSED, which is no verdict on reachability): the
      full ladder -- inject-as-probe, asleep resolution, wake-and-deliver, then
      a durable demotion naming each failed lane. Raises
      ``UnreachableTokenError`` when no store knows the token, so the caller
      exits 16 having queued nothing, and ``AmbiguousTokenError`` rather than
      guessing between two sessions.
    - neither: durable-only, addressed to ``recipient``.

    ``reply_to`` stamps BOTH the wire attr and the bus ``in_reply_to`` from ONE
    msg-id. Exits 12 on a durable-floor write failure."""
    from fno.agents.dispatch import (
        BUS_ONLY_POLICY,
        _mail_inject_claude,
        _mail_inject_codex,
        _mail_inject_keeper,
        _mux_pane_send,
    )
    from fno.agents.harness_map import thread_lane_or_none
    from fno.agents.registry import AgentResolutionError, resolve_agent
    from fno.agents.self_stamp import (
        resolve_self_harness,
        resolve_self_model,
        stamp_from,
    )
    from fno.agents.store_fallback import is_full_session_id, is_session_shaped
    from fno.harness_identity import canonical_handle, session_identity_key
    from fno.inbox.store import (
        classify_durable_owner,
        generate_msg_id,
        write_new_thread,
    )
    from fno.mail.envelope import wrap_fno_mail

    self_send = False
    # The recipient's full session id when a lane resolved one; it stamps that
    # session's own role into the live envelope.
    recipient_session: Optional[str] = None
    if resolved is not None:
        recipient_session = resolved.session_id
        recipient = canonical_handle(resolved.session_id)
        provider = resolved.agent
        self_send = _self_recipient(
            recipient, resolved_session_id=resolved.session_id
        ) is not None
    elif token is not None:
        # Resolve BEFORE addressing (docs/architecture/cross-agent-bus-log.md
        # #name-lane-address-resolution). A clean miss may still be this ambient
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
            # The collision escape hatch: address the durable copy by the full
            # id, never canonical_handle.
            recipient = session_identity_key(token)
        elif token_reachable is not None:
            recipient = canonical_handle(token_reachable.session_id)
        else:
            # A non-id token is not a mail address and would strand; --force
            # is the exception because it writes no durable row. Both rules:
            # docs/architecture/cross-agent-bus-log.md
            # #name-lane-address-resolution. A bare hex short-handle of a
            # session no store knows still earns a durable write.
            if not is_session_shaped(token):
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
                recipient_session = forced_session
            else:
                recipient = token
        if recipient_session is None:
            recipient_session = token_reachable.session_id if token_reachable else (
                session_identity_key(token) if is_full_session_id(token) else None)
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
    sender_harness = resolve_self_harness()
    sender_model = resolve_self_model()
    # The collision-safe reply address: `from` is the display handle,
    # `from_session` the full id a recipient can answer when two workers share a
    # head-8 clock bucket. None when unprovable, and then omitted, never guessed.
    sender_session = _reply_session_for(from_name)
    def _envelope(to_session: Optional[str] = None) -> str:
        return wrap_fno_mail(
            message,
            from_=sender,
            harness=sender_harness,
            to=recipient,
            id=msg_id,
            reply_to=reply_to,
            from_session=sender_session,
            origin=origin,
            to_session=to_session,
            subject=subject,
        )

    # Live carries the recipient's role; the durable floor below carries none,
    # being read whenever the recipient drains.
    wrapped = _envelope(recipient_session)

    # --force (node): change the TRANSPORT, keep every mail semantic. The
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
            subject=subject,
            record_body=wrapped,
        )
        return

    from fno.bus.log import record_hosted_delivery, stage_before_live

    record: dict[str, Any] = dict(
        msg_id=msg_id, sender=sender, recipient=recipient, body=wrapped,
        from_harness=sender_harness, in_reply_to=reply_to, from_session=sender_session,
        from_model=sender_model, to_kind="name", word_count=authored_words, subject=subject,
    )
    # Stage the body row first for a known recipient: the bus row is the body's
    # only home, so it must be on the bus before the inject can confirm. A token
    # no store knows exits 16 having sent nothing, so it gets no staged row. A
    # staging failure means the bus door is unhealthy; skip the live rungs and
    # let the durable floor below carry the message.
    live_reason: Optional[str] = None
    staged = False
    if not self_send and (resolved is not None or (token and token_reachable)):
        try:
            stage_before_live(**record)
            staged = True
        except Exception as exc:  # noqa: BLE001 - demote to durable
            print(f"body row not staged; skipping live delivery: {exc}", file=sys.stderr)
            live_reason = "stage-failed"
    injected = False
    woken_as: Optional[str] = None
    lanes: list[str] = []
    # node: the live lane's own cause when a claude inject misses, so the
    # durable receipt names it (e.g. not-confirmed) instead of a bare live-miss.
    to_session: Optional[str] = recipient_session
    to_harness: Optional[str] = resolved.agent if resolved is not None else None

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
        elif live_reason == "stage-failed":
            # The bus door refused the staged body row, so no rung may confirm
            # into the id; demote to the durable floor. A stage that was never
            # attempted (no resolvable recipient) keeps the base ladder below.
            lanes.append("stage-failed")
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
            # Always the probed session, even with no registry row for it, so
            # the landed check has something to grep either way.
            to_session = probe_target
            if probe_agent == "codex":
                _codex_probe_reason: list = []
                injected = _mail_inject_codex(
                    probe_target, wrapped, reason_out=_codex_probe_reason
                )
                if injected:
                    to_harness = "codex"
                if not injected:
                    live_reason = ";".join(_codex_probe_reason) or None
            else:
                _probe_reason: list = []
                injected = _mail_inject_claude(probe_target, wrapped, reason_out=_probe_reason)
                if injected:
                    to_harness = "claude"
                if not injected:
                    live_reason = ";".join(_probe_reason) or None
                if not injected and probe_agent is None:
                    _both_reason: list = []
                    injected = _mail_inject_codex(
                        probe_target, wrapped, reason_out=_both_reason
                    )
                    if injected:
                        to_harness = "codex"
                    if not injected and _both_reason:
                        live_reason = ";".join(_both_reason) or None
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
                    # bus-only also declines the wake rung. Waking
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
    elif staged and resolved is not None:
        # keeper-lane membership is contract-derived. The non-raising
        # read keeps a row whose harness the table has dropped on its
        # pre-keeper fall-through lanes (roster rung, durable floor).
        lane_harness = resolved.agent
        _keeper_recipient = thread_lane_or_none(lane_harness) == "keeper"
        # And only when the row really hosts a keeper thread. A keeper-LANE
        # harness can also sit in a live mux pane (opencode, agy): those rows
        # carry no mux/threads socket, the verb would answer not-injectable,
        # and intercepting them here would strand the pane rung below. The
        # predicate mirrors the verb's own row resolution.
        _keeper_thread_row = _keeper_recipient and _row_hosts_keeper_thread(
            resolved.session_id
        )
        if provider == "claude":
            _resolved_reason: list = []
            injected = _mail_inject_claude(
                resolved.session_id, wrapped, reason_out=_resolved_reason
            )
            if not injected:
                live_reason = ";".join(_resolved_reason) or None
        elif provider == "codex":
            _resolved_codex_reason: list = []
            injected = _mail_inject_codex(
                resolved.session_id, wrapped, reason_out=_resolved_codex_reason
            )
            if not injected:
                live_reason = ";".join(_resolved_codex_reason) or None
        elif _keeper_thread_row:
            # a keeper-hosted lane-B thread has neither lane-A socket.
            # Its live transport is the keeper's own unix socket, resolved by
            # the verb from the registry row the same resolution above already
            # read `provider` from. A miss falls through with the verb's own
            # reason token (no-keeper-listener / not-confirmed / ...), so the
            # durable demotion below names why instead of a bare live-miss.
            _resolved_keeper_reason: list = []
            injected = _mail_inject_keeper(
                resolved.session_id,
                wrapped,
                harness=lane_harness,
                reason_out=_resolved_keeper_reason,
            )
            if not injected:
                live_reason = ";".join(_resolved_keeper_reason) or None
        if not injected and not _keeper_thread_row:
            # A send addressed by session id never consults the roster, so a
            # mux-hosted session of any provider would demote to durable with a
            # live pane right there. Not-found means "not mux-hosted", not an error.
            # A keeper THREAD row sits this rung out: its live transport
            # IS the keeper socket the rung above just attempted, the row hosts
            # no pane by design, and a stale ref would type into an unrelated
            # pane and read as delivered.
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
    if injected:
        try:
            # The lane-refined `to_harness` is the harness actually injected
            # into, more truthful than the routing token's `provider`.
            record_hosted_delivery(**record, to_harness=to_harness, to_session=to_session)
        except Exception as exc:  # noqa: BLE001 - delivery already succeeded
            print(
                "delivery succeeded; receipt row failed; the staged bus row "
                f"holds the body; do not retry: {exc}",
                file=sys.stderr,
            )
    if injected and woken_as:
        print(json_receipt(
            msg_id,
            to=recipient,
            status=f"delivered (woken) [revived as bg thread {woken_as}]",
            subject=subject,
        ))
        if corr:
            print(corr.strip())
        return
    if injected:
        print(json_receipt(
            msg_id, to=recipient, status=f"delivered (hosted){live}{corr}", subject=subject,
        ))
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

    # a bus-only queue is DESIGNED, not stranded. The recipient polls
    # the durable bus at each turn boundary (the native verb), so this is
    # delivery on the recipient's terms -- no recovery warning, no escalation,
    # and a receipt that says so instead of reading as a live-miss.
    bus_only = live_reason == BUS_ONLY_POLICY
    try:
        th = write_new_thread(
            recipient=recipient,
            sender=stamp_from(from_name),
            kind="send",
            body=_envelope(),
            msg_id=msg_id,
            to_kind="name",
            to_harness=provider,
            replies_to=reply_to,
            owner=owner.value,
            # The durable floor carries the same full sender id the live
            # envelope does, so a drained reply resolves the collision-safe
            # address exactly as a live one does (node).
            from_session=sender_session,
            # And the same sender provenance the hosted and typed rows carry:
            # the compact envelope no longer renders the model, so the durable
            # row is where audit reads it.
            from_harness=sender_harness,
            from_model=sender_model,
            word_count=authored_words,
            origin=origin,
            subject=subject,
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
    # named its own cause (node), carry that token so a miss to a LIVE
    # recipient reads as its real cause (e.g. not-confirmed), not a bare
    # live-miss that reads as a dead recipient. A bus-only queue is neither: it
    # is the recipient's declared delivery policy, and its receipt says the
    # message WILL surface at the recipient's turn boundary.
    # a busy-mode hold is a bus-only flag with a clock on it, and the
    # generic receipt above would promise a turn boundary that is not coming.
    # Say what is actually true: it is held, here is when it lands.
    hold_note = None
    if bus_only:
        from fno.mail import hold as _hold

        hold_note = _hold.bounce_reason(recipient)
    if bus_only:
        reason = hold_note or "DND (bus-only): recipient polls the bus at each turn boundary"
    else:
        # a live-lane failure renders as legs on stdout; the raw token
        # (io-error, attach-failed, ...) stays diagnostic on stderr, because an
        # error string inside a success receipt reads as a broken lane.
        reason = durable_leg_story(live_reason, recipient) or (
            "self-send" if self_send else (live_reason or "live-miss")
        )
        if reason == "live-miss":
            reason = f"live-miss{_live_miss_age_suffix(recipient)}"
    window_tail = durable_window_clause(owner.value)
    hint = ""
    if hold_note:
        hint = f" `fno agents mail withdraw {msg_id}` retracts it."
    if not self_send and not bus_only and provider == "codex" and _codex_daemon_socket_absent():
        hint = (
            " codex app-server daemon not running: run "
            "`codex app-server daemon start`, then restart the session "
            "(the socket must exist before the codex TUI starts)"
        )
    print(json_receipt(
        th.thread_id,
        to=recipient,
        status=f"queued (durable){live}{corr} [{reason}]{window_tail}",
        subject=subject,
    ))
    if hint:
        print(hint.strip())
    # Live-miss escalation lane (node widened this from attended-only). A
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

    # Post-send verify: a durable receipt is not a landing. A bus-only queue
    # and a self-send are designed lanes, not misses, and keep their designed
    # receipts. NOT LANDED exits non-zero so a last-line reader cannot record
    # it as delivered.
    if not bus_only and not self_send:
        from fno.mail.receipts import NOT_LANDED_EXIT, report_landing

        if not report_landing(
            msg_id,
            target=recipient,
            to=recipient,
            to_harness=to_harness or provider,
            to_session=to_session,
        ):
            raise typer.Exit(code=NOT_LANDED_EXIT)


# Send-time human escalation for a question, per (sender, recipient). A burst
# re-nudges every window rather than once forever (marker refreshed only on an
# actual escalation, so the window runs from the last nudge, not the first send).
# Send-time human escalation moved to fno.mail.receipts (file-budget: this
# module is shrink-only); import it there.


def _resolve_to_lead_address(
    scope: str,
    name: str | None,
    message: str | None,
    *,
    conflict: str | None,
) -> tuple[str, str]:
    """Resolve ``--to-lead <scope>`` to (holder handle, body), or refuse.

    The body is the sole positional, so it parks in ``name``. A refusal queues
    nothing: a vacant role has no reader who answers as lead.
    """
    from fno.agents.role import resolve_to_lead

    if conflict is not None or message is not None:
        clash = conflict or "a second positional (the message is the only one)"
        print(
            f"error: --to-lead and {clash} are mutually exclusive. Run "
            f"`fno agents team` and address that handle for the other lane.",
            file=sys.stderr,
        )
        raise typer.Exit(2)
    if not name:
        print("usage: fno agents mail send --to-lead <scope> <message>", file=sys.stderr)
        raise typer.Exit(2)
    try:
        holders = resolve_to_lead(scope)
    except (OSError, ValueError, RuntimeError) as exc:  # RegistryVersionError
        print(f"error: --to-lead {scope!r}: registry unreadable: {exc}", file=sys.stderr)
        raise typer.Exit(12) from exc
    if not holders:
        print(
            f"--to-lead {scope!r} refused: no live row holds this role right "
            f"now, and nothing was queued. Run `fno agents team` to see who "
            f"does, or address a specific handle.",
            file=sys.stderr,
        )
        raise typer.Exit(16)
    if len(holders) > 1:
        print(
            f"--to-lead {scope!r} is a split role: {len(holders)} live rows "
            f"hold it ({', '.join(holders)}), so it went to none. Run "
            f"`fno agents team` and resolve it, or address one by name.",
            file=sys.stderr,
        )
        raise typer.Exit(17)
    print(f"--to-lead {scope}: resolved to {holders[0]}", file=sys.stderr)
    return holders[0], name


@mail_app.command("send")
def mail_send_forward(
    name: str | None = typer.Argument(
        None,
        help=(
            "Agent name, short-id, or full session id. A codex head-8 is a "
            "65.536-second UUIDv7 bucket, not random, and is refused against "
            "a codex row; claude ids are UUIDv4, so either form works."
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
        help="XML-safe sender. Unset: session handle or 'fno' for agents; project for inbox notes.",
    ),
    origin: str | None = typer.Option(
        None,
        "--origin",
        hidden=True,
        help="Stamped machine mail origin: operator, peer, scheduler, or recovery.",
    ),
    ruling: str | None = typer.Option(
        None,
        "--ruling",
        help="Append the message to the governed node's details as a dated ruling block.",
    ),
    from_self: bool = typer.Option(
        False, "--from-self",
        help=(
            "Stamp the sender with this session's own canonical mail handle "
            "(the reply handle `fno whoami` shows). Fails loud (exit 2) with "
            "no ambient harness identity."
        ),
    ),
    to_project: str | None = typer.Option(
        None, "--to-project",
        help=(
            "Anycast: deliver to whoever works on this project (live if exactly "
            "one peer, durable queue if none). Use instead of <name>."
        ),
    ),
    to_lead: str | None = typer.Option(
        None, "--to-lead",
        help=(
            "Anycast over the role: deliver to whoever holds this scope "
            "RIGHT NOW, resolved at send time; no live holder queues nothing."
        ),
    ),
    any_live: bool = typer.Option(
        False, "--any",
        help="With --to-project, break a multi-live-peer tie (most recent activity wins).",
    ),
    kind: str | None = typer.Option(
        None, "--kind", "-k",
        help=(
            "Inbox kind (heads-up | question | fyi); pair with --to-project. "
            "Omit for a plain agent-to-agent send."
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
        help="Message body in ANY mode (alternative to the positional arg).",
    ),
    body_file: Path | None = typer.Option(
        None, "--body-file",
        help="Read the body from a file ('-' = stdin); binds in every mode.",
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
            "Inject the payload UNWRAPPED at the recipient's prompt line: one "
            "line, typed verbatim - a command only: it must start with / or $. "
            "A status report or any authored message goes WRAPPED (drop --raw) "
            "so its sender stays visible. A payload starting /fno: or $fno: is "
            "an fno verb, and the lane rewrites the marker to the form of the "
            "receiving harness. A showing prompt is answered with `fno agents "
            "ask`. Never queues durable. An actor OTHER than the model must "
            "supply the trigger; self-injection is barred unless --to-self. "
            "Mechanics and the reviewer-off-the-author rationale: "
            "docs/architecture/review-lanes.md."
        ),
    ),
    check: bool = typer.Option(
        False, "--check",
        help=(
            "With --raw: report whether an injection path EXISTS and inject "
            "nothing: 'injectable: <lane>' (exit 0), 'not-injectable' (1), "
            "'unmeasurable' (3), malformed payload (2). Reports a PATH only."
        ),
    ),
    to_self: bool = typer.Option(
        False, "--to-self",
        help=(
            "Address this session as the recipient (no <id> needed); with "
            "--raw the envelope is stripped so a slash command parses at your "
            "own prompt line."
        ),
    ),
    force: bool = typer.Option(
        False, "--force", "-F",
        help=(
            "Deliver over the PANE transport: type the wrapped body at the "
            "recipient's prompt, keeping every mail semantic. The receipt says "
            "`typed (pane <id>)`, NEVER `delivered` - bytes written to a PTY "
            "is not action. It reads the pane first and refuses a showing "
            "prompt."
        ),
    ),
    style_exception: str | None = typer.Option(
        None, "--style-exception",
        help="Bypass the style check for this body with a stated reason.",
    ),
) -> None:
    """Forward the wrapped send verb to the Rust leaf.

    The send body lives in the Rust mail front now; Python owns --help and
    this shell. The relay's byte-verbatim forward lands here for a plain
    name it cannot bind, and this execs the native verb.
    """
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    argv = sys.argv[1:]
    if "send" in argv:
        argv = argv[argv.index("send") + 1:]
    else:
        argv = []
    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno mail send: the fno-agents binary was not found. "
            "It ships in the `pip install fno` wheel and with the plugin; "
            "reinstall fno or run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=127)
    result = subprocess.run(
        [str(binary), "mail", "send", *argv],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.stdout:
        typer.echo(result.stdout.rstrip("\n"))
    if result.stderr:
        typer.echo(result.stderr.rstrip("\n"), err=True)
    raise typer.Exit(code=propagate_returncode(result.returncode))

@mail_app.command("team", context_settings={"allow_extra_args": True, "ignore_unknown_options": True})
def team_forward(ctx: typer.Context) -> None:
    """Announce one body to a fleet scope as ONE bus line.

    The body lives in the Rust mail front now; Python owns transport only,
    the same split as the hold forward. The body checks (forged envelope,
    brevity cap, style) and the announce writer run on the native leg.
    Announcement flags belong to the Rust writer and relay verbatim:
    `--subject=S` (supersede key; the = spelling keeps click from parking
    the value in the positional body), `--expires=45m|24h|7d` (standing
    window, default 24h, max 7d), `--urgent`. Any unrecognized flag is
    passed through the same way and refused there, so this shim adds no
    Python flag surface.
    """
    _forward_mail(["team", *ctx.args], label="fno mail team")


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
    cursor. JSON when stdout is not a TTY or ``--json`` is passed. This leaf
    forwards to the native fno-agents reader and relays its streams.
    """
    _forward_unread(name, json_out)


def _forward_unread(name: str, json_out: bool) -> None:
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno agents mail unread: the fno-agents binary was not found; reinstall fno "
            "or run `fno doctor update --rust`, or set FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=127)
    # The binary's own bus ladder skips the settings file, so pin FNO_BUS_DIR.
    env = dict(os.environ, FNO_BUS_DIR=str(paths.bus_dir()))
    # A terminal inherits stdio; a captured stdout (pipe, tests) is relayed.
    tty = sys.stdout is not None and sys.stdout.isatty()
    result = subprocess.run(
        [str(binary), "mail", "unread", "--name", name, *(["--json"] if json_out else [])],
        env=env, capture_output=not tty, text=True, check=False,
    )
    if not tty:
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
    raise typer.Exit(code=propagate_returncode(result.returncode))


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

    ``landed`` proves the text reached the recipient's own transcript, never
    that the agent read or acted on it -- that fourth state isn't observable.
    """
    # The body lives in the Rust mail front now; Python forwards, the same
    # split as the hold verbs.
    argv = ["sent"]
    if unclaimed_only:
        argv.append("--unclaimed")
    if from_name is not None:
        argv += ["--from-name", from_name]
    if json_out:
        argv.append("--json")
    _forward_mail(argv, label="fno mail sent")


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
    _forward_mail(
        [
            "withdraw",
            msg_id,
            *(["--from-name", from_name] if from_name is not None else []),
        ],
        label="fno mail withdraw",
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
    _forward_mail(["ack", msg_id, "--name", name], label="fno mail ack")


def _forward_mail(argv: list[str], label: str = "fno mail hold") -> None:
    """Forward one mail-front verb to the bundled fno-agents binary.

    The hold, team, sent, withdraw, ack, and drain-self verbs' bodies live in
    the Rust
    mail front now; Python owns transport only, the same split as the claims
    forward.
    """
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        typer.echo(
            f"{label}: the fno-agents binary was not found. "
            "It ships in the `pip install fno` wheel and with the plugin; "
            "reinstall fno or run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=127)
    # Captured and re-emitted through typer: the leaf's output IS the
    # operator surface, and a CliRunner-hosted caller (the tests) must see it.
    result = subprocess.run(
        [str(binary), "mail", *argv],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.stdout:
        typer.echo(result.stdout.rstrip("\n"))
    if result.stderr:
        typer.echo(result.stderr.rstrip("\n"), err=True)
    raise typer.Exit(code=propagate_returncode(result.returncode))


def _forward_mail_pane_prepare(argv: list[str]) -> None:
    """Forward the pane-send renderer to the bundled fno-agents binary.

    Same split as the hold forward with one deliberate difference: stdout is
    written byte-exact, because the ``fno mux pane send`` caller types these
    bytes at the pane verbatim and an appended newline becomes a submitted
    blank line. The hold verbs print for an operator, where the extra newline
    is nothing.
    """
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno mail pane-prepare: the fno-agents binary was not found. "
            "It ships in the `pip install fno` wheel and with the plugin; "
            "reinstall fno or run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=127)
    # Byte mode on purpose: text=True enables universal newlines, which
    # rewrites the envelope's CRLF line endings and breaks the byte-exact
    # contract the docstring states.
    result = subprocess.run(
        [str(binary), "mail", *argv],
        capture_output=True,
        check=False,
    )
    if result.stdout:
        sys.stdout.buffer.write(result.stdout)
    if result.stderr:
        typer.echo(result.stderr.decode(errors="replace").rstrip("\n"), err=True)
    raise typer.Exit(code=propagate_returncode(result.returncode))


def cmd_hold(
    minutes: int = typer.Option(
        None,
        "--minutes",
        "-m",
        help="Idle minutes before the hold lifts by itself (default 5). The "
        "quiet window restarts every prompt and ends at 2x the requested window.",
    ),
    for_minutes: int = typer.Option(
        None,
        "--for",
        help="Wall-clock minutes before the hold lifts. The deadline never moves.",
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
    Either clock DELIVERS without a new prompt, so a hold whose only drain
    trigger is the operator cannot stall.
    """
    argv = ["hold"]
    if minutes is not None:
        argv += ["--minutes", str(minutes)]
    if for_minutes is not None:
        argv += ["--for", str(for_minutes)]
    if off:
        argv.append("--off")
    if status:
        argv.append("--status")
    _forward_mail(argv)


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
    _forward_mail(["hold-release", "--handle", handle, "--poll-s", str(poll_s)])


mail_app.command("hold")(cmd_hold)
mail_app.command("hold-release", hidden=True)(cmd_hold_release)


def cmd_seed_provenance_front() -> None:
    """Forward the hidden seed-provenance verb to the bundled fno-agents binary.

    The render body lives in the Rust mail front now; Python owns transport
    only, the same split as the hold forward. The launcher
    (hooks/spawn-seed-provenance-session-start.sh) exports the FNO_SEED_PROV_*
    sidecar, which owns the source=startup gate and the hook JSON.

    Exit 1 with no output when there is nothing to attribute: a hand-started
    session, an operator spawn, or an unusable sidecar. The hook stays silent
    on that, because a session with no peer sender has nothing to attribute.
    """
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        typer.echo(
            "fno mail seed-provenance: the fno-agents binary was not found. "
            "It ships in the `pip install fno` wheel and with the plugin; "
            "reinstall fno or run `fno doctor update --rust`, or set "
            "FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=127)
    result = subprocess.run(
        [str(binary), "mail", "seed-provenance"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.stdout:
        typer.echo(result.stdout.rstrip("\n"))
    if result.stderr:
        typer.echo(result.stderr.rstrip("\n"), err=True)
    raise typer.Exit(code=propagate_returncode(result.returncode))


mail_app.command("seed-provenance", hidden=True)(cmd_seed_provenance_front)


@mail_app.command("drain-self", hidden=True)
def cmd_drain_self_forward(
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON regardless of TTY."
    ),
) -> None:
    """Drain THIS session's own cross-harness inbox and mark it seen (US5).

    Forward-only: the body lives in the Rust mail front, on the same mount as
    the hold verbs. Python owns transport only. The behavior contract is
    characterized byte-for-byte by the Rust parity suite frozen from this
    leaf's goldens (crates/fno-agents/tests/mail_drain_self_parity.rs).
    """
    argv = ["drain-self"]
    if json_out:
        argv.append("--json")
    _forward_mail(argv, label="fno agents mail drain-self")


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

    LD2 : the jsonl bus log is the source of truth; the per-recipient
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
