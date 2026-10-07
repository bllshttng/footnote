"""High-level claim verbs.

Seven operations on top of io + staleness:

    acquire_claim     - try to take a claim; idempotent re-acquire,
                        stale recovery, live-other detection.
    release_claim     - drop a claim we own.
    refresh_claim     - extend TTL on a claim we own (no-op for PID-liveness).
    claim_status      - inspect a single key.
    list_claims       - enumerate all live (and optionally stale) claims.
    force_release_claim - administrative override, always succeeds.
    reap_dead_claims  - archive every provably-dead claim (GC).

Rust owns claim persistence and audit receipts in graph.db. Python is a transport client.
"""

from __future__ import annotations

import os
from subprocess import PIPE as _SUBPROCESS_PIPE
from subprocess import Popen as _SubprocessPopen
from pathlib import Path
from typing import Any, Callable, NamedTuple, Optional

from urllib.parse import quote as _url_quote

from .io import (ClaimAlreadyHeld, ClaimCorrupted, ClaimGoneAway, claim_path, encode_key, dedup_claims_roots, global_claims_root, read_claim_file)
from .verdict import (
    ClaimSweepOmission,
    ClaimVerdictError,
    ClaimVerdictUnavailable,
    claim_verdicts,
)
from .types import (MAX_ENCODED_FILENAME_BYTES, MAX_KEY_LENGTH, MAX_TTL_MS, MIN_TTL_MS, Claim, ClaimState)


class ClaimHeldByOther(Exception):
    """A live claim is held by a different holder."""

    def __init__(self, holder: str, pid: Optional[int], host: str, key: str) -> None:
        self.holder = holder
        self.pid = pid
        self.host = host
        self.key = key
        super().__init__(f"claim {key!r} held by {holder} (pid={pid}, host={host})")


class HolderMismatch(Exception):
    """release/refresh called with a different holder than the existing claim."""

    def __init__(self, expected: str, actual: str, key: str) -> None:
        self.expected = expected
        self.actual = actual
        self.key = key
        super().__init__(f"claim {key!r}: holder mismatch (expected {expected!r}, got {actual!r})")


class ClaimValidationError(ValueError):
    """Inputs to a verb failed validation (ttl out of range, key too long, ...)."""


class ClaimContended(Exception):
    """acquire_claim/refresh_claim gave up after the native retry budget
    contention retries on the same key's recovery mutex.

    A distinct type from ClaimHeldByOther (a live claim is held by someone
    else) even though a caller usually treats both the same way ("can't have
    this one right now, retry later") - the recovery mutex being contended
    says nothing about who, if anyone, ends up holding the claim. Callers
    that only catch this narrow type cannot accidentally reclassify an
    unrelated RuntimeError raised deeper in the call stack (pydantic,
    resolve_self_identity, serialize_claim, ...) as contention.

    Callers of acquire_claim/refresh_claim should catch this ALONGSIDE
    ClaimHeldByOther, not instead of it - an except clause naming only one of
    the two lets the other escape uncaught.
    """


# Every acquire_claim/refresh_claim caller that treats "someone else has this
# right now" and "the recovery mutex is too busy to tell" the same way
# ("can't have this one right now, retry/skip") should catch this tuple
# instead of hand-rolling `except (ClaimHeldByOther, ClaimContended):` plus a
# restated comment at each site. A caller that needs to read `.holder`/`.pid`/
# `.host` (ClaimHeldByOther-only attributes) still needs its own separate
# `except ClaimHeldByOther as exc:` block ahead of this one.
CLAIM_UNAVAILABLE = (
    ClaimHeldByOther,
    ClaimContended,
    ClaimVerdictError,
    ClaimVerdictUnavailable,
)


class RebindRefused(Exception):
    """``compare_and_rebind`` refused to move the claim (fail-closed).

    Native resume never silently believes it owns a claim: every path that is
    not an affirmative same-holder local rebind raises this with a named
    reason. Carries the observed ``state``/``holder``/``pid`` so the caller can
    render a loud, specific refusal.
    """

    def __init__(
        self,
        reason: str,
        *,
        state: Optional[str] = None,
        holder: Optional[str] = None,
        pid: Optional[int] = None,
    ) -> None:
        self.reason = reason
        self.state = state
        self.holder = holder
        self.pid = pid
        super().__init__(reason)


