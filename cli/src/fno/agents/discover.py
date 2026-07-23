"""fno.agents.discover — discover live hand-started Claude Code sessions.

Group A / P1 of the live-session-comms epic (ab-098967b4). A transport-free
read over Claude Code's own per-session registry at
``~/.claude/sessions/<pid>.json`` (Locked Decision 3: no MCP /
register-channel dependency — the registry already exists on disk). Surfaces
live, un-adopted sessions in ``fno agents list`` so they are addressable by a
legible handle without a UUID. When that sidecar is absent or repurposed, it
falls back to the canonical transcript store ``~/.claude/projects`` (x-a1d5).

Host-local (Locked Decision 8): PID liveness is per-machine, so only this
host's sessions are discovered; the lane never claims to see another host's.

Robustness contract (US5 / AC1-ERR/EDGE/FR): a malformed, mid-write, or
``.sync-conflict-*`` file is skipped, never fatal. A vanished file (a session
that exits mid-scan) is treated as not-live. Discovery must add only a
readdir + ~N stat/parse of the strict-pattern live set, never a full scan of
a 7000+ entry directory.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Iterator, Optional

from fno import paths

# A real per-session registry file is named ``<pid>.json``. The strict guard
# is load-bearing: a 7000+ entry sessions dir holds ``.sync-conflict-*.json``
# (iCloud) and ``<uuid>-*.md`` transcripts that must never be parsed
# (AC1-EDGE). ``^\d+\.json$`` admits only the real pid files.
_PID_FILE_RE = re.compile(r"^\d+\.json$")

# The hex handle is the addressable id (== jobId == CC's ``name`` default,
# verified present on 2.1.169). The friendly alias is UX layered on top.
NAME_MAP_FILENAME = "session-names.json"


# Test/operator seam: point discovery at a different registry dir. The agents
# test suite sets this to an empty tmp dir so a default-on `agents list` never
# reads the developer's real ~/.claude/sessions.
SESSIONS_DIR_ENV = "FNO_CLAUDE_SESSIONS_DIR"

# Canonical session store (x-a1d5). The ``<pid>.json`` sidecar above is absent
# or repurposed on some hosts (observed live: a user syncs cleared/compacted
# ``.md`` exports into ``~/.claude/sessions``), so the sidecar scan finds zero.
# The canonical store is the transcript jsonl at
# ``~/.claude/projects/<cwd-enc>/<session-id>.jsonl``. Test/operator seam +
# store-location and age-threshold test seams mirror the sidecar seam above.
PROJECTS_DIR_ENV = "FNO_CLAUDE_PROJECTS_DIR"
RECENCY_SECONDS_ENV = "FNO_CLAUDE_SESSION_RECENCY_SECONDS"
_DEFAULT_RECENCY_SECONDS = 600.0


def default_sessions_dir() -> Path:
    """Claude Code's per-session registry directory on this host."""
    override = os.environ.get(SESSIONS_DIR_ENV)
    if override:
        return Path(override)
    return Path(os.path.expanduser("~")) / ".claude" / "sessions"


def default_projects_dir() -> Path:
    """Claude Code's canonical transcript store on this host (x-a1d5)."""
    override = os.environ.get(PROJECTS_DIR_ENV)
    if override:
        return Path(override)
    return Path(os.path.expanduser("~")) / ".claude" / "projects"


def _recency_seconds() -> float:
    """Transcript-mtime liveness window (env-overridable, positive only)."""
    raw = os.environ.get(RECENCY_SECONDS_ENV)
    if raw:
        try:
            v = float(raw)
        except ValueError:
            v = 0.0
        if v > 0:
            return v
    return _DEFAULT_RECENCY_SECONDS


def default_name_map_path() -> Path:
    """Persisted hex->legible alias overlay (``~/.fno/session-names.json``)."""
    return paths.state_dir() / NAME_MAP_FILENAME


# Codex's transcript store is a structural mirror of claude's projects store,
# one directory over: rollout jsonl under ``~/.codex/sessions/YYYY/MM/DD/`` whose
# first line is a ``session_meta`` record carrying the session id + cwd verbatim.
# Reading it makes a hand-started codex session ``fno mail``-able even when it
# never ran the SessionStart register hook ("whether fno-spawned or not").
CODEX_SESSIONS_DIR_ENV = "FNO_CODEX_SESSIONS_DIR"
_CODEX_DAEMON_DISCOVERY_TIMEOUT_SECONDS = 12.0


def default_codex_sessions_dir() -> Path:
    """Codex's rollout transcript store on this host (mirror of x-a1d5)."""
    override = os.environ.get(CODEX_SESSIONS_DIR_ENV)
    if override:
        return Path(override)
    return Path(os.path.expanduser("~")) / ".codex" / "sessions"


