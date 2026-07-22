"""Inbox store: thread-per-file data layer.

Each thread is one markdown file under
``<inbox-agents-root>/{recipient}/inbox/{YYYY-MM-DD}-{slug}.md`` (the root
is vault-derived when Obsidian is enabled, else under the state dir; see
``paths.inbox_agents_root``). Replies append to the same thread file rather
than creating a new one.

Frontmatter (YAML):
    thread_id: msg-XXXXXX
    from: sender-project       # original sender of the root message
    to: recipient-project
    kind: heads-up | question | fyi
    created: 2026-05-08T13:48:58Z
    read_at: 2026-05-08T14:00:00Z   # optional; absent = unread
    replies_to: msg-YYYYYY          # optional; cross-thread reference
    persist_to_memory: true         # optional; only with `--persist memory`
    ref_pr: 112                     # optional refs (passed through)
    ref_node: ab-...
    ref_gate: name
    mission_id: ...
    source_mission: ...
    cascade_of: ...

Body (one or more message blocks):
    ## msg-{id} · 2026-05-08T13:48:58Z · from:sender

    Message body text.

    ## msg-{id2} · 2026-05-08T14:00:00Z · from:other-sender

    Reply body.

Concurrency:
    Writes use a ``mkdir <path>.lock.d`` mutex (POSIX-atomic, macOS portable)
    keyed on the thread file path. Two senders racing on the same thread
    serialize on the directory create; each releases by removing the lock dir.

Public API:
    Kind                    - enum: HEADS_UP, QUESTION, FYI
    ThreadMessage           - one message inside a thread
    ThreadHandle            - one thread (frontmatter + messages)
    write_new_thread        - create a new thread file, return handle
    append_to_thread        - append a message to an existing thread file
    read_thread             - parse one thread file
    read_unread_threads     - list threads where ``read_at`` is absent
    read_all_threads        - list every thread file under a recipient
    mark_thread_read        - set ``read_at`` in frontmatter
    find_thread_by_msg_id   - find a thread containing a given msg-id
    inbox_dir_for           - path to a recipient's ``inbox/`` directory
    generate_msg_id         - generate a unique 'msg-XXXXXX' id
    resolve_project         - resolve project name from .fno/settings.yaml
    ProjectIdentificationError
"""
from __future__ import annotations

import json
import os
import re
import secrets
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Optional

import yaml


# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------

class Kind(str, Enum):
    HEADS_UP = "heads-up"
    QUESTION = "question"
    FYI = "fyi"
    # G2 cross-agent bus: async fire-and-forget message (send verb)
    SEND = "send"


VALID_KINDS = frozenset(k.value for k in Kind)


class DurableOwner(str, Enum):
    """Who drains a durable thread (the four-terminal invariant, US6).

    ``queued (durable)`` is transitional, never terminal: every durable write is
    stamped with the owner that will drain it, so a thread nobody can reach is a
    ``dead-letter`` from birth rather than silent quicksand.
    """

    LIVE_DRAIN = "live-drain"      # recipient has a turn boundary; drains next turn
    WAKE_DAEMON = "wake-daemon"    # asleep-but-resumable; woken by the wake daemon
    INBOX_DRAIN = "inbox-drain"    # deliberate --kind/--to-project note; project inbox drains it
    DEAD_LETTER = "dead-letter"    # no live session, no resumable session, no drain wiring


# TTL horizon per owner class (hours). Executor's discretion (epic Claude's
# Discretion 1). A dead-letter's ttl_at is its birth: it escalates immediately,
# not after a wait. The others give their drain one horizon to run before the
# sweep reclassifies an unread thread as a stranded dead-letter.
_OWNER_TTL_HOURS: dict[str, float] = {
    DurableOwner.LIVE_DRAIN.value: 1.0,
    DurableOwner.WAKE_DAEMON.value: 6.0,
    DurableOwner.INBOX_DRAIN.value: 24.0,
    DurableOwner.DEAD_LETTER.value: 0.0,
}


def classify_durable_owner(
    *,
    param_forced: bool,
    recipient_live: bool,
    recipient_resumable: bool,
) -> DurableOwner:
    """Classify a durable write's owner from the signals a send choke point has.

    Precedence is deliberate: a ``--kind``/``--to-project`` send chose the durable
    inbox lane on purpose (``param_forced``), so it owns as ``inbox-drain`` even
    when a live peer exists. Otherwise a live recipient drains on its next turn,
    an asleep-but-resumable one is woken by the daemon, and anything else is a
    dead-letter at birth.
    """
    if param_forced:
        return DurableOwner.INBOX_DRAIN
    if recipient_live:
        return DurableOwner.LIVE_DRAIN
    if recipient_resumable:
        return DurableOwner.WAKE_DAEMON
    return DurableOwner.DEAD_LETTER


def ttl_at_for(owner: DurableOwner | str, created: datetime) -> datetime:
    """The ``ttl_at`` horizon for an owner class, measured from ``created``."""
    from datetime import timedelta

    key = owner.value if isinstance(owner, DurableOwner) else owner
    return created + timedelta(hours=_OWNER_TTL_HOURS.get(key, 0.0))

