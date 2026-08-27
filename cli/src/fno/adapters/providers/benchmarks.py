"""OpenRouter benchmark snapshot + model reachability for tier-based routing.

``fno config accounts benchmarks refresh`` caches OpenRouter's coding benchmark scores
to ``benchmarks.json`` (resolved via :func:`fno.paths.benchmarks_json`); ``show``
renders it with a staleness warning. The snapshot is OPTIONAL enrichment, never
the foundation: it may supply a percentile that derives a band for a declared
row whose ``band`` the operator left unset, and its absence never makes routing
inert. A percentile cannot say how to invoke a model on anybody's machine, so
the invocation facts live in the declared inventory (``config.routing.models``)
and the hardcoded reachability/tier tables are gone - adding a model is a config
edit, not a source edit. Refresh fails LOUD on any network/auth error and never
leaves a truncated file (temp write + atomic rename). Authentication uses the
``OPENROUTER_API_KEY`` env var (OpenRouter's own convention) rather than a new
config field: a provider record that exports that env satisfies it automatically.
"""
from __future__ import annotations

import json
import os
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Mapping, Optional

from fno.paths import benchmarks_json

OPENROUTER_BENCHMARKS_URL = "https://openrouter.ai/api/v1/benchmarks"
_TASK_TYPE = "coding"
STALE_AFTER_SECONDS = 14 * 24 * 3600
_API_KEY_ENV = "OPENROUTER_API_KEY"


class BenchmarkError(RuntimeError):
    """A benchmark refresh/read failed loudly (never a silent/truncated file)."""


# FALLBACK TABLE, never the authority. `config.routing.models` overrides any row
# here per field and extends the set with models it never named, so adding a
# model is a config edit and not an edit to this file. Kept because a tier
# request must still resolve on an install that declares nothing.
#
# Reachability: benchmark model name -> (provider harness, --model value). A row
# absent here is invisible to routing (unmapped -> unreachable); name-massaging
# lives ONLY here, never downstream. GLM routes on the claude harness via the
# z.ai secondary lane (the GLM routing work owns that flag mapping). These are
# curated defaults an operator edits, not an exhaustive registry.
#
# Every id here was verified against a configured provider at implementation
# time (2026-08-26): the codex ids against the codex app-server model surface,
# the GLM spellings against the z.ai lane (glm-5.3[1m] is the 1M-context
# suffix form that lane serves), the claude ids against the harness's own
# current model-id set. Two operator-named candidates were OMITTED because no
# configured provider serves an id matching them: `gemini-3.7-flash` (the
# google surface here tops out at antigravity-gemini-3.x and gemini-2.5) and
# opencode's "0x Alpha Free" tier (a marketing name; opencode's real free ids
# are hy3-free, mimo-v2.5-free, muse-spark-1.2-contributor-free,
# nemotron-3-ultra-free, nemotron-3.5-lightning-free - none carries the 0x
# or alpha name). The omission is the rule working: an unverifiable id is
# never guessed into a routing table.
REACHABILITY: dict[str, tuple[str, str]] = {
    "claude-fable-5": ("claude", "claude-fable-5"),
    "claude-opus-5": ("claude", "claude-opus-5"),
    "claude-sonnet-5": ("claude", "claude-sonnet-5"),
    "claude-haiku-4-5": ("claude", "claude-haiku-4-5"),
    "glm-5.3[1m]": ("claude", "glm-5.3[1m]"),
    "glm-4.7": ("claude", "glm-4.7"),
    "gpt-5.6-sol": ("codex", "gpt-5.6-sol"),
    "gpt-5.6-terra": ("codex", "gpt-5.6-terra"),
    "gpt-5.6-luna": ("codex", "gpt-5.6-luna"),
}

