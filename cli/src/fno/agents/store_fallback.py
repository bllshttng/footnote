"""Resolve a session token against its harness's OWN store when the fno
registry misses, then adopt the row (x-9cc5).

The registry is a cache of reality, not a gate in front of it. A session with no
roster row -- reaped after a terminal stop, or never spawn-created -- was
unreachable by every fno verb even though the harness itself resumes it fine.
This module is the miss-path healer behind ``registry.resolve_agent``: probe the
harness stores for a session-shaped token, and on exactly one match register the
row so the verb proceeds AND the session returns to the roster.

Three rules keep it from guessing:

- **Shape gate** -- only a session-shaped token (8-hex, UUID, ``ses_...``) is
  probed, so a plain unknown name still fails exactly as before.
- **Exactly one match** -- two stores (or two sessions in one store) matching
  refuses with the candidate list; git's ambiguous-short-SHA posture.
- **Never live** -- a store row proves the session EXISTS, never that it is
  running, so the adopted row is ``orphaned``. Store membership must not
  resurrect a dead session into lane caps or live anycast (the x-830c lesson).
"""
from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    from fno.agents.registry import AgentEntry

# Session-shaped tokens only. A name that merely misses (`reviewer`, `deadbeef`
# as a registry name) never reaches a store probe -- registry names win first in
# resolve_agent, and anything not matching these shapes raises as it always did.
_SHORT_RE = re.compile(r"^[0-9a-f]{8}$")
_UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
_OPENCODE_RE = re.compile(r"^ses_[A-Za-z0-9]+$")

# Transcript lines before the session's `cwd` is recorded (line 1 is a summary /
# meta record on current claude). Bounded so a probe never streams a 100 MB log.
_CWD_SCAN_LINES = 50


@dataclass(frozen=True)
class StoreHit:
    """One harness store's answer for a token: the session it names."""

    harness: str
    session_id: str
    cwd: str

    @property
    def short_id(self) -> str:
        return self.session_id.split("-", 1)[0][:8]


def _normalize(token: str) -> str:
    """Trim, and lowercase a hex-shaped token. opencode ids are mixed-case by
    construction, so they are left exactly as given."""
    t = (token or "").strip()
    return t if _OPENCODE_RE.match(t) else t.lower()


def is_session_shaped(token: str) -> bool:
    """True for a token worth probing a harness store with."""
    t = _normalize(token)
    return bool(_SHORT_RE.match(t) or _UUID_RE.match(t) or _OPENCODE_RE.match(t))


def _claude_projects_dir() -> Path:
    return Path.home() / ".claude" / "projects"


def _codex_sessions_dir() -> Path:
    return Path.home() / ".codex" / "sessions"


def _transcript_cwd(path: Path) -> str:
    """The session's own recorded cwd, or "" when it never recorded one."""
    # errors="replace" so an invalid UTF-8 byte mid-transcript cannot raise
    # UnicodeDecodeError from the ITERATION itself, outside the per-line guard.
    # A mangled line simply fails to parse as JSON and is skipped.
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for _, line in zip(range(_CWD_SCAN_LINES), fh):
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                cwd = rec.get("cwd") if isinstance(rec, dict) else None
                if isinstance(cwd, str) and cwd:
                    return cwd
    except (OSError, ValueError):
        return ""
    return ""


def _probe_claude(token: str) -> list[StoreHit]:
    """Claude's canonical transcript store -- the same one ``claude -r`` reads.

    The filename IS the session UUID, so an 8-hex short matches by prefix. The
    ``<pid>.json`` sidecar is deliberately not consulted: it covers only live bg
    supervisors, and the session this fallback exists for is precisely the one
    with no live supervisor left.
    """
    if _OPENCODE_RE.match(token):
        return []
    pattern = f"*/{token}.jsonl" if _UUID_RE.match(token) else f"*/{token}-*.jsonl"
    hits: dict[str, StoreHit] = {}
    try:
        found = list(_claude_projects_dir().glob(pattern))
    except OSError:
        return []
    for path in sorted(found):
        name = path.name
        if ".sync-conflict-" in name:
            continue
        sid = name[: -len(".jsonl")]
        if sid not in hits:
            hits[sid] = StoreHit("claude", sid, _transcript_cwd(path))
    return list(hits.values())