# Map deprecated kinds -> what to use instead. Reading these tokens from
# the CLI exits non-zero with a hint pointing at the replacement.
DEPRECATED_KINDS: dict[str, str] = {
    "notification": "fyi",
    "lesson": "fyi --persist memory",
    "answer": "fyi --reply-to <msg-id>  (or any kind with --reply-to)",
    "complete": "fyi --reply-to <msg-id>",
}


# ---------------------------------------------------------------------------
# Dataclasses
# ---------------------------------------------------------------------------

@dataclass
class ThreadMessage:
    """One message block within a thread file body."""
    msg_id: str
    timestamp: datetime
    from_project: str
    body: str


@dataclass
class ThreadHandle:
    """One thread on disk: a single file under ``{recipient}/inbox/``."""
    thread_id: str
    path: Path
    from_project: str
    to_project: str
    kind: str
    created: datetime
    read_at: Optional[datetime]
    replies_to: Optional[str]
    persist_to_memory: bool
    refs: dict[str, str]
    messages: list[ThreadMessage] = field(default_factory=list)

    @property
    def is_unread(self) -> bool:
        return self.read_at is None

    @property
    def root_msg_id(self) -> str:
        if self.messages:
            return self.messages[0].msg_id
        return self.thread_id


# ---------------------------------------------------------------------------
# Exception
# ---------------------------------------------------------------------------

class ProjectIdentificationError(Exception):
    """Raised when project name cannot be resolved."""


# ---------------------------------------------------------------------------
# Path helpers
# ---------------------------------------------------------------------------

def _inbox_root() -> Optional[Path]:
    """Base directory holding per-project ``<project>/inbox/`` folders.

    Override via ``FNO_INBOX_ROOT`` env var (used by tests). When this
    override is set, callers compose ``root/<project>/inbox`` themselves.

    Returns ``None`` when no env override exists; production callers route
    through ``paths.inbox_root_for(project)`` which consults the path-config
    settings (configurable via ``config.paths.inbox_dir`` in settings.yaml).
    """
    override = os.environ.get("FNO_INBOX_ROOT")
    if override:
        return Path(override)
    return None


_PROJECT_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


def inbox_dir_for(project: str) -> Path:
    """Return the ``inbox/`` directory for a recipient project.

    Validates ``project`` strictly: it must match
    ``^[A-Za-z0-9][A-Za-z0-9._-]*$`` and contain no ``..`` segments. This
    closes a path-traversal hole where a maliciously crafted recipient
    name (``..`` / ``../etc``) would let writes escape the inbox tree.

    Path resolution order:
      1. ``FNO_INBOX_ROOT`` env var (test override) -> ``$ROOT/<project>/inbox``.
      2. ``paths.inbox_root_for(project)`` (production) -> consults
         ``config.paths.inbox_dir`` from settings.yaml with the recipient's
         project name substituted into the ``{project}`` template.
    """
    if "/" in project or "\\" in project:
        raise ValueError(f"project name must not contain path separators: {project!r}")
    if ".." in project:
        raise ValueError(f"project name must not contain '..': {project!r}")
    if not _PROJECT_NAME_RE.match(project):
        raise ValueError(
            f"project name must match {_PROJECT_NAME_RE.pattern}: {project!r}"
        )
    override_root = _inbox_root()
    if override_root is not None:
        return override_root / project / "inbox"
    # Production path: consult settings.yaml via path-config (ab-6fe0d039).
    from fno import paths as _paths
    return _paths.inbox_root_for(project)


# ---------------------------------------------------------------------------
# Lock helper - mkdir mutex pattern, POSIX-atomic on macOS
# ---------------------------------------------------------------------------

def _acquire_lock(path: Path, timeout: float = 30.0, poll: float = 0.05) -> Path:
    """Acquire a per-path mkdir lock. Returns the lock-dir path; caller releases."""
    lock_dir = path.with_suffix(path.suffix + ".lock.d")
    deadline = time.time() + timeout
    while True:
        try:
            lock_dir.parent.mkdir(parents=True, exist_ok=True)
            os.mkdir(str(lock_dir))
            return lock_dir
        except FileExistsError:
            if time.time() >= deadline:
                raise TimeoutError(f"could not acquire lock at {lock_dir} within {timeout}s")
            time.sleep(poll)


def _release_lock(lock_dir: Path) -> None:
    try:
        os.rmdir(str(lock_dir))
    except OSError as exc:
        log_inbox_error(
            "lock_release_failed",
            path=str(lock_dir),
            error=f"{type(exc).__name__}: {exc}",
        )


def _atomic_write_text(target: Path, content: str) -> None:
    """Write to a sibling temp file then ``os.replace`` so a partial write
    can never leave the target truncated. Caller must already hold the
    per-path lock returned by ``_acquire_lock``.
    """
    import tempfile

    target.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_str = tempfile.mkstemp(
        dir=str(target.parent),
        prefix=f".{target.name}.tmp.",
        suffix=".part",
    )
    tmp = Path(tmp_str)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
        os.replace(str(tmp), str(target))
    except Exception:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise


# ---------------------------------------------------------------------------
# Slug helper
# ---------------------------------------------------------------------------

_SLUG_NONWORD = re.compile(r"[^a-z0-9]+")


