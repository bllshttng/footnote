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
- **Project confinement** -- a store hit is adopted only into the CALLER's
  project. The probes scan machine-wide (a transcript store is global), and
  before this rule a bare handle from a foreign repo healed into scope and got
  woken as a side effect (defect 1: a regready session revived from footnote).
  Membership is settings ``project`` first, then the shared ``git-common-dir``
  (NOT toplevel -- footnote is worktree-first, so toplevel differs per worktree
  and would refuse canonical->worktree traffic), then refuse. An out-of-project
  hit is refused with the candidate named, copying the ambiguity posture; an
  explicit ``cross_project`` flag is the only override (a spawn into a foreign
  repo). The confinement lives here because every store-adoption path routes
  through :func:`heal_from_harness_store`; ``resume`` does not (it matches loaded
  registry entries via ``resolve_agent_in``), so it is uncovered by design.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Optional

from fno.agents.fs_scan import path_exists_strict, scan_files
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
    return bool(_SHORT_RE.match(t) or is_full_session_id(t))


def is_full_session_id(token: str) -> bool:
    """True when ``token`` is a complete, collision-free harness session id."""
    t = _normalize(token)
    return bool(_UUID_RE.match(t) or _OPENCODE_RE.match(t))


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
    found = scan_files(
        root,
        max_depth=1,
        include=lambda name: name.endswith(".jsonl") and needle in name.lower(),
    )
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
    found = scan_files(
        root,
        include=lambda name: name.startswith("rollout-")
        and name.endswith(".jsonl"),
    )
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
    from fno.agents.discover import (
        _opencode_session_info,
        default_opencode_db_path,
        default_opencode_storage_dir,
        opencode_query,
    )

    db = default_opencode_db_path()
    if not path_exists_strict(db):
        paths = scan_files(
            default_opencode_storage_dir() / "session",
            max_depth=1,
            include=lambda name: name.endswith(".json") and token in name,
        )
        hits: list[StoreHit] = []
        for path in paths:
            info = _opencode_session_info(path)
            if info is None:
                raise OSError(f"unreadable opencode session candidate: {path}")
            sid, cwd = info
            if session_handle_tier(token, sid) is not None:
                hits.append(StoreHit("opencode", sid, cwd))
        return hits
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


