"""Resolve a session token against its harness's OWN store and adopt store-only
sessions when the fno registry misses (x-9cc5).

The registry is a cache of reality, not a gate in front of it. A session with no
roster row -- reaped after a terminal stop, or never spawn-created -- was
unreachable by every fno verb even though the harness itself resumes it fine.
This module owns the store probes behind ``registry.resolve_agent``. Every short
session-shaped registry hit is checked against them for cross-source ambiguity;
on a registry miss, exactly one match is registered so the verb proceeds AND the
session returns to the roster.

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

from fno.harness_identity import claude_transport_short_id, session_handle_tier

if TYPE_CHECKING:
    from fno.agents.registry import AgentEntry

# Session-shaped tokens only. Eight alphanumeric characters can be an OpenCode
# tail even when they look like a friendly name, so those tokens share the store
# ambiguity check. Names outside these shapes never pay for a store read.
_SHORT_RE = re.compile(r"^[A-Za-z0-9]{8}$")
_UUID_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
    re.IGNORECASE,
)
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
        """Claude's legacy first-eight transport key, not a mailbox address."""
        return claude_transport_short_id(self.session_id)


def _normalize(token: str) -> str:
    """Trim only; the shared identity owner applies harness-safe case rules."""
    return (token or "").strip()


def is_session_shaped(token: str) -> bool:
    """True for a token worth probing a harness store with."""
    t = _normalize(token)
    return bool(_SHORT_RE.match(t) or _UUID_RE.match(t) or _OPENCODE_RE.match(t))


def _claude_projects_dir() -> Path:
    """Delegated, not re-derived: ``discover`` already owns the store roots and
    their ``FNO_*_DIR`` test overrides, so a probe here cannot drift from what
    discovery reads (and needs no placement-rule allowlist entry of its own)."""
    from fno.agents.discover import default_projects_dir

    return default_projects_dir()


def _codex_sessions_dir() -> Path:
    from fno.agents.discover import default_codex_sessions_dir

    return default_codex_sessions_dir()


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
    needle = token.lower()
    hits: dict[str, StoreHit] = {}
    root = _claude_projects_dir()
    found = {
        path
        for pattern in (
            f"*/{needle}.jsonl",      # full id
            f"*/*{needle}.jsonl",     # canonical tail
            f"*/{needle}*.jsonl",     # legacy prefix
        )
        for path in root.glob(pattern)
    }
    for path in sorted(found):
        name = path.name
        if ".sync-conflict-" in name:
            continue
        sid = name[: -len(".jsonl")]
        if session_handle_tier(token, sid) is not None and sid not in hits:
            hits[sid] = StoreHit("claude", sid, _transcript_cwd(path))
    return list(hits.values())


def _probe_codex(token: str) -> list[StoreHit]:
    """Codex rollouts (``rollout-<ts>-<uuid>.jsonl``); cwd from ``session_meta``."""
    if _OPENCODE_RE.match(token):
        return []
    from fno.agents.discover import _codex_meta

    hits: dict[str, StoreHit] = {}
    root = _codex_sessions_dir()
    needle = token.lower()
    found = {
        path
        for pattern in (
            f"rollout-*-{needle}.jsonl",   # full id
            f"rollout-*-*{needle}.jsonl",  # canonical tail
            f"rollout-*-{needle}*.jsonl",  # legacy prefix
        )
        for path in root.rglob(pattern)
    }
    for path in sorted(found):
        meta = _codex_meta(path)
        if meta is None:
            raise OSError(f"unreadable codex session candidate: {path}")
        sid, cwd = meta
        if session_handle_tier(token, sid) is None:
            continue
        hits.setdefault(sid, StoreHit("codex", sid, cwd))
    return list(hits.values())


def _probe_opencode(token: str) -> list[StoreHit]:
    """opencode's SQLite store. Its ids are ``ses_``-prefixed, so a hex token
    never reaches here -- and a `ses_` token never reaches the other two."""
    if not (_OPENCODE_RE.match(token) or _SHORT_RE.match(token)):
        return []
    from fno.agents.discover import default_opencode_db_path, opencode_query

    db = default_opencode_db_path()
    if not db.exists():
        return []
    rows = opencode_query(
        db,
        "SELECT id, directory FROM session "
        "WHERE id = ? OR substr(id, -8) = ? OR substr(id, 1, 8) = ?",
        (token, token, token),
        raise_on_error=True,
    )
    return [
        StoreHit("opencode", sid, directory if isinstance(directory, str) else "")
        for sid, directory in rows
        if isinstance(sid, str) and sid and session_handle_tier(token, sid) is not None
    ]


_PROBES = (_probe_claude, _probe_codex, _probe_opencode)


def probe_stores(token: str, *, require_complete: bool = True) -> list[StoreHit]:
    """Every harness store's answer for ``token``.

    Callers that explicitly pass ``require_complete=False`` retain the
    historical partial-result behavior. Identity selection uses the strict
    default because a partial answer cannot prove a short token unique across
    the shared namespace.
    """
    token = _normalize(token)
    if not is_session_shaped(token):
        return []
    hits: list[StoreHit] = []
    failed: list[str] = []
    for probe in _PROBES:
        try:
            hits.extend(probe(token))
        except Exception:  # noqa: BLE001 - collect every readable store before refusing
            failed.append(probe.__name__.removeprefix("_probe_"))
            continue
    matched = [
        hit
        for hit in hits
        if session_handle_tier(token, hit.session_id) is not None
    ]
    distinct = list({(hit.harness, hit.session_id): hit for hit in matched}.values())
    if failed and require_complete:
        from fno.agents.registry import AgentResolutionError

        candidates = ""
        if distinct:
            candidates = " Visible candidates: " + ", ".join(
                f"{hit.session_id} ({hit.harness})"
                for hit in sorted(distinct, key=lambda hit: (hit.harness, hit.session_id))
            ) + "."
        raise AgentResolutionError(
            f"token {token!r} could not be checked for cross-store uniqueness "
            f"because these harness stores were unreadable: {', '.join(failed)}. "
            f"{candidates} Use the full session id.",
            ambiguous=True,
        )
    return distinct


def complete_store_hits(token: str) -> list[StoreHit]:
    """Return a complete cross-harness answer or refuse short-token selection."""
    return probe_stores(token, require_complete=True)


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

    hits = complete_store_hits(token)
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
    except AgentResolutionError:
        raise
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
