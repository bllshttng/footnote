"""Shared ambient harness session identity resolution."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from typing import Callable, Mapping, Optional


# Highest precedence first. Callers that need ambiguity detection may inspect
# the same marker facts without duplicating names or harness mappings.
HARNESS_SESSION_MARKERS: tuple[tuple[str, str], ...] = (
    ("CODEX_THREAD_ID", "codex"),
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_SESSION_ID", "codex"),
    ("GEMINI_SESSION_ID", "gemini"),
    ("OPENCODE_SESSION_ID", "opencode"),
)

LEGACY_HARNESS_SESSION_MARKERS: tuple[tuple[str, str], ...] = (
    ("CLAUDE_SESSION_ID", "claude"),
)


# Ambient session-identity env names that a HERMETIC run must not see. This is
# deliberately WIDER than the two tuples above: those define what
# resolve_harness_identity() consults, in precedence order, while this defines
# what has to be absent for a test or a preflight leg to behave like a fresh
# checkout. Several modules read a session marker directly rather than through
# the resolver - carveout/core.py, done/cli.py and log_cmd.py read
# CLAUDECODE_SESSION_ID, adapters/hermes.py additionally treats
# HERMES_SESSION_ID as proof of an in-session run - so scrubbing only the
# resolver's tuples leaves those paths resolving the live session.
#
# Adding a name here changes ONLY what gets scrubbed, never what resolves, which
# is why the direct-read markers belong here and not in the tuples: promoting one
# to HARNESS_SESSION_MARKERS would silently change resolution precedence and the
# harness a claim is tagged with.
AMBIENT_IDENTITY_ENV: tuple[str, ...] = (
    *(marker for marker, _ in HARNESS_SESSION_MARKERS),
    *(marker for marker, _ in LEGACY_HARNESS_SESSION_MARKERS),
    "CLAUDECODE_SESSION_ID",
    "HERMES_SESSION_ID",
)


def scrub_ambient_identity(environ: Optional[dict] = None) -> tuple[str, ...]:
    """Remove every ambient session-identity name from ``environ`` (default
    ``os.environ``); return the names actually removed.

    Both pytest trees call this at conftest module load, because a fixture runs
    too late for the modules that read a marker at import time, and because a
    scrub on one tree is decorative while the other still resolves the live
    session.
    """
    target = os.environ if environ is None else environ
    return tuple(name for name in AMBIENT_IDENTITY_ENV if target.pop(name, None) is not None)


# The mailbox handle is the random tail of the session id. The signature takes
# no harness ON PURPOSE: harness is an envelope attribute, never part of an
# address, and no code path may recover it from a handle string. A
# harness-prefixed address (`claude-<short8>`) is a
# retired form that is NOT accepted anywhere - a caller still producing one is a
# bug to fix at the source, so resolution refuses it by name rather than quietly
# translating it.
#
# This function is the Python source of the generated string: discovery,
# registration, receipts, send, and drain all call it. The Rust lifecycle client
# carries a parity-tested mirror because it cannot import Python. If those two
# rules differ, a durable send can address one handle while its recipient drains
# another and silently strand on the bus.
def session_identity_key(session_id: str) -> str:
    """Normalize one session id for identity comparison across stores.

    UUID-family ids are case-insensitive. OpenCode's ``ses_`` ids are not.
    """
    return session_id if session_id.startswith("ses_") else session_id.lower()


def canonical_handle(session_id: str) -> str:
    """The mailbox address: the final eight characters of the session id."""
    return session_identity_key(session_id)[-8:]


def legacy_prefix_handle(session_id: str) -> str:
    """The retired first-eight address, for fail-closed lookup compatibility only."""
    return session_id[:8]


def claude_transport_short_id(session_id: str) -> str:
    """Claude's first-eight attach/job key, which is not a mailbox address."""
    return legacy_prefix_handle(session_id)


def session_handle_tier(token: str, session_id: str) -> Optional[int]:
    """Return full/canonical/legacy match tier (0/1/2), or ``None``.

    OpenCode identifiers are case-sensitive; UUID-family identifiers retain the
    historical case-insensitive paste behavior. Callers may prefer the explicit
    full-id tier, but must union canonical and legacy matches with every other
    short address category before deciding uniqueness.
    """
    token = (token or "").strip()
    if not token or not session_id:
        return None
    exact_case = session_id.startswith("ses_")

    def equal(value: str) -> bool:
        return token == value if exact_case else token.lower() == value.lower()
    for tier, value in enumerate(
        (session_id, canonical_handle(session_id), legacy_prefix_handle(session_id))
    ):
        if equal(value):
            return tier
    return None