def _slug_for(body: str, fallback_msg_id: str, max_len: int = 40) -> str:
    """First 5 words of body, kebab-cased, capped at ``max_len`` chars.

    Falls back to ``msg-{short}`` when the body is empty or sluggified to
    nothing. The 5-word window picks up topical anchors without dragging
    in punctuation or markdown tokens.
    """
    first_line = body.strip().splitlines()[0] if body.strip() else ""
    words = first_line.split()[:5]
    raw = " ".join(words).lower()
    slug = _SLUG_NONWORD.sub("-", raw).strip("-")
    if not slug:
        return fallback_msg_id
    return slug[:max_len].rstrip("-") or fallback_msg_id


# ---------------------------------------------------------------------------
# ID generation
# ---------------------------------------------------------------------------

def generate_msg_id() -> str:
    """Generate a 'msg-XXXXXX' id with 6 hex characters."""
    return "msg-" + secrets.token_hex(3)


# ---------------------------------------------------------------------------
# Format helpers
# ---------------------------------------------------------------------------

_MSG_HEADER_RE = re.compile(
    r"^## (msg-[0-9a-zA-Z]+) · (\S+) · from:(\S+)\s*$"
)
_FRONTMATTER_RE = re.compile(r"^---\s*\n(.*?)\n---\s*\n", re.DOTALL)


def _format_dt(ts: datetime) -> str:
    """ISO-8601 UTC. ``2026-05-08T13:48:58Z`` shape."""
    if ts.tzinfo is None:
        ts = ts.replace(tzinfo=timezone.utc)
    return ts.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _parse_dt(s) -> Optional[datetime]:
    """Parse the formats we write. Returns None on failure.

    YAML can pre-parse ``2026-05-08T10:00:00Z``-shaped strings into
    ``datetime`` already; accept that shape transparently.
    """
    if s is None or s == "":
        return None
    if isinstance(s, datetime):
        if s.tzinfo is None:
            return s.replace(tzinfo=timezone.utc)
        return s
    s = str(s).strip().strip('"').strip("'")
    fmts = (
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d %H:%M:%S",
    )
    for fmt in fmts:
        try:
            dt = datetime.strptime(s, fmt)
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=timezone.utc)
            return dt
        except ValueError:
            continue
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00"))
    except ValueError:
        return None


def _format_thread(handle: ThreadHandle) -> str:
    """Render a ThreadHandle as the on-disk markdown content."""
    fm: dict[str, object] = {
        "thread_id": handle.thread_id,
        "from": handle.from_project,
        "to": handle.to_project,
        "kind": handle.kind,
        "created": _format_dt(handle.created),
    }
    if handle.read_at is not None:
        fm["read_at"] = _format_dt(handle.read_at)
    if handle.replies_to:
        fm["replies_to"] = handle.replies_to
    if handle.persist_to_memory:
        fm["persist_to_memory"] = True
    for k, v in handle.refs.items():
        fm[k] = v

    body_parts: list[str] = []
    for m in handle.messages:
        header = f"## {m.msg_id} · {_format_dt(m.timestamp)} · from:{m.from_project}"
        body_parts.append(header)
        body_parts.append("")
        body_parts.append(m.body.rstrip("\n"))
        body_parts.append("")

    fm_text = yaml.safe_dump(fm, sort_keys=False).strip()
    return f"---\n{fm_text}\n---\n\n" + "\n".join(body_parts).rstrip("\n") + "\n"


def _parse_thread_text(text: str, path: Path) -> Optional[ThreadHandle]:
    """Parse a thread file's text into a ThreadHandle. Returns None on bad shape."""
    m = _FRONTMATTER_RE.match(text)
    if not m:
        return None
    fm_text = m.group(1)
    try:
        fm = yaml.safe_load(fm_text) or {}
    except yaml.YAMLError:
        return None
    if not isinstance(fm, dict):
        return None

    thread_id = fm.get("thread_id")
    from_project = fm.get("from")
    to_project = fm.get("to")
    kind = fm.get("kind")
    created = _parse_dt(fm.get("created", ""))

    if not all([thread_id, from_project, to_project, kind, created]):
        return None
    assert created is not None  # the all(...) guard above ensures it parsed

    read_at_raw = fm.get("read_at")
    read_at = _parse_dt(read_at_raw) if read_at_raw else None
    replies_to = fm.get("replies_to")
    persist_to_memory = bool(fm.get("persist_to_memory", False))
    ref_keys = (
        "ref_pr",
        "ref_node",
        "ref_gate",
        "mission_id",
        "source_mission",
        "cascade_of",
    )
    refs = {k: str(fm[k]) for k in ref_keys if k in fm and fm[k] is not None}

    body_text = text[m.end():]
    messages = _parse_messages(body_text)

    return ThreadHandle(
        thread_id=str(thread_id),
        path=path,
        from_project=str(from_project),
        to_project=str(to_project),
        kind=str(kind),
        created=created,
        read_at=read_at,
        replies_to=str(replies_to) if replies_to else None,
        persist_to_memory=persist_to_memory,
        refs=refs,
        messages=messages,
    )


