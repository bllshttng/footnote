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
import json
import logging
import os
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable

from fno.adapters.providers.model import ProviderRecord

logger = logging.getLogger(__name__)

PROBE_TIMEOUT_SECONDS = 5

# [VERIFY-AT-IMPL] The claude OAuth usage endpoint orca's `claude-usage` module
# and Claude Code's own `/usage` read. Confirm host, path, auth header shape,
# and response schema against a real account before merge.
_CLAUDE_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"


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

    The token lives at ``<credentials_source>/.credentials.json`` (Claude
    Code's store) and rotates on CLI refresh, so it is read fresh per probe
    and never cached. Returns None on any failure (missing dir, keychain-only
    auth in a headless shell, malformed JSON, missing key).
    """
    src = record.credentials_source
    if src is None:
        return None
    # [VERIFY-AT-IMPL] credentials.json layout: {"claudeAiOauth": {"accessToken": ...}}
    cred_path = Path(src) / ".credentials.json"
    try:
        raw = json.loads(cred_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if not isinstance(raw, dict):
        return None
    oauth = raw.get("claudeAiOauth")
    if isinstance(oauth, dict):
        token = oauth.get("accessToken")
        if isinstance(token, str) and token:
            return token
    return None


def _parse_claude_windows(payload: Any) -> tuple[UsageWindow, ...]:
    """Parse the claude usage payload into windows. [VERIFY-AT-IMPL] shape.

    Expected shape (confirm against a real account): a ``windows`` list of
    ``{label, utilization | used_pct, resets_at | reset_at}``. A malformed
    entry is skipped, not raised - drift degrades to fewer/no windows.
    """
    if not isinstance(payload, dict):
        return ()
    raw_windows = payload.get("windows")
    if not isinstance(raw_windows, list):
        return ()
    out: list[UsageWindow] = []
    for entry in raw_windows:
        if not isinstance(entry, dict):
            continue
        label = entry.get("label") or entry.get("name")
        pct = entry.get("used_pct")
        if pct is None:
            pct = entry.get("utilization")
        resets = entry.get("resets_at")
        if resets is None:
            resets = entry.get("reset_at")
        try:
            out.append(
                UsageWindow(
                    label=str(label) if label else "window",
                    used_pct=float(pct),
                    resets_at=float(resets),
                )
            )
        except (TypeError, ValueError):
            continue
    return tuple(out)


def _probe_claude(record: ProviderRecord, now: float) -> UsageSnapshot | None:
    """Probe the claude OAuth usage endpoint. [VERIFY-AT-IMPL] endpoint + auth."""
    bearer = _read_claude_bearer(record)
    if not bearer:
        return None
    req = urllib.request.Request(
        _CLAUDE_USAGE_URL,
        headers={
            "Authorization": f"Bearer {bearer}",
            "anthropic-beta": "oauth-2025-04-20",
            "User-Agent": "fno-quota-probe",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=PROBE_TIMEOUT_SECONDS) as resp:
            body = resp.read()
    except (urllib.error.URLError, OSError, TimeoutError):
        return None
    try:
        payload = json.loads(body)
    except (json.JSONDecodeError, ValueError):
        return None
    windows = _parse_claude_windows(payload)
    return UsageSnapshot(
        provider_id=record.id,
        windows=windows,
        probed_at=now,
        source="oauth-endpoint",
    )


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


def _parse_codex_rate_limits(payload: Any) -> tuple[UsageWindow, ...]:
    """Parse a codex ``rate_limits`` payload into windows. [VERIFY-AT-IMPL].

    Expected shape (confirm against a live codex): a ``rate_limits`` object
    with ``primary`` (~5h) and ``secondary`` (weekly) sub-objects, each
    ``{used_percent, resets_in_seconds}``. Offsets are converted to absolute
    epochs against the event's own timestamp when present, else ``now``.
    """
    if not isinstance(payload, dict):
        return ()
    rl = payload.get("rate_limits")
    if not isinstance(rl, dict):
        return ()
    base_ts = payload.get("ts")
    try:
        anchor = float(base_ts) if base_ts is not None else time.time()
    except (TypeError, ValueError):
        anchor = time.time()
    out: list[UsageWindow] = []
    for key, label in (("primary", "5h"), ("secondary", "weekly")):
        sub = rl.get(key)
        if not isinstance(sub, dict):
            continue
        pct = sub.get("used_percent")
        resets_in = sub.get("resets_in_seconds")
        if pct is None or resets_in is None:
            continue
        try:
            out.append(
                UsageWindow(
                    label=label,
                    used_pct=float(pct),
                    resets_at=anchor + float(resets_in),
                )
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
