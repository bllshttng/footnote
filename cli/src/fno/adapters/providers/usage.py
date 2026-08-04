"""Per-provider usage/rate-limit probe (quota-aware dispatch, x-5d3e).

Predictive layer on top of the reactive failover substrate: read remaining
quota + reset time per provider BEFORE a dispatch decision, instead of burning
a failed call to learn it. Orca (2026-07-09) only DISPLAYS this data and makes
the human swap accounts; footnote acts on it (defer, reroute) autonomously.

This module is pure probe + data shapes. The caching, the headroom predicate,
and the routing/scheduling behavior live in ``runtime_state.py`` and its
consumers. ``probe_usage`` NEVER raises and NEVER writes: a failure of any kind
(endpoint drift, 401, malformed body, timeout, missing files) returns ``None``
and the whole system degrades to today's reactive behavior (fail-open).

``[VERIFY-AT-IMPL]`` markers (Hermes A3 precedent) flag the unofficial external
surfaces - the claude OAuth usage endpoint and the codex ``rate_limits`` event
shape - which must be confirmed against a real account before merge. Their
drift is a feature loss (UNKNOWN headroom), never an outage.

Security: snapshots carry only labels, percentages, and epoch seconds. No
token, bearer, or credential material is ever logged, emitted, or persisted.
"""
from __future__ import annotations

import dataclasses
import hashlib
import json
import logging
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime
from pathlib import Path
from typing import Any, Callable

from fno.adapters.providers.model import ProviderRecord

logger = logging.getLogger(__name__)

PROBE_TIMEOUT_SECONDS = 10  # matches Claude Code / orca's own 10s usage-fetch budget

# The claude OAuth usage endpoint Claude Code's `/usage` and orca's claude-fetcher
# read. Verified live against a real account (x-6bcf): the response is top-level
# window objects (five_hour / seven_day), each `{utilization: 0-100 float,
# resets_at: ISO-8601 string}` - NOT a `windows[]` array of epoch floats.
_CLAUDE_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
_CLAUDE_USER_AGENT = "claude-code/2.1.0"  # a custom UA risks being rejected
_CLAUDE_KEYCHAIN_SERVICE = "Claude Code-credentials"  # macOS Keychain item (orca-verified)
# The API's known window keys mapped to our short labels. The response also
# carries model-specific weekly windows (seven_day_opus, seven_day_sonnet, ...);
# those are captured generically by _parse_claude_windows (any five_hour /
# seven_day* object) so a maxed Opus weekly binds headroom instead of being
# dropped, letting Opus work dispatch until the reactive 429 (x-6bcf review).
_CLAUDE_KNOWN_LABELS = {"five_hour": "5h", "seven_day": "weekly"}


def _clamp_pct(value: float) -> float:
    """Clamp a used-percentage into [0, 100]. Boundaries: 0, 100, >100."""
    if value < 0:
        return 0.0
    if value > 100:
        return 100.0
    return float(value)


@dataclasses.dataclass(frozen=True)
class UsageWindow:
    """One rate-limit window's utilization and reset time.

    ``used_pct`` is clamped to [0, 100] on construction (and again on
    disk-read) so a provider reporting 103% or a hand-corrupted -5 never
    escapes the invariant. ``resets_at`` is unix epoch seconds UTC, matching
    ``rate_limited_until``; a value already in the past means the window has
    reset and never binds a headroom verdict.
    """

    label: str  # "5h" | "weekly" | provider-native label
    used_pct: float
    resets_at: float

    def __post_init__(self) -> None:
        clamped = _clamp_pct(self.used_pct)
        if clamped != self.used_pct:
            object.__setattr__(self, "used_pct", clamped)


@dataclasses.dataclass(frozen=True)
class UsageSnapshot:
    """A point-in-time reading of one provider's usage windows.

    ``windows`` may be empty (probe reached the source but it reported no
    windows); an empty tuple reads as UNKNOWN headroom, never OK. ``source``
    records how the reading was obtained for the ``fno config accounts usage``
    display and for debugging drift.
    """

    provider_id: str
    windows: tuple[UsageWindow, ...]
    probed_at: float
    source: str  # "oauth-endpoint" | "session-events" | ...


