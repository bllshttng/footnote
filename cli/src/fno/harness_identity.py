"""Shared ambient harness session identity resolution."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from typing import Callable, Mapping, Optional

from fno.harness_names import KNOWN_HARNESSES


# --- FNO_AGENT_HARNESS env resolution (with pre-cutover compat window) -------
# Spawn injects FNO_AGENT_HARNESS (the CLI binary). A worker spawned before the
# FNO_AGENT_PROVIDER -> FNO_AGENT_HARNESS rename carries the old name in an
# process can rewrite, so the read side accepts it for one release and warns.
# The warning fires at most once per process so a dispatch that resolves context
# and then whoami does not print it twice. The window is time-boxed: it is
# removed when no in-flight worker can carry the old variable.
_HARNESS_ENV_WARNED = False


def harness_from_env(env: "Mapping[str, str]", *, warn: bool = True) -> "Optional[str]":
    """Resolve the ambient harness from the process environment.

    Prefers ``FNO_AGENT_HARNESS``; falls back to the pre-cutover
    ``FNO_AGENT_PROVIDER``. Returns the non-empty value or ``None``. On fallback
    writes one stderr line naming the current variable (``warn=True``, at most
    once per process).
    """
    global _HARNESS_ENV_WARNED
    val = env.get("FNO_AGENT_HARNESS")
    if val:
        return val
    legacy = env.get("FNO_AGENT_PROVIDER")
    if legacy and warn and not _HARNESS_ENV_WARNED:
        import sys
        sys.stderr.write(
            "FNO_AGENT_PROVIDER is the pre-cutover name for FNO_AGENT_HARNESS; "
            "this worker predates the rename. New spawns set FNO_AGENT_HARNESS.\n"
        )
        sys.stderr.flush()
        _HARNESS_ENV_WARNED = True
    return legacy or None


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


# Markers a harness binary writes about ITSELF at startup, rather than session
# ids that name a specific run. The claude binary sets CLAUDECODE=1.
#
# NOT an identity resolver input, and the reason is worth stating because the
# opposite reads as obvious. A shell that never ran claude cannot produce
# CLAUDECODE, which sounds like proof of a claude self. It is not: the variable
# survives a fork, so a codex session started from a shell that HAD run claude
# inherits it. Treating it as proof there contradicts that session's own
# CODEX_THREAD_ID and degrades a cleanly resolving codex session to ambiguous.
# The poisoned-claude case and the codex-under-claude case carry the identical
# name set, so environment alone cannot separate them; only the process-tree
# walk in claims/session_pid.py can, and it is the sole prover
# (resolve_self_identity).
#
# What the table IS for: the setup doctor's remedy line reads it, so that
# mapping has one home instead of a private literal, and AMBIENT_IDENTITY_ENV
# below scrubs it so a spawned child does not inherit its parent's marker and
# send `fno doctor` to the wrong settings file. The doctor's own
# CLAUDE_CONFIG_DIR / CODEX_HOME entries stay local to it: those name where
# config lives, not which binary is running.
SELF_SET_HARNESS_MARKERS: tuple[tuple[str, str], ...] = (
    ("CLAUDECODE", "claude"),
)


# Ambient session-identity env names that a HERMETIC run must not see. This is
# deliberately WIDER than the two tuples above: those define what
# resolve_harness_identity() consults, in precedence order, while this defines
# what has to be absent for a test or a preflight leg to behave like a fresh
# checkout. Several modules read a session marker directly rather than through
# the resolver - carveout/core.py and done/cli.py read
# CLAUDECODE_SESSION_ID, adapters/hermes.py additionally treats
# HERMES_SESSION_ID as proof of an in-session run - so scrubbing only the
# resolver's tuples leaves those paths resolving the live session.
#
# Adding a name here changes ONLY what gets scrubbed, never what resolves, which
# is why the direct-read markers belong here and not in the tuples: promoting one
# to HARNESS_SESSION_MARKERS would silently change resolution precedence and the
# harness a claim is tagged with.
#
# TARGET_SESSION_ID is fno plumbing, not a harness marker: the run id a driver
# pre-assigns and init adopts verbatim as session id and claim owner. It belongs
# in the scrub anyway, for the same reason as the markers: carveout/core.py
# matches live claims through it, so a suite or spawned child that inherits it
# resolves (or stamps) the live run's identity instead of its own.
#
# The five CODEX_* names below are the rest of a live codex session's identity
# env, measured 2026-08-21 (node x-b57a): a poisoned claude session carrying
# them could not self-compact until all seven codex names were stripped, and
# the list held only the two the resolver consults. They are identity-only -
# CODEX_HOME is routing/config and deliberately NOT here - and stay out of
# HARNESS_SESSION_MARKERS for the reason two paragraphs up.
_EXTRA_IDENTITY_NAMES: tuple[tuple[str, str], ...] = (
    ("CLAUDECODE_SESSION_ID", "claude"),
    ("HERMES_SESSION_ID", "hermes"),
    ("TARGET_SESSION_ID", "fno"),
    ("CODEX_CI", "codex"),
    ("CODEX_INTERNAL_ORIGINATOR_OVERRIDE", "codex"),
    ("CODEX_SHELL", "codex"),
    ("CODEX_COMPANION_SESSION_ID", "codex"),
    ("CODEX_COMPANION_TRANSCRIPT_PATH", "codex"),
)

AMBIENT_IDENTITY_ENV: tuple[str, ...] = (
    *(marker for marker, _ in HARNESS_SESSION_MARKERS),
    *(marker for marker, _ in LEGACY_HARNESS_SESSION_MARKERS),
    # The self-set markers scrub too. CLAUDECODE survives a fork, so a codex
    # child spawned by a claude parent inherits it and carries claude's marker
    # for its whole life, which sends `fno doctor` to claude's settings file for
    # a codex session. Each harness re-mints its own at startup, so the scrub is
    # lossless. It changes what a CHILD sees; nothing here resolves identity
    # from it (see SELF_SET_HARNESS_MARKERS above).
    *(marker for marker, _ in SELF_SET_HARNESS_MARKERS),
    *(name for name, _ in _EXTRA_IDENTITY_NAMES),
)

# Name -> harness family for every scrubbed identity name, so a refusal can
# name the strip set for one whole foreign family: the resolver markers alone
# are two of codex's seven, and a strip line built from two of seven did
# nothing (x-b57a). Family here labels which harness a name belongs to, not
# which harness resolves - TARGET_SESSION_ID belongs to fno itself.
AMBIENT_IDENTITY_FAMILY: dict[str, str] = {
    **{marker: family for marker, family in HARNESS_SESSION_MARKERS},
    **{marker: family for marker, family in LEGACY_HARNESS_SESSION_MARKERS},
    **dict(SELF_SET_HARNESS_MARKERS),
    **dict(_EXTRA_IDENTITY_NAMES),
}

_RESOLVER_IDENTITY_NAMES: frozenset[str] = frozenset(
    marker for marker, _ in (*HARNESS_SESSION_MARKERS, *LEGACY_HARNESS_SESSION_MARKERS)
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


def ambient_identity_env_unset_args() -> list[str]:
    """``env -u`` flag pairs that strip every ambient identity name, for a child
    launched through an ``env`` argv (the pane substrate).

    The dict substrate (bg / headless) uses :func:`scrub_ambient_identity`;
    both read :data:`AMBIENT_IDENTITY_ENV`, so the two shapes cannot drift on
    which names count as identity. A spawned child inherits its parent's ROUTE
    but never its parent's IDENTITY - an ambient marker riding through is how a
    claude reviewer spawned from a codex parent comes to stamp the parent's
    session - and each harness re-mints its own, so the scrub is
    lossless.
    """
    flags: list[str] = []
    for _name in AMBIENT_IDENTITY_ENV:
        flags += ["-u", _name]
    return flags


# Families a strip suggestion never names, whatever session is asking.
#
#   fno     - TARGET_SESSION_ID is this run's own claim linkage. The resolver
#             never consults it, so stripping it cannot cure an ambiguity, and
#             dropping it would mis-key the retried command's claims.
#   hermes  - HERMES_SESSION_ID is a fail-closed guard, not a session identity:
#             HermesCliAdapter reads it as "we are inside a CLI agent session,
#             shell spawn is FORBIDDEN" (adapters/hermes.py). It is never a
#             keep_family, so without this entry every remedy line offered to
#             delete it, which turns that refusal into a real spawn.
_NEVER_STRIPPED_FAMILIES: frozenset[str] = frozenset({"fno", "hermes"})


def ambient_identity_strip_flags(
    keep_family: str, env: Optional[Mapping[str, str]] = None
) -> list[str]:
    """``env -u`` flag pairs for identity names PRESENT in ``env`` that belong
    to a foreign HARNESS family (every family except ``keep_family`` and fno
    plumbing). A foreign-family name with the same value as the resolved
    keep-family session id is a duplicate naming this session, not foreign
    lineage, and is omitted.

    The one-line self-rescue for a poisoned session: it cannot prove which
    harness it is (that is the ambiguity), but the operator reading the
    refusal knows, and stripping the foreign family restores self-resolution
    while keeping the session's own markers. Names not in
    :data:`AMBIENT_IDENTITY_FAMILY` are never stripped here, and neither is any
    family in :data:`_NEVER_STRIPPED_FAMILIES`.
    """
    environ = os.environ if env is None else env
    keep_session_id: Optional[str] = None
    for marker, family in HARNESS_SESSION_MARKERS:
        if family != keep_family:
            continue
        value = (environ.get(marker) or "").strip()
        if value:
            keep_session_id = value
            break
    if keep_session_id is None:
        for marker, family in LEGACY_HARNESS_SESSION_MARKERS:
            if family != keep_family:
                continue
            value = (environ.get(marker) or "").strip()
            if value:
                keep_session_id = value
                break

    flags: list[str] = []
    for name in AMBIENT_IDENTITY_ENV:
        candidate_family = AMBIENT_IDENTITY_FAMILY.get(name)
        # An unmapped name is skipped, which is what the docstring has always
        # said and what the code did not do: `.get(name) in (...)` compares None
        # against the keep list, finds no match, and strips it. So a name added
        # to AMBIENT_IDENTITY_ENV without a family entry landed in the remedy
        # line by default. Fail closed instead - never suggest deleting a
        # variable nobody has classified.
        if (
            candidate_family is None
            or candidate_family == keep_family
            or candidate_family in _NEVER_STRIPPED_FAMILIES
        ):
            continue
        value = (environ.get(name) or "").strip()
        # Resolver markers remain removable: equal values across families still
        # leave the resolver ambiguous, so the remedy must clear that marker.
        if not value or (
            value == keep_session_id and name not in _RESOLVER_IDENTITY_NAMES
        ):
            continue
        flags += ["-u", name]
    return flags


# A mail address is EITHER the harness's own short-id (the first eight of the
# session id) OR the full session id. Nothing else. If a short-id is discovered
# to have duplicates, resolution fails closed and asks for the full id rather
# than guessing. Codex session ids are time-prefixed, so their first-8 collides
# across sessions started in the same window; codex addressing is therefore
# often the full id in practice, and the send path says so on ambiguity.
#
# Harness is an envelope attribute, never part of an address: no code path may
# recover it from a handle string, and a harness-prefixed address
# (`claude-<short8>`) is a retired form that is NOT accepted anywhere. A caller
# still producing one is a bug to fix at the source, so resolution refuses it by
# name rather than quietly translating it.
#
# canonical_handle is the Python source of the generated address: discovery,
# registration, receipts, send, and drain all call it. The Rust lifecycle client
# (crates/fno-agents/src/identity.rs) carries a parity-tested mirror because it
# cannot import Python. If those two rules differ, a durable send can address
# one handle while its recipient drains another and silently strand on the bus.
#
# legacy_suffix_handle (the last-8) is NOT an address. It survives only as a
# read-only lookup so mail addressed before the 2026-08-10 flip back to first-8
# (when last-8 was the address) still drains while the bus turns over. It plays
# the same read-only-compatibility role first-8 played across the earlier
# (2026-07-30) cutover, and is removed once no in-flight handle is a last-8.
def session_identity_key(session_id: str) -> str:
    """Normalize one session id for identity comparison across stores.

    UUID-family ids are case-insensitive. OpenCode's ``ses_`` ids are not.
    """
    return session_id if session_id.startswith("ses_") else session_id.lower()


def canonical_handle(session_id: str) -> str:
    """The mailbox address: the first eight characters of the session id.

    This is the harness's own short-id. A mail address is this short-id OR the
    full session id; on a short-id collision, resolution fails closed and asks
    for the full id. (Codex ids are time-prefixed, so their first-8 collides
    across same-window sessions; codex addressing is often the full id.)
    """
    return session_identity_key(session_id)[:8]


def legacy_suffix_handle(session_id: str) -> str:
    """The retired last-eight address, read-only lookup compatibility only.

    Pre-2026-08-10 handles were addressed by the last eight (e.g. ``08e8c104``);
    this lets those in-flight messages drain while the bus turns over. Never
    generated, never an accepted address for new mail.
    """
    return session_identity_key(session_id)[-8:]


def claude_transport_short_id(session_id: str) -> str:
    """Claude's first-eight attach/job key.

    Equal in value to :func:`canonical_handle` since the 2026-08-10 flip made
    the harness's own short-id the mailbox address; kept as the named seam for
    call sites that conceptually want claude's native job key, not the address.
    """
    return canonical_handle(session_id)


def session_handle_tier(token: str, session_id: str) -> Optional[int]:
    """Return full/canonical/legacy match tier (0/1/2), or ``None``.

    Tier 1 is the canonical first-eight address; tier 2 is the retired
    last-eight (read-only transition lookup). OpenCode identifiers are
    case-sensitive; UUID-family identifiers retain the historical
    case-insensitive paste behavior. Callers may prefer the explicit full-id
    tier, but must union canonical and legacy matches with every other short
    address category before deciding uniqueness.
    """
    token = (token or "").strip()
    if not token or not session_id:
        return None
    exact_case = session_id.startswith("ses_")

    def equal(value: str) -> bool:
        return token == value if exact_case else token.lower() == value.lower()
    for tier, value in enumerate(
        (session_id, canonical_handle(session_id), legacy_suffix_handle(session_id))
    ):
        if equal(value):
            return tier
    return None


# A UUID head-8 (`01a025f8`): exactly eight hex characters and nothing else.
# Deliberately NOT anchored to a harness -- the caller supplies the harness,
# because the same eight characters are safe under v4 and degenerate under v7.
_HEAD8_RE = re.compile(r"^[0-9a-fA-F]{8}$")

#: The one sentence every surface prints for a refused codex short address. One
#: string so `mail send`, `mail reply`, and the help text cannot drift.
CODEX_SHORT_ADDRESS_RULE = (
    "use the full session_id or pane; codex head-8 is a 65.536-second "
    "timestamp bucket"
)


def is_unsafe_short_address(token: str, harness: Optional[str]) -> bool:
    """Whether ``token`` is a head-8 slice that cannot safely address ``harness``.

    True only for a bare eight-hex token aimed at codex. Codex session ids are
    UUIDv7, whose first 48 bits are a truncated millisecond timestamp, so the
    first eight hex characters name a ~65.536-second clock bucket rather than 32
    random bits. Every codex session started inside one bucket shares them, which
    is exactly what a fleet does: three sessions landed on ``01a025f8`` in one
    night, and ``fno agents mail reply`` then refused outright with no
    disambiguation flag, leaving a threaded reply impossible.

    False for claude, whose ids are UUIDv4 and whose head-8 is 32 random bits.
    The slice is a display aid on both; it is only an ADDRESS on one.

    Uniqueness right now is not a defence. A head-8 that resolves uniquely at
    this instant collides the moment a sibling spawns in the same minute, and the
    failure is silent until it is a hard refusal. So this refuses on SHAPE, which
    is the only property that does not change under you.
    """
    if not token or harness != "codex":
        return False
    return bool(_HEAD8_RE.match(token.strip()))


# The retired harness-prefixed address. Kept ONLY so the send path can recognize
# one and refuse it with a message naming the fix, and so `fno doctor` can still
# report mail queued to one before the flip as the dead letter it is. Never an
# accepted address, never generated.
#
# Built from the harness map rather than a literal list: a hardcoded copy silently
# stops covering a harness the moment one is added, which is the same drift that
# produced the two-conventions mess this address change exists to end.
def _legacy_handle_re() -> "re.Pattern[str]":
    return re.compile(rf"^(?:{'|'.join(KNOWN_HARNESSES)})-[0-9a-fA-F]{{6,}}$")


# Built eagerly from the canonical harness-name list (fno.harness_names) rather
# than the capability table: this module is platform-layer and must not reach
# into the runtime for the name set (x-cec8). The name list is the source of
# truth and the capability table asserts against it, so a new harness is covered
# here the moment it lands there - the same anti-drift property the old
# derivation (names read FROM fno.agents.harness_map) had, with the dependency
# direction inverted so no fno.agents import is needed at all.
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
        if harness == "unknown":
            # Explicit unknown: the resolver could not prove a harness. Do NOT
            # backfill from a legacy marker - that would resurrect a foreign
            # inherited id (e.g. an inherited CODEX_THREAD_ID recorded additively
            # for diagnosis) as this session's identity, the exact leak this field
            # exists to prevent. Distinct from an absent harness (pre-migration),
            # which still scans all keys below.
            return data
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
    """Resolve the ambient harness identity, or refuse on mixed families.

    Precedence applies WITHIN one harness family (codex thread id over the
    legacy codex session id). Across families there is no precedence: markers
    from two harness families mean one is foreign and inherited - a claude
    child carrying its codex parent's ``CODEX_THREAD_ID`` - and the answer is
    ``(None, None)`` so no caller can launder the foreign marker into this
    session's identity. This is the same disposition
    :func:`fno.dispatch_flags.infer_invoking_harness` gives the same
    environment; the two must not diverge (x-b57a).

    Byte-identical to :func:`resolve_owned_identity`'s ``single`` disposition
    whenever exactly one family is present (the dominant case).
    """
    environ = os.environ if env is None else env
    families: list[str] = []
    winner: Optional[HarnessIdentity] = None
    for marker, harness in HARNESS_SESSION_MARKERS:
        session_id = (environ.get(marker) or "").strip()
        if session_id:
            if harness not in families:
                families.append(harness)
            if winner is None:
                winner = HarnessIdentity(session_id=session_id, harness=harness)
    if len(families) > 1:
        return HarnessIdentity(session_id=None, harness=None)
    return winner if winner is not None else HarnessIdentity(session_id=None, harness=None)


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

    * ``single``   - the resolved family was the only one present; byte-identical
                     to :func:`resolve_harness_identity` for the dominant case.
    * ``proven``   - more than one family was present and exactly one was proven
                     ours (the others foreign or owned by another live row).
    * ``ambiguous``- no proven marker could be resolved to a specific id. The
                     proven harness may still be carried (id unknown); when not
                     even the harness is proven, both are ``None``. Never guesses
                     by precedence.
    * ``empty``    - no marker present.

    ``markers_present`` carries every marker seen (with its value) and
    ``rejected`` the ids a live row already owns, so an ambiguous resolve can be
    reconstructed from the event record alone.
    """

    session_id: Optional[str]
    harness: Optional[str]
    markers_present: tuple[tuple[str, str, str], ...] = ()
    disposition: str = "empty"
    rejected: tuple[dict[str, str], ...] = field(default_factory=tuple)