# Re-export low-level exceptions so callers can ``from fno.claims import ClaimGoneAway``.
__all__ = [
    "CLAIM_UNAVAILABLE",
    "ClaimAlreadyHeld",
    "ClaimContended",
    "ClaimCorrupted",
    "ClaimGoneAway",
    "ClaimHeldByOther",
    "ClaimSweepOmission",
    "ClaimValidationError",
    "ClaimVerdictError",
    "ClaimVerdictUnavailable",
    "ForceReleaseOutcome",
    "HolderMismatch",
    "RebindRefused",
    "acquire_claim",
    "claim_status",
    "compare_and_rebind",
    "force_release_claim",
    "list_claims",
    "list_claims_with_counts",
    "reap_dead_claims",
    "sweep_verdict",
    "refresh_claim",
    "release_claim",
]


def _validate_inputs(
    key: str,
    holder: str,
    ttl_ms: Optional[int],
    *,
    pid: Optional[int] = None,
    pid_unavailable: bool = False,
) -> None:
    if not key:
        raise ClaimValidationError("key must be non-empty")
    if len(key) > MAX_KEY_LENGTH:
        raise ClaimValidationError(f"key length {len(key)} exceeds MAX_KEY_LENGTH={MAX_KEY_LENGTH}")
    # Raw length passing MAX_KEY_LENGTH does not guarantee the encoded
    # filename fits the filesystem's 255-byte name limit. Check the
    # URL-encoded form explicitly: keys with many reserved characters
    # (slashes, colons) expand up to 3x.
    encoded_len = len(_url_quote(key, safe="").encode("utf-8"))
    if encoded_len > MAX_ENCODED_FILENAME_BYTES:
        raise ClaimValidationError(
            f"URL-encoded key length {encoded_len} exceeds "
            f"MAX_ENCODED_FILENAME_BYTES={MAX_ENCODED_FILENAME_BYTES}"
        )
    if not holder:
        raise ClaimValidationError("holder must be non-empty")
    if pid_unavailable and ttl_ms is None:
        raise ClaimValidationError("pid_unavailable requires a TTL claim")
    if pid_unavailable and pid is not None:
        raise ClaimValidationError("--pid and --pid-unavailable are mutually exclusive")
    if pid is not None and pid <= 0:
        raise ClaimValidationError("pid must be positive")
    if ttl_ms is not None and not (MIN_TTL_MS <= ttl_ms <= MAX_TTL_MS):
        raise ClaimValidationError(f"ttl_ms={ttl_ms} out of range [{MIN_TTL_MS}, {MAX_TTL_MS}]")


def compare_and_rebind(
    key: str,
    expected_holder: str,
    *,
    new_holder: Optional[str] = None,
    new_reason: Optional[str] = None,
    new_harness: Optional[str] = None,
    new_metadata: Optional[dict] = None,
    new_pid: Optional[int] = None,
    new_pid_unavailable: bool = False,
    ttl_ms: Optional[int] = None,
    root: Optional[Path] = None,
    emit: bool = True,
    fno_id: Optional[str] = None,
    harness_tag: Optional[str] = None,
    harness_session_id: Optional[str] = None,
) -> tuple[Claim, str]:
    del emit, fno_id, harness_tag
    import json
    flags = ["--holder", new_holder or expected_holder, "--handover-from", expected_holder, "--bind-only"]
    for flag,value in [("--reason",new_reason),("--harness",new_harness),("--session-id",harness_session_id),("--pid",new_pid),("--ttl-ms",ttl_ms)]:
        if value is not None:
            flags.extend((flag,str(value)))
    if new_metadata is not None:
        flags.extend(("--metadata",json.dumps(new_metadata)))
    if new_pid_unavailable:
        flags.append("--pid-unavailable")
    flags.extend(_native_root_flags(root or _configured_claim_root()))
    try:
        payload = _native_claim("acquire",key,flags)
    except ClaimVerdictError as exc:
        raise RebindRefused(str(exc)) from exc
    return _native_claim_model(payload), str(payload["mode"])