def _discover_from_codex_daemon() -> list[dict]:
    """Codex threads currently loaded in the app-server daemon.

    The Rust probe owns the Unix-WebSocket protocol. Any missing/stale binary,
    unavailable daemon, incompatible response, or timeout contributes no rows;
    recent rollout and registry discovery remain available.
    """
    from fno.agents import rust_runtime

    binary = rust_runtime.resolve_installed_binary()
    if binary is None:
        return []
    try:
        proc = subprocess.run(
            [str(binary), "codex-loaded-threads"],
            capture_output=True,
            text=True,
            timeout=_CODEX_DAEMON_DISCOVERY_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if proc.returncode != 0:
        return []
    try:
        result = json.loads(proc.stdout.strip())
    except (ValueError, AttributeError):
        return []
    if not isinstance(result, dict) or result.get("available") is not True:
        return []
    threads = result.get("threads")
    if not isinstance(threads, list):
        return []

    rows: list[dict] = []
    seen: set[str] = set()
    for thread in threads:
        if not isinstance(thread, dict):
            continue
        sid = thread.get("session_id")
        if not isinstance(sid, str) or not sid or sid in seen:
            continue
        cwd = thread.get("cwd")
        rows.append(
            {
                "session_id": sid,
                "short_id": sid[:8],
                "pid": 0,
                "cwd": cwd if isinstance(cwd, str) else "",
                "status": None,
                "agent": "codex",
            }
        )
        seen.add(sid)
    return rows


def _codex_meta(path: Path) -> Optional[tuple[str, str]]:
    """``(session_id, cwd)`` from a rollout's first ``session_meta`` line, or None.

    Codex 0.1x writes ``{"type":"session_meta","payload":{"id":...,"cwd":...}}``
    as line 1 (verified on a real rollout). A file that is unreadable, whose
    first line is not JSON, or is not a session_meta record is skipped (returns
    None), never raised — same posture as the claude readers.
    """
    try:
        with open(path, encoding="utf-8") as fh:
            first = fh.readline()
    except OSError:
        return None
    try:
        rec = json.loads(first)
    except (ValueError, UnicodeDecodeError):
        return None
    if not isinstance(rec, dict) or rec.get("type") != "session_meta":
        return None
    payload = rec.get("payload")
    if not isinstance(payload, dict):
        return None
    sid, cwd = payload.get("id"), payload.get("cwd")
    if not isinstance(sid, str) or not sid:
        return None
    return sid, str(cwd or "")


def codex_rollout_for_session(
    session_id: str, *, sessions_dir: Optional[Path] = None
) -> Optional[Path]:
    """Rollout jsonl for a codex ``session_id``, or None. Never raises.

    Fast path: codex embeds the session uuid in the rollout filename, so a
    filename substring match wins without opening a file. Fallback: a rollout
    named by a turn id still carries the session id in its first ``session_meta``
    line (reused via ``_codex_meta``); scan newest-first. ``sessions_dir`` is
    injectable so tests never touch the developer's real ``~/.codex``.
    """
    if not session_id:
        return None
    root = sessions_dir if sessions_dir is not None else default_codex_sessions_dir()
    try:
        paths = list(root.rglob("rollout-*.jsonl"))
    except OSError:
        return None
    for path in paths:
        if session_id in path.name:
            return path
    for path in sorted(paths, key=_safe_rollout_mtime, reverse=True):
        meta = _codex_meta(path)
        if meta is not None and meta[0] == session_id:
            return path
    return None


def _safe_rollout_mtime(path: Path) -> float:
    try:
        return path.stat().st_mtime
    except OSError:
        return 0.0


def _discover_from_codex(
    codex_sessions_dir: Path,
    *,
    recency_seconds: float,
    exclude_session_ids: Iterable[str] = (),
    now: Optional[float] = None,
) -> list[dict]:
    """Enumerate codex sessions from the rollout store (US2).

    Mtime is not an enumeration predicate: an old final assistant turn can still
    be ``watching`` or ``your-move``. Family 1 classifies every candidate after
    enumeration. Rows are shaped like the claude loops' so the shared
    dedup/alias pipeline consumes them unchanged; ``pid`` is 0 (no OS handle)
    and ``agent`` is ``codex``.

    ponytail: full rglob + mtime ordering. Runs at send-time (interactive
    resolution), not in the hot drain path, so O(rollouts) stats is acceptable;
    prune to recent date-dirs by mtime if a heavy codex user's send drags.
    """
    del recency_seconds, now  # retained for call compatibility; family 1 owns age
    exclude_sids = {s for s in (exclude_session_ids or ()) if s}
    rows: list[dict] = []
    seen: set[str] = set()
    dated: list[tuple[float, Path]] = []
    try:
        for path in codex_sessions_dir.rglob("rollout-*.jsonl"):
            try:
                mt = path.stat().st_mtime
            except OSError:
                continue  # vanished mid-scan: skip, never abort the whole scan
            dated.append((mt, path))
    except OSError:
        return rows
    for _mt, path in sorted(dated, key=lambda t: t[0], reverse=True):
        meta = _codex_meta(path)
        if meta is None:
            continue
        sid, cwd = meta
        if sid in seen or sid in exclude_sids:
            continue
        seen.add(sid)
        rows.append(
            {
                "session_id": sid,
                "short_id": sid[:8],
                "pid": 0,
                "cwd": cwd,
                "status": None,
                "agent": "codex",
                "transcript_path": str(path),
            }
        )
    return rows


OPENCODE_STORAGE_DIR_ENV = "FNO_OPENCODE_STORAGE_DIR"


def default_opencode_storage_dir() -> Path:
    """opencode's on-disk storage root on this host (mirror of the codex seam)."""
    override = os.environ.get(OPENCODE_STORAGE_DIR_ENV)
    if override:
        return Path(override)
    return Path(os.path.expanduser("~")) / ".local" / "share" / "opencode" / "storage"


def default_opencode_db_path(storage_dir: Optional[Path] = None) -> Path:
    """opencode's SQLite store, the sibling of the legacy storage tree.

    Current opencode (verified on 1.14.50) writes sessions, messages and parts
    here; the ``storage/`` JSON tree is the legacy layout and stops being
    written. Derived from the storage dir so one env override seams both.
    """
    return (storage_dir or default_opencode_storage_dir()).parent / "opencode.db"


def opencode_connect(db_path: Path):
    """A read-only connection to opencode's store, or None if unavailable.

    Read-only URI mode is load-bearing, not decoration: a live opencode holds
    this database open in WAL mode, and observing must not perturb the
    observed. Callers that issue more than one query should share a single
    connection so their reads come from one snapshot.
    """
    import sqlite3

    if not db_path.exists():
        return None
    try:
        return sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=1.0)
    except sqlite3.Error:
        return None


def opencode_query(db_path: Path, sql: str, params: tuple = ()) -> list[tuple]:
    """Run one read-only query, returning ``[]`` on any failure.

    A missing file, a lock, or schema drift on a future opencode all degrade to
    no rows rather than raising, matching the disk readers. Callers must not
    read "no rows" as "no database" — see the dispatch in discover.
    """
    import sqlite3

    con = opencode_connect(db_path)
    if con is None:
        return []
    try:
        return list(con.execute(sql, params))
    except sqlite3.Error:
        return []
    finally:
        con.close()


def _discover_from_opencode_db(
    db_path: Path,
    *,
    recency_seconds: float,
    exclude_session_ids: Iterable[str] = (),
    now: Optional[float] = None,
) -> list[dict]:
    """Enumerate opencode sessions from the SQLite store.

    ``session.time_updated`` remains useful to family 1, but cannot exclude a
    session before its content-aware verdict runs. Timestamps are milliseconds.
    """
    del recency_seconds, now  # retained for call compatibility; family 1 owns age
    exclude_sids = {s for s in (exclude_session_ids or ()) if s}
    rows: list[dict] = []
    seen: set[str] = set()
    for sid, directory, _updated in opencode_query(
        db_path,
        "SELECT id, directory, time_updated FROM session "
        "ORDER BY time_updated DESC, id DESC",
    ):
        if not isinstance(sid, str) or not sid or sid in seen or sid in exclude_sids:
            continue
        seen.add(sid)
        rows.append(
            {
                "session_id": sid,
                "short_id": sid[:8],
                "pid": 0,
                "cwd": directory if isinstance(directory, str) else "",
                "status": None,
                "agent": "opencode",
            }
        )
    return rows


def _opencode_session_info(path: Path) -> Optional[tuple[str, str]]:
    """``(session_id, cwd)`` from a session-info JSON, or None.

    cwd is the info file's ``directory`` key (NOT ``cwd`` — verified against a
    real session). Unreadable/malformed/non-dict files return None, never raise.
    """
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return None
    if not isinstance(data, dict):
        return None
    sid = data.get("id")
    if not isinstance(sid, str) or not sid:
        return None
    return sid, str(data.get("directory") or "")


# How far past the recency window a session still gets the deeper liveness look
# below. A single turn can stream for many minutes but not for hours, so this
# bounds the extra scan to plausibly-active sessions instead of every session
# in a store that accumulates thousands.
_OPENCODE_LONG_TURN_SLACK_SECONDS = 6 * 3600


def _opencode_activity_mtime(
    info_path: Path, msg_root: Path, part_root: Path, deep_cutoff: float
) -> Optional[float]:
    """Newest activity timestamp for one session, or None if unreadable.

    The cheap signals (session info + message dir) both stop moving once a turn
    is underway: a directory's mtime tracks entries being created, not an
    existing file being rewritten, and a streaming turn writes into
    ``part/<msg_id>/`` whose parent is not the message dir. So a session in a
    long tool turn would age out of discovery and become unaddressable while
    still alive. For a session recent enough that a turn could still be running,
    look at the newest message and its parts; everything older skips the scan.
    """
    try:
        mt = info_path.stat().st_mtime
    except OSError:
        return None
    mdir = msg_root / info_path.stem
    try:
        mt = max(mt, mdir.stat().st_mtime)
    except OSError:
        return mt  # no messages yet: the info mtime is all there is
    if mt < deep_cutoff:
        return mt
    newest_mt, newest_name = 0.0, ""
    try:
        for entry in os.scandir(mdir):
            try:
                emt = entry.stat().st_mtime
            except OSError:
                continue  # vanished mid-scan
            if emt > newest_mt:
                newest_mt, newest_name = emt, entry.name
    except OSError:
        return mt
    if not newest_name:
        return mt
    mt = max(mt, newest_mt)
    try:
        mt = max(mt, (part_root / Path(newest_name).stem).stat().st_mtime)
    except OSError:
        pass  # parts not written yet
    return mt