def resolve_owned_identity(
    env: Optional[Mapping[str, str]] = None,
    *,
    prove: Optional[Callable[[str, str], Optional[bool]]] = None,
    collide: Optional[Callable[[str, str], Optional[str]]] = None,
) -> OwnedHarnessIdentity:
    """Resolve the harness identity this process can PROVE it owns.

    Unlike :func:`resolve_harness_identity`, when more than one harness family
    is present this never silently picks the higher-precedence one: an inherited
    marker (a codex worker's ``CODEX_THREAD_ID`` lingering in a claude child's
    environment) would otherwise be laundered into ownership. It prefers a
    marker that is provably this process's, and refuses to guess when it cannot
    resolve one.

    ``prove(harness, session_id) -> True | False | None`` attests a marker:
    ``True`` is this process's (a process-tree match), ``False`` is provably
    NOT this process's (the tree resolves to a different harness), ``None`` is
    "cannot tell" (no harness ancestor, e.g. a CI runner). Default ``None``
    (callback absent) means "cannot tell" for every marker.
    ``collide(harness, session_id) -> owner | None`` reports when a live
    registry row already owns an id (two live sessions cannot share one);
    default ``None`` skips the check. Both default off so this module stays
    dependency-free; the consuming verb injects the real prover and collider.

    Resolution order: a uniquely proven family wins; collision rejects ids a
    live row owns (recorded regardless of proof, so the owner is named); a
    marker the prover actively contradicts is excluded; among the rest, the sole
    surviving family wins or the result degrades to ``None``.
    """
    environ = os.environ if env is None else env
    markers = present_harness_markers(environ)
    present = tuple(markers)
    if not markers:
        return OwnedHarnessIdentity(None, None, (), "empty")
    distinct = {harness for _, harness, _ in markers}

    rejected: list[dict[str, str]] = []
    proven: list[tuple[str, str, str]] = []
    contradicted = False
    unresolved: list[tuple[str, str, str]] = []
    for marker, harness, value in markers:
        verdict = prove(harness, value) if prove is not None else None
        if verdict is True:
            # PROOF is self: a live row holding this id is the session's own row,
            # not a foreign owner, so a proven marker is never decided by collision.
            proven.append((marker, harness, value))
            continue
        owner = collide(harness, value) if collide is not None else None
        if owner:
            # Observability only: records the owner for the event. Collision never
            # stamps a fallback here - without proof it can reject the session's
            # own row and leave an inherited foreign marker as the winner.
            rejected.append(
                {
                    "marker": marker,
                    "harness": harness,
                    "session_id": value,
                    "reason": "owned_by_live_row",
                    "owner": owner,
                }
            )
            continue
        if verdict is False:
            # The prover actively says this marker is NOT this process's.
            contradicted = True
            continue
        unresolved.append((marker, harness, value))

    rejected_t = tuple(rejected)
    # A uniquely proven family wins (proof is self). The prover attests one
    # process-tree harness, so proven spans 0 or 1 family.
    proven_by_family: dict[str, set[str]] = {}
    for _marker, harness, value in proven:
        proven_by_family.setdefault(harness, set()).add(value)
    if len(proven_by_family) == 1:
        family, ids = next(iter(proven_by_family.items()))
        if len(ids) == 1:
            _marker, _h, value = next(p for p in proven if p[1] == family)
            return OwnedHarnessIdentity(
                value, family, present, "single" if len(distinct) == 1 else "proven", rejected_t
            )
        # Proven family but multiple DISTINCT ids: proof is harness-level, not
        # id-level, so the specific id is unknown. Keep the proven harness (do
        # not mislabel a proven-codex session as claude) and null the id, rather
        # than pick by precedence and risk an inherited same-family stranger.
        return OwnedHarnessIdentity(None, family, present, "ambiguous", rejected_t)
    if len(proven_by_family) > 1:
        return OwnedHarnessIdentity(None, None, present, "ambiguous", rejected_t)

    # No marker proven.
    if len(distinct) == 1 and not contradicted and not rejected_t and unresolved:
        # SINGLE family, prover absent or silent, no collision and no
        # contradiction: byte-identical to resolve_harness_identity (AC2-HP, the
        # dominant case). A collision or an active contradiction makes the
        # remaining unproven marker suspect (a same-family sibling could be
        # foreign), so those degrade below rather than stamp by elimination.
        _marker, harness, value = unresolved[0]
        return OwnedHarnessIdentity(value, harness, present, "single", rejected_t)
    # Multi-family with no proof, or a single family the prover contradicted:
    # cannot resolve safely, so degrade rather than guess by precedence.
    return OwnedHarnessIdentity(None, None, present, "ambiguous", rejected_t)