# ---------------------------------------------------------------------------
# Per-CLI probes. Each returns a snapshot or None (unknown). Registered by
# record.harness, mirroring the runtime adapter dispatch. A CLI with no probe
# (gemini, glm, openclaw, hermes, api_key records) is UNKNOWN in v1.
# ---------------------------------------------------------------------------


def _read_claude_bearer(record: ProviderRecord) -> str | None:
    """Read the OAuth access token from the record's resolved credentials dir.

    Deprecated single-token shim: returns the FIRST candidate (see
    :func:`_claude_bearer_candidates`). Kept for callers/tests that want one
    token; the probe itself tries every candidate because a scoped Keychain
    item can hold a STALE token (401) while the unscoped one is live.
    """
    cands = _claude_bearer_candidates(record)
    return cands[0] if cands else None


def _token_from_blob(blob: str | None) -> str | None:
    """Extract ``claudeAiOauth.accessToken`` from a credential JSON blob."""
    if not blob:
        return None
    try:
        raw = json.loads(blob)
    except (json.JSONDecodeError, ValueError):
        return None
    if isinstance(raw, dict):
        oauth = raw.get("claudeAiOauth")
        if isinstance(oauth, dict):
            token = oauth.get("accessToken")
            if isinstance(token, str) and token:
                return token
    return None


def _canonical_claude_slot_dir() -> Path:
    """The shared claude slot, ``~/.claude``, ignoring any ambient pin.

    Deliberately NOT ``managed._claude_slot_config_dir()``, which honors
    ``CLAUDE_CONFIG_DIR``: that is right for a slot WRITE performed by an
    operator verb, and wrong for an attribution read, because a worker pinned to
    another account exports that variable and would make the probe read its
    credential while the stamp names someone else.
    """
    return Path.home() / ".claude"


def _record_credential_dir(record: ProviderRecord) -> Path | None:
    """The record's OWN credential dir, or None when it has no per-record source.

    ``config_dir`` (the x-d012 per-account login) outranks ``credentials_source``
    (the oauth_dir staging lane) for the same reason ``resolve_account_overlay``
    ranks them that way: a converged account always rides its own dir.
    """
    if record.config_dir is not None:
        return Path(record.config_dir)
    if record.credentials_source is not None:
        return Path(record.credentials_source)
    return None


def _is_active_slot_occupant(record: ProviderRecord) -> bool:
    """True when ``record`` is provably the account in its CLI's shared slot.

    A TAINTED slot fails this check even when the stamp names ``record``. The
    taint marker exists precisely to say "a pinned session may have overwritten
    the slot since this stamp was written", so the stamp is no longer proof of
    what the credential is - and reading it anyway would file account B's usage
    under account A's id, the one thing this attribution rule exists to prevent.
    `fno config accounts doctor` reports the taint; until it is cleared the record is
    UNKNOWN, which is the honest answer rather than a confident wrong one.

    Any store read failure is False too: an unreadable stamp must never let a
    record borrow another's numbers.
    """
    try:
        from fno.adapters.providers.managed import (
            active_slot_id,
            slot_tainted,
            store_root,
        )

        if record.id != active_slot_id(record.harness):
            return False
        return not slot_tainted(record.harness, store_root())
    except Exception:  # noqa: BLE001 - an unreadable store is "not attributable"
        return False