def _parse_messages(body_text: str) -> list[ThreadMessage]:
    """Parse ``## msg-{id} · {ts} · from:{sender}`` blocks out of body text."""
    lines = body_text.splitlines()
    messages: list[ThreadMessage] = []
    i = 0
    n = len(lines)
    while i < n:
        line = lines[i]
        m = _MSG_HEADER_RE.match(line)
        if not m:
            i += 1
            continue
        msg_id = m.group(1)
        ts = _parse_dt(m.group(2))
        sender = m.group(3)
        i += 1
        body_lines: list[str] = []
        while i < n and not _MSG_HEADER_RE.match(lines[i]):
            body_lines.append(lines[i])
            i += 1
        if ts is None:
            continue
        body = "\n".join(body_lines).strip("\n").strip()
        messages.append(
            ThreadMessage(
                msg_id=msg_id,
                timestamp=ts,
                from_project=sender,
                body=body,
            )
        )
    return messages


# ---------------------------------------------------------------------------
# Public API: read
# ---------------------------------------------------------------------------

def read_thread(path: Path) -> Optional[ThreadHandle]:
    """Parse a single thread file. Returns None if the file is malformed."""
    if not path.exists():
        return None
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    return _parse_thread_text(text, path)


def read_all_threads(recipient: str) -> list[ThreadHandle]:
    """Return every thread file in ``{recipient}/inbox/`` (oldest-first)."""
    inbox = inbox_dir_for(recipient)
    if not inbox.exists():
        return []
    out: list[ThreadHandle] = []
    for p in sorted(inbox.glob("*.md")):
        h = read_thread(p)
        if h is not None:
            out.append(h)
    return out


def read_unread_threads(recipient: str) -> list[ThreadHandle]:
    """Return threads where ``read_at`` is absent (oldest-first)."""
    return [h for h in read_all_threads(recipient) if h.is_unread]


def find_thread_by_msg_id(recipient: str, msg_id: str) -> Optional[ThreadHandle]:
    """Find the thread file containing ``msg_id`` (root or appended reply).

    Returns None when no thread under the recipient contains that msg-id.
    """
    if not msg_id:
        return None
    for h in read_all_threads(recipient):
        for m in h.messages:
            if m.msg_id == msg_id:
                return h
        if h.thread_id == msg_id:
            return h
    return None


# ---------------------------------------------------------------------------
# Durable write (jsonl-canon) + derived render (ab-cee91152, Move A)
# ---------------------------------------------------------------------------
# LD2: the global JSONL bus log is the durable-first system of record; the
# per-recipient markdown thread file is a derived, regenerable render. This is
# the FLIP of the pre-cutover order, where the markdown was written durable-first
# and the log was a best-effort mirror. Now:
#   - ``_append_to_bus`` is the durable write the caller depends on: a failure
#     RAISES and the send fails (never a silent loss).
#   - ``_write_render_best_effort`` writes the derived markdown: a failure is
#     logged loudly but never fatal, because the durable bus append already
#     landed and ``rebuild_render`` can regenerate the render from the log.

def _append_to_bus(
    *,
    msg_id: str,
    thread_id: str,
    sender: str,
    recipient: str,
    kind: str,
    body: str,
    timestamp: datetime,
    in_reply_to: Optional[str] = None,
    persist_to_memory: bool = False,
    refs: Optional[dict[str, str]] = None,
    render_path: Optional[Path] = None,
    provider_from: Optional[str] = None,
    provider_to: Optional[str] = None,
    from_session: Optional[str] = None,
    from_model: Optional[str] = None,
    to_kind: Optional[str] = None,
    owner: Optional[str] = None,
    ttl_at: Optional[datetime] = None,
) -> None:
    """Append a versioned envelope to the canonical bus log (the durable write).

    LD2 (ab-cee91152): the JSONL bus log is the durable-first system of record.
    A failure here FAILS the caller's send - the message is NOT durably stored,
    so reporting success would be a silent loss. This raises (it does NOT swallow);
    the markdown render is the best-effort, regenerable half.
    """
    from fno.bus.log import Envelope, append as _bus_append

    meta: dict[str, object] = {}
    if refs:
        meta["refs"] = dict(refs)
    if persist_to_memory:
        meta["persist_to_memory"] = True
    if render_path is not None:
        meta["render_path"] = str(render_path)
    # Terminal classification (US6): stamp who drains this durable thread and
    # when the dead-letter sweep should reclassify it if they have not. The bus
    # envelope is the system of record the sweep reads.
    if owner is not None:
        meta["owner"] = owner
    if ttl_at is not None:
        meta["ttl_at"] = _format_dt(ttl_at)
    _bus_append(
        Envelope(
            id=msg_id,
            thread=thread_id,
            from_=sender,
            to=recipient,
            kind=kind,
            body=body,
            ts=_format_dt(timestamp),
            in_reply_to=in_reply_to,
            meta=meta,
            provider_from=provider_from,
            provider_to=provider_to,
            from_session=from_session,
            from_model=from_model,
            to_kind=to_kind,
        )
    )


