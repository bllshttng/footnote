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
# The API's window keys mapped to our short labels. Order fixed for stability.
_CLAUDE_WINDOW_LABELS = (("five_hour", "5h"), ("seven_day", "weekly"))


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
    records how the reading was obtained for the ``fno providers usage``
    display and for debugging drift.
    """

    provider_id: str
    windows: tuple[UsageWindow, ...]
    probed_at: float
    source: str  # "oauth-endpoint" | "session-events" | ...


# ---------------------------------------------------------------------------
# Per-CLI probes. Each returns a snapshot or None (unknown). Registered by
# record.cli, mirroring the runtime adapter dispatch. A CLI with no probe
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


def _claude_bearer_candidates(record: ProviderRecord) -> list[str]:
    """All candidate OAuth bearer tokens for ``record``, in preference order.

    Claude Code stores the token in a ``<credentials_source>/.credentials.json``
    file (Linux / symlinked setups) OR the macOS Keychain (the darwin default,
    where no file exists - the reason a file-only read returned None here). The
    Keychain item is scoped by config dir (``Claude Code-credentials-<sha256[:8]>``)
    with the unscoped ``Claude Code-credentials`` as fallback. BOTH can exist,
    and a stale scoped item yields a 401 while the unscoped one is live - so we
    return every distinct token and let the probe try each until one works. All
    read fresh per probe (tokens rotate); never cached, never logged.
    """
    tokens: list[str] = []
    seen: set[str] = set()

    def _add(tok: str | None) -> None:
        if tok and tok not in seen:
            seen.add(tok)
            tokens.append(tok)

    src = record.credentials_source
    if src is not None:
        try:
            _add(_token_from_blob((Path(src) / ".credentials.json").read_text(encoding="utf-8")))
        except OSError:
            pass
    for blob in _read_claude_keychain_blobs(src):
        _add(_token_from_blob(blob))
    return tokens


def _read_claude_keychain_blobs(config_dir: Path | None) -> list[str]:
    """Return the raw credential blobs from every candidate Keychain item.

    Tries the config-dir-scoped item first, then the unscoped one (orca's
    ordering). Returns BOTH when both exist (a stale scoped + a live unscoped is
    the observed reality). Non-darwin or a denied access prompt yields [].
    """
    if os.uname().sysname != "Darwin":
        return []
    account = os.environ.get("USER") or os.environ.get("USERNAME") or "user"
    services: list[str] = []
    if config_dir is not None:
        suffix = hashlib.sha256(str(config_dir).encode()).hexdigest()[:8]
        services.append(f"{_CLAUDE_KEYCHAIN_SERVICE}-{suffix}")
    services.append(_CLAUDE_KEYCHAIN_SERVICE)
    blobs: list[str] = []
    for service in services:
        try:
            out = subprocess.run(
                ["security", "find-generic-password", "-s", service, "-a", account, "-w"],
                capture_output=True, text=True, timeout=5,
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if out.returncode == 0 and out.stdout.strip():
            blobs.append(out.stdout.strip())
    return blobs


def _iso_to_epoch(value: Any) -> float | None:
    """Parse an ISO-8601 string (or a bare epoch) to unix epoch seconds, or None.

    The claude usage API returns ``resets_at`` as an ISO-8601 string
    (``2026-07-12T02:09:59.521372+00:00``); accept a numeric epoch too for
    forward-safety if the shape ever changes.
    """
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str) and value:
        try:
            return datetime.fromisoformat(value).timestamp()
        except ValueError:
            return None
    return None


def _parse_claude_windows(payload: Any) -> tuple[UsageWindow, ...]:
    """Parse the claude ``/api/oauth/usage`` payload into windows.

    Verified live (x-6bcf): the payload has top-level window OBJECTS keyed by
    name (``five_hour``, ``seven_day``), each ``{utilization: float already on a
    0-100 scale, resets_at: ISO-8601 string, ...dollar fields}``. A window whose
    object is absent/null or missing either field is skipped (drift degrades to
    fewer/no windows, never a raise).
    """
    if not isinstance(payload, dict):
        return ()
    out: list[UsageWindow] = []
    for api_key, label in _CLAUDE_WINDOW_LABELS:
        w = payload.get(api_key)
        if not isinstance(w, dict):
            continue
        util = w.get("utilization")
        epoch = _iso_to_epoch(w.get("resets_at"))
        if util is None or epoch is None:
            continue
        try:
            out.append(UsageWindow(label=label, used_pct=float(util), resets_at=epoch))
        except (TypeError, ValueError):
            continue
    return tuple(out)


def _probe_claude(record: ProviderRecord, now: float) -> UsageSnapshot | None:
    """Probe the claude ``/api/oauth/usage`` endpoint (verified x-6bcf).

    Tries every candidate bearer token until one returns 200: a stale scoped
    Keychain item 401s while the live unscoped item succeeds, so a single-token
    probe would silently fail. A 401/403 skips to the next token; any other
    network error aborts (fail-open None).
    """
    for bearer in _claude_bearer_candidates(record):
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
            return None
        except (urllib.error.URLError, OSError, TimeoutError):
            return None
        try:
            payload = json.loads(body)
        except (json.JSONDecodeError, ValueError):
            return None
        return UsageSnapshot(
            provider_id=record.id,
            windows=_parse_claude_windows(payload),
            probed_at=now,
            source="oauth-endpoint",
        )
    return None


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


def _probe_codex(record: ProviderRecord, now: float) -> UsageSnapshot | None:
    """Probe the most recent codex session's rate_limits event. [VERIFY-AT-IMPL]."""
    session = _latest_codex_session(record)
    if session is None:
        return None
    try:
        lines = session.read_text(encoding="utf-8").splitlines()
    except OSError:
        return None
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
            )
    return None


_PROBES: dict[str, Callable[[ProviderRecord, float], "UsageSnapshot | None"]] = {
    "claude": _probe_claude,
    "codex": _probe_codex,
}


def probe_usage(record: ProviderRecord, now: float | None = None) -> UsageSnapshot | None:
    """Return a fresh usage snapshot for ``record``, or None if unknown.

    Dispatches by ``record.cli``. NEVER raises: any exception inside a per-CLI
    probe is contained here (AC1-FR), logged once at debug, and mapped to None
    so a dispatch decision proceeds fail-open. api_key records and CLIs without
    a probe (gemini, glm, openclaw, hermes) return None in v1.
    """
    if now is None:
        now = time.time()
    if record.auth != "oauth_dir":
        return None
    probe = _PROBES.get(record.cli)
    if probe is None:
        return None
    try:
        return probe(record, now)
    except Exception as exc:  # noqa: BLE001 - crash containment boundary (AC1-FR)
        logger.debug("usage probe crashed for %r: %s", record.id, exc)
        return None
