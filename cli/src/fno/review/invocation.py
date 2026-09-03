"""Build and join review-invocation observations.

The sender and reviewer run in different processes, so the invocation id is
kept in a best-effort per-session sidecar while the canonical event remains in
the worktree's ``.fno/events.jsonl`` journal.
"""
from __future__ import annotations

import json
import os
import secrets
import shlex
from pathlib import Path
from typing import Any


REVIEW_VERBS = frozenset({"code-review", "review", "review-changes", "sigma-review"})
REVIEW_LEVELS = frozenset({"low", "medium", "high", "xhigh", "max"})

# The review flags whose spelling this module is the authority for: the router
# prose cites this vocabulary rather than restating it, so a spelling accepted
# in one place is accepted in the other and the two cannot drift.
KNOWN_REVIEW_FLAGS = frozenset({"comment", "fix"})
_EM_DASH = "\u2014"


def canonical_flag(token: str) -> "str | None":
    """Return the canonical ``--`` spelling when ``token`` is a review flag.

    Three spellings reach the router and this parser for the same flag: the
    canonical double hyphen, the bare word an operator drops the hyphens
    from, and the single em dash a phone autocorrect substitutes for the
    double hyphen. The em-dash alias is deliberately narrow: exactly one
    character, immediately followed by a KNOWN flag name - no other dash (en
    dash, minus sign, horizontal bar) is a flag, because no legitimate target
    begins with an em dash while those characters do occur in prose and
    paths. Bare words count only for the known names, so a branch literally
    named ``fix`` is the one casualty and stays reachable through its
    double-hyphen spelling or a fuller ref form. Unknown ``--`` tokens return
    None here; the parser still records them verbatim.
    """
    name = token
    if name.startswith("--"):
        name = name[2:]
    elif name.startswith(_EM_DASH):
        name = name[1:]
    return f"--{name}" if name in KNOWN_REVIEW_FLAGS else None


def mint_invocation_id() -> str:
    """Return a unique, human-identifiable review invocation id."""
    return f"ri-{secrets.token_hex(16)}"


def _home(home: Path | None) -> Path:
    if home is not None:
        return Path(home)
    configured_home = os.environ.get("FNO_HOME")
    if configured_home:
        return Path(configured_home)
    from fno import paths

    return paths.state_dir()


def pending_invocation_path(target_session_id: str, *, home: Path | None = None) -> Path:
    """Return the sidecar path for one target session."""
    if not target_session_id or Path(target_session_id).name != target_session_id:
        raise ValueError("target_session_id must be a non-empty path component")
    return _home(home) / "review-invocations" / f"{target_session_id}.json"


def write_pending_invocation(
    *,
    target_session_id: str,
    invocation_id: str,
    home: Path | None = None,
) -> bool:
    """Write one pending id without replacing a concurrent sender's id.

    A sidecar failure is deliberately best-effort. The sender event remains
    useful by itself, and the reviewer can mint an unjoined id if adoption is
    unavailable.
    """
    try:
        path = pending_invocation_path(target_session_id, home=home)
        path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                json.dump(
                    {
                        "invocation_id": invocation_id,
                        "target_session_id": target_session_id,
                    },
                    stream,
                    separators=(",", ":"),
                )
        except Exception:
            try:
                path.unlink()
            except OSError:
                pass
            raise
        return True
    except (OSError, TypeError, ValueError):
        return False


def adopt_pending_invocation(
    target_session_id: str,
    *,
    home: Path | None = None,
) -> str | None:
    """Read and consume a pending id, returning ``None`` on any miss.

    The sidecar is ALWAYS consumed when it exists, even on an unparseable
    read: one corrupt leftover must not block every later join by making the
    O_EXCL writer fail forever while senders keep minting unjoinable ids.
    """
    try:
        path = pending_invocation_path(target_session_id, home=home)
    except (TypeError, ValueError):
        return None
    invocation_id = None
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(raw, dict):
            candidate = raw.get("invocation_id")
            if isinstance(candidate, str) and candidate:
                invocation_id = candidate
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    try:
        path.unlink()
    except OSError:
        pass
    return invocation_id


