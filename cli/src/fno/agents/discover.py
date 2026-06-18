"""fno.agents.discover — discover live hand-started Claude Code sessions.

Group A / P1 of the live-session-comms epic (ab-098967b4). A transport-free
read over Claude Code's own per-session registry at
``~/.claude/sessions/<pid>.json`` (Locked Decision 3: no MCP /
register-channel dependency — the registry already exists on disk). Surfaces
live, un-adopted sessions in ``fno agents list`` so they are addressable by a
legible handle without a UUID.

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


def default_sessions_dir() -> Path:
    """Claude Code's per-session registry directory on this host."""
    override = os.environ.get(SESSIONS_DIR_ENV)
    if override:
        return Path(override)
    return Path(os.path.expanduser("~")) / ".claude" / "sessions"


def default_name_map_path() -> Path:
    """Persisted hex->legible alias overlay (``~/.fno/session-names.json``)."""
    return paths.state_dir() / NAME_MAP_FILENAME


@dataclass
class DiscoveredSession:
    """One live, host-local Claude Code session surfaced in the lane."""

    session_id: str
    short_id: str  # hex handle (jobId), the addressable id
    handle: str  # friendly alias, or short_id when no alias is mapped
    pid: int
    cwd: str
    project: Optional[str]
    status: Optional[str]  # registry status: idle/busy/waiting
    agent: str = "claude"

    def to_row(self) -> dict:
        """Canonical dict shape for the JSON/table renderers."""
        return {
            "handle": self.handle,
            "short_id": self.short_id,
            "session_id": self.session_id,
            "pid": self.pid,
            "cwd": self.cwd,
            "project": self.project,
            "status": self.status,
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


def _create_time_epoch(pid: int, psutil_mod) -> Optional[float]:
    """OS-reported process create time in epoch seconds, or None if dead.

    None means the PID is not running here (or we cannot inspect it) — treat
    as not-live, mirroring the claim system's reuse-safe liveness.
    """
    try:
        return float(psutil_mod.Process(pid).create_time())
    except Exception:
        # psutil.NoSuchProcess / AccessDenied / any inspection failure — a
        # process we cannot validate is one we will not claim is live.
        return None


def _ctime_matches(create_time: float, proc_start: str) -> bool:
    """True iff a process create time matches the registry ``procStart`` string.

    ``procStart`` is a ctime-format string written by Claude Code from the same
    OS create time, so a ctime-string match proves the PID was not reused since
    the file was written — without epoch parsing. Verified on 2.1.169, CC
    renders it in **UTC** (e.g. ``"Tue Jun  9 18:54:16 2026"`` for an 11:54
    PDT start), so we compare against the UTC rendering (``asctime(gmtime)``)
    AND the local rendering (``ctime``) to stay correct whichever clock a CC
    build uses. A +/-1s window absorbs sub-second rounding; whitespace is
    collapsed so single- vs double-space day padding never causes a miss.

    Accepting both renderings does not weaken reuse detection: a reused PID's
    new create time differs from the old ``procStart`` by far more than 1s in
    either timezone, so neither rendering would spuriously match.
    """
    want = " ".join(proc_start.split())
    if not want:
        return False
    for delta in (0.0, -1.0, 1.0):
        t = create_time + delta
        for rendered in (time.asctime(time.gmtime(t)), time.ctime(t)):
            if " ".join(rendered.split()) == want:
                return True
    return False


def _is_live(pid: int, proc_start: str, psutil_mod) -> bool:
    """Reuse-safe liveness: PID running here AND create-time matches procStart.

    When ``procStart`` is absent (rare; present on every probed 2.1.169 file)
    a running PID is accepted — the reuse guard simply cannot run, and the
    alternative (dropping a genuinely-live session) is worse than the
    vanishingly-small reuse window with no recorded create time.
    """
    create_time = _create_time_epoch(pid, psutil_mod)
    if create_time is None:
        return False
    if not proc_start:
        return True
    return _ctime_matches(create_time, proc_start)


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
        import yaml

        from fno.graph._intake import _settings_candidate_paths
    except ImportError:
        return
    for path in _settings_candidate_paths():
        if not path.exists():
            continue
        try:
            data = yaml.safe_load(path.read_text(encoding="utf-8"))
        except (OSError, yaml.YAMLError):
            continue
        if not isinstance(data, dict):
            continue
        # config.work is canonical; fall back to legacy top-level work.
        work = (data.get("config") or {}).get("work") or data.get("work")
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
    return f"{base}-{short_id}"


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
                    if sid in pruned and isinstance(pruned[sid], str) and pruned[sid]:
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
    name_map_path: Optional[Path] = None,
    project_resolver: Optional[Callable[[str], Optional[str]]] = None,
    psutil_mod=None,
) -> tuple[Optional[DiscoveredSession], list[str]]:
    """Resolve a send handle to a live session, or suggest the closest ones (US2).

    A handle is a friendly alias (``<project>-<short>``) or the bare hex
    short-id. Returns ``(session, [])`` on an exact match, else
    ``(None, [closest handles])`` for the AC2-ERR error message. One discovery
    scan serves both the match and the suggestions. No exclusion: the user
    named a specific live session, so even an adopted one resolves.
    """
    sessions = discover_live_sessions(
        sessions_dir=sessions_dir,
        name_map_path=name_map_path,
        project_resolver=project_resolver,
        psutil_mod=psutil_mod,
    )
    for s in sessions:
        if handle and (s.handle == handle or s.short_id == handle):
            return s, []
    import difflib

    candidates: list[str] = []
    for s in sessions:
        candidates.append(s.handle)
        if s.short_id not in candidates:
            candidates.append(s.short_id)
    return None, difflib.get_close_matches(handle or "", candidates, n=limit, cutoff=0.3)


def discover_live_sessions(
    *,
    sessions_dir: Optional[Path] = None,
    name_map_path: Optional[Path] = None,
    exclude_short_ids: Iterable[str] = (),
    project_resolver: Optional[Callable[[str], Optional[str]]] = None,
    psutil_mod=None,
) -> list[DiscoveredSession]:
    """Return live, host-local Claude Code sessions, deduped + aliased.

    ``exclude_short_ids`` drops sessions already present in the fno registry so
    the discovered lane means "live but not adopted" (no double-listing).
    ``project_resolver`` / ``psutil_mod`` are test seams.
    """
    sdir = sessions_dir or default_sessions_dir()
    resolver = project_resolver or resolve_project_for_cwd
    psu = psutil_mod or _import_psutil()
    exclude = {s for s in (exclude_short_ids or ()) if s}

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
            pid = int(pid)
        except (TypeError, ValueError):
            try:
                pid = int(f.stem)
            except ValueError:
                continue
        proc_start = data.get("procStart") or ""
        if not _is_live(pid, str(proc_start), psu):
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

    # Dedup on session_id (Invariant: one row per live sessionId, not per pid).
    by_sid: dict[str, dict] = {}
    for r in candidates:
        by_sid.setdefault(r["session_id"], r)
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
        )
        for r in live
    ]
    # Stable render order: by handle.
    sessions.sort(key=lambda s: s.handle)
    return sessions