#: Holder prefix marking a claim taken by `fno agents spawn --node` on behalf of
#: a worker that does not exist yet. The handover branch above accepts ONLY this
#: prefix as a replaceable prior holder, which is why the constant lives here
#: rather than in the command module that re-exports it.
#:
#: WHAT THIS IS NOT: a secret. `fno agents claim status` publishes every holder, so
#: naming one back proves nothing about who is asking. What the prefix restricts
#: is the BLAST RADIUS - only a launch-window claim can be taken over this way,
#: never a working session's `target-session:` claim, which `ClaimHeldByOther`
#: still protects. The window is TTL-bound (`HANDOVER_TTL`), so the exposure is
#: bounded to it, and before this change that same window carried NO claim at
#: all. Closing it properly needs a secret the worker alone holds, which is its
#: own change.
HANDOVER_HOLDER_PREFIX = "spawn-handover:"

#: The requeue pseudo-holder (`backlog/requeue`): the session that takes the
#: node, not an agent. `holder_agent_name` is the one resolver.
TARGET_SESSION_HOLDER_PREFIX = "target-session:"

#: A subagent planner holds node:<id> under this prefix between `session open`
#: and `session close`, mirroring target-session. Resolved in the same branch.
BLUEPRINT_HOLDER_PREFIX = "blueprint-session:"


def holder_agent_name(holder: Optional[str], rows: Any) -> Optional[str]:
    """Resolve a claim holder to the agent behind it, or None.

    None means no row in ``rows`` stands behind the name: a skip, not a
    failure. ``rows`` is the caller's read; no agents import here.
    """
    if not holder:
        return None

    if holder.startswith(HANDOVER_HOLDER_PREFIX):
        name = holder[len(HANDOVER_HOLDER_PREFIX):]
        return name if any(row.name == name for row in rows) else None
    if holder.startswith((TARGET_SESSION_HOLDER_PREFIX, BLUEPRINT_HOLDER_PREFIX)):
        sid = holder.split(":", 1)[1]
        row = next(
            (
                r
                for r in rows
                if sid in {
                    r.harness_session_id,
                    getattr(r, "session_id", None),
                    getattr(r, "cc_session_id", None),
                }
            ),
            None,
        )
        return row.name if row else None
    return holder

#: Suffix of the per-claim recovery mutex directory. One definition: this
#: string was written out at six call sites, and a seventh (the dispatch
#: guard's targeted recovery) is what made the duplication worth collapsing.
#: A caller that spells it differently takes a DIFFERENT lock and silently
#: serializes against nobody.
RECOVERY_LOCK_SUFFIX = ".recovery.d"




def _claim_verdict(claim: Claim, *, root: Optional[Path] = None) -> dict[str, Any]:
    """Read one claim verdict from the native batch door."""
    verdict = claim_verdicts([claim.key], root=root).get(claim.key)
    if verdict is None:
        raise ClaimVerdictError(f"native verdict omitted readable claim {claim.key!r}")
    return verdict


def _claim_state(claim: Claim, *, root: Optional[Path] = None) -> ClaimState:
    """Project a native row into the Python enum used by control flow."""
    try:
        return ClaimState(_claim_verdict(claim, root=root).get("state", "corrupted"))
    except ValueError:
        return ClaimState.CORRUPTED


def _existing_is_live(existing: Claim, *, root: Optional[Path] = None) -> bool:
    """Authoritative acquire/recovery liveness predicate.

    Delegates to ``classify`` so the mutex honors the SAME hybrid TTL-or-pid
    liveness as the selection/status reads : an expired TTL claim
    whose recorded pid is alive on this host is LIVE and must NOT be reclaimed
    by a peer (otherwise a suspended-but-alive session's node is stolen). One
    predicate means acquire and ``status``/``list`` can never diverge.

    SUSPECT counts as live here: a TTL-unexpired claim with a dead pid
    is a respawned worker's protected slot, so acquire must refuse it exactly
    like LIVE (never steal). Only TTL expiry (-> STALE) makes it reclaimable.
    """
    return _claim_state(existing, root=root) in (ClaimState.LIVE, ClaimState.SUSPECT)


