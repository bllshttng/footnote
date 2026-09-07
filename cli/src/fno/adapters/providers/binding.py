"""The one effective-account answer every claude launch and usage reader shares.

A path says where a credential is meant to be and a stamp says who put it
there. Neither says who the credential presents as, so four readers deciding
identity for themselves could disagree and each still name an account. This
joins the evidence ``managed`` already collects to the credential root a launch
will read. See docs/provider-rotation.md, the effective-account binding.
"""
from __future__ import annotations

import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from fno.adapters.providers import managed
from fno.adapters.providers.model import ProviderRecord

MATCHED = "matched"
MISMATCH = "mismatch"
UNKNOWN = "unknown"
AMBIGUOUS = "ambiguous"

MISMATCH_RECEIPT = "account_identity_mismatch"
UNKNOWN_RECEIPT = "account_identity_unknown"


@dataclass(frozen=True)
class AccountBinding:
    """Who the credential serving this launch provably belongs to.

    ``credential_root`` is ``None`` for the shared ``~/.claude`` slot, a
    location and not an identity. ``credential_ref`` is a digest, so a caller
    names the credential generation without a token reaching disk.
    """

    harness: str
    status: str
    credential_root: Path | None = None
    requested_record: str | None = None
    observed_principal: str | None = None
    observed_label: str | None = None
    matched_record: str | None = None
    credential_ref: str | None = None
    observed_at: float = 0.0
    reason: str | None = None

    @property
    def ok(self) -> bool:
        return self.status == MATCHED

    @property
    def receipt(self) -> str:
        """A line safe to print. It never names an account it cannot prove."""
        # A fresh proof carries the label; a cached one carries the identity key
        # only, so the record name stands in before a bare uuid pair does.
        who = (
            self.observed_label
            or self.matched_record
            or self.observed_principal
            or "an unproven account"
        )
        where = str(self.credential_root or "the shared ~/.claude slot")
        if self.status == MISMATCH:
            return (
                f"{MISMATCH_RECEIPT}: {self.requested_record!r} is pinned, but "
                f"{where} serves {who}; sign that account in where it belongs, or "
                "pin the record whose identity it is"
            )
        if self.status == MATCHED:
            return (
                "account_identity_matched: "
                f"{where} serves {self.matched_record or self.requested_record}"
            )
        if self.status == AMBIGUOUS:
            return (
                f"{UNKNOWN_RECEIPT}: {where} is ambiguous ({self.reason}); whichever "
                "account were named, some reader would get the other"
            )
        return f"{UNKNOWN_RECEIPT}: {where} could not be attributed ({self.reason})"


def credential_root(record: ProviderRecord) -> Path | None:
    """The dir whose credential serves ``record``; ``None`` = the shared slot.

    ``config_dir`` outranks the ``credentials_source`` staging lane, so a
    converged account always rides its own dir.
    """
    if record.config_dir is not None:
        return Path(record.config_dir)
    if record.credentials_source is not None:
        return Path(record.credentials_source)
    return None


def credential_blobs(harness: str, root: Path | None) -> list[str]:
    """Every distinct credential a reader of ``root`` can be served.

    A dir of its own reads the Keychain item SCOPED to it plus its own
    ``.credentials.json``, never the unscoped item, which belongs to whoever
    occupies the shared slot. Its transcript folders may symlink anywhere;
    neither source read here is a transcript. A source that could not be READ
    raises: shrinking the candidate set on a denied read is how an
    unambiguous-looking slot ends up holding two accounts.
    """
    if harness != "claude":
        return []
    if root is None:
        return managed.canonical_slot_blobs(harness)
    out: list[str] = []
    if sys.platform == "darwin":
        blob = managed._read_claude_keychain_item(managed._claude_scoped_service(root))
        if blob:
            out.append(blob)
    try:
        blob = (root / ".credentials.json").read_text(encoding="utf-8")
    except OSError:
        blob = ""
    if blob.strip() and managed._token_present(blob) and blob not in out:
        out.append(blob)
    return out