# --- attester identity: bound to the emitting process -------------------------
#
# review_attestation.attester_session_id is the one field that varies with WHO
# emitted, and it used to be read straight from the emitting process's own env.
# One `CODEX_THREAD_ID=<other-session> bash emit-attestation.sh` on the command
# line then wrote the attestation under any session id, refreshing that
# session's stale verdict onto a head it never saw. The resolver below keeps
# the env read (it is the only signal for WHICH marker) but corroborates it
# against the process ancestry: an ancestor carrying a DIFFERENT value for the
# same marker is the override shape and raises.

_MAX_ANCESTRY_DEPTH = 25
# The argv token that names each marker's harness family: the process that can
# MINT a marker value is a process of that family, and its own name/argv
# carries the token (claude, codex, gemini, opencode).
_MARKER_FAMILY_TOKEN = {
    marker: family for marker, family in HARNESS_SESSION_MARKERS
}


class AttesterIdentityConflict(Exception):
    """An ancestor carries a DIFFERENT value for the winning session marker.

    The signature of an identity override on the command line: the harness
    process up the tree says one session and the emitting environment says
    another.
    """

    def __init__(self, marker: str, ancestor_value: str, env_value: str) -> None:
        self.marker = marker
        self.ancestor_value = ancestor_value
        self.env_value = env_value
        super().__init__(
            f"attester identity conflict on {marker}: process ancestry carries "
            f"{ancestor_value!r}, the emitting environment carries {env_value!r}"
        )