def _attributed_credential_dir(record: ProviderRecord) -> tuple[bool, Path | None]:
    """``(probeable, dir)`` - whether a reading for ``record`` is attributable.

    This is the invariant that keeps a probe from reporting one account's usage
    under another's name: a snapshot is written under a record id only when the
    bearer that produced it provably belongs to that record. Three shapes
    qualify, and ``dir`` says where the credential lives:

    - an own ``config_dir`` / ``credentials_source`` -> that dir (scoped only)
    - a managed record that IS its CLI's active slot occupant -> ``None``,
      meaning the shared slot (the unscoped Keychain item genuinely is its token)
    - anything else (a non-active managed record, an api_key record) -> not
      probeable, so the probe returns None and headroom degrades to UNKNOWN

    The managed store's ``~/.fno/providers/<id>/blob`` is deliberately NOT a
    source: it is a capture-time copy that goes stale and can duplicate across
    ids, so a probe reading it would report a dead token's window or another
    account's usage. Its job is slot materialization, not identity.
    """
    own = _record_credential_dir(record)
    if own is not None:
        return True, own
    if record.auth == "managed" and _is_active_slot_occupant(record):
        return True, None
    return False, None


def _load_records() -> dict[str, ProviderRecord]:
    """Configured records by id, for the reconciliation hook; ``{}`` on failure.

    Read here rather than threaded through ``probe_usage``: reconciliation needs
    every record's proven identity to answer "is this match unique?", and no
    probe caller has that set. An unreadable config yields no candidates, so
    reconciliation refuses with ``zero-match`` instead of guessing.
    """
    try:
        from fno.adapters.providers.loader import load_providers

        return load_providers().by_id
    except Exception:  # noqa: BLE001 - a probe must never break on a config read
        return {}


def _shares_the_slot(record: ProviderRecord) -> bool:
    """Only a managed record with no dir of its own rides the shared slot.

    A ``config_dir`` record is attributable without the slot, so neither the
    taint nor a drifted stamp can affect it and it never enters any repair.
    """
    return record.auth == "managed" and _record_credential_dir(record) is None


def _reconcile_slot_once(record: ProviderRecord, now: float) -> bool:
    """Try ONCE to prove the shared slot's identity and repair it. True if it did.

    A REFUSAL is backed off briefly - a slot whose principal matches nothing
    must not re-hit the profile endpoint on every probe - while never being
    cached as proof: the backoff only delays the next attempt, it never
    satisfies one.
    """
    if not _shares_the_slot(record):
        return False
    try:
        from fno.adapters.providers import managed

        root = managed.store_root()
        if managed.reconcile_backoff_active(record.harness, root, now=now):
            return False
        # Note the attempt BEFORE making it, so a crash mid-reconcile still
        # backs off. A success clears this file along with the taint.
        managed.note_reconcile_attempt(record.harness, root, now=now)
        return managed.reconcile_slot(
            record.harness, by_id=_load_records(), root=root
        ).ok
    except Exception:  # noqa: BLE001 - repair is best-effort; UNKNOWN is the fallback
        return False


def _reconcile_tainted_slot(record: ProviderRecord, now: float) -> bool:
    """Repair a TAINTED slot, the case that used to be terminal.

    A taint made ``_is_active_slot_occupant`` False forever, the probe degraded
    to None by design, and nothing announced it - a five-day silent outage ended
    by deleting a marker file by hand. A fresh probe now asks whether the taint
    is a false positive, and resumes ONLY if identity was proven.
    """
    if not _shares_the_slot(record):
        return False
    try:
        from fno.adapters.providers import managed

        if not managed.slot_tainted(record.harness, managed.store_root()):
            return False  # not a taint refusal; there is nothing to repair
    except Exception:  # noqa: BLE001 - an unreadable store is not repairable here
        return False
    return _reconcile_slot_once(record, now)