def _discover_from_opencode(
    storage_dir: Path,
    *,
    recency_seconds: float,
    exclude_session_ids: Iterable[str] = (),
    now: Optional[float] = None,
) -> list[dict]:
    """Enumerate opencode sessions from the storage tree.

    opencode splits a session across three sibling trees rather than one
    transcript file: ``session/<projectID>/<ses_id>.json`` (info, cwd under
    ``directory``), ``message/<ses_id>/<msg_id>.json`` (one file per turn), and
    ``part/<msg_id>/`` (that turn's text, which only peek needs). Verified
    against a live 1.0.223 install; the nesting and the ``directory`` key are
    both easy to guess wrong.

    ``_opencode_activity_mtime`` supplies stable ordering and the deep-scan
    optimization. It never excludes a candidate; family 1 owns the verdict.

    Rows are shaped like the codex lane's so the shared dedup/alias pipeline
    consumes them unchanged.

    ponytail: full glob + two stats per session, on the same interactive
    resolution path as the codex scan; the deeper per-message scan is bounded to
    sessions recent enough to still be mid-turn. Prune by project dir if a heavy
    opencode user's send drags.
    """
    reference = now if now is not None else time.time()
    deep_cutoff = reference - recency_seconds - _OPENCODE_LONG_TURN_SLACK_SECONDS
    exclude_sids = {s for s in (exclude_session_ids or ()) if s}
    msg_root = storage_dir / "message"
    part_root = storage_dir / "part"
    rows: list[dict] = []
    seen: set[str] = set()
    dated: list[tuple[float, Path]] = []
    try:
        for path in (storage_dir / "session").glob("*/*.json"):
            mt = _opencode_activity_mtime(path, msg_root, part_root, deep_cutoff)
            if mt is not None:
                dated.append((mt, path))
    except OSError:
        return rows
    for _mt, path in sorted(dated, key=lambda t: t[0], reverse=True):
        info = _opencode_session_info(path)
        if info is None:
            continue
        sid, cwd = info
        if sid in seen or sid in exclude_sids:
            continue
        seen.add(sid)
        rows.append(
            {
                "session_id": sid,
                "short_id": sid[:8],
                "pid": 0,
                "cwd": cwd,
                "status": None,
                "agent": "opencode",
            }
        )
    return rows


def _discover_from_roster(*, exclude_session_ids: Iterable[str] = ()) -> list[dict]:
    """Live claude sessions from the daemon roster (US1, x-605c).

    A ``claude --bg`` worker leaves no pid-sidecar and is dropped from the live
    process scan, so the roster is the ONLY source that surfaces it -- the exact
    handle that failed to resolve at send time. Lenient by construction (the
    reader returns ``[]`` on any roster read/parse failure)."""
    from fno.agents.providers._claude_session_registry import roster_sessions

    exclude = {s for s in (exclude_session_ids or ()) if s}
    return [r for r in roster_sessions() if r["session_id"] not in exclude]


def _discover_from_registry(
    registry_path: Optional[Path] = None,
    *,
    exclude_session_ids: Iterable[str] = (),
) -> list[dict]:
    """Registered fno-agent sessions, resolvable by canonical handle (US2, x-605c).

    A spawned worker registered under a name (e.g. ``x-d899-us8-build``) also
    answers to its bare ``<short8>`` handle, because its harness session id
    is surfaced as a discover row. The harness -> id mapping is
    ``HARNESS_SESSION_ID_FIELDS`` (the single source of truth also read by the
    resume path), so a new harness needs a field there, not a resolver edit. For
    claude the row carries the FULL session uuid when known so it dedups against
    the roster/disk rows for the same session; ``short_id`` stays the 8-hex jobId
    the inject verb addresses."""
    from fno.agents.registry import HARNESS_SESSION_ID_FIELDS, load_registry

    exclude = {s for s in (exclude_session_ids or ()) if s}
    rows: list[dict] = []
    seen: set[str] = set()
    try:
        entries = load_registry(registry_path)
    except Exception:  # noqa: BLE001 — a torn/version-drifted registry contributes no rows
        return rows
    for e in entries:
        # Identity is one axis (x-8dfc): gate LIVE discovery on the row's
        # harness (provider fallback). A known-harness row keeps resolving; an
        # alien harness stays excluded here (no live transport exists for it)
        # while remaining durably mail-routable -- the live/durable split the
        # relay peer table depends on, preserved, not widened.
        harness = getattr(e, "harness", None)
        if harness not in HARNESS_SESSION_ID_FIELDS:
            continue
        # Registry status is enumeration metadata, not a liveness verdict.
        # Family-1 transcript truth below decides whether callers may route.
        if harness == "claude":
            # session_id keeps the full uuid for dedup/canonical identity, but
            # short_id MUST be the authoritative jobId -- the stored short and
            # the uuid's first 8 hex can differ, and the jobId is what
            # `fno mail send <short>` and mail-inject key on.
            # Canonical harness_session_id leads (x-ec59): a row whose only
            # identity is the canonical field (a heal-backfilled bg row) resolves
            # here, where before it fell through to durable-only forever.
            short_val = getattr(e, "short_id", "") or None
            sid = getattr(e, "harness_session_id", None) or short_val
            short = short_val or (sid[:8] if sid else None)
        else:
            sid = getattr(e, "harness_session_id", None) or getattr(e, "session_id", None)
            short = sid[:8] if sid else None
        if not sid or sid in exclude or sid in seen:
            continue
        seen.add(sid)
        rows.append(
            {
                "session_id": sid,
                "short_id": short,
                "pid": 0,
                "cwd": getattr(e, "cwd", "") or "",
                "status": None,
                "agent": harness,
            }
        )
    return rows


@dataclass
class DiscoveredSession:
    """One host-local session candidate with its family-1 truth verdict."""

    session_id: str
    short_id: str  # hex handle (jobId), the addressable id
    handle: str  # friendly alias, or short_id when no alias is mapped
    pid: int
    cwd: str
    project: Optional[str]
    status: Optional[str]  # registry status: idle/busy/waiting
    agent: str = "claude"
    truth_state: str = "unknown"
    transcript_path: Optional[str] = None

    @property
    def is_alive(self) -> bool:
        return self.truth_state in {"working", "watching", "your-move"}

    def to_row(self) -> dict:
        """Canonical dict shape for the JSON/table renderers."""
        return {
            "handle": self.handle,
            "short_id": self.short_id,
            "session_id": self.session_id,
            "pid": self.pid,
            "cwd": self.cwd,
            "project": self.project,
            "status": (
                "live"
                if self.is_alive
                else "orphaned"
                if self.truth_state in {"done", "stalled"}
                else "unknown"
            ),
            "agent": self.agent,
        }


# --------------------------------------------------------------------------
# Registry file iteration + liveness
# --------------------------------------------------------------------------


def _iter_pid_files(sessions_dir: Path) -> Iterator[Path]:
    """Yield only strict ``<pid>.json`` files, skipping sync-conflicts.

    An absent/empty directory yields nothing (AC1-EDGE boundary). The
    explicit ``.sync-conflict-`` skip is belt-and-suspenders: those names
    fail ``^\\d+\\.json$`` anyway, but the design names the skip so the
    intent is unmistakable.
    """
    try:
        names = os.listdir(sessions_dir)
    except OSError:
        return
    for name in names:
        if name.startswith(".sync-conflict-"):
            continue
        if not _PID_FILE_RE.match(name):
            continue
        yield sessions_dir / name


def _read_registry_file(path: Path) -> Optional[dict]:
    """Parse one registry file; return None on any read/parse failure.

    A mid-write or truncated file (concurrency: registry changing under the
    scan) yields None and is skipped, never raised (AC1-ERR / Concurrency).
    """
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        data = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return None
    return data if isinstance(data, dict) else None