class ForceReleaseOutcome(NamedTuple):
    """What a force-release found at the path; archived=False means nothing was there."""

    path: Path
    archived: bool
    previous_holder: Optional[str]


def sweep_verdict(
    claim: Claim,
    *,
    abandonment_probe: Optional[Callable[..., Optional[bool]]] = None,
    node_settlement: Optional[Callable[..., Optional[bool]]] = None,
    native_verdict: Optional[dict[str, Any]] = None,
) -> tuple[bool, str]:
    """Can this claim be archived, and if not, which bucket says why?

    ``node_settlement`` runs FIRST on a ``node:`` claim and answers the
    question a pid cannot: is the claim's own node still the holder's
    workplace? A node that closed under the claim, or a holder whose roster
    row resolves to a DIFFERENT node, is positive evidence of abandonment -
    measured on the live 2026-08-21 specimen where a session that finished
    one node and moved to the next kept a LIVE claim on the dead one for 16
    hours, because "holder alive" was the only question asked.
    ``True`` settles (reapable); anything else falls through to liveness,
    which stays the authority for every unsettled shape.

    THE single reap decision. The sweep's lock-free triage, its under-mutex
    re-verify, and the spawn guard's targeted recovery all call this, so no two
    of them can drift into different answers about the same claim - the shape
    the first pitfalls entry names.

    Buckets: ``""`` (reapable), ``"live"``, ``"offhost"``, ``"suspect"`` (no
    probe was supplied), ``"suspect_alive"`` (a worker is on the node) and
    ``"suspect_unprobed"`` (the probe could not run). The last two are kept
    apart deliberately: one is a measurement and the other is its absence.
    """
    verdict = native_verdict or _claim_verdict(claim)
    if node_settlement is not None and claim.key.startswith("node:"):
        # A settlement instrument that raises answers nothing; a broken probe
        # must never become a verdict. The CLI-built settlement never raises
        # by contract - this is the belt under it.
        try:
            if node_settlement(claim, native_verdict=verdict) is True:
                return True, ""
        except Exception:  # noqa: BLE001 - unknown keeps
            pass
    if "provably_dead" not in verdict or "bucket" not in verdict:
        raise RuntimeError("native claim verdict omitted sweep fields")
    provably_dead = bool(verdict["provably_dead"])
    bucket = str(verdict["bucket"] or "")
    if provably_dead or bucket != "suspect":
        return provably_dead, bucket
    if abandonment_probe is None or not claim.key.startswith("node:"):
        return False, "suspect"
    probe_verdict = abandonment_probe(claim, native_verdict=verdict)
    if probe_verdict is True:
        return True, ""
    # False: a live worker holds this node. None: the probe could not run,
    # which is unknown, and unknown keeps.
    return False, "suspect_alive" if probe_verdict is False else "suspect_unprobed"


def _default_reap_roots() -> list[Path]:
    """Both claims roots swept by a bare ``fno agents claim reap`` (AC2).

    Global node claims live at ``claims_dir(global_claims_root())``
    (``~/.fno/claims`` by default); a repo's own root is
    ``claims_dir(None)`` (canonical repo root). A cwd whose canonical repo
    root IS the global root sweeps once, not twice - see
    :func:`fno.claims.io.dedup_claims_roots`.
    """
    return _dedup_roots([global_claims_root(), None])


def _dedup_roots(roots: list[Optional[Path]]) -> list[Path]:
    """Resolve + dedup an explicit ``--root`` list, returning the claims dirs."""
    return [cdir for _, cdir in dedup_claims_roots(roots)]


def native_claims_root(key: str) -> Optional[Path]:
    """The claims root the native leg resolves ``key`` against, or None for a
    repo-local key. The ONE routing read for callers that need the PATH (the
    prefix list lives once, in the Rust
    ``claims_root.rs``); the lockfile operations themselves never needed it.
    Raises :class:`ClaimVerdictUnavailable` when the fno-agents binary is
    missing, like every native claim call."""
    payload = _native_claim("root", key, [])
    root = payload.get("root")
    return Path(str(root)) if root else None