def _bearer_verdict(record: ProviderRecord, bearer: str, now: float) -> str:
    """May ``bearer``'s usage be filed under ``record``? Checked per credential.

    Deliberately keyed to the bearer rather than to "the slot": the scoped and
    unscoped Keychain items can hold different accounts, which is why the probe
    tries several bearers in the first place. A check that proved one credential
    while the request used another would reintroduce the misattribution it was
    added to prevent.

    A record with its own dir is attributable by construction and skips this
    entirely.
    """
    if not _shares_the_slot(record):
        return "unsupported"
    try:
        from fno.adapters.providers import managed

        # A slot presenting more than one distinct credential is not
        # attributable at all, however well this particular bearer proves out:
        # claude reads the scoped Keychain item first while this probe reads the
        # unscoped one, so a matching bearer here can still be a different
        # account from the one actually being billed. Checked offline - the
        # candidate count alone settles it, no profile call needed.
        if len(managed.canonical_slot_blobs(record.harness)) > 1:
            return "unprovable"
        return managed.bearer_principal_verdict(
            record.harness, record.id, managed.store_root(), bearer, now=now
        )
    except Exception:  # noqa: BLE001 - an unreadable store cannot vouch for a bearer
        return "unprovable"


def _claude_bearer_candidates(record: ProviderRecord) -> list[str]:
    """All candidate OAuth bearer tokens for ``record``, in preference order.

    Claude Code stores the token in a ``<dir>/.credentials.json`` file (Linux /
    symlinked setups) OR the macOS Keychain (the darwin default, where no file
    exists - the reason a file-only read returned None here).

    The candidate set is bounded by attribution (see
    :func:`_attributed_credential_dir`): a record with its own dir reads ONLY
    that dir's scoped Keychain item, never the unscoped fallback, because the
    unscoped item belongs to whoever occupies the shared ``~/.claude`` slot and
    borrowing it would file the active account's usage under this record's name.
    A record with no own dir is probed only when it IS the slot occupant, in
    which case the unscoped item is its own token.

    All read fresh per probe (tokens rotate); never cached, never logged.
    """
    probeable, src = _attributed_credential_dir(record)
    if not probeable:
        return []

    tokens: list[str] = []
    seen: set[str] = set()

    def _add(tok: str | None) -> None:
        if tok and tok not in seen:
            seen.add(tok)
            tokens.append(tok)

    # The shared slot is the CANONICAL ~/.claude, never the ambient
    # CLAUDE_CONFIG_DIR. A worker pinned to account B runs with B's dir exported,
    # so honoring the env here would read B's credential while the slot stamp
    # says A - and file B's usage under A's id, the exact lie this attribution
    # rule exists to prevent. `resolve_account_overlay`'s managed-active lane
    # pins the canonical path for the same reason.
    creds_dir = src if src is not None else _canonical_claude_slot_dir()
    try:
        _add(_token_from_blob((creds_dir / ".credentials.json").read_text(encoding="utf-8")))
    except OSError:
        pass
    for blob in _read_claude_keychain_blobs(src):
        _add(_token_from_blob(blob))
    return tokens


def _read_claude_keychain_blobs(config_dir: Path | None) -> list[str]:
    """Return the raw credential blob(s) attributable to ``config_dir``.

    A dir reads its SCOPED item (``Claude Code-credentials-<sha256[:8]>``) only;
    ``None`` means the shared slot and reads the unscoped ``Claude
    Code-credentials``. The two are never mixed: falling back from a stale scoped
    item to the unscoped one is exactly how a per-account probe ends up reporting
    the active account's numbers. Non-darwin or a denied access prompt yields [].
    """
    if sys.platform != "darwin":
        return []
    account = os.environ.get("USER") or os.environ.get("USERNAME") or "user"
    if config_dir is not None:
        suffix = hashlib.sha256(str(config_dir).encode()).hexdigest()[:8]
        service = f"{_CLAUDE_KEYCHAIN_SERVICE}-{suffix}"
    else:
        service = _CLAUDE_KEYCHAIN_SERVICE
    try:
        out = subprocess.run(
            ["security", "find-generic-password", "-s", service, "-a", account, "-w"],
            capture_output=True, text=True, timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if out.returncode == 0 and out.stdout.strip():
        return [out.stdout.strip()]
    return []


def _iso_to_epoch(value: Any) -> float | None:
    """Parse an ISO-8601 string (or a bare epoch) to unix epoch seconds, or None.

    The claude usage API returns ``resets_at`` as an ISO-8601 string
    (``2026-07-12T02:09:59.521372+00:00``); accept a numeric epoch too for
    forward-safety if the shape ever changes.
    """
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str) and value:
        # Py<3.11's fromisoformat rejects a trailing 'Z'; normalize to +00:00.
        normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
        try:
            return datetime.fromisoformat(normalized).timestamp()
        except ValueError:
            return None
    return None