# --------------------------------------------------------------------------
# Canonical transcript-store discovery (x-a1d5)
# --------------------------------------------------------------------------


# CC encodes a session's cwd into its projects subdir name by replacing
# separators with ``-`` (verified round-tripping real dirs: ``/`` and ``.`` both
# map to ``-``, e.g. ``/Users/x/.claude/p`` -> ``-Users-x--claude-p``). The
# mapping is lossy, so we never decode it; we encode a known cwd to FIND the dir.
# Whether a given CC version preserves ``_`` is version-specific, so we try both
# the underscore-collapsing and underscore-preserving forms and use whichever
# directory actually exists (the common no-underscore path yields one name).
def _encode_cwd(cwd: str) -> str:
    return re.sub(r"[^a-zA-Z0-9]", "-", cwd)


def _candidate_dir_names(cwd: str) -> list[str]:
    names: list[str] = []
    for pat in (r"[^a-zA-Z0-9]", r"[^a-zA-Z0-9_]"):
        name = re.sub(pat, "-", cwd)
        if name not in names:
            names.append(name)
    return names


def _live_claude_procs(psutil_mod) -> list[tuple[int, str]]:
    """``(pid, cwd)`` for each running Claude Code CLI process on this host.

    Selects the ``claude`` launcher and the versioned binary
    (``.../claude/versions/<v>``) and drops the daemon infra that shares that
    binary (``--bg-pty-host`` / ``--bg-spare`` etc.). This bounds the projects
    scan below to live sessions' dirs only — never the full 454-dir / 13k-file
    store (the plan's no-full-scan contract). Best-effort: any psutil failure
    yields fewer rows, never raises.
    """
    out: list[tuple[int, str]] = []
    try:
        procs = list(psutil_mod.process_iter(["pid", "cmdline"]))
    except Exception:  # noqa: BLE001 — psutil unavailable/erroring -> no rows
        return out
    for p in procs:
        try:
            cmd = (p.info.get("cmdline") if hasattr(p, "info") else None) or []
        except Exception:  # noqa: BLE001
            continue
        if not cmd:
            continue
        arg0 = str(cmd[0])
        is_claude = os.path.basename(arg0) == "claude" or "/claude/versions/" in arg0
        if not is_claude:
            continue
        if any(isinstance(a, str) and a.startswith("--bg-") for a in cmd):
            continue  # pty-host / spare daemon, not a session
        try:
            pid = int(p.info["pid"])
            cwd = psutil_mod.Process(pid).cwd()
        except Exception:  # noqa: BLE001 — vanished / not inspectable
            continue
        if cwd:
            out.append((pid, cwd))
    return out


def _newest_transcript(pdir: Path) -> Optional[str]:
    """Return the newest transcript identity in ``pdir``.

    Only the dir's top-level ``*.jsonl`` are transcripts (UUID subdirs are
    ``tool-results``). A ``.sync-conflict-`` copy is skipped — the marker is an
    infix (``<sid>.sync-conflict-<ts>.jsonl``), so a substring test. ``None`` if
    the dir is absent or holds no transcript.
    """
    best_sid: Optional[str] = None
    best_mt = float("-inf")
    try:
        entries = list(os.scandir(pdir))
    except OSError:
        return None
    for e in entries:
        name = e.name
        if ".sync-conflict-" in name or not name.endswith(".jsonl"):
            continue
        try:
            if not e.is_file() or e.stat().st_mtime < best_mt:
                continue
        except OSError:
            continue
        best_mt = e.stat().st_mtime
        best_sid = name[: -len(".jsonl")]
    return best_sid or None


def _discover_from_projects(
    projects_dir: Path,
    *,
    psutil_mod,
    exclude_session_ids: Iterable[str] = (),
) -> list[dict]:
    """Fallback discovery from the canonical transcript store (x-a1d5).

    The ``<pid>.json`` sidecar is gone, so liveness comes from a running
    ``claude`` process: each live process' cwd maps to a projects subdir, and the
    newest ``*.jsonl`` there identifies its candidate session. cwd comes from
    the process; pid is real. Family 1 classifies that transcript later; this
    enumerator never interprets transcript age as a liveness verdict.

    ``exclude_session_ids`` drops sessions already adopted into the fno registry
    (matched on full session_id, since a transcript row's short_id is the uuid
    prefix, not the registry's hex handle) so the lane stays "live but unadopted".

    ponytail: one row per live cwd — two sessions sharing a cwd collapse to the
    newest transcript (rare; the sidecar lane handled per-pid).
    """
    exclude_sids = {s for s in (exclude_session_ids or ()) if s}
    rows: list[dict] = []
    seen_cwd: set[str] = set()
    for pid, cwd in _live_claude_procs(psutil_mod):
        if cwd in seen_cwd:
            continue
        seen_cwd.add(cwd)
        sid = None
        for name in _candidate_dir_names(cwd):
            sid = _newest_transcript(projects_dir / name)
            if sid:
                break
        if not sid or sid in exclude_sids:
            continue
        rows.append(
            {
                "session_id": sid,
                "short_id": sid[:8],
                "pid": pid,
                "cwd": cwd,
                "status": None,
                "agent": "claude",
            }
        )
    return rows


# --------------------------------------------------------------------------
# Project resolution (cwd -> settings project, worktree-aware)
# --------------------------------------------------------------------------


def _iter_settings_projects() -> Iterator[tuple[str, str]]:
    """Yield ``(project_name, abs_path)`` from the settings work-map.

    Reuses the same candidate-file walk as
    ``graph._intake.detect_project_from_settings`` so the two cannot point at
    different settings files. Silent on any read/parse failure.
    """
    try:
        import yaml  # noqa: F401  availability probe: return early if PyYAML absent

        from fno.graph._intake import _settings_candidate_paths
    except ImportError:
        return
    from fno.config import read_config_flat

    for path in _settings_candidate_paths():
        if not path.exists():
            continue
        # config.toml (or legacy settings.yaml) -> flat dict; work is top-level.
        work = read_config_flat(path).get("work")
        if not isinstance(work, dict):
            continue
        workspaces = work.get("workspaces")
        if isinstance(workspaces, dict):
            for ws in workspaces.values():
                if not isinstance(ws, dict):
                    continue
                for p in ws.get("projects") or []:
                    if not isinstance(p, dict):
                        continue
                    name, raw_path = p.get("name"), p.get("path")
                    if name and raw_path:
                        yield str(name), os.path.normpath(os.path.expanduser(str(raw_path)))
        flat = work.get("projects")
        if isinstance(flat, dict):
            for name, cfg in flat.items():
                if isinstance(cfg, dict) and cfg.get("path"):
                    yield str(name), os.path.normpath(os.path.expanduser(str(cfg["path"])))


def _project_by_repo_basename(repo: str) -> Optional[str]:
    """Map a conductor ``<repo>`` segment to its configured project name."""
    for name, abs_path in _iter_settings_projects():
        if os.path.basename(abs_path) == repo:
            return name
    return None


def resolve_project_for_cwd(cwd: str) -> Optional[str]:
    """Resolve a session cwd to a settings project, worktree-aware (AC1-EDGE2).

    Handles the two worktree layouts the design names so a worktree session is
    attributed to its parent repo, not surfaced as an orphan:

    - ``<root>/.claude/worktrees/<name>`` -> resolve ``<root>``.
    - ``~/conductor/workspaces/<repo>/<name>`` -> map ``<repo>`` basename.

    Falls back to a direct settings match on the cwd itself.
    """
    if not cwd:
        return None
    from fno.graph._intake import detect_project_from_settings

    p = os.path.normpath(os.path.expanduser(cwd))

    marker = os.sep + ".claude" + os.sep + "worktrees" + os.sep
    if marker in p:
        root = p.split(marker)[0]
        proj = detect_project_from_settings(root)
        if proj:
            return proj

    parts = p.split(os.sep)
    if "workspaces" in parts:
        i = parts.index("workspaces")
        if i + 1 < len(parts):
            proj = _project_by_repo_basename(parts[i + 1])
            if proj:
                return proj

    return detect_project_from_settings(p)