def _native_claim(operation: str, key: str, flags: list[str]) -> dict[str, Any]:
    """Run one native claim operation and decode its JSON reply."""
    import json
    from fno.rust_binary import resolve_binary
    binary = resolve_binary()
    if binary is None:
        raise ClaimVerdictUnavailable(
            "fno-agents claim unavailable: set FNO_AGENTS_BIN or reinstall fno"
        )
    command = [str(binary), "claim", operation]
    # The root op's key IS the question, and an empty key is a legal input
    # (a colon-less key routes repo-local); list/reap pass key="" to mean no
    # key at all, so the falsy skip stays for them.
    if key or operation == "root":
        command.append(key)
    command.extend(flags)
    command.append("--json")
    try:
        result = _SubprocessPopen(command, stdout=_SUBPROCESS_PIPE,
                                  stderr=_SUBPROCESS_PIPE, text=True)
        stdout, stderr = result.communicate()
    except OSError as exc:
        raise ClaimVerdictUnavailable(f"fno-agents claim could not run: {exc}") from exc
    try:
        payload = json.loads(stdout) if stdout.strip() else {}
    except json.JSONDecodeError as exc:
        raise ClaimVerdictError(f"fno-agents claim returned invalid JSON: {exc}") from exc
    if result.returncode == 1 and payload.get("outcome") == "held_by_other":
        raise ClaimHeldByOther(
            str(payload.get("holder") or "unknown"),
            payload.get("pid"),
            str(payload.get("host") or "unknown"),
            key,
        )
    if result.returncode != 0:
        detail = stderr.strip() or stdout.strip() or f"exit {result.returncode}"
        raise ClaimVerdictError(f"fno-agents claim {operation} failed: {detail}")
    if isinstance(payload, list) and operation == "list":
        return {"rows": payload}
    if not isinstance(payload, dict):
        raise ClaimVerdictError("fno-agents claim returned a non-object JSON value")
    return payload
def _native_root_flags(root: Optional[Path]) -> list[str]:
    return ["--root", str(root)] if root is not None else []

def _native_claim_model(payload: dict[str, Any]) -> Claim:
    body = payload.get("claim", payload)
    if not isinstance(body, dict):
        raise ClaimVerdictError("fno-agents claim returned no claim object")
    return Claim.model_validate(body)


def _configured_claim_root() -> Optional[Path]:
    value = os.environ.get("FNO_CLAIMS_ROOT", "").strip()
    return Path(value) if value else None


def acquire_claim(
    key: str,
    holder: str,
    *,
    reason: Optional[str] = None,
    ttl_ms: Optional[int] = None,
    metadata: Optional[dict[str, Any]] = None,
    pid: Optional[int] = None,
    pid_unavailable: bool = False,
    host: Optional[str] = None,
    harness: Optional[str] = None,
    pid_provenance: Optional[str] = None,
    harness_session_id: Optional[str] = None,
    root: Optional[Path] = None,
    _attempt: int = 0,
) -> Claim:
    del _attempt
    _validate_inputs(key, holder, ttl_ms, pid=pid, pid_unavailable=pid_unavailable)
    native_root = root or _configured_claim_root()
    flags = ["--holder", holder]
    if ttl_ms is not None:
        flags.extend(("--ttl-ms", str(ttl_ms)))
    if reason is not None:
        flags.extend(("--reason", reason))
    if metadata is not None:
        import json

        flags.extend(("--metadata", json.dumps(metadata, separators=(",", ":"))))
    if not pid_unavailable:
        flags.extend(("--pid", str(pid if pid is not None else os.getpid())))
    if pid_unavailable:
        flags.append("--pid-unavailable")
    for flag,value in [("--host",host),("--harness",harness),("--pid-provenance",pid_provenance),("--session-id",harness_session_id)]:
        if value is not None:
            flags.extend((flag,str(value)))
    flags.extend(_native_root_flags(native_root))
    return _native_claim_model(_native_claim("acquire", key, flags))