# Static fallback tier bands (curated) used ONLY when no snapshot exists, so tier
# resolution still works offline / on a virgin install: routing degrades, never
# blocks. The resolver picks the cheapest reachable model within a band; order
# here is not significant. `max` is the deliberate exception: a max request
# takes the STRONGEST reachable model, not the cheapest that clears (see
# route_resolve.resolve_tier). The previous generation of this table (gpt-5.5 /
# gpt-5.4 / claude-opus-4-8) drifted a full model release with nothing
# detecting it; the staleness test in cli/tests/unit/test_tier_table.py is the
# tripwire that fires when it drifts again.
STATIC_TIERS: dict[str, list[str]] = {
    "max": ["claude-fable-5", "gpt-5.6-sol"],
    "high": ["claude-opus-5", "gpt-5.6-sol"],
    "medium": ["claude-sonnet-5", "glm-5.3[1m]", "gpt-5.6-terra"],
    "low": ["glm-4.7", "claude-haiku-4-5", "gpt-5.6-luna"],
}


def reachable(name: str) -> Optional[tuple[str, str]]:
    """Return ``(harness, model)`` for a DECLARED inventory row, or None.

    The declared inventory (``config.routing.models``) is the only reachability
    source: a model nobody declared is invisible to routing, and the surface
    that answers "what can this installation reach" is ``fno doctor route``.
    Lazily imports the resolver so this module stays importable from it.
    """
    from fno.route_resolve import resolve_inventory

    row = resolve_inventory().rows.get(name)
    if row is None or not row.harness or not row.model:
        return None
    return (row.harness, row.model)


def unreachable_tier_ids(
    tiers: Optional[Mapping[str, list]] = None,
    reach: Optional[Mapping[str, tuple[str, str]]] = None,
) -> list[str]:
    """Tier-table ids no reachability row serves, listed BY NAME.

    A tier naming a model nobody mapped resolves to a fallback nobody chose,
    which is the failure class a full model generation of silent drift
    produced. Pure: both arguments exist so a test can plant a dead id in a
    copy and watch the flag fire by name (the positive control).
    """
    table = tiers if tiers is not None else STATIC_TIERS
    rows = reach if reach is not None else REACHABILITY
    seen: set[str] = set()
    flagged: list[str] = []
    for names in table.values():
        for name in names:
            if name in seen:
                continue
            seen.add(name)
            if name not in rows:
                flagged.append(name)
    return sorted(flagged)


def empty_bands_for_harness(
    harnesses: tuple[str, ...] = ("claude", "codex"),
    tiers: Optional[Mapping[str, list]] = None,
    reach: Optional[Mapping[str, tuple[str, str]]] = None,
) -> dict[str, list[str]]:
    """Bands whose static tier resolves to NOTHING for a harness, per harness.

    The doctor-line shape: a band an entire provider cannot serve is a
    routing hole an operator must see, not discover at dispatch.

    Reads the STATIC reachability map, not :func:`reachable`, and takes
    ``reach`` for the same reason ``unreachable_tier_ids`` does. Both answer
    one question - does the curated tier table line up with the curated
    reachability map - and that question is about the static tables alone. Its
    sibling already read the map directly; this one called ``reachable``, which
    became inventory-backed and made every band read as a hole wherever no
    inventory is declared.
    """
    table = tiers if tiers is not None else STATIC_TIERS
    rows = reach if reach is not None else REACHABILITY
    out: dict[str, list[str]] = {}
    for harness in harnesses:
        empty = [
            band
            for band, names in table.items()
            if not any(
                (row := rows.get(n)) is not None and row[0] == harness
                for n in names
            )
        ]
        if empty:
            out[harness] = sorted(empty)
    return out


def _api_key(env: Optional[Mapping[str, str]] = None) -> str:
    key = ((env if env is not None else os.environ).get(_API_KEY_ENV) or "").strip()
    if not key:
        raise BenchmarkError(
            f"no OpenRouter API key: set {_API_KEY_ENV} to refresh benchmarks"
        )
    return key