# --------------------------------------------------------------------------
# Friendly-name overlay (~/.fno/session-names.json)
# --------------------------------------------------------------------------


def _default_alias(project: Optional[str], short_id: str) -> str:
    """Default legible alias: ``<project-basename>-<short-id>``.

    ``short_id`` is the unique hex handle, so the default alias is unique by
    construction; the disambiguation pass only fires on hand-edited collisions.
    """
    base = os.path.basename(project) if project else "session"
    alias = f"{base}-{short_id}"
    from fno.harness_identity import LEGACY_HANDLE_RE

    return f"project-{alias}" if LEGACY_HANDLE_RE.fullmatch(alias) else alias


def _load_name_map(path: Path) -> dict:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


def _resolve_aliases(
    live: list[dict], name_map_path: Path
) -> dict[str, str]:
    """Assign + persist a stable, unique alias per live session_id.

    Holds an exclusive flock around load -> retire-dead -> assign -> write so
    two concurrent ``agents list`` calls serialize on the map and never
    interleave a half-written file (Concurrency / Invariant). Retires entries
    whose session_id is no longer live so an exited/restarted session never
    resurfaces under a stale alias (AC1-EDGE2). Best-effort: a write failure
    falls back to the in-memory aliases rather than crashing the list.
    """
    import fcntl
    from fno.harness_identity import LEGACY_HANDLE_RE

    # No live sessions: nothing to render and nothing to retire against. Do NOT
    # rewrite the map here — a transient empty scan (e.g. a simultaneous psutil
    # probe miss) would otherwise wipe hand-edited aliases. Dead entries are
    # pruned on the next scan that sees >=1 live session, and discovery only
    # ever surfaces live sessions, so a lingering stale alias is never shown.
    if not live:
        return {}

    live_sids = {r["session_id"] for r in live}
    name_map_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = name_map_path.with_suffix(name_map_path.suffix + ".lock")

    aliases: dict[str, str] = {}
    try:
        with open(lock_path, "w") as lock_fh:
            fcntl.flock(lock_fh.fileno(), fcntl.LOCK_EX)
            try:
                stored = _load_name_map(name_map_path)
                # Retire any alias whose session is no longer live.
                pruned = {sid: nm for sid, nm in stored.items() if sid in live_sids}
                for r in live:
                    sid = r["session_id"]
                    if (
                        sid in pruned
                        and isinstance(pruned[sid], str)
                        and pruned[sid]
                        and not LEGACY_HANDLE_RE.fullmatch(pruned[sid])
                    ):
                        aliases[sid] = pruned[sid]
                    else:
                        aliases[sid] = _default_alias(r.get("project"), r["short_id"])
                aliases = _disambiguate(aliases, live)
                if aliases != stored:
                    _atomic_write_json(name_map_path, aliases)
            finally:
                fcntl.flock(lock_fh.fileno(), fcntl.LOCK_UN)
    except OSError:
        # Lock / write failed — fall back to fresh in-memory aliases so the
        # hex handle still addresses every session (overlay is UX, not a
        # correctness requirement).
        for r in live:
            aliases.setdefault(
                r["session_id"], _default_alias(r.get("project"), r["short_id"])
            )
        aliases = _disambiguate(aliases, live)
    return aliases


def _disambiguate(aliases: dict[str, str], live: list[dict]) -> dict[str, str]:
    """Guarantee aliases are unique within a render (Invariant).

    Default aliases embed the unique hex, so this only fires when a hand-edited
    map maps two sessions to the same name; the loser gets its short-id
    appended deterministically (sorted by session_id, never silently dropped).
    """
    seen: dict[str, str] = {}
    short_by_sid = {r["session_id"]: r["short_id"] for r in live}
    out: dict[str, str] = {}
    for sid in sorted(aliases):
        name = aliases[sid]
        if name in seen.values():
            name = f"{name}-{short_by_sid.get(sid, sid[:8])}"
        out[sid] = name
        seen[sid] = name
    return out


def _atomic_write_json(target: Path, data: dict) -> None:
    """temp-file write + ``os.replace`` (atomic; caller holds the flock)."""
    import tempfile

    target.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_str = tempfile.mkstemp(
        dir=str(target.parent), prefix=f".{target.name}.tmp.", suffix=".part"
    )
    tmp = Path(tmp_str)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2, sort_keys=True)
        os.replace(str(tmp), str(target))
    except Exception:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise


# --------------------------------------------------------------------------
# Public entry point
# --------------------------------------------------------------------------


def _import_psutil():
    import psutil

    return psutil


def resolve_or_suggest(
    handle: str,
    *,
    limit: int = 3,
    sessions_dir: Optional[Path] = None,
    projects_dir: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
    opencode_storage_dir: Optional[Path] = None,
    name_map_path: Optional[Path] = None,
    registry_path: Optional[Path] = None,
    project_resolver: Optional[Callable[[str], Optional[str]]] = None,
    psutil_mod=None,
    truth_fn: Optional[Callable[[DiscoveredSession], dict]] = None,
    require_alive: bool = True,
) -> tuple[Optional[DiscoveredSession], list[str]]:
    """Resolve a send handle to a live session, or suggest the closest ones (US2).

    A handle is a friendly alias (``<project>-<short>``) or the bare hex
    short-id. Returns ``(session, [])`` on an exact match, else
    ``(None, [closest handles])`` for the AC2-ERR error message. One discovery
    scan serves both the match and the suggestions. No exclusion: the user
    named a specific live session, so even an adopted one resolves.
    """
    from fno.harness_identity import LEGACY_HANDLE_RE, canonical_handle

    sessions = discover_live_sessions(
        sessions_dir=sessions_dir,
        projects_dir=projects_dir,
        codex_sessions_dir=codex_sessions_dir,
        opencode_storage_dir=opencode_storage_dir,
        name_map_path=name_map_path,
        registry_path=registry_path,
        project_resolver=project_resolver,
        psutil_mod=psutil_mod,
        truth_fn=truth_fn,
        classify_truth=require_alive,
    )
    if require_alive:
        sessions = [s for s in sessions if s.is_alive]
    retired = bool(handle and LEGACY_HANDLE_RE.fullmatch(handle))
    # An address is the friendly <project>-<short8> alias, the bare hex short-id,
    # or a stored row name. The retired <harness>-<short8> form is NOT accepted:
    # nothing generates it any more, so a caller still passing one is a bug, and
    # translating it silently would hide the bug forever.
    if not retired:
        for s in sessions:
            if handle and (
                s.handle == handle
                or s.short_id == handle
                or canonical_handle(s.session_id) == handle
            ):
                return s, []
    import difflib

    candidates: list[str] = []
    for s in sessions:
        for cand in (s.handle, s.short_id, canonical_handle(s.session_id)):
            if cand not in candidates:
                candidates.append(cand)
    # Name the bug rather than emitting a bare "not found": a harness-prefixed
    # address means some caller (a stale binary, a hardcoded string, a note
    # copied out of an old transcript) is still building addresses the retired
    # way. Lead the suggestions with the bare form it should have used.
    if retired:
        bare = handle.split("-", 1)[1][:8]
        return None, [bare] + [c for c in candidates if c != bare][: max(limit - 1, 0)]
    return None, difflib.get_close_matches(handle or "", candidates, n=limit, cutoff=0.3)