def release_claim(
    key: str,
    holder: str,
    *,
    strict: bool = False,
    root: Optional[Path] = None,
) -> Optional[Claim]:
    if not key or not holder:
        raise ClaimValidationError("key and holder must be non-empty")
    native_root = root or _configured_claim_root()
    prior_payload = _native_claim("status", key, _native_root_flags(native_root)) if strict else {}
    prior = None
    if prior_payload.get("state") not in {None, "free"} and prior_payload.get("holder"):
        prior = Claim.model_validate(prior_payload)
        if prior.holder != holder and strict:
            raise HolderMismatch(holder, prior.holder, key)
    # --with-claim: the wave-2 leaf port moved the release surface into
    # claim_cli; the operator --json receipt is the frozen two-field shape,
    # and the engine leg asks for the removed record so this parse is
    # unchanged.
    receipt = _native_claim(
        "release", key, ["--holder", holder, "--with-claim", *_native_root_flags(native_root)]
    )
    return _native_claim_model(receipt) if receipt.get("released") is True else None


def refresh_claim(
    key: str,
    holder: str,
    *,
    ttl_ms: Optional[int] = None,
    root: Optional[Path] = None,
    _attempt: int = 0,
) -> Optional[Claim]:
    del _attempt
    if ttl_ms is not None and ttl_ms <= 0:
        raise ClaimValidationError("ttl_ms must be positive")
    native_root = root or _configured_claim_root()
    flags = _native_root_flags(native_root)
    if ttl_ms is None:
        status = _native_claim("status", key, flags)
        state = status.get("state")
        if state == "free":
            raise ClaimGoneAway(str(claim_path(key, root=root)))
        if state == "corrupted":
            raise ClaimCorrupted(str(status.get("error") or key))
        if state == "stale":
            raise ClaimValidationError(f"claim {key!r} expired and cannot be refreshed")
        if status.get("expires_at") is None:
            return None
        ttl_ms = MIN_TTL_MS
    payload = _native_claim(
        "renew",
        key,
        ["--holder", holder, "--ttl-ms", str(ttl_ms), *flags],
    )
    if payload.get("refreshed") is False or payload.get("outcome") == "unchanged":
        return None
    return _native_claim_model(payload)


def _unrouted_key_verdict(key: str) -> Optional[dict[str, Any]]:
    """A bare well-formed node id names no store, and ``free`` there reads as
    safe-to-dispatch. Both legs answer the same unknown."""
    if not key or ":" in key:
        return None
    from fno.graph._constants import is_wellformed_node_id

    if not is_wellformed_node_id(key):
        return None
    return {
        "key": key,
        "state": "unknown",
        "basis": "key-unrouted",
        "detail": f"{key!r} has no claim prefix; node claims are keyed node:{key}",
    }


def claim_status(key: str, *, root: Optional[Path] = None) -> dict[str, Any]:
    unrouted = _unrouted_key_verdict(key)
    if unrouted is not None:
        return unrouted
    if not key:
        raise ClaimValidationError("key must be non-empty")
    return _native_claim("status", key, _native_root_flags(root or _configured_claim_root()))


def list_claims(
    *,
    prefix: Optional[str] = None,
    include_stale: bool = False,
    root: Optional[Path] = None,
) -> list[dict[str, Any]]:
    flags = _native_root_flags(root or _configured_claim_root())
    if prefix is not None:
        flags.extend(("--prefix", prefix))
    if include_stale:
        flags.append("--include-stale")
    payload = _native_claim("list", "", flags)
    return payload.get("rows", []) if isinstance(payload.get("rows"), list) else []