def _claude_window_label(api_key: str) -> str:
    """Short label for a claude usage window key.

    ``five_hour`` -> ``5h``, ``seven_day`` -> ``weekly``, and a model-specific
    ``seven_day_opus`` -> ``weekly-opus`` so display + attribution stay legible.
    """
    known = _CLAUDE_KNOWN_LABELS.get(api_key)
    if known is not None:
        return known
    if api_key.startswith("seven_day_"):
        return "weekly-" + api_key[len("seven_day_"):]
    return api_key


def _parse_claude_windows(payload: Any) -> tuple[UsageWindow, ...]:
    """Parse the claude ``/api/oauth/usage`` payload into windows.

    Verified live (x-6bcf): the payload has top-level window OBJECTS keyed by
    name, each ``{utilization: float already on a 0-100 scale, resets_at:
    ISO-8601 string, ...dollar fields}``. Includes the general ``five_hour`` /
    ``seven_day`` AND every model-specific weekly (``seven_day_opus``,
    ``seven_day_sonnet``, ...) so a maxed model window binds headroom rather than
    being silently dropped (x-6bcf review). The obfuscated promo/experimental
    buckets (``tangelo``, ``nimbus_quill``, ...) do not match the
    ``five_hour``/``seven_day*`` prefix and are excluded. A window whose object
    is absent/null or missing either field is skipped (never a raise).
    """
    if not isinstance(payload, dict):
        return ()
    out: list[UsageWindow] = []
    for api_key, w in payload.items():
        if not (api_key == "five_hour" or api_key.startswith("seven_day")):
            continue
        if not isinstance(w, dict):
            continue
        util = w.get("utilization")
        epoch = _iso_to_epoch(w.get("resets_at"))
        if util is None or epoch is None:
            continue
        try:
            out.append(
                UsageWindow(
                    label=_claude_window_label(api_key),
                    used_pct=float(util),
                    resets_at=epoch,
                )
            )
        except (TypeError, ValueError):
            continue
    return tuple(out)