def build_review_invocation_data(
    *,
    invocation_id: str,
    stage: str,
    verb: str,
    **observations: Any,
) -> dict[str, Any]:
    """Build payload data while omitting only unmeasured ``None`` values."""
    data: dict[str, Any] = {
        "invocation_id": invocation_id,
        "stage": stage,
        "verb": verb,
    }
    data.update({key: value for key, value in observations.items() if value is not None})
    return data


def build_review_invocation_event(
    *,
    source: str,
    invocation_id: str,
    stage: str,
    verb: str,
    **observations: Any,
) -> dict[str, Any]:
    """Build and schema-validate one canonical review invocation event."""
    from fno.events import _build

    return _build(
        "review_invocation",
        source,
        build_review_invocation_data(
            invocation_id=invocation_id,
            stage=stage,
            verb=verb,
            **observations,
        ),
    )


def emit_review_invocation(
    *,
    source: str,
    invocation_id: str,
    stage: str,
    verb: str,
    events_path: Path | None = None,
    **observations: Any,
) -> dict[str, Any] | None:
    """Append one event, returning it; event failures never block a review."""
    from fno.events import append_event

    try:
        event = build_review_invocation_event(
            source=source,
            invocation_id=invocation_id,
            stage=stage,
            verb=verb,
            **observations,
        )
        append_event(event, events_path=events_path)
        return event
    except Exception:
        return None


def parse_review_invocation(raw: str) -> dict[str, Any] | None:
    """Parse a review command without altering its raw argument string."""
    candidate = raw.strip()
    if not candidate:
        return None
    parts = candidate.split(None, 1)
    token = parts[0].lstrip("/")
    name = token.rsplit(":", 1)[-1]
    if name not in REVIEW_VERBS:
        return None
    args_raw = parts[1] if len(parts) == 2 else ""
    try:
        args = shlex.split(args_raw)
    except ValueError:
        args = args_raw.split()
    level = "unset"
    level_source = "fallback"
    if args and args[0] == "ultra":
        level = "ultra"
        level_source = "ultra_forced"
    elif args and args[0] in REVIEW_LEVELS:
        level = args[0]
        level_source = "explicit"
    # One flag normalizer serves the telemetry and the router prose. A known
    # flag records its canonical spelling whatever it arrived as; an unknown
    # double-hyphen token still records verbatim, so a novel flag is visible
    # in the record rather than silently dropped.
    flags: "list[str]" = []
    for arg in args:
        canonical = canonical_flag(arg)
        if canonical:
            flags.append(canonical)
        elif arg.startswith("--"):
            flags.append(arg)
    return {
        "verb": f"/{name}",
        "args_raw": args_raw,
        "level": level,
        "level_source": level_source,
        "flags": flags,
    }


#: The invocation id the single writer stamps when it cannot join a real one;
#: an attestation carrying it settles nothing (no row exists to close).
_UNJOINED = "UNJOINED"