def resolve_account_binding(
    record: ProviderRecord | None,
    *,
    harness: str = "claude",
    bearer: str | None = None,
    root: Path | None = None,
    by_id: dict[str, ProviderRecord] | None = None,
    now: float | None = None,
    ttl: float = managed._PRINCIPAL_TTL_S,
) -> AccountBinding:
    """Bind ``record``, or the unpinned shared slot, to a proven principal.

    ``record=None`` asks who is signed in right now, independently of the
    active-slot label: the label is what an out-of-band ``claude /login`` leaves
    wrong and untainted. ``bearer`` narrows it to one exact credential, which is
    what a usage probe needs. Never raises, never writes a record; every failure
    is a typed ``unknown``.
    """
    now = time.time() if now is None else now
    root = root or managed.store_root()
    requested = record.id if record is not None else None
    if record is not None:
        harness = record.harness
    cred_root = credential_root(record) if record is not None else None

    def _at(status: str, **kw) -> AccountBinding:
        kw.setdefault("credential_root", cred_root)
        kw.setdefault("requested_record", requested)
        return AccountBinding(harness, status, observed_at=now, **kw)

    if harness != "claude":
        return _at(UNKNOWN, reason="unsupported-harness")
    if record is not None and record.auth == "api_key":
        # An API key is its own access path; the ambient subscription is not
        # what it bills, so its principal is not the question asked here.
        return _at(UNKNOWN, reason="api-key-route")
    if bearer is not None:
        # The bearer lane owns its own unbound case, in `managed`, so the
        # short-circuit below must not run ahead of it.
        return _bearer_binding(_at, harness, requested, cred_root, root, bearer, now, ttl)

    # An unbound record has no reference identity to compare against, so
    # answering here keeps the launch path offline on the common path. `doctor`
    # reports the unbound record with its repair.
    want = None
    if requested is not None:
        want = managed.identity_key(managed.record_principal(requested, root))
        if want is None:
            return _at(UNKNOWN, reason="unbound-principal")

    try:
        blobs = credential_blobs(harness, cred_root)
    except managed.KeychainError:
        return _at(UNKNOWN, reason="credential-unreadable")
    if not blobs:
        return _at(UNKNOWN, reason="no-slot-credential")

    observed, label, ref = None, None, None
    if len(blobs) == 1:
        cached = managed.cached_slot_principal(harness, root, blobs[0], now=now, ttl=ttl)
        if cached is not None:
            observed, ref = cached, managed.credential_digest(blobs[0])
    if observed is None:
        principal, proven, failure = managed.principal_of_blobs(blobs)
        if principal is None:
            status = AMBIGUOUS if failure == "ambiguous-slot" else UNKNOWN
            return _at(status, reason=failure or "profile-unavailable")
        observed = managed.identity_key(principal)
        if observed is None:
            return _at(UNKNOWN, reason="malformed-profile")
        label = principal.get("email") or principal.get("account_uuid")
        ref = managed.credential_digest(proven or blobs[0])
        if len(blobs) == 1:
            managed.note_slot_principal(harness, root, observed, blobs[0], now=now)

    matches = sorted(
        rid
        for rid, other in (by_id or {}).items()
        if other.harness == harness
        and managed.identity_key(managed.record_principal(rid, root)) == observed
    )
    matched = matches[0] if len(matches) == 1 else None
    common = {
        "observed_principal": observed,
        "observed_label": label,
        "matched_record": matched,
        "credential_ref": ref,
    }
    if requested is None:
        if len(matches) > 1:
            return _at(AMBIGUOUS, reason="ambiguous-match", **common)
        if matched is None:
            return _at(UNKNOWN, reason="zero-match", **common)
        return _at(MATCHED, **common)
    return _at(MATCHED if want == observed else MISMATCH, **common)


def _bearer_binding(
    _at,
    harness: str,
    requested: Optional[str],
    cred_root: Path | None,
    root: Path,
    bearer: str,
    now: float,
    ttl: float,
) -> AccountBinding:
    """Bind one exact bearer, deferring to ``managed``'s per-bearer verdict.

    A shared-slot bearer that proves out is still unattributable when the slot
    presents two distinct credentials: claude reads the scoped Keychain item
    while the probe reads the unscoped one. Settled offline on the count.
    """
    ref = managed.credential_digest(bearer)
    if requested is None:
        return _at(UNKNOWN, reason="bearer-needs-record", credential_ref=ref)
    try:
        if cred_root is None and len(managed.canonical_slot_blobs(harness)) > 1:
            return _at(AMBIGUOUS, reason="ambiguous-slot", credential_ref=ref)
        verdict = managed.bearer_principal_verdict(
            harness, requested, root, bearer, now=now, ttl=ttl
        )
    except Exception:  # noqa: BLE001 - an unreadable store cannot vouch for a bearer
        return _at(UNKNOWN, reason="credential-unreadable", credential_ref=ref)
    status = {"match": MATCHED, "mismatch": MISMATCH}.get(verdict, UNKNOWN)
    return _at(
        status,
        credential_ref=ref,
        matched_record=requested if status == MATCHED else None,
        reason=None if status == MATCHED else verdict,
    )