def _git_common_dir(cwd: Path) -> Optional[str]:
    """The shared git common-dir for ``cwd``, absolute and worktree-stable.

    ``git rev-parse --git-common-dir`` is ``<canonical>/.git`` from the canonical
    checkout AND from every one of its worktrees (including ones outside the
    checkout, like ``~/.fno/worktrees/...``), so it identifies a project across
    its whole worktree family. ``--show-toplevel`` does NOT -- it differs per
    worktree, so it would refuse canonical->worktree traffic. Returns None for a
    non-repo or any git failure (the caller then refuses to adopt).
    """
    try:
        out = subprocess.run(
            ["git", "-C", str(cwd), "rev-parse",
             "--path-format=absolute", "--git-common-dir"],
            capture_output=True, text=True, check=False, timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    common = (getattr(out, "stdout", "") or "").strip()
    if getattr(out, "returncode", 1) == 0 and common:
        try:
            return str(Path(common).resolve())
        except OSError:
            return None
    return None


def _project_identity(cwd: Optional[str]) -> "tuple[Optional[str], Optional[str]]":
    """A comparable project identity for ``cwd``: ``(settings_project_id, git_common_dir)``.

    Either field may be None. Settings project id wins when both cwds answer it
    (the canonical fno notion of project); the git common-dir is the
    worktree-stable fallback for a cwd whose project config did not link.
    """
    pid: Optional[str] = None
    if cwd:
        try:
            from fno.inbox.store import ProjectIdentificationError, resolve_project

            pid = resolve_project(cwd=Path(cwd))
        except ProjectIdentificationError:
            pid = None
        except Exception:  # noqa: BLE001 - membership must degrade, never crash the verb
            pid = None
    gdir = _git_common_dir(Path(cwd)) if cwd else None
    return (pid, gdir)


def _membership(
    s_ident: "tuple[Optional[str], Optional[str]]",
    h_ident: "tuple[Optional[str], Optional[str]]",
) -> Optional[bool]:
    """True if two project identities match; False if a different project; None
    if membership is unresolvable (cannot be proven either way).

    Settings project id compared first; when one or both lack it, the git
    common-dir decides. Two foreign repos differ; a canonical checkout and its
    worktree match (same common-dir). Unresolvable on both axes -> None.
    """
    s_pid, s_gdir = s_ident
    h_pid, h_gdir = h_ident
    if s_pid is not None and h_pid is not None:
        return s_pid == h_pid
    if s_gdir is not None and h_gdir is not None:
        return s_gdir == h_gdir
    # One resolved by settings, the other only by git (or not at all). A settings
    # project id cannot be cross-compared to a git dir, so we cannot prove a match.
    return None


def _same_project(scope_cwd: Optional[str], hit_cwd: Optional[str]) -> Optional[bool]:
    """True if ``hit_cwd`` is in ``scope_cwd``'s project; False if a different one;
    None if membership is unresolvable (the caller refuses).

    Thin two-cwd wrapper over :func:`_membership`; callers that resolve one side
    once across many hits ( confinement ) should call ``_membership`` directly
    with the precomputed scope identity rather than re-spawning git per hit.
    """
    if not scope_cwd or not hit_cwd:
        return None
    return _membership(_project_identity(scope_cwd), _project_identity(hit_cwd))


def _confine_to_project(
    token: str,
    hits: list[StoreHit],
    *,
    scope_cwd: Optional[str],
    cross_project: bool,
) -> list[StoreHit]:
    """Filter ``hits`` to the caller's project; refuse when none is in-project.

    The refused posture copies ambiguity: an out-of-project candidate is named,
    never silently adopted and woken. Returns ``hits`` unchanged when confinement
    does not apply -- ``cross_project`` set, or the scope is itself not a known
    project (no settings project and not a git repo), in which case there is no
    "across" to protect and the historical behavior stands. Raises
    :class:`AgentResolutionError` (``ambiguous=True``) on the refuse.
    """
    if cross_project:
        return hits
    scope = scope_cwd or os.getcwd()
    # ponytail: compute the scope identity ONCE here, not per hit. The scope is
    # invariant across the loop, and _project_identity spawns a git subprocess;
    # re-resolving it per hit (via _same_project) was N redundant fork+exec calls.
    s_ident = _project_identity(scope)
    if s_ident == (None, None):
        return hits
    in_project: list[StoreHit] = []
    cross: list[StoreHit] = []
    unknown: list[StoreHit] = []
    for h in hits:
        verdict = _membership(s_ident, _project_identity(h.cwd))
        if verdict is True:
            in_project.append(h)
        elif verdict is False:
            cross.append(h)
        else:
            unknown.append(h)
    if in_project:
        return in_project
    from fno.agents.registry import AgentResolutionError

    refused = sorted(cross + unknown, key=lambda h: (h.harness, h.session_id))
    cands = ", ".join(
        f"{h.session_id} ({h.harness}, cwd={h.cwd or '?'})"
        for h in refused
    )
    # Name the real reason: confirmed foreign, unresolvable (e.g. a transcript
    # that never recorded a cwd, or a since-deleted worktree), or both. Calling
    # an unresolvable hit "cross-project" sends the operator looking in the wrong
    # place.
    if cross and unknown:
        reason = (
            "cross-project and project-unresolvable candidate(s) refused"
        )
    elif cross:
        reason = "cross-project candidate(s) refused"
    else:
        reason = (
            "candidate(s) whose project membership could not be determined "
            "(cwd unset or no longer resolvable)"
        )
    raise AgentResolutionError(
        f"token {token!r} matches no session in this project; "
        f"{reason}: {cands}. "
        f"Disambiguate with the full session id in scope, or pass cross-project.",
        ambiguous=True,
    )


def heal_from_harness_store(
    token: str, *, registry_path: Optional[Path] = None,
    scope_cwd: Optional[str] = None, cross_project: bool = False,
) -> Optional["AgentEntry"]:
    """Adopt the session ``token`` names into the registry and return its row.

    ``None`` when the token is not session-shaped or no store knows it -- the
    caller then raises its own unchanged not-found error. Raises
    :class:`~fno.agents.registry.AgentResolutionError` naming the candidates when
    more than one session matches: an ambiguous token is refused, never guessed.

    Adoption is CONFINED to the caller's project unless ``cross_project`` is set
    (defect 1: a bare handle from a foreign repo healed into scope and got woken).
    ``scope_cwd`` defaults to the process cwd; an out-of-project hit is refused
    with the candidate named, copying the ambiguity posture. See the module
    docstring's project-confinement rule.

    Registration is best-effort. If the registry write fails, the synthesized row
    is still returned so the verb reaches the session anyway -- reaching it wins,
    and the row appears on the next resolution.
    """
    from fno.agents.registry import AgentEntry, AgentResolutionError, register_existing_session

    hits = complete_store_hits(token)
    if not hits:
        return None
    # Project confinement: adopt only a session in the caller's project, else
    # refuse (an out-of-project hit named, not silently healed and woken).
    hits = _confine_to_project(
        token, hits, scope_cwd=scope_cwd, cross_project=cross_project
    )
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
            # This row is ADOPTED from the claude store, not a spawn receipt:
            # nothing here proves footnote started the session, and it is
            # routinely an operator's own terminal that no SessionStart hook
            # registered. `origin` carries that because it is durable - `status`
            # is a liveness stamp `reconcile` flips back to "live" the moment
            # the session answers a probe, so a reader keying on it gets one
            # pass of protection and then none. Lanes that stop a session read
            # this field to answer "footnote-spawned?" with unknown rather than
            # with the absence of an operator marker.
            origin="adopted",
            registry_path=registry_path,
        )
    except AgentResolutionError:
        raise
    except Exception as exc:  # noqa: BLE001 - reaching the session beats the roster row
        sys.stderr.write(
            f"WARN: resolved {token!r} from the {hit.harness} store but could not "
            f"register it ({exc}); the row will appear on a later resolution.\n"
        )
        # Same parent edge the registered row above now stamps (x-132c): the
        # adopting session vouches for this row, and this fallback copy reaches
        # the caller without passing register_session's stamping path.
        from fno.agents.dispatch import _capture_parent_edge

        _sb_session, _sb_harness, _sb_cwd = _capture_parent_edge()
        entry = AgentEntry(
            name=_fallback_name(hit.session_id),
            cwd=hit.cwd,
            log_path="",
            harness=hit.harness,
            harness_session_id=hit.session_id,
            status="orphaned",
            short_id=short_id,
            spawned_by_session=_sb_session,
            spawned_by_harness=_sb_harness,
            spawned_by_cwd=_sb_cwd,
            # Same fact as the registered row above, and it has to be stated
            # here too: this one is handed straight back to the caller when
            # registration fails, so it reaches a reader without ever passing
            # the path that would have marked it.
            origin="adopted",
        )
        return entry


def _fallback_name(session_id: str) -> str:
    from fno.harness_identity import canonical_handle

    return canonical_handle(session_id)