def _parse_models(payload: object) -> list[dict]:
    """Extract ``[{name, coding_percentile}]`` from the OpenRouter response."""
    rows = payload.get("models") if isinstance(payload, dict) else payload
    if not isinstance(rows, list):
        raise BenchmarkError("OpenRouter benchmarks response has no model list")
    out: list[dict] = []
    for r in rows:
        if not isinstance(r, dict):
            continue
        name = r.get("name") or r.get("model") or r.get("id")
        if not name:
            continue
        out.append(
            {
                "name": str(name),
                "coding_percentile": r.get("coding_percentile", r.get("percentile")),
            }
        )
    if not out:
        raise BenchmarkError("OpenRouter benchmarks response had zero usable models")
    return out


def refresh(
    *,
    path: Optional[Path] = None,
    url: str = OPENROUTER_BENCHMARKS_URL,
    timeout: float = 30,
    env: Optional[Mapping[str, str]] = None,
    opener: Optional[Callable] = None,
    now: Optional[float] = None,
) -> dict:
    """Fetch the coding benchmark snapshot and write it atomically. Fails loud.

    ``opener``/``now`` are injection seams for tests; production passes neither.
    A 429 fails loud with a retry hint (no auto-retry loop: at fortnightly cadence
    a manual re-run IS the backoff).
    """
    key = _api_key(env)
    req = urllib.request.Request(
        f"{url}?task_type={_TASK_TYPE}",
        headers={"Authorization": f"Bearer {key}"},
    )
    _open = opener or (lambda r, timeout: urllib.request.urlopen(r, timeout=timeout))
    try:
        resp = _open(req, timeout=timeout)
        raw = resp.read() if hasattr(resp, "read") else resp
    except urllib.error.HTTPError as exc:
        if exc.code == 429:
            raise BenchmarkError(
                "OpenRouter rate limit (HTTP 429); retry the refresh later"
            ) from exc
        raise BenchmarkError(
            f"OpenRouter benchmarks fetch failed: HTTP {exc.code}"
        ) from exc
    except urllib.error.URLError as exc:
        raise BenchmarkError(
            f"OpenRouter benchmarks fetch failed: {exc.reason}"
        ) from exc
    try:
        payload = json.loads(raw)
    except (ValueError, TypeError) as exc:
        raise BenchmarkError(
            f"OpenRouter benchmarks response was not JSON: {exc}"
        ) from exc

    models = _parse_models(payload)
    ts = now if now is not None else time.time()
    snapshot = {
        "fetched_at": datetime.fromtimestamp(ts, timezone.utc).isoformat(),
        "source": url,
        "models": models,
    }
    _write_atomic(snapshot, path)
    return snapshot


def _write_atomic(snapshot: dict, path: Optional[Path] = None) -> Path:
    dest = Path(path) if path is not None else benchmarks_json()
    dest.parent.mkdir(parents=True, exist_ok=True)
    # temp + atomic rename: a reader never sees a half-written or truncated file,
    # and concurrent refreshes are last-writer-wins rather than corrupting.
    tmp = dest.with_name(f"{dest.name}.tmp.{os.getpid()}")
    tmp.write_text(json.dumps(snapshot, indent=2) + "\n", encoding="utf-8")
    os.replace(tmp, dest)
    return dest


def load_snapshot(path: Optional[Path] = None) -> Optional[dict]:
    """Return the cached snapshot, or None when absent/unreadable/invalid.

    A snapshot missing ``fetched_at`` or ``source`` is invalid and ignored.
    """
    src = Path(path) if path is not None else benchmarks_json()
    try:
        data = json.loads(src.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict) or not data.get("fetched_at") or not data.get("source"):
        return None
    return data


def staleness_seconds(snapshot: dict, *, now: Optional[float] = None) -> Optional[float]:
    """Age of the snapshot in seconds, or None if ``fetched_at`` is unparseable."""
    try:
        fetched = datetime.fromisoformat(snapshot["fetched_at"]).timestamp()
    except (KeyError, ValueError, TypeError):
        return None
    ts = now if now is not None else time.time()
    return max(0.0, ts - fetched)


def is_stale(snapshot: dict, *, now: Optional[float] = None) -> bool:
    age = staleness_seconds(snapshot, now=now)
    return age is not None and age > STALE_AFTER_SECONDS