@dataclass(frozen=True)
class ReachableSession:
    """A session some durable store knows about, whether or not it is live.

    Distinct from :class:`DiscoveredSession` on purpose: that one answers "what
    should I list?" and is liveness-gated, while this one answers "is this token
    reachable at all?" and is deliberately liveness-blind. Conflating the two is
    the root cause this type exists to prevent -- an asleep session is absent
    from every listing yet fully resumable.
    """

    session_id: str
    source: str  # transcript | registry | roster | graph
    agent: str = "claude"
    # Claude resume is cwd-scoped, so a wake launched from the sender's
    # directory would fail to revive a recipient that lives in another repo.
    # None means no store recorded one and the caller must fall back.
    cwd: Optional[str] = None


class StoreReadError(Exception):
    """A reachability store could not be read, so absence cannot be proven.

    The distinction this exists to preserve: "read the store, the token is not
    there" and "could not read the store" look identical as an empty list, but
    they must not be treated identically. The first earns exit 16 (nothing is
    queued, because nothing would ever drain it); the second must NOT, because
    demoting to the durable queue costs one stranded envelope while a wrong
    exit 16 costs the message permanently. When we cannot prove unreachable, we
    fall toward keeping the mail.
    """

    def __init__(self, failed: list[str], resolved=None) -> None:
        super().__init__(f"unreadable reachability stores: {', '.join(failed)}")
        self.failed = failed
        # The lone candidate, when one was found but uniqueness could not be
        # proven. Carrying it lets the caller address the durable copy to a real
        # session rather than to the raw token it was handed.
        self.resolved = resolved


# Each helper returns ``(hits, read_ok)``. ``read_ok=False`` means the store
# could not be consulted at all -- never that it was consulted and came back
# empty. Collapsing those two into one empty list is what loses mail.
# (session_id, agent, cwd, cwd_is_verbatim). The last flag matters: a cwd
# decoded from a transcript directory name is a lossy GUESS, while a registry
# or roster row records the path verbatim. A verbatim cwd must be able to
# correct a decoded one even though the decoding source ranks higher overall.
_Hits = list[tuple[str, str, Optional[str], bool]]


def _decode_project_dir(name: str) -> Optional[str]:
    """Best-effort cwd for a transcript directory name (``-Users-x-proj``).

    The encoding replaces every non-alphanumeric character with ``-``, so it is
    lossy and cannot be inverted exactly: ``-repo-foo-bar`` is produced by both
    ``/repo/foo-bar`` and ``/repo/foo/bar``. Validating with ``is_dir()`` rules
    out nonsense but CANNOT disambiguate two real paths, so this stays a guess
    and is flagged non-verbatim by its caller. Any source that records the cwd
    literally overrides it.
    """
    if not name.startswith("-"):
        return None
    candidate = "/" + name[1:].replace("-", "/")
    try:
        if Path(candidate).is_dir():
            return candidate
    except OSError:
        return None
    return None


def _alias_to_session_ids(
    token: str, name_map_path: Optional[Path]
) -> tuple[list[str], bool]:
    """Session ids whose persisted friendly alias equals ``token``.

    A user who addressed ``<project>-<short8>`` while a session was live should
    not lose that address the moment it falls out of the live listing. The map
    is keyed session_id -> alias, so resolving an alias means inverting it.

    Partial by nature: the alias map is pruned of non-live sessions on any scan
    that sees at least one live session, so a long-asleep session may have no
    alias left to resolve. That is a miss, not a wrong answer.

    Returns ``(session_ids, read_ok)``. The read is done here rather than via
    ``_load_name_map`` because that helper folds OSError, ValueError and decode
    failures into an empty dict -- fine for a display path, but here it would
    make an existing-but-unreadable map look absent and send a session that is
    only addressable by its alias to exit 16 with nothing queued.
    """
    path = name_map_path or default_name_map_path()
    if not path.exists():
        return [], True
    try:
        stored = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return [], False
    if not isinstance(stored, dict):
        return [], False
    return [
        sid
        for sid, alias in stored.items()
        if isinstance(sid, str) and alias == token
    ], True


def _reachable_from_transcripts(token: str, projects_dir: Path) -> tuple[_Hits, bool]:
    """Session uuids whose transcript file exists on disk, matched on token.

    The transcript store is the broadest source: a session that ever ran wrote a
    ``<uuid>.jsonl`` here, and the file outlives the process. No recency cutoff
    is applied -- staleness is precisely what makes a session asleep rather than
    absent.

    An ABSENT directory is a definitive empty answer, not a read failure: no
    transcript store means no claude session ever ran under this HOME, so
    "nothing is reachable" is true rather than unknown. A directory that exists
    but cannot be read (permissions, EIO, a torn mount) IS a read failure --
    that is the case where absence is unproven.

    The distinction matters in both directions. Treating absent as unreadable
    makes every typo queue durably on a host that has never run claude, which
    strands envelopes and destroys the exit-16 typo guard. Treating a read
    ERROR as empty loses real mail.
    """
    hits: _Hits = []
    try:
        entries = list(projects_dir.glob("*/*.jsonl"))
    except OSError:
        return [], False
    seen: set[str] = set()
    for path in entries:
        sid = path.name[: -len(".jsonl")]
        if _token_matches(token, sid) and sid not in seen:
            seen.add(sid)
            hits.append((sid, "claude", _decode_project_dir(path.parent.name), False))
    return hits, True


def _token_matches(token: str, session_id: str) -> bool:
    """A token addresses a session by full uuid or by its 8-hex short form.

    Deliberately NOT a loose prefix match: a 2-char token would otherwise sweep
    in half the store and turn every send into an ambiguity error.

    Case folding applies to HEX-shaped ids only. Uuids and 8-hex short ids are
    case-insensitive by definition, so a token pasted from a UI or typed in caps
    names the same session. An opencode id (``ses_...``) is mixed-case BY
    CONSTRUCTION, so folding it would let two distinct sessions differing only
    in case collide -- and a wrong collision here wakes a stranger's session.
    Matches the normalization rule in ``agents.store_fallback`` deliberately;
    the two must not drift.
    """
    if not token or not session_id:
        return False
    tok = _fold_token(token)
    sid = _fold_token(session_id)
    return tok == sid or tok == sid[:8]


_OPENCODE_ID_RE = re.compile(r"^ses_[A-Za-z0-9]+$")


def _fold_token(value: str) -> str:
    """Lowercase a hex-shaped id; leave a mixed-case opencode id untouched."""
    v = (value or "").strip()
    return v if _OPENCODE_ID_RE.match(v) else v.lower()


def _reachable_from_registry(
    token: str, registry_path: Optional[Path]
) -> tuple[_Hits, bool]:
    """Registry rows including dead-pid and exited ones.

    An exited row is exactly the case the live lane drops and this lane keeps:
    the row is a durable record that this uuid exists, not a liveness claim.

    Carries each row's harness through: the registry holds rows for every
    provider, and waking a codex thread as claude would resume the wrong
    session entirely.
    """
    from fno.agents.registry import RegistryVersionError, load_registry

    try:
        entries = load_registry(registry_path)
    except (OSError, ValueError, RegistryVersionError):
        # A torn or version-drifted registry cannot be consulted. It reports
        # unreadable rather than empty, so the aggregate can tell "this token
        # is unknown" from "we could not look".
        return [], False
    hits: _Hits = []
    seen: set[str] = set()
    for e in entries:
        # NOT ``AgentEntry.session_id``: that property is harness-polymorphic
        # and resolves to ``short_id`` for claude -- the 8-hex daemon transport
        # key, not a resumable uuid. Feeding it to a wake would run
        # ``claude -r <jobId>`` against a session id that does not exist.
        # ``harness_session_id`` is the canonical uuid for every harness.
        sid = getattr(e, "harness_session_id", None)
        if not isinstance(sid, str):
            continue
        if _token_matches(token, sid) and sid not in seen:
            seen.add(sid)
            harness = getattr(e, "harness", None)
            cwd = getattr(e, "cwd", None)
            hits.append((
                sid,
                harness if isinstance(harness, str) and harness else "claude",
                cwd if isinstance(cwd, str) and cwd else None,
                True,
            ))
    return hits, True