def settle_lost_invocations(
    *,
    home: Path | None = None,
    ttl_minutes: int = 15,
    cwd: Path | None = None,
    events_path: Path | None = None,
    now: Any = None,
) -> "list[dict[str, Any]]":
    """Turn every lost review invocation into one settled ledger row.

    A ``review_invocation`` row with ``stage: sent`` and no answering
    ``review_attestation`` after ``ttl_minutes`` reads as "never asked"; this
    emits ONE ``review_attestation`` per lost row through the single validated
    builder (verdict ``fail``, ``output_contract: lost``), so coverage reads
    ``uncovered`` with a named reason and the loop that already re-fires on
    uncovered re-dispatches. Settling makes the loss VISIBLE; it schedules
    nothing new. Idempotent by a positive marker: any attestation carrying the
    id answers it, once. ``settled: False`` names the refusal and writes
    nothing.
    """
    from datetime import datetime, timedelta, timezone

    if events_path is None:
        from fno.paths import project_events_json

        events_path = project_events_json()
    observed_at = now or datetime.now(timezone.utc)
    cutoff = observed_at - timedelta(minutes=ttl_minutes)
    sent: "dict[str, tuple[datetime, dict[str, Any]]]" = {}
    answered: "set[str]" = set()
    try:
        with Path(events_path).open(encoding="utf-8") as stream:
            for raw in stream:  # prefiltered: the journal is tens of MB on the stop path
                if "review_invocation" not in raw and "review_attestation" not in raw:
                    continue
                try:
                    event = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                data = event.get("data")
                if not isinstance(data, dict):
                    continue
                invocation_id = data.get("invocation_id")
                if not isinstance(invocation_id, str) or not invocation_id:
                    continue
                if invocation_id == _UNJOINED:
                    continue
                if event.get("type") == "review_invocation":
                    if data.get("stage") == "sent":
                        try:
                            event_time = datetime.fromisoformat(
                                str(event.get("ts", "")).replace("Z", "+00:00")
                            )
                        except ValueError:
                            continue
                        if event_time.tzinfo is None:
                            event_time = event_time.replace(tzinfo=timezone.utc)
                        # First sent row wins: the OLDER timestamp is the
                        # conservative TTL input.
                        sent.setdefault(invocation_id, (event_time, data))
                    elif data.get("stage") == "refused":
                        answered.add(invocation_id)
                elif event.get("type") == "review_attestation":
                    answered.add(invocation_id)
    except (FileNotFoundError, OSError):
        return []

    lost = [
        (invocation_id, event_time, data)
        for invocation_id, (event_time, data) in sorted(sent.items())
        if event_time <= cutoff and invocation_id not in answered
    ]
    if not lost:
        return []
    head_sha, branch = _settle_head_pin(cwd)
    results: "list[dict[str, Any]]" = []
    for invocation_id, _event_time, data in lost:
        if not head_sha or not branch:
            results.append(
                {
                    "invocation_id": invocation_id,
                    "settled": False,
                    "reason": "no readable head to pin (not a git repo or detached HEAD)",
                }
            )
            continue
        from fno.events import _build, append_event

        reviewer = str(data.get("verb") or "/code-review").lstrip("/").split(":")[-1]
        try:
            append_event(
                _build(
                    "review_attestation",
                    "hook",
                    {
                        "reviewer": reviewer or "code-review",
                        "head_sha": head_sha,
                        "verdict": "fail",
                        # The settle context, not a reviewer's session: this row
                        # answers "the attempt died", never "a review ran".
                        "session_id": "settle",
                        "output_contract": "lost",
                        "invocation_id": invocation_id,
                        "settle_note": (
                            "invocation lost: sent, delivered, and never answered; "
                            "emitted by the settle sweep"
                        ),
                    },
                ),
                events_path=events_path,
            )
        except Exception as exc:  # noqa: BLE001 - a failed settle is reported, not raised
            results.append(
                {"invocation_id": invocation_id, "settled": False, "reason": str(exc)}
            )
            continue
        results.append({"invocation_id": invocation_id, "settled": True, "head": head_sha})
    return results


def _settle_head_pin(cwd: Path | None) -> "tuple[str, str]":
    """`(head_sha, branch)` at ``cwd``, both empty when either is unreadable
    (the writer refuses a detached HEAD; the settle inherits that refusal)."""
    import subprocess

    def _git(*args: str) -> str:
        try:
            proc = subprocess.run(
                ["git", *args],
                cwd=str(cwd) if cwd else None,
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (OSError, subprocess.TimeoutExpired):
            return ""
        return proc.stdout.strip() if proc.returncode == 0 else ""

    head_sha = _git("rev-parse", "HEAD")
    branch = _git("rev-parse", "--abbrev-ref", "HEAD")
    if not head_sha or not branch or branch == "HEAD":
        return "", ""
    return head_sha, branch