# The retired harness-prefixed address. Kept ONLY so the send path can recognize
# one and refuse it with a message naming the fix, and so `fno doctor` can still
# report mail queued to one before the flip as the dead letter it is. Never an
# accepted address, never generated.
#
# Built from the harness map rather than a literal list: a hardcoded copy silently
# stops covering a harness the moment one is added, which is the same drift that
# produced the two-conventions mess this address change exists to end.
def _legacy_handle_re() -> "re.Pattern[str]":
    from fno.agents.harness_map import known_harnesses

    return re.compile(rf"^(?:{'|'.join(known_harnesses())})-[0-9a-fA-F]{{6,}}$")


LEGACY_HANDLE_RE = _legacy_handle_re()


def sync_harness_aliases(data: dict, legacy_session_keys: Mapping[str, str]) -> dict:
    """Two-way sync of ``harness_session_id`` with a store's legacy per-harness
    session-id key. The ONE source of the sync rule (x-ec59): the target manifest
    shim (``schemas/target.py``) and the agent-registry row coercion both call it,
    so canonical<->legacy resolution can never drift between the two.

    ``legacy_session_keys`` maps a harness name to that store's legacy session-id
    field, because the stores disagree on the claude key: the manifest uses
    ``claude_session_id``, the registry ``claude_session_uuid``.

    Rule (canonical wins): when ``harness_session_id`` is set it is authoritative
    and syncs the matching legacy key (a stale/conflicting legacy value is
    overwritten, never leaked); otherwise the first present non-null legacy value
    back-fills ``harness_session_id``. Mutates and returns ``data``. The harness
    <-> provider alias is store-specific and stays with each caller.
    """
    if not isinstance(data, dict):
        return data
    harness = str(data.get("harness") or "").lower()
    if data.get("harness_session_id"):
        legacy_key = legacy_session_keys.get(harness)
        if legacy_key:
            data[legacy_key] = data["harness_session_id"]
    else:
        # Adopt from THIS harness's own legacy key when the harness is known, so a
        # row carrying a stale legacy id of a DIFFERENT harness can't cross-
        # contaminate. Only a genuinely unknown/absent harness scans all keys (the
        # pre-migration row whose harness has not yet been resolved).
        if harness in legacy_session_keys:
            candidate_keys = [legacy_session_keys[harness]]
        else:
            candidate_keys = list(legacy_session_keys.values())
        for legacy_key in candidate_keys:
            value = data.get(legacy_key)
            if value and str(value).strip() and str(value).strip().lower() != "null":
                data["harness_session_id"] = value
                break
    return data


@dataclass(frozen=True)
class HarnessIdentity:
    """The resolved session id and its harness, or two ``None`` values."""

    session_id: Optional[str]
    harness: Optional[str]


def resolve_harness_identity(
    env: Optional[Mapping[str, str]] = None,
) -> HarnessIdentity:
    """Resolve the first nonblank ambient harness marker by shared precedence.

    Returns the precedence winner. This is the raw precedence primitive: it is
    correct when exactly one harness family is present (the dominant case), and
    callers that STAMP identity onto a durable record must instead use
    :func:`resolve_owned_identity`, which refuses to guess when two harness
    families disagree (an inherited marker must not be laundered into ownership).
    """
    environ = os.environ if env is None else env
    for marker, harness in HARNESS_SESSION_MARKERS:
        session_id = (environ.get(marker) or "").strip()
        if session_id:
            return HarnessIdentity(session_id=session_id, harness=harness)
    return HarnessIdentity(session_id=None, harness=None)


def present_harness_markers(
    env: Optional[Mapping[str, str]] = None,
) -> tuple[tuple[str, str, str], ...]:
    """Every nonblank ambient harness marker, in shared precedence order.

    Returns ``(marker, harness, value)`` triples. Two markers mapping to
    DIFFERENT harnesses is the ambiguous shape :func:`resolve_owned_identity`
    exists to resolve without guessing; the order here is the single source of
    what "precedence" means across every caller.
    """
    environ = os.environ if env is None else env
    return tuple(
        (marker, harness, value)
        for marker, harness in HARNESS_SESSION_MARKERS
        if (value := (environ.get(marker) or "").strip())
    )