def _write_render_best_effort(target: Path, content: str) -> bool:
    """Write the derived markdown render under the per-path lock. Best-effort.

    LD2 (ab-cee91152): the render is derived from the canonical log, so a write
    failure here is logged loudly but never fatal - the durable bus append has
    already landed, and ``rebuild_render`` regenerates the render from the log.
    Returns True on success, False on a logged failure.
    """
    try:
        lock = _acquire_lock(target)
        try:
            _atomic_write_text(target, content)
        finally:
            _release_lock(lock)
        return True
    except Exception as exc:  # noqa: BLE001 - render is derived; never fail the send
        log_inbox_error(
            "render_write_failed",
            path=str(target),
            error=f"{type(exc).__name__}: {exc}",
        )
        print(
            f"warning: markdown render write failed for {target} "
            f"({type(exc).__name__}: {exc}); the message is durable on the bus log, "
            f"regenerate the render with `fno mail rebuild-render`",
            file=sys.stderr,
        )
        return False


# ---------------------------------------------------------------------------
# Public API: write
# ---------------------------------------------------------------------------

@dataclass
class PostResult:
    """Outcome of :func:`post_inbox_message`.

    ``appended`` is True when the body was threaded onto an existing
    thread (``reply_to`` matched); ``orphan`` is True when ``reply_to``
    was given but no matching thread existed, so a new thread was created
    carrying ``replies_to`` for a durable cross-thread link.
    """

    msg_id: str
    thread_path: Path
    appended: bool
    orphan: bool


def post_inbox_message(
    *,
    recipient: str,
    sender: str,
    kind: str,
    body: str,
    persist_to_memory: bool = False,
    reply_to: Optional[str] = None,
    refs: Optional[dict[str, str]] = None,
) -> PostResult:
    """Post a project-addressed inbox message (the durable write path).

    Pure data layer (no Typer): reproduces ``fno mail send`` semantics so
    ``fno mail send --to-project`` can carry the inbox kinds the recipient
    drain dispatches on (heads-up -> triage, question -> wake-signal, fyi /
    fyi+persist). Validation raises ``ValueError``; the caller maps it to a
    CLI exit. With ``reply_to`` set it appends to the matching thread, else
    creates an orphan-reply thread; otherwise it writes a fresh thread.
    """
    if kind not in VALID_KINDS:
        raise ValueError(
            f"invalid kind: {kind!r}; expected one of {sorted(VALID_KINDS)}"
        )
    if persist_to_memory and kind != Kind.FYI.value:
        raise ValueError("persist_to_memory is only valid with kind 'fyi'")

    if reply_to:
        existing = find_thread_by_msg_id(recipient, reply_to)
        if existing is not None:
            new_id = append_to_thread(existing.path, sender, body)
            return PostResult(
                msg_id=new_id, thread_path=existing.path, appended=True, orphan=False
            )
        # No thread under recipient carries reply_to: create one with
        # replies_to set so the cross-thread link survives (matches the
        # old inbox-send orphan-reply path).
        handle = write_new_thread(
            recipient, sender, kind, body,
            replies_to=reply_to,
            persist_to_memory=persist_to_memory,
            refs=refs,
            owner=DurableOwner.INBOX_DRAIN.value,
        )
        return PostResult(
            msg_id=handle.thread_id, thread_path=handle.path, appended=False, orphan=True
        )

    handle = write_new_thread(
        recipient, sender, kind, body,
        persist_to_memory=persist_to_memory,
        refs=refs,
        owner=DurableOwner.INBOX_DRAIN.value,
    )
    return PostResult(
        msg_id=handle.thread_id, thread_path=handle.path, appended=False, orphan=False
    )