def _read_ancestor_marker(pid: int, marker: str) -> Optional[str]:
    """The value of MARKER in ancestor PID's environment, or None.

    None covers both "not carried" and "unreadable" because the two never
    change a caller decision: the conflict raise needs a readable DIFFERENT
    value, the process witness needs a readable EQUAL one, and everything
    else is env_only. Linux reads /proc exactly; darwin falls back to `ps
    eww`, whose readability is PARTIAL by measurement: it exposed a live
    harness chain's markers while showing nothing for other same-user
    processes. A darwin walk therefore corroborates what it can and records
    env_only for the rest - recorded, never gated on.
    """
    raw: Optional[bytes]
    try:
        with open(f"/proc/{pid}/environ", "rb") as fh:
            raw = fh.read()
    except OSError:
        raw = None
    if raw is not None:
        prefix = marker.encode() + b"="
        for entry in raw.split(b"\0"):
            if entry.startswith(prefix):
                return entry[len(prefix) :].decode("utf-8", "replace")
        return None
    try:
        import subprocess

        out = subprocess.run(
            ["ps", "eww", "-o", "command=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    text_prefix = marker + "="
    for token in out.stdout.split():
        if token.startswith(text_prefix):
            return token[len(text_prefix) :]
    return None


def _attester_witness(
    marker: str,
    session_id: str,
    chain: "list[Optional[str]]",
    carrier_is_family: "list[bool]",
) -> str:
    """The witness for a marker value from its ancestry (None = absent or
    unreadable; ``carrier_is_family`` says whether that ancestor is a process
    of the marker's own harness family, the only kind that can MINT the id).

    The NEAREST family carrier decides: agree -> ``process``; disagree ->
    :class:`AttesterIdentityConflict`. An any-ancestor veto (this function's
    earlier rule) wedges the daemon-carrier lane: a long-lived ancestor (tmux
    server, bg daemon) retains a PREVIOUS session's marker in its env, and
    vetoing on it refuses every emit from sessions under it, making the
    self-review obligation unsatisfiable there. The stale value above the
    session was never the minter of THIS id - the session process was, and it
    is the nearer carrier. The override shape stays caught: the shell between
    this process and the harness carries the override, but it is not a family
    process, so the walk continues to the harness, whose value disagrees and
    raises. No family carrier at all -> ``env_only``.
    """
    for value, family in zip(chain, carrier_is_family):
        if value is None or not family:
            continue
        if value != session_id:
            raise AttesterIdentityConflict(marker, value, session_id)
        return "process"
    return "env_only"


def resolve_attester_identity(
    env: Optional[Mapping[str, str]] = None,
) -> "tuple[str, str]":
    """The attester for an event emitted by THIS process: ``(session_id,
    witness)``.

    The session id is the winning marker from the env on the shared
    :data:`HARNESS_SESSION_MARKERS` precedence. The witness is ``process``
    when a readable ancestor carries the same value (the harness that minted
    the id is in this process's ancestry) and ``env_only`` when nothing
    corroborates it - a bare shell, dead ancestry, or an OS that does not
    expose ancestor environments. Witness is recorded, never gating: an
    env_only attestation from such a lane is still a real review.

    A mixed-family env (markers from two harnesses, one foreign and
    inherited) resolves empty rather than by precedence, matching
    :func:`resolve_harness_identity`'s refusal to launder. Raises
    :class:`AttesterIdentityConflict` when a readable ancestor carries a
    DIFFERENT value for the same marker - the command-line override shape.
    """
    environ = os.environ if env is None else env
    marker_name: Optional[str] = None
    session_id = ""
    families: list[str] = []
    for marker, family in HARNESS_SESSION_MARKERS:
        value = (environ.get(marker) or "").strip()
        if value:
            if family not in families:
                families.append(family)
            if marker_name is None:
                marker_name, session_id = marker, value
    if len(families) > 1 or marker_name is None:
        return ("", "env_only")
    import psutil

    family_token = _MARKER_FAMILY_TOKEN[marker_name]
    chain: list[Optional[str]] = []
    carrier_is_family: list[bool] = []
    try:
        proc: "Optional[psutil.Process]" = psutil.Process(os.getppid())
    except (psutil.NoSuchProcess, psutil.AccessDenied, ValueError):
        return (session_id, "env_only")
    depth = 0
    while proc is not None and depth < _MAX_ANCESTRY_DEPTH:
        chain.append(_read_ancestor_marker(proc.pid, marker_name))
        # Family membership from the EXECUTABLE name alone (cmdline[0], which
        # preserves an argv0 rename), never the whole argv: an `env
        # CLAUDE_CODE_SESSION_ID=<forged>` wrapper or a shell assignment
        # carries the MARKER SPELLING in its argv, and the marker contains
        # the family token - matching the full argv would crown that wrapper
        # the deciding family carrier and stamp the forgery `process`
        # (reproduced live in review round 2). Unreadable -> not family,
        # which can only degrade the witness, never forge one.
        try:
            argv: list = getattr(proc, "cmdline", lambda: [])() or []
            exe_name = (argv[0] if argv else proc.name()).lower()
        except psutil.Error:
            exe_name = ""
        carrier_is_family.append(family_token in exe_name)
        if carrier_is_family[-1]:
            # The nearest family carrier decides, READABLE OR NOT: stopping
            # here on an unreadable one yields env_only (no corroboration),
            # while walking past it would let a farther STALE family carrier
            # (a daemon with a previous session's marker) raise and wedge the
            # lane. The break also stops paying one `ps` per ancestor.
            break
        try:
            proc = proc.parent()
        except psutil.Error:
            break
        depth += 1
    return (
        session_id,
        _attester_witness(marker_name, session_id, chain, carrier_is_family),
    )


def current_session_id(env: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """Return the canonical ambient session id, with legacy Claude fallback.

    The legacy fallback serves an env with NO canonical marker (a pre-migration
    claude session), never one whose canonical markers disagree: falling
    through there would pick the claude-legacy id in a mixed env, the same
    cross-family launder :func:`resolve_harness_identity` just refused.
    """
    environ = os.environ if env is None else env
    identity = resolve_harness_identity(environ)
    if identity.session_id:
        return identity.session_id
    if present_harness_markers(environ):
        return None
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