@dataclass(frozen=True)
class OwnedHarnessIdentity:
    """The harness identity this process can PROVE it owns, plus diagnostics.

    ``disposition`` is one of:

    * ``single``   - exactly one harness family present; byte-identical to
                     :func:`resolve_harness_identity` (the dominant case).
    * ``proven``   - two families disagreed and exactly one was proven ours.
    * ``fallback`` - two families disagreed, none was proven, but collision
                     rejected every other family, leaving one survivor.
    * ``ambiguous``- two families disagreed and none was provably ours;
                     ``session_id``/``harness`` are ``None`` (degrade, do not
                     guess by precedence).
    * ``empty``    - no marker present.

    ``markers_present`` and ``rejected`` carry what the resolver saw, so an
    ambiguous resolve can be reconstructed from the event record alone.
    """

    session_id: Optional[str]
    harness: Optional[str]
    markers_present: tuple[tuple[str, str], ...] = ()
    disposition: str = "empty"
    rejected: tuple[dict[str, str], ...] = field(default_factory=tuple)


def resolve_owned_identity(
    env: Optional[Mapping[str, str]] = None,
    *,
    prove: Optional[Callable[[str, str], bool]] = None,
    collide: Optional[Callable[[str, str], Optional[str]]] = None,
) -> OwnedHarnessIdentity:
    """Resolve the harness identity this process can PROVE it owns.

    Unlike :func:`resolve_harness_identity`, when two harness families are both
    present this never silently picks the higher-precedence one: an inherited
    marker (a codex worker's ``CODEX_THREAD_ID`` lingering in a claude child's
    environment) would otherwise be laundered into ownership. Instead it prefers
    a marker that is provably this process's, and refuses to guess when it
    cannot prove one.

    ``prove(harness, session_id) -> bool`` attests a marker is this process's
    own (a fresh transcript exists for it); default ``None`` attests nothing.
    ``collide(harness, session_id) -> owner | None`` reports when a live
    registry row already owns an id (two live sessions cannot share one);
    default ``None`` skips the check. Both default off so this module stays
    dependency-free; the consuming verb injects the real prover and collider.

    A rejected or unprovable disagreement degrades to ``None`` rather than
    guessing: ``harness_session_id`` is nullable, and a record that names no
    identity is honest where one that names a stranger's is the bug.
    """
    environ = os.environ if env is None else env
    markers = present_harness_markers(environ)
    present = tuple((marker, harness) for marker, harness, _ in markers)
    if not markers:
        return OwnedHarnessIdentity(None, None, (), "empty")
    if len({harness for _, harness, _ in markers}) == 1:
        marker, harness, value = markers[0]
        return OwnedHarnessIdentity(value, harness, present, "single")

    rejected: list[dict[str, str]] = []
    proven: list[tuple[str, str, str]] = []
    survivors: list[tuple[str, str, str]] = []
    for marker, harness, value in markers:
        # PROOF before COLLISION. A process-tree-proven marker IS this session,
        # so a live registry row holding it is the session's own row, not a
        # foreign owner: collision-checking it first would reject the session's
        # own marker when it has registered itself, leaving an unproven foreign
        # marker as the sole fallback. Proven markers skip the collision check
        # entirely; collision applies only to markers proof could not attest
        # (i.e. foreign ones).
        if prove is not None and prove(harness, value):
            proven.append((marker, harness, value))
            continue
        owner = collide(harness, value) if collide is not None else None
        if owner:
            rejected.append(
                {
                    "marker": marker,
                    "harness": harness,
                    "session_id": value,
                    "reason": "owned_by_live_row",
                    "owner": owner,
                }
            )
        else:
            survivors.append((marker, harness, value))
    if len(proven) == 1:
        _marker, harness, value = proven[0]
        return OwnedHarnessIdentity(value, harness, present, "proven", tuple(rejected))
    if not proven and len(survivors) == 1:
        # No proof available, but collision eliminated every other family: the
        # sole survivor is the only candidate left. A colliding id is provably
        # another live row's, so rejecting it leaves the remaining family as ours
        # without guessing by precedence. Two+ unprovable survivors is a genuine
        # unknown and degrades below.
        _marker, harness, value = survivors[0]
        return OwnedHarnessIdentity(value, harness, present, "fallback", tuple(rejected))
    return OwnedHarnessIdentity(None, None, present, "ambiguous", tuple(rejected))



def current_session_id(env: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """Return the canonical ambient session id, with legacy Claude fallback."""
    environ = os.environ if env is None else env
    identity = resolve_harness_identity(environ)
    if identity.session_id:
        return identity.session_id
    for marker, _ in LEGACY_HARNESS_SESSION_MARKERS:
        session_id = (environ.get(marker) or "").strip()
        if session_id:
            return session_id
    return None


def current_session_ids(env: Optional[Mapping[str, str]] = None) -> set[str]:
    """Return every nonblank canonical or legacy ambient session id."""
    environ = os.environ if env is None else env
    return {
        session_id
        for marker, _ in (*HARNESS_SESSION_MARKERS, *LEGACY_HARNESS_SESSION_MARKERS)
        if (session_id := (environ.get(marker) or "").strip())
    }