def _probe_codex(token: str) -> list[StoreHit]:
    """Codex rollouts (``rollout-<ts>-<uuid>.jsonl``); cwd from ``session_meta``."""
    if _OPENCODE_RE.match(token):
        return []
    from fno.agents.discover import _codex_meta

    hits: dict[str, StoreHit] = {}
    try:
        found = list(_codex_sessions_dir().rglob(f"rollout-*-{token}*.jsonl"))
    except OSError:
        return []
    for path in sorted(found):
        meta = _codex_meta(path)
        if meta is None:
            continue
        sid, cwd = meta
        # The glob matches on the filename; confirm against the record itself so
        # a token that lands mid-uuid never counts as a match.
        if not (sid == token or sid.startswith(token)):
            continue
        hits.setdefault(sid, StoreHit("codex", sid, cwd))
    return list(hits.values())


def _probe_opencode(token: str) -> list[StoreHit]:
    """opencode's SQLite store. Its ids are ``ses_``-prefixed, so a hex token
    never reaches here -- and a `ses_` token never reaches the other two."""
    if not _OPENCODE_RE.match(token):
        return []
    from fno.agents.discover import default_opencode_db_path, opencode_query

    db = default_opencode_db_path()
    if not db.exists():
        return []
    rows = opencode_query(db, "SELECT id, directory FROM session WHERE id = ?", (token,))
    return [
        StoreHit("opencode", sid, directory if isinstance(directory, str) else "")
        for sid, directory in rows
        if isinstance(sid, str) and sid
    ]


_PROBES = (_probe_claude, _probe_codex, _probe_opencode)


def probe_stores(token: str) -> list[StoreHit]:
    """Every harness store's answer for ``token``. Never raises: a corrupt or
    missing store contributes no rows rather than denying the whole probe."""
    token = _normalize(token)
    if not is_session_shaped(token):
        return []
    hits: list[StoreHit] = []
    for probe in _PROBES:
        try:
            hits.extend(probe(token))
        except Exception:  # noqa: BLE001 - one broken store never denies the rest
            continue
    return hits


def heal_from_harness_store(
    token: str, *, registry_path: Optional[Path] = None
) -> Optional["AgentEntry"]:
    """Adopt the session ``token`` names into the registry and return its row.

    ``None`` when the token is not session-shaped or no store knows it -- the
    caller then raises its own unchanged not-found error. Raises
    :class:`~fno.agents.registry.AgentResolutionError` naming the candidates when
    more than one session matches: an ambiguous token is refused, never guessed.

    Registration is best-effort. If the registry write fails, the synthesized row
    is still returned so the verb reaches the session anyway -- reaching it wins,
    and the row appears on the next resolution.
    """
    from fno.agents.registry import AgentEntry, AgentResolutionError, register_existing_session

    hits = probe_stores(token)
    if not hits:
        return None
    if len(hits) > 1:
        cands = ", ".join(f"{h.session_id} ({h.harness})" for h in sorted(
            hits, key=lambda h: (h.harness, h.session_id)
        ))
        raise AgentResolutionError(
            f"token {token!r} matches {len(hits)} sessions across harness stores: "
            f"{cands}. Disambiguate with the full session id.",
            ambiguous=True,
        )

    hit = hits[0]
    # claude's transport key is the 8-hex jobId (`claude attach <jobId>`), NOT
    # the full UUID that HARNESS_SESSION_ID_FIELDS would otherwise write there.
    short_id = hit.short_id if hit.harness == "claude" else ""
    try:
        return register_existing_session(
            provider=hit.harness,
            session_id=hit.session_id,
            cwd=hit.cwd,
            short_id=short_id,
            status="orphaned",
            registry_path=registry_path,
        )
    except Exception as exc:  # noqa: BLE001 - reaching the session beats the roster row
        sys.stderr.write(
            f"WARN: resolved {token!r} from the {hit.harness} store but could not "
            f"register it ({exc}); the row will appear on a later resolution.\n"
        )
        entry = AgentEntry(
            name=_fallback_name(hit.session_id),
            cwd=hit.cwd,
            log_path="",
            harness=hit.harness,
            harness_session_id=hit.session_id,
            status="orphaned",
            short_id=short_id,
        )
        return entry


def _fallback_name(session_id: str) -> str:
    from fno.harness_identity import canonical_handle

    return canonical_handle(session_id)