def _reachable_from_roster(token: str, daemon_dir: Optional[Path]) -> tuple[_Hits, bool]:
    """Daemon roster rows, including ones stamped exited.

    Resolves the daemon dir the same way every other roster reader does. An
    earlier version fell back to ``Path(os.environ.get(..., ""))``, which is
    ``Path('.')`` rather than a falsy value -- so with the env var unset (the
    normal case) it read ``./roster.json`` and this whole source silently
    never fired.
    """
    base = daemon_dir
    if base is None:
        override = os.environ.get("FNO_CLAUDE_DAEMON_DIR")
        base = Path(override) if override else Path.home() / ".claude" / "daemon"
    if not (base / "roster.json").exists():
        # No roster file is a real, readable answer: the claude daemon is not
        # running, so it hosts nothing. Distinct from an unreadable one.
        return [], True
    try:
        raw = json.loads((base / "roster.json").read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return [], False
    if not isinstance(raw, dict):
        return [], False
    workers = raw.get("workers")
    if workers is None:
        return [], True
    if not isinstance(workers, dict):
        # A type-drifted roster is unreadable, not empty. Calling .values() on
        # a list here would raise straight out through `fno mail send`.
        return [], False
    hits: _Hits = []
    seen: set[str] = set()
    for row in workers.values():
        if not isinstance(row, dict):
            continue
        sid = row.get("sessionId")
        if not isinstance(sid, str):
            # A type-drifted leaf must not reach _token_matches, which would
            # call .lower() on it and raise straight out of `fno mail send`.
            continue
        if _token_matches(token, sid) and sid not in seen:
            seen.add(sid)
            cwd = row.get("cwd")
            hits.append((sid, "claude", cwd if isinstance(cwd, str) and cwd else None, True))
    return hits, True


def _reachable_from_graph(token: str) -> tuple[_Hits, bool]:
    """Session ids stamped onto backlog nodes (``sessions[]`` provenance).

    The weakest source and the last consulted: a node stamp proves a session
    once existed for some phase of some node, which is enough to attempt a wake
    but never enough to claim liveness.
    """
    try:
        from fno.graph.load import GraphCorruptionError, load_graph
    except ImportError:
        return [], False
    try:
        entries = load_graph()
    except (OSError, ValueError, GraphCorruptionError):
        # Corrupt, torn, or hash-mismatched: unreadable, NOT empty. Reporting
        # empty here would let a graph problem masquerade as "this token names
        # nothing" and drop the mail.
        return [], False
    hits: _Hits = []
    seen: set[str] = set()
    malformed = False
    for node in entries or []:
        if not isinstance(node, dict):
            malformed = True
            continue
        sessions = node.get("sessions")
        if sessions is None:
            continue
        if not isinstance(sessions, list):
            # Malformed, NOT absent. Skipping it silently while reporting the
            # store readable would let a corrupt node hide the only durable
            # record of the addressed session, turning a demotion into exit 16.
            malformed = True
            continue
        for entry in sessions:
            if not isinstance(entry, dict):
                malformed = True
                continue
            sid = entry.get("session_id")
            if not isinstance(sid, str):
                malformed = True
                continue
            if _token_matches(token, sid) and sid not in seen:
                seen.add(sid)
                harness = entry.get("harness")
                cwd = node.get("cwd")
                hits.append((
                    sid,
                    harness if isinstance(harness, str) and harness else "claude",
                    cwd if isinstance(cwd, str) and cwd else None,
                    True,
                ))
    return hits, not malformed


def resolve_reachable(
    token: str,
    *,
    projects_dir: Optional[Path] = None,
    registry_path: Optional[Path] = None,
    daemon_dir: Optional[Path] = None,
    name_map_path: Optional[Path] = None,
) -> tuple[Optional[ReachableSession], list[str]]:
    """Resolve ``token`` against the durable stores, ignoring liveness entirely.

    This is the rung below discovery. ``discover_live_sessions`` answers a
    LISTING question and is liveness-gated by design; when it misses, the token
    may still name a session that is merely asleep -- and asleep is a resumable
    state, not voicemail. Consulting the stores here is what turns a wall into a
    wake.

    Returns ``(session, [])`` on a unique hit, ``(None, [uuids])`` when the token
    is ambiguous across two stored sessions (never guess -- waking the wrong one
    means waking a stranger's session), and ``(None, [])`` on a full miss, which
    is the only case that still earns exit 16.

    EVERY readable store is consulted before uniqueness is declared. Returning
    on the first source that answers would let an 8-hex token with one
    transcript hit and a DIFFERENT matching uuid in the registry look unique,
    and the never-guess rule would be violated by an early return rather than
    by a bad choice. Richer metadata wins on merge: sources are ordered by
    confidence, so the first source to contribute a uuid also supplies its
    agent, and a later source only fills a cwd the earlier one lacked.
    """
    if not token or not token.strip():
        return None, []

    pdir = projects_dir or default_projects_dir()
    # A friendly <project>-<short8> alias must keep working once its session
    # falls out of the live listing; resolve it to real uuids and match those
    # alongside the raw token.
    alias_sids, alias_ok = _alias_to_session_ids(token, name_map_path)

    sources = (
        ("transcript", lambda t: _reachable_from_transcripts(t, pdir)),
        ("registry", lambda t: _reachable_from_registry(t, registry_path)),
        ("roster", lambda t: _reachable_from_roster(t, daemon_dir)),
        ("graph", lambda t: _reachable_from_graph(t)),
    )
    tokens = [token, *alias_sids]

    degraded: list[str] = [] if alias_ok else ["alias-map"]
    # Keyed case-insensitively: _token_matches is case-insensitive, so keying on
    # the stored spelling would make one uuid recorded lowercase in one store and
    # uppercase in another look like two sessions -- a false ambiguity, and one
    # the raw-token/alias expansion below would hit routinely.
    found: dict[str, ReachableSession] = {}
    cwd_verbatim: dict[str, bool] = {}
    for source, lookup in sources:
        for tok in tokens:
            hits, read_ok = lookup(tok)
            if not read_ok:
                if source not in degraded:
                    degraded.append(source)
                continue
            for sid, agent, cwd, verbatim in hits:
                key = sid.lower()
                prior = found.get(key)
                if prior is None:
                    found[key] = ReachableSession(
                        session_id=sid, source=source, agent=agent, cwd=cwd
                    )
                    cwd_verbatim[key] = verbatim and cwd is not None
                    continue
                # Keep the higher-confidence source and agent. Take a cwd only
                # when it improves on what we have: filling a missing one, or
                # replacing a lossy decoded guess with a verbatim record.
                take = (prior.cwd is None and cwd is not None) or (
                    cwd is not None and verbatim and not cwd_verbatim.get(key, False)
                )
                if take:
                    found[key] = ReachableSession(
                        session_id=prior.session_id,
                        source=prior.source,
                        agent=prior.agent,
                        cwd=cwd,
                    )
                    cwd_verbatim[key] = verbatim

    if len(found) > 1:
        return None, sorted(f.session_id for f in found.values())
    if len(found) == 1:
        if degraded:
            # Exactly one hit, but a store we could not read might hold a
            # colliding session. Uniqueness is therefore unproven, and waking on
            # an unproven-unique short id is the guess this refuses to make.
            # StoreReadError demotes durably to a real recipient rather than
            # waking a possible stranger.
            raise StoreReadError(degraded, resolved=next(iter(found.values())))
        return next(iter(found.values())), []
    if degraded:
        # Every source that COULD be read came back empty, but at least one
        # could not be read at all -- so absence is unproven. Refusing here
        # would exit 16 and queue nothing; the caller demotes durably instead.
        raise StoreReadError(degraded)
    return None, []


def discover_live_sessions(
    *,
    sessions_dir: Optional[Path] = None,
    projects_dir: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
    opencode_storage_dir: Optional[Path] = None,
    name_map_path: Optional[Path] = None,
    registry_path: Optional[Path] = None,
    exclude_short_ids: Iterable[str] = (),
    exclude_session_ids: Iterable[str] = (),
    project_resolver: Optional[Callable[[str], Optional[str]]] = None,
    psutil_mod=None,
    truth_fn: Optional[Callable[[DiscoveredSession], dict]] = None,
    classify_truth: bool = True,
) -> list[DiscoveredSession]:
    """Enumerate host-local session candidates and attach family-1 truth.

    Unions candidates from sidecars, canonical transcript stores, daemon
    rosters, and the fno registry. None of those enumeration signals can prove
    death. ``classify_truth=False`` is the resolver-only lane: it returns
    candidates without recursively classifying them so one requested truth
    lookup reads only that session's tail.

    ``exclude_short_ids`` drops sessions already present in the fno registry so
    the discovered lane does not double-list adopted sessions. Callers route
    only candidates whose :attr:`DiscoveredSession.is_alive` is true.
    ``projects_dir`` / ``project_resolver`` / ``psutil_mod`` are test seams.
    """
    sdir = sessions_dir or default_sessions_dir()
    resolver = project_resolver or resolve_project_for_cwd
    psu = psutil_mod or _import_psutil()
    exclude = {s for s in (exclude_short_ids or ()) if s}
    excluded_session_ids = {s for s in (exclude_session_ids or ()) if s}

    candidates: list[dict] = []
    for f in _iter_pid_files(sdir):
        data = _read_registry_file(f)
        if not data:
            continue
        session_id = data.get("sessionId")
        if not isinstance(session_id, str) or not session_id:
            continue
        pid = data.get("pid")
        try:
            pid = int(pid)  # type: ignore[arg-type]  # None/bad -> caught below
        except (TypeError, ValueError):
            try:
                pid = int(f.stem)
            except ValueError:
                continue
        short_id = data.get("jobId") or data.get("name") or session_id[:8]
        short_id = str(short_id)
        if short_id in exclude:
            continue
        status = data.get("status")
        candidates.append(
            {
                "session_id": session_id,
                "short_id": short_id,
                "pid": pid,
                "cwd": data.get("cwd") or "",
                "status": str(status) if status else None,
                "agent": str(data.get("agent") or "claude"),
            }
        )

    # Union the canonical transcript-store lane even when sidecar candidates
    # exist. A stale sidecar is enumeration metadata now; it must not suppress a
    # separate projects-only live session on the same host.
    pdir = projects_dir or default_projects_dir()
    project_rows = _discover_from_projects(
        pdir,
        psutil_mod=psu,
        exclude_session_ids=excluded_session_ids,
    )
    for r in project_rows:
        if r["short_id"] in exclude:
            continue
        candidates.append(r)

    # Daemon-loaded Codex threads are the primary candidate source: loaded
    # presence is age-free, while turn/start remains the delivery authority.
    for r in _discover_from_codex_daemon():
        if r["short_id"] in exclude or r["session_id"] in excluded_session_ids:
            continue
        candidates.append(r)

    # Codex disk-discovery (US2/US4). Unioned ALWAYS — a host can run
    # live claude AND codex sessions at once, so this is not gated on the claude
    # sources being empty (unlike the projects fallback above). Dedup on
    # session_id below folds any overlap. Zero-effect on a host with no codex
    # store (empty rglob), so claude-only behavior is unchanged.
    codex_rows = _discover_from_codex(
        codex_sessions_dir or default_codex_sessions_dir(),
        recency_seconds=_recency_seconds(),
        exclude_session_ids=excluded_session_ids,
    )
    for r in codex_rows:
        if r["short_id"] in exclude:
            continue
        candidates.append(r)

    # opencode discovery. Unioned ALWAYS for the same reason as the codex lane,
    # and zero-effect on a host with no opencode install. The SQLite store is
    # where current opencode writes; the legacy JSON tree is consulted only
    # when there is no database, so an old install still resolves.
    opencode_store = opencode_storage_dir or default_opencode_storage_dir()
    opencode_db = default_opencode_db_path(opencode_store)
    # Branch on the database EXISTING, never on it returning no rows. A query
    # can come back empty because nothing is live, because the store is locked,
    # or because a future schema drifted — and falling back on any of those
    # would resurrect the legacy tree's long-dead sessions as if they were live.
    if opencode_db.exists():
        opencode_rows = _discover_from_opencode_db(
            opencode_db,
            recency_seconds=_recency_seconds(),
            exclude_session_ids=excluded_session_ids,
        )
    else:
        opencode_rows = _discover_from_opencode(
            opencode_store,
            recency_seconds=_recency_seconds(),
            exclude_session_ids=excluded_session_ids,
        )
    for r in opencode_rows:
        if r["short_id"] in exclude:
            continue
        candidates.append(r)

    # Daemon roster + fno-agents registry (US1/US2, x-605c). Unioned ALWAYS,
    # like the codex source: a rostered bg worker (no pid-sidecar) or a named
    # registered session must resolve alongside live disk sessions. Dedup on
    # session_id below folds any overlap. Both readers are lenient -> zero rows
    # (never an error) when the roster/registry is absent, so claude-only hosts
    # are unchanged.
    for r in _discover_from_roster(exclude_session_ids=excluded_session_ids):
        if r["short_id"] in exclude:
            continue
        candidates.append(r)
    for r in _discover_from_registry(
        registry_path, exclude_session_ids=excluded_session_ids
    ):
        if r["short_id"] in exclude:
            continue
        candidates.append(r)

    # Dedup on session_id (Invariant: one row per live sessionId, not per pid).
    # Source order preserves daemon liveness precedence; later rows only enrich
    # missing metadata on the primary candidate.
    by_sid: dict[str, dict] = {}
    for r in candidates:
        existing = by_sid.setdefault(r["session_id"], r)
        if existing is not r:
            if not existing.get("cwd") and r.get("cwd"):
                existing["cwd"] = r["cwd"]
            if not existing.get("transcript_path") and r.get("transcript_path"):
                existing["transcript_path"] = r["transcript_path"]
    live = list(by_sid.values())

    for r in live:
        r["project"] = resolver(r["cwd"]) if r["cwd"] else None

    aliases = _resolve_aliases(live, name_map_path or default_name_map_path())

    sessions = [
        DiscoveredSession(
            session_id=r["session_id"],
            short_id=r["short_id"],
            handle=aliases.get(r["session_id"], r["short_id"]),
            pid=r["pid"],
            cwd=r["cwd"],
            project=r.get("project"),
            status=r["status"],
            agent=r["agent"],
            transcript_path=r.get("transcript_path"),
        )
        for r in live
    ]
    if not classify_truth:
        sessions.sort(key=lambda s: s.handle)
        return sessions
    if truth_fn is None:
        from fno.agents.session_truth import resolve_session_truth

        def truth_fn(session: DiscoveredSession) -> dict:
            return resolve_session_truth(
                session.handle,
                resolve=lambda _handle: (session, []),
                projects_root=projects_dir,
                codex_sessions_dir=codex_sessions_dir,
                opencode_storage_dir=opencode_storage_dir,
            )
    for session in sessions:
        session.truth_state = str(truth_fn(session).get("state") or "unknown")
    # Stable render order: by handle.
    sessions.sort(key=lambda s: s.handle)
    return sessions