def _probe_claude(
    record: ProviderRecord, now: float
) -> tuple[UsageSnapshot | None, str | None]:
    """Probe the claude ``/api/oauth/usage`` endpoint (verified x-6bcf).

    Tries every candidate bearer token until one returns 200: a stale scoped
    Keychain item 401s while the live unscoped item succeeds, so a single-token
    probe would silently fail. A 401/403 skips to the next token; any other
    network error aborts (fail-open None).

    Reports ``unattributed`` rather than ``probe-failed`` when every candidate
    bearer was REJECTED, because no usage request was ever issued: the fault is
    a stale or unprovable account binding, not the endpoint. Calling that a
    probe failure sends an operator to debug a network path that was never
    used - a confident wrong reason, which is worse than a bare unknown.
    """
    unattributable = False
    for bearer in _claude_bearer_candidates(record):
        # Prove BEFORE the usage request, so another account's usage is never
        # fetched at all - not fetched and then discarded.
        verdict = _bearer_verdict(record, bearer, now)
        if verdict not in ("match", "unsupported"):
            unattributable = True
            continue
        req = urllib.request.Request(
            _CLAUDE_USAGE_URL,
            headers={
                "Authorization": f"Bearer {bearer}",
                "anthropic-beta": "oauth-2025-04-20",
                "User-Agent": _CLAUDE_USER_AGENT,
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=PROBE_TIMEOUT_SECONDS) as resp:
                body = resp.read()
        except urllib.error.HTTPError as exc:
            if exc.code in (401, 403):
                continue  # stale/invalid token - try the next candidate
            return None, "probe-failed"
        except (urllib.error.URLError, OSError, TimeoutError):
            return None, "probe-failed"
        try:
            payload = json.loads(body)
        except (json.JSONDecodeError, ValueError):
            return None, "probe-failed"
        return UsageSnapshot(
            provider_id=record.id,
            windows=_parse_claude_windows(payload),
            probed_at=now,
            source="oauth-endpoint",
        ), None
    if unattributable:
        # Every candidate credential either belongs to someone else or could not
        # be proven. An out-of-band /login is the usual cause, so try the repair
        # once; the slot commonly turns out to belong to a DIFFERENT record,
        # which correctly leaves this one unknown.
        _reconcile_slot_once(record, now)
        return None, "unattributed"
    return None, "probe-failed"


def _codex_home() -> Path:
    return Path(os.environ.get("CODEX_HOME") or (Path.home() / ".codex"))


def _latest_codex_session(record: ProviderRecord) -> Path | None:
    """Most-recently-modified codex session JSONL under the record's home.

    An account-scoped record points its ``credentials_source`` at a ``.codex``
    dir; a bare record falls back to ``$CODEX_HOME``/``~/.codex``. Returns None
    when nothing recent exists (a cold codex is UNKNOWN, not OK).
    """
    base = Path(record.credentials_source) if record.credentials_source else _codex_home()
    sessions_dir = base / "sessions"
    search = sessions_dir if sessions_dir.is_dir() else base
    if not search.is_dir():
        return None
    try:
        candidates = sorted(
            (p for p in search.rglob("*.jsonl") if p.is_file()),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
    except OSError:
        return None
    return candidates[0] if candidates else None


def _find_rate_limits(obj: Any) -> dict | None:
    """Recursively locate the first ``rate_limits`` dict in a codex event.

    Verified live (x-6bcf): the shape is an ``event_msg`` line
    ``{timestamp, type, payload}`` with ``rate_limits`` at ``payload.rate_limits``.
    Searching recursively keeps the probe robust if a codex version re-nests it.
    """
    if isinstance(obj, dict):
        rl = obj.get("rate_limits")
        if isinstance(rl, dict):
            return rl
        for v in obj.values():
            found = _find_rate_limits(v)
            if found is not None:
                return found
    elif isinstance(obj, list):
        for v in obj:
            found = _find_rate_limits(v)
            if found is not None:
                return found
    return None


def _parse_codex_rate_limits(payload: Any) -> tuple[UsageWindow, ...]:
    """Parse a codex ``rate_limits`` payload into windows.

    Verified live (x-6bcf): ``rate_limits`` has ``primary`` (~5h) and
    ``secondary`` (weekly) sub-objects, each ``{used_percent: 0-100 float,
    resets_at: ABSOLUTE unix epoch seconds, window_minutes: int}``. ``resets_at``
    is absolute (NOT an offset), so it is used directly. A sub-object missing
    either field is skipped.
    """
    rl = _find_rate_limits(payload)
    if rl is None:
        return ()
    out: list[UsageWindow] = []
    for key, label in (("primary", "5h"), ("secondary", "weekly")):
        sub = rl.get(key)
        if not isinstance(sub, dict):
            continue
        pct = sub.get("used_percent")
        resets_at = sub.get("resets_at")
        if pct is None or resets_at is None:
            continue
        try:
            out.append(
                UsageWindow(label=label, used_pct=float(pct), resets_at=float(resets_at))
            )
        except (TypeError, ValueError):
            continue
    return tuple(out)


def _probe_codex(
    record: ProviderRecord, now: float
) -> tuple[UsageSnapshot | None, str | None]:
    """Probe the most recent codex session's rate_limits event. [VERIFY-AT-IMPL]."""
    session = _latest_codex_session(record)
    if session is None:
        return None, "probe-failed"
    try:
        lines = session.read_text(encoding="utf-8").splitlines()
    except OSError:
        return None, "probe-failed"
    # Scan newest-first for the last event carrying rate_limits.
    for line in reversed(lines):
        line = line.strip()
        if not line or "rate_limits" not in line:
            continue
        try:
            payload = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            continue
        windows = _parse_codex_rate_limits(payload)
        if windows:
            return UsageSnapshot(
                provider_id=record.id,
                windows=windows,
                probed_at=now,
                source="session-events",
            ), None
    return None, "probe-failed"


# Each probe returns ``(snapshot, unknown_reason)``: the reason is decided where
# the failure happens, because only the probe knows whether it ever issued a
# request. Deriving it afterwards from a bare None is what mislabels a rejected
# credential as an endpoint failure.
_PROBES: dict[
    str, Callable[[ProviderRecord, float], "tuple[UsageSnapshot | None, str | None]"]
] = {
    "claude": _probe_claude,
    "codex": _probe_codex,
}


def probe_usage_detail(
    record: ProviderRecord, now: float | None = None
) -> tuple[UsageSnapshot | None, str | None]:
    """``(snapshot, unknown_reason)`` - the probe plus WHY it came back unknown.

    The single implementation; :func:`probe_usage` is the compatibility wrapper
    over it, so there is exactly one probe path to stub, guard, or reason about.

    A bare ``None`` is one value with four causes, and telling them apart is the
    difference between "repair attribution" and "the endpoint moved". A five-day
    quota outage looked exactly like a cold cache because both printed
    ``unknown``. The reason is a stable, non-secret slug - never a token, a
    bearer, or a remote response body:

    - ``unattributed``       - no credential provably belongs to this record.
      Covers both the pre-probe refusal AND a probe that rejected every
      candidate bearer, since in neither case was a usage request issued.
    - ``harness-unsupported``- no probe registered for ``record.harness``
    - ``probe-failed``       - the probe issued a request and could not read usage
    - ``probe-error``        - the probe raised (contained here, AC1-FR)

    ``reason`` is None exactly when ``snapshot`` is not None.
    """
    if now is None:
        now = time.time()
    if not _attributed_credential_dir(record)[0]:
        # A tainted slot may be a FALSE taint (the five-day outage). Ask once
        # whether identity can be proven, then re-read attribution - a proven
        # slot may well belong to a different record than this one, in which
        # case this record correctly stays unknown and that one becomes
        # probeable on its own probe.
        if not _reconcile_tainted_slot(record, now):
            return None, "unattributed"
        if not _attributed_credential_dir(record)[0]:
            return None, "unattributed"
    probe = _PROBES.get(record.harness)
    if probe is None:
        return None, "harness-unsupported"
    try:
        snapshot, reason = probe(record, now)
    except Exception as exc:  # noqa: BLE001 - crash containment boundary (AC1-FR)
        logger.debug("usage probe crashed for %r: %s", record.id, exc)
        return None, "probe-error"
    if snapshot is None:
        return None, reason or "probe-failed"
    return snapshot, None


def probe_usage(record: ProviderRecord, now: float | None = None) -> UsageSnapshot | None:
    """Return a fresh usage snapshot for ``record``, or None if unknown.

    Compatibility wrapper over :func:`probe_usage_detail` for callers that only
    need the snapshot. Dispatches by ``record.harness``. NEVER raises: any
    exception inside a per-CLI probe is contained (AC1-FR), logged once at debug,
    and mapped to None so a dispatch decision proceeds fail-open. api_key records
    and CLIs without a probe (gemini, glm, openclaw, hermes) return None in v1.

    The gate is attribution, not auth strategy: a record is probeable when a
    credential provably its own is resolvable (see
    :func:`_attributed_credential_dir`). The old ``auth != "oauth_dir"`` refusal
    made every ``managed`` account permanently UNKNOWN - the whole reason
    ``fno config accounts usage`` printed ``unknown`` at exit 0 while the endpoint,
    the bearer discovery, and the parser all worked.
    """
    return probe_usage_detail(record, now)[0]