def write_new_thread(
    recipient: str,
    sender: str,
    kind: str,
    body: str,
    *,
    msg_id: Optional[str] = None,
    timestamp: Optional[datetime] = None,
    replies_to: Optional[str] = None,
    persist_to_memory: bool = False,
    refs: Optional[dict[str, str]] = None,
    provider_from: Optional[str] = None,
    provider_to: Optional[str] = None,
    from_session: Optional[str] = None,
    from_model: Optional[str] = None,
    to_kind: Optional[str] = None,
    owner: Optional[str] = None,
    ttl_at: Optional[datetime] = None,
) -> ThreadHandle:
    """Create a new thread file. Returns the resulting handle.

    Filename: ``{YYYY-MM-DD}-{slug}.md`` (slug from first 5 words of body).
    On collision, appends ``-1``, ``-2``, ... until an unused name is found.

    ``owner``/``ttl_at`` stamp the durable thread's terminal classification (US6):
    who will drain it and when the dead-letter sweep should reclassify it if they
    have not. They ride the bus envelope ``meta`` (the system of record the sweep
    reads), so a caller that omits them writes a thread indistinguishable from
    before.
    """
    if kind not in VALID_KINDS:
        raise ValueError(f"invalid kind: {kind!r}; expected one of {sorted(VALID_KINDS)}")
    if msg_id is None:
        msg_id = generate_msg_id()
    if timestamp is None:
        timestamp = datetime.now(tz=timezone.utc)
    # Derive the sweep horizon from the owner class once the created timestamp is
    # known, so every caller passes only the classification. An explicit ttl_at
    # (e.g. a caller reclassifying) wins.
    if owner is not None and ttl_at is None:
        ttl_at = ttl_at_for(owner, timestamp)

    inbox = inbox_dir_for(recipient)
    inbox.mkdir(parents=True, exist_ok=True)

    date_part = timestamp.astimezone(timezone.utc).strftime("%Y-%m-%d")
    slug = _slug_for(body, fallback_msg_id=msg_id)
    base = f"{date_part}-{slug}"
    target: Optional[Path] = None
    suffix = ""
    n = 0
    # Atomically claim the filename via O_CREAT|O_EXCL. The previous
    # check-then-acquire was racy: two senders could observe the same
    # name unused, both proceed, and clobber each other's content under
    # the lock. Now whichever caller wins the create() owns the name and
    # the loser bumps the suffix.
    while True:
        candidate = inbox / f"{base}{suffix}.md"
        try:
            fd = os.open(str(candidate), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
            os.close(fd)
            target = candidate
            break
        except FileExistsError:
            n += 1
            suffix = f"-{n}"
            if n > 1000:
                raise RuntimeError(
                    f"could not allocate a thread filename under {inbox} "
                    f"(tried {n} suffixes)"
                )

    handle = ThreadHandle(
        thread_id=msg_id,
        path=target,
        from_project=sender,
        to_project=recipient,
        kind=kind,
        created=timestamp,
        read_at=None,
        replies_to=replies_to,
        persist_to_memory=persist_to_memory,
        refs=dict(refs or {}),
        messages=[
            ThreadMessage(
                msg_id=msg_id,
                timestamp=timestamp,
                from_project=sender,
                body=body,
            )
        ],
    )

    # Durable-first (LD2): the bus-log append is the write the caller depends on.
    # If it fails the send fails; remove the reserved (empty) render file so a
    # failed durable send leaves no orphan render masquerading as a real message.
    try:
        _append_to_bus(
            msg_id=msg_id,
            thread_id=msg_id,
            sender=sender,
            recipient=recipient,
            kind=kind,
            body=body,
            timestamp=timestamp,
            in_reply_to=replies_to,
            persist_to_memory=persist_to_memory,
            refs=refs,
            render_path=target,
            provider_from=provider_from,
            provider_to=provider_to,
            from_session=from_session,
            from_model=from_model,
            to_kind=to_kind,
            owner=owner,
            ttl_at=ttl_at,
        )
    except Exception:
        try:
            target.unlink()
        except OSError:
            pass
        raise

    # Best-effort: the derived markdown render. A failure is logged, not fatal.
    _write_render_best_effort(target, _format_thread(handle))

    return handle


def append_to_thread(
    thread_path: Path,
    sender: str,
    body: str,
    *,
    msg_id: Optional[str] = None,
    timestamp: Optional[datetime] = None,
) -> str:
    """Append a message block to an existing thread file. Returns new msg-id."""
    if not thread_path.exists():
        raise FileNotFoundError(f"thread file not found: {thread_path}")
    if msg_id is None:
        msg_id = generate_msg_id()
    if timestamp is None:
        timestamp = datetime.now(tz=timezone.utc)

    # Read the thread's metadata (thread_id, owner, kind) for the bus envelope.
    existing = read_thread(thread_path)
    if existing is None:
        raise ValueError(f"malformed thread file: {thread_path}")

    # Durable-first (LD2): append the reply envelope to the canonical bus log.
    # It stays addressed to the thread owner (existing.to_project) so the same
    # recipient's drain/cursor sees the reply, matching the model where appends
    # resurface the owner's thread. A bus failure raises (the reply is not
    # durably stored).
    # NOTE for a future move: when the triage drain is rewired to read the bus
    # cursor, a reply authored BY the thread owner back to the original sender
    # will still carry to==owner here; that addressing must be revisited so
    # owner-authored replies route to the original sender rather than the owner.
    _append_to_bus(
        msg_id=msg_id,
        thread_id=existing.thread_id,
        sender=sender,
        recipient=existing.to_project,
        kind=existing.kind,
        body=body,
        timestamp=timestamp,
        in_reply_to=existing.thread_id,
        render_path=thread_path,
    )

    # Best-effort: update the derived markdown render under the per-path lock.
    # A reply to an already-read thread resets read_at so the md view resurfaces
    # it for drain. A render failure is logged, not fatal - the durable bus
    # append already landed and rebuild_render can regenerate the render.
    lock = _acquire_lock(thread_path)
    try:
        current = read_thread(thread_path)
        if current is None:
            log_inbox_error("render_reread_failed", path=str(thread_path))
        else:
            current.messages.append(
                ThreadMessage(
                    msg_id=msg_id,
                    timestamp=timestamp,
                    from_project=sender,
                    body=body,
                )
            )
            current.read_at = None
            try:
                _atomic_write_text(thread_path, _format_thread(current))
            except Exception as exc:  # noqa: BLE001 - render is derived; never fatal
                log_inbox_error(
                    "render_write_failed",
                    path=str(thread_path),
                    error=f"{type(exc).__name__}: {exc}",
                )
                print(
                    f"warning: markdown render update failed for {thread_path} "
                    f"({type(exc).__name__}: {exc}); the reply is durable on the "
                    f"bus log, regenerate with `fno mail rebuild-render`",
                    file=sys.stderr,
                )
    finally:
        _release_lock(lock)

    return msg_id


def mark_thread_read(thread_path: Path, ts: Optional[datetime] = None) -> None:
    """Set ``read_at`` in the thread's frontmatter. Idempotent."""
    if ts is None:
        ts = datetime.now(tz=timezone.utc)
    lock = _acquire_lock(thread_path)
    try:
        h = read_thread(thread_path)
        if h is None:
            raise ValueError(f"malformed thread file: {thread_path}")
        h.read_at = ts
        _atomic_write_text(thread_path, _format_thread(h))
    finally:
        _release_lock(lock)


def _unique_render_path(inbox: Path, base: str) -> Path:
    """Pick an unused ``{base}.md`` (bumping ``-1``, ``-2``, ...) under ``inbox``.

    Used by ``rebuild_render`` after the render dir has been cleared, so the only
    collisions are two threads whose bodies slug to the same base name.
    """
    candidate = inbox / f"{base}.md"
    n = 0
    while candidate.exists():
        n += 1
        candidate = inbox / f"{base}-{n}.md"
    return candidate


def rebuild_render(recipient: str) -> int:
    """Regenerate ``recipient``'s markdown render from the canonical bus log.

    LD2 / AC1-EDGE (ab-cee91152): the JSONL bus log is the source of truth and
    the per-recipient markdown is a derived, throwaway view. This clears the
    recipient's existing render files and rewrites them from the log so a
    deleted or corrupted render is recovered with no message lost. Idempotent:
    after a rebuild the render exactly mirrors the log, so re-running it changes
    nothing. Returns the number of threads written.

    Note: the bus log carries no per-recipient read/ack state, so a rebuilt
    render shows every message as unread in the markdown view. The canonical
    consume position is the per-recipient bus cursor, which this never touches.
    """
    from fno.bus.log import Envelope, iter_messages

    # Collect this recipient's envelopes, grouped by thread, preserving log
    # (oldest-first) order within each thread.
    threads: dict[str, list[Envelope]] = {}
    order: list[str] = []
    for env in iter_messages():
        if env.to != recipient:
            continue
        tid = env.thread or env.id
        if tid not in threads:
            threads[tid] = []
            order.append(tid)
        threads[tid].append(env)

    inbox = inbox_dir_for(recipient)
    inbox.mkdir(parents=True, exist_ok=True)
    # The render is throwaway: clear it, then rewrite to exactly match the log.
    for p in inbox.glob("*.md"):
        try:
            p.unlink()
        except OSError:
            pass

    written = 0
    for tid in order:
        envs = threads[tid]
        # The thread root is the envelope whose id == thread id (or the first).
        root = next((e for e in envs if e.id == tid), envs[0])
        meta = root.meta or {}
        refs: dict[str, str] = {}
        if isinstance(meta.get("refs"), dict):
            refs = {k: str(v) for k, v in meta["refs"].items()}
        messages = [
            ThreadMessage(
                msg_id=e.id,
                timestamp=_parse_dt(e.ts) or datetime.now(tz=timezone.utc),
                from_project=e.from_,
                body=e.body,
            )
            for e in envs
        ]
        created = _parse_dt(root.ts) or datetime.now(tz=timezone.utc)
        date_part = created.astimezone(timezone.utc).strftime("%Y-%m-%d")
        slug = _slug_for(root.body, fallback_msg_id=tid)
        target = _unique_render_path(inbox, f"{date_part}-{slug}")
        handle = ThreadHandle(
            thread_id=tid,
            path=target,
            from_project=root.from_,
            to_project=recipient,
            kind=root.kind,
            created=created,
            read_at=None,
            replies_to=root.in_reply_to,
            persist_to_memory=bool(meta.get("persist_to_memory", False)),
            refs=refs,
            messages=messages,
        )
        lock = _acquire_lock(target)
        try:
            _atomic_write_text(target, _format_thread(handle))
        finally:
            _release_lock(lock)
        written += 1
    return written


# ---------------------------------------------------------------------------
# Project resolution
# ---------------------------------------------------------------------------

def _git_root() -> Path:
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0:
            return Path(result.stdout.strip())
    except FileNotFoundError:
        pass
    return Path.cwd()


def _project_id_from_settings(data: dict) -> Optional[str]:
    """Extract the project id from parsed settings.yaml data.

    ``config.project.id`` is canonical; the top-level ``project`` key is the
    deprecated alias and may be either a bare string (legacy) or a mapping
    ``{id, vision, goals, constraints}`` (current). Mirrors the precedence in
    ``fno.paths`` (``config.project.id or project.id``). Returns None
    when no usable id is present so the caller keeps walking up the tree.
    """
    cfg = data.get("config")
    if isinstance(cfg, dict):
        cp = cfg.get("project")
        if isinstance(cp, dict) and cp.get("id"):
            return str(cp["id"])

    proj = data.get("project")
    if isinstance(proj, dict):
        return str(proj["id"]) if proj.get("id") else None
    if isinstance(proj, str) and proj:
        return proj
    return None


def resolve_project(
    cwd: Optional[Path] = None,
    override: Optional[str] = None,
    flag_hint: str = "--from",
) -> str:
    """Resolve the local project name from ``.fno/config.toml``.

    ``flag_hint`` names the flag the caller actually exposes, so the failure
    message points at a real flag (inbox verbs have ``--from``; ``mail send``
    has ``--from-name``/``--from-self``)."""
    if override is not None:
        return override

    from fno.config import read_config_flat

    search = cwd if cwd is not None else Path.cwd()

    while True:
        fno_dir = search / ".fno"
        candidate = fno_dir / "config.toml"
        if not candidate.is_file():
            candidate = fno_dir / "settings.yaml"
        if candidate.is_file():
            # read_config_flat parses config.toml (or a legacy settings.yaml) into
            # the FLAT dict and degrades a malformed file to {}.
            data = read_config_flat(candidate)
            if isinstance(data, dict):
                pid = _project_id_from_settings(data)
                if pid:
                    return pid

        parent = search.parent
        if parent == search:
            break
        search = parent

    raise ProjectIdentificationError(
        f"set 'project' in .fno/config.toml or pass {flag_hint}"
    )


# ---------------------------------------------------------------------------
# Error log helper (for callers that need to record parse / dispatch errors)
# ---------------------------------------------------------------------------

def log_inbox_error(reason: str, **extra) -> None:
    """Append a JSON line to ``.fno/inbox-errors.jsonl``."""
    from fno.paths import project_log

    errors_path = project_log("inbox-errors.jsonl")
    errors_path.parent.mkdir(parents=True, exist_ok=True)
    entry = {
        "ts": datetime.now(timezone.utc).isoformat(),
        "reason": reason,
        **extra,
    }
    with errors_path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")


# ---------------------------------------------------------------------------
# Legacy md-thread migration into the bus (Group 3 Task 3.4)
# ---------------------------------------------------------------------------
# Markdown threads written before the bus log existed (or by a stale pre-G3
# `fno`) live only on disk, not in the canonical log. The cutover backfills them
# so a cursor scan / agent inbox never strands unread mail invisibly. Dedup by
# message-id makes the migration idempotent (re-running re-sees nothing new).

@dataclass
class MigrationResult:
    migrated: int
    threads_scanned: int
    recipients: list[str]
    failed: int = 0  # messages skipped because their bus append raised


def _inbox_base() -> Optional[Path]:
    """Base dir holding ``<recipient>/inbox/`` folders, for recipient enumeration.

    ``FNO_INBOX_ROOT`` (test override) wins; otherwise the vault-/state-derived
    ``paths.inbox_agents_root()``. A custom ``config.paths.inbox_dir`` template
    is not auto-enumerated (its ``{project}`` placeholder is ambiguous to walk);
    callers in that rare setup pass ``recipients`` explicitly.
    """
    override = _inbox_root()
    if override is not None:
        return override
    from fno import paths as _paths
    return _paths.inbox_agents_root()


def _enumerate_recipients() -> list[str]:
    base = _inbox_base()
    if base is None or not base.exists():
        return []
    try:
        return [
            d.name
            for d in sorted(base.iterdir())
            if d.is_dir() and (d / "inbox").is_dir()
        ]
    except OSError as exc:
        print(
            f"warning: cannot enumerate inbox recipients in {base}: {exc}",
            file=sys.stderr,
        )
        return []


def migrate_md_threads_to_bus(
    *, recipients: Optional[list[str]] = None
) -> MigrationResult:
    """Backfill markdown threads into the bus log; idempotent (dedup by msg-id).

    Every message in every recipient's md threads that is not already in the log
    is appended as an envelope (root messages thread under their own id; appended
    messages correlate via ``in_reply_to`` to the thread root). Returns counts so
    a caller (the ``fno mail migrate-bus`` verb) can report what moved.
    """
    from fno.bus.log import Envelope, append as _bus_append, iter_messages

    existing_ids = {e.id for e in iter_messages()}
    recips = recipients if recipients is not None else _enumerate_recipients()

    migrated = 0
    scanned = 0
    failed = 0
    for recip in recips:
        for h in read_all_threads(recip):
            scanned += 1
            for m in h.messages:
                if m.msg_id in existing_ids:
                    continue
                meta: dict[str, object] = {"render_path": str(h.path)}
                if h.refs:
                    meta["refs"] = dict(h.refs)
                if h.persist_to_memory:
                    meta["persist_to_memory"] = True
                # One unappendable message must not abort the whole migration
                # (mirrors the skip-corrupt-and-keep-going discipline of the
                # reader). Count it, warn, and continue.
                try:
                    _bus_append(
                        Envelope(
                            id=m.msg_id,
                            thread=h.thread_id,
                            from_=m.from_project,
                            to=h.to_project,
                            kind=h.kind,
                            body=m.body,
                            ts=_format_dt(m.timestamp),
                            in_reply_to=(
                                h.thread_id if m.msg_id != h.thread_id else None
                            ),
                            meta=meta,
                        )
                    )
                except Exception as exc:  # noqa: BLE001 - resilient batch migration
                    failed += 1
                    log_inbox_error(
                        "bus_migration_failed",
                        msg_id=m.msg_id,
                        error=f"{type(exc).__name__}: {exc}",
                    )
                    print(
                        f"warning: could not migrate {m.msg_id} to the bus "
                        f"({type(exc).__name__}: {exc}); skipping",
                        file=sys.stderr,
                    )
                    continue
                existing_ids.add(m.msg_id)
                migrated += 1
    return MigrationResult(
        migrated=migrated,
        threads_scanned=scanned,
        recipients=list(recips),
        failed=failed,
    )