def list_claims_with_counts(
    *,
    prefix: Optional[str] = None,
    include_stale: bool = False,
    root: Optional[Path] = None,
) -> tuple[list[dict[str, Any]], dict[str, int], dict[str, str]]:
    rows = list_claims(prefix=prefix, include_stale=True, root=root)
    counts = {state: 0 for state in ("live", "suspect", "stale", "corrupted", "free")}
    states: dict[str, str] = {}
    for row in rows:
        state = str(row.get("state") or "corrupted")
        counts[state] = counts.get(state, 0) + 1
        if isinstance(row.get("key"), str):
            states[row["key"]] = state
    if not include_stale:
        rows = [row for row in rows if row.get("state") in {"live", "suspect"}]
    counts["total"] = sum(counts.values())
    return rows, counts, states


def force_release_claim(
    key: str,
    reason: str,
    *,
    root: Optional[Path] = None,
    holding_recovery_lock: bool = False,
    expected_claim: Optional[Claim] = None,
) -> ForceReleaseOutcome:
    if not key:
        raise ClaimValidationError("key must be non-empty")
    if not reason:
        raise ClaimValidationError("reason must be non-empty for force-release")
    flags = ["--reason", reason]
    if holding_recovery_lock:
        flags.append("--holding-recovery-lock")
    if expected_claim is not None:
        import json
        flags.extend(("--expected-claim",json.dumps(expected_claim.model_dump())))
    payload = _native_claim(
        "force-release", key,
        [*_native_root_flags(root or _configured_claim_root()), *flags],
    )
    return ForceReleaseOutcome(
        path=Path(str(payload.get("path") or "")),
        archived=bool(payload.get("archived")),
        previous_holder=payload.get("previous_holder"),
    )


def reap_dead_claims(
    *,
    roots: Optional[list[Optional[Path]]] = None,
    apply: bool = False,
    abandonment_probe: Optional[Callable[..., Optional[bool]]] = None,
    node_settlement: Optional[Callable[..., Optional[bool]]] = None,
    optout_sink: Optional[list[Claim]] = None,
) -> dict[str, Any]:
    """Apply caller policy to native verdicts and retire only the observed row."""
    import json
    directories = _default_reap_roots() if roots is None else _dedup_roots(roots)
    summary: dict[str, Any] = dict.fromkeys(("scanned", "reaped", "would_reap", "kept_offhost", "kept_suspect", "kept_live", "kept_suspect_alive", "kept_suspect_unprobed", "kept_unclassified", "corrupted", "vanished", "contended"), 0)
    summary.update(roots=[str(d) for d in directories], reap_failed=[], unclassified_dirs={}, kept_suspect_unprobed_by={})
    for directory in directories:
        verdicts = claim_verdicts(claims_dir_path=directory)
        for key, verdict in verdicts.items():
            if verdict.get("state") == "free":
                continue
            summary["scanned"] += 1
            try:
                claim = read_claim_file(directory / f"{encode_key(key)}.lock")
                dead, bucket = sweep_verdict(claim, abandonment_probe=abandonment_probe, node_settlement=node_settlement, native_verdict=verdict)
                if not dead:
                    summary["kept_" + bucket] = summary.get("kept_" + bucket, 0) + 1
                    if bucket == "suspect_unprobed":
                        token = verdict.get("probe_basis") or "probe-unanswered"
                        counts = summary["kept_suspect_unprobed_by"]
                        counts[token] = counts.get(token, 0) + 1
                    continue
                summary["would_reap"] += 1
                if not apply:
                    continue
                fresh = claim_verdicts([key], claims_dir_path=directory).get(key)
                if fresh is None or not sweep_verdict(claim, abandonment_probe=abandonment_probe, node_settlement=node_settlement, native_verdict=fresh)[0]:
                    summary["contended"] += 1
                    continue
                result = _native_claim("force-release", key, ["--claims-dir", str(directory), "--reason", "reap proven-dead claim", "--expected-claim", json.dumps(claim.model_dump())])
                if result.get("archived"):
                    summary["reaped"] += 1
                    if optout_sink is not None and key.startswith("config-optout:"):
                        optout_sink.append(claim)
                else:
                    summary["contended"] += 1
            except ClaimGoneAway:
                summary["vanished"] += 1
            except ClaimCorrupted:
                summary["corrupted"] += 1
            except Exception as exc:
                summary["reap_failed"].append((str(directory / f"{encode_key(key)}.lock"), str(exc)))
    return summary
