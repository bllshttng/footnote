"""Resolve the session(s) that own a merged PR, from ``ledger.json`` (x-f47f).

The join the ``/fno:pr merged`` ritual used to do in markdown bash (a jq
pipeline filtered through ``grep -vxE 'null|'``, an empty alternation that
ugrep rejects). Every failure there collapsed to an empty variable, so a real
ledger entry read as "no owning session" and the ritual silently declined to
consume the PR's backfills. Here the failure is a REASON, not an absence:
:func:`resolve_pr_sessions` returns why it resolved nothing, so a caller can
print it instead of manufacturing a plausible no-op.

``ledger.json`` is GLOBAL and PR numbers collide across repos, so an entry is
only attributable with a known repo slug; without one the join refuses rather
than risk claiming a same-numbered foreign PR's sessions.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

#: Opening words of every reason that names an INFRA failure - something that
#: should have worked did not - as opposed to a benign no-match. Kept beside the
#: return sites that produce them so the two cannot drift apart silently.
_INFRA_REASON_OPENERS = (
    "repo slug unresolved",
    "ledger unreadable",
    "ledger malformed",
)


def reason_is_infra_failure(reason: Optional[str]) -> bool:
    """Whether ``reason`` names a broken environment rather than a clean miss.

    A caller that flattens the result to a bare list needs this: without it a
    ledger that would not parse reads exactly like "this PR has no owning
    session", and the infra failure is never investigated.
    """
    return bool(reason) and reason.startswith(_INFRA_REASON_OPENERS)


def _entry_owns_pr(entry: dict, pr: int, slug_l: str, allow_unattributed: bool) -> bool:
    url = entry.get("pr_url")
    # A hand-written url may carry a query, fragment, or trailing slash, and the
    # owner/repo slug is case-insensitive. Normalizing here prevents a false
    # "no owning session" - the same silently-wrong-empty this module replaces.
    url_s = (
        url.split("?", 1)[0].split("#", 1)[0].strip().rstrip("/").lower()
        if isinstance(url, str)
        else ""
    )
    if url_s:
        return url_s.endswith(f"/{slug_l}/pull/{pr}")
    if not allow_unattributed:
        # A url-less row carries no repo, and this ledger is GLOBAL - so matching
        # it on the bare number can claim a foreign repo's session for this PR.
        # Refused by default because the caller that resolves sessions in order
        # to CONSUME carve-outs would then destroy another PR's backfills.
        return False
    # Fall back to the bare numeric field. Coerce to int so a string-stored
    # pr ("522") still matches the int arg.
    for key in ("pr", "pr_number"):
        val = entry.get(key)
        if val is None:
            continue
        try:
            if int(val) == pr:
                return True
        except (ValueError, TypeError):
            pass
    return False


def resolve_pr_sessions(
    ledger_path: Optional[Path],
    pr: int,
    repo_slug: Optional[str],
    *,
    allow_unattributed: bool = False,
) -> "tuple[list[str], Optional[str]]":
    """Return ``(session_ids, reason)`` for the PR's owning ledger entries.

    ``reason`` is None when ids were resolved, and otherwise a printable phrase
    naming why none were: an unresolvable repo, an absent/unreadable/malformed
    ledger, or a genuine no-match. Callers MUST surface it - "no owning session"
    that is really "the ledger would not parse" is the failure this returns for.

    ``allow_unattributed`` admits a url-less row on a bare ``pr``/``pr_number``
    match. Default False: the ledger is global, so such a row may belong to any
    repo, and a caller that resolves sessions in order to CONSUME their
    carve-outs must never claim a foreign PR's. Only a read-only, additive
    caller (retro's harvest) should opt in.
    """
    if not repo_slug:
        return [], (
            f"repo slug unresolved, so PR #{pr} cannot be attributed across repos"
        )
    if ledger_path is None:
        return [], "no ledger at <unset>"
    path = Path(ledger_path)
    if not path.exists():
        return [], f"no ledger at {path}"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return [], f"ledger unreadable: {exc}"
    entries = data.get("entries") if isinstance(data, dict) else data
    if not isinstance(entries, list):
        return [], "ledger malformed: no entries list"

    slug_l = repo_slug.lower()
    out: list[str] = []
    seen: set[str] = set()
    for e in entries:
        if not isinstance(e, dict) or not _entry_owns_pr(
            e, pr, slug_l, allow_unattributed
        ):
            continue
        # Defensive: a non-list ``sessions`` (e.g. a stray string) must NOT be
        # spread into per-character ids - guard the type before list().
        sessions_val = e.get("sessions")
        sids = list(sessions_val) if isinstance(sessions_val, list) else []
        if e.get("session_id"):
            sids.append(e["session_id"])
        for s in sids:
            # Strip: a whitespace-padded id would not match the same id elsewhere,
            # and a whitespace-only one is junk that must not become a filter.
            s = str(s).strip()
            if s and s not in seen:
                seen.add(s)
                out.append(s)
    if not out:
        return [], f"no ledger entry for PR #{pr}"
    return out, None
