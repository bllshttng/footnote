"""OpenRouter benchmark snapshot + model reachability for tier-based routing.

``fno providers benchmarks refresh`` caches OpenRouter's coding benchmark scores
to ``benchmarks.json`` (resolved via :func:`fno.paths.benchmarks_json`); ``show``
renders it with a staleness warning. The snapshot is the single routing source of
truth: tier resolution (a separate step) reads it, never the network, so a stale
or absent snapshot degrades to the static table below rather than blocking a
dispatch.

A model is only routable if it maps to an installed harness AND a concrete
``--model`` value; that reachability mapping (and a curated static fallback tier
table for when no snapshot exists) live here too. Refresh fails LOUD on any
network/auth error and never leaves a truncated file (temp write + atomic
rename). Authentication uses the ``OPENROUTER_API_KEY`` env var (OpenRouter's own
convention) rather than a new config field: a provider record that exports that
env satisfies it automatically.
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


# Reachability: benchmark model name -> (provider harness, --model value). A row
# absent here is invisible to routing (unmapped -> unreachable); name-massaging
# lives ONLY here, never downstream. GLM routes on the claude harness via the
# z.ai secondary lane (the GLM routing work owns that flag mapping). These are
# curated defaults an operator edits, not an exhaustive registry.
REACHABILITY: dict[str, tuple[str, str]] = {
    "claude-opus-4-8": ("claude", "claude-opus-4-8"),
    "claude-sonnet-5": ("claude", "claude-sonnet-5"),
    "claude-haiku-4-5": ("claude", "claude-haiku-4-5"),
    "glm-5.2": ("claude", "glm-5.2"),
    "glm-4.7": ("claude", "glm-4.7"),
    "glm-4.5-air": ("claude", "glm-4.5-air"),
    "gpt-5.5": ("codex", "gpt-5.5"),
    "gpt-5.4": ("codex", "gpt-5.4"),
}

# Static fallback tier bands (curated) used ONLY when no snapshot exists, so tier
# resolution still works offline / on a virgin install: routing degrades, never
# blocks. The resolver picks the cheapest reachable model within a band; order
# here is not significant.
STATIC_TIERS: dict[str, list[str]] = {
    # gpt-5.5 is the current codex flagship (high); gpt-5.4, the prior flagship,
    # sits a band down (medium). Both route on the codex harness.
    "high": ["claude-opus-4-8", "gpt-5.5"],
    "medium": ["claude-sonnet-5", "gpt-5.4", "glm-5.2"],
    "low": ["glm-4.7", "glm-4.5-air", "claude-haiku-4-5"],
}


def reachable(name: str) -> Optional[tuple[str, str]]:
    """Return ``(provider, model)`` for a benchmark model name, or None if unmapped."""
    return REACHABILITY.get(name)


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
