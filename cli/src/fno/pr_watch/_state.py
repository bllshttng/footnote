"""PR-state watcher: atomic watermark store.

Persists per-PR polling state to ``~/.fno/pr-watcher-state.json`` (default).
The path is injectable for tests.

Entry schema per key::

    {
        "last_review_ts": str | None,   # ISO-8601 ts of last dispatched review
        "last_seen_state": str,          # PR state at last observation ("OPEN", "MERGED", ...)
        "merge_dispatched": bool,        # True once /fno:pr merged was fired
        "retries": int,                  # consecutive dispatch failures
        "parked": str | None,            # non-None = reason we stopped polling
    }

Key format: ``"{repo_slug}#{pr_number}"`` (globally unique across repos).
Legacy bare-number keys are normalized or discarded on the next tick; new
writes require a repository slug.

Baseline discipline:
    A PR with NO existing entry is first-seen.  The caller (tick) records
    current state WITHOUT firing.  Fire only on a later OBSERVED TRANSITION.
    A corrupt/missing store resets to empty, and tick re-baselines all
    candidates from current gh state rather than mass-firing.
"""

from __future__ import annotations

import json
import logging
import os
import re
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

from fno.paths import state_dir

log = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Key helper
# ---------------------------------------------------------------------------


_QUALIFIED_KEY_RE = re.compile(
    r"^(?P<owner>[A-Za-z0-9_.-]+)/(?P<repo>[A-Za-z0-9_.-]+)#(?P<number>[1-9][0-9]*)$"
)


def make_watermark_key(*, repo_slug: Optional[str], pr_number: int) -> str:
    """Return the watermark dict key for a single PR.

    A repository slug is required because a bare PR number is not globally
    unique. Callers with an unparseable PR URL must skip the record rather than
    persist an ambiguous key.
    """
    if not repo_slug:
        raise ValueError("repo_slug is required for a PR watermark key")
    key = f"{repo_slug.lower()}#{pr_number}"
    if parse_watermark_key(key) is None:
        raise ValueError(f"invalid PR watermark key: {key}")
    return key


def parse_watermark_key(key: str) -> Optional[tuple[str, int]]:
    """Return ``(owner/repo, number)`` for a qualified watermark key."""
    if not isinstance(key, str):
        return None
    match = _QUALIFIED_KEY_RE.fullmatch(key)
    if match is None:
        return None
    return (
        f"{match.group('owner')}/{match.group('repo')}".lower(),
        int(match.group("number")),
    )


@dataclass(frozen=True)
class KeyNormalization:
    """Receipt for legacy key repair performed during a store load."""

    normalized: list[dict[str, str]]
    dropped: list[dict[str, Any]]


def _as_nonnegative_int(value: Any) -> int:
    try:
        return max(0, int(value or 0))
    except (TypeError, ValueError):
        return 0


def _merge_entries(legacy: dict, canonical: dict) -> dict:
    """Collapse duplicate watermark values without losing dispatched work."""
    merged = dict(legacy)
    merged.update(canonical)
    timestamps = [
        value
        for value in (legacy.get("last_review_ts"), canonical.get("last_review_ts"))
        if isinstance(value, str) and value
    ]
    merged["last_review_ts"] = max(timestamps) if timestamps else None
    merged["merge_dispatched"] = bool(
        legacy.get("merge_dispatched") or canonical.get("merge_dispatched")
    )
    merged["retries"] = max(
        _as_nonnegative_int(legacy.get("retries")),
        _as_nonnegative_int(canonical.get("retries")),
    )
    merged["parked"] = canonical.get("parked") or legacy.get("parked")
    return merged


# ---------------------------------------------------------------------------
# Default path resolver
# ---------------------------------------------------------------------------


def pr_watcher_state_path() -> Path:
    """Return the default path to the pr-watcher state JSON file.

    Mirrors graph_json() / ledger_json() style: delegates to state_dir()
    so the path follows any user-configured ``config.state_dir`` override
    (and the test HOME redirect in conftest.py keeps it out of ~/.fno).
    """
    return state_dir() / "pr-watcher-state.json"


# ---------------------------------------------------------------------------
# WatermarkStore
# ---------------------------------------------------------------------------


class WatermarkStore:
    """Thin atomic-JSON store for per-PR watcher watermarks.

    The in-memory state is loaded once on first access (lazy) and written
    back atomically via ``tmp + os.replace`` on every ``set()`` call.
    A corrupt or missing file is treated as an empty store -- the tick will
    re-baseline all PRs from their current gh state rather than mass-firing.

    All public methods are safe under concurrent ticks: the per-tick mutex
    (``pr-watch:tick`` claim) prevents concurrent ticks from the same
    daemon; the atomic write prevents partial reads from a file-level race
    on platforms where ``os.replace`` is atomic (POSIX).

    Parameters
    ----------
    path:
        Path to the JSON state file.  Defaults to
        ``pr_watcher_state_path()`` (i.e. ``~/.fno/pr-watcher-state.json``).
    """

    def __init__(self, path: Optional[Path] = None) -> None:
        self._path: Path = path if path is not None else pr_watcher_state_path()
        self._data: Optional[dict] = None  # lazy-loaded

    # ------------------------------------------------------------------
    # Public interface
    # ------------------------------------------------------------------

    def load(self) -> dict:
        """Return the full in-memory state dict, loading from disk if needed.

        Missing file -> returns ``{}``.
        Corrupt JSON  -> logs a warning, returns ``{}``, does NOT raise.
        """
        if self._data is not None:
            return self._data
        self._data = self._read_or_reset()
        return self._data

    def get(self, key: str) -> Optional[dict]:
        """Return the watermark entry for *key*, or None if absent."""
        return self.load().get(key)

    def set(self, key: str, entry: dict) -> None:
        """Upsert *entry* under *key* and persist atomically.

        Uses a tmp file + ``os.replace`` so readers never see a partial
        write.  The tmp file is created in the same directory as the state
        file (same filesystem) to guarantee the replace is atomic on POSIX.
        The tmp file is removed on failure so no garbage is left behind.
        """
        self.load()  # ensure _data is initialised
        assert self._data is not None
        self._data[key] = entry
        self._persist()

    def normalize_keys(self) -> KeyNormalization:
        """Rewrite legacy keys to globally-qualified keys in memory.

        A bare number is recoverable only when exactly one qualified twin
        already exists in the store. Anything else - ambiguous, orphaned, or
        matched only by a same-numbered current candidate - is discarded
        rather than guessed: a candidate from a different repository would
        otherwise inherit a transplanted parked or merge_dispatched record
        and be silently suppressed forever. The caller persists once after
        the measured sweep, avoiding one full-file rewrite per record.
        """
        data = self.load()
        qualified_by_number: dict[int, set[str]] = {}
        for raw_key in data:
            parsed = parse_watermark_key(raw_key)
            if parsed is None:
                continue
            slug, number = parsed
            canonical_key = make_watermark_key(repo_slug=slug, pr_number=number)
            qualified_by_number.setdefault(number, set()).add(canonical_key)

        normalized: list[dict[str, str]] = []
        dropped: list[dict[str, Any]] = []
        rebuilt: dict[str, dict] = {}
        bare: list[tuple[str, Any]] = []

        for raw_key, entry in data.items():
            parsed = parse_watermark_key(raw_key)
            if parsed is None:
                bare.append((raw_key, entry))
                continue
            if not isinstance(entry, dict):
                dropped.append({"key": raw_key, "reason": "invalid-entry", "state": "UNKNOWN"})
                continue
            slug, number = parsed
            canonical_key = make_watermark_key(repo_slug=slug, pr_number=number)
            if canonical_key in rebuilt:
                rebuilt[canonical_key] = _merge_entries(entry, rebuilt[canonical_key])
                normalized.append({"from": raw_key, "to": canonical_key})
            else:
                rebuilt[canonical_key] = dict(entry)
                if raw_key != canonical_key:
                    normalized.append({"from": raw_key, "to": canonical_key})

        for raw_key, entry in bare:
            if not isinstance(entry, dict):
                dropped.append({"key": raw_key, "reason": "invalid-entry", "state": "UNKNOWN"})
                continue
            if isinstance(raw_key, str) and raw_key.isdigit() and int(raw_key) > 0:
                targets = sorted(qualified_by_number.get(int(raw_key), set()))
                if len(targets) == 1:
                    target = targets[0]
                    rebuilt[target] = _merge_entries(entry, rebuilt.get(target, {}))
                    normalized.append({"from": raw_key, "to": target})
                    continue
                reason = "ambiguous-key" if len(targets) > 1 else "unresolvable-key"
            else:
                reason = "invalid-key"
            dropped.append(
                {
                    "key": raw_key,
                    "reason": reason,
                    "state": str(entry.get("last_seen_state") or "UNKNOWN"),
                }
            )

        data.clear()
        data.update(rebuilt)
        return KeyNormalization(normalized=normalized, dropped=dropped)

    def persist(self) -> None:
        """Persist the current in-memory snapshot atomically."""
        self.load()
        self._persist()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _read_or_reset(self) -> dict:
        if not self._path.exists():
            return {}
        try:
            text = self._path.read_text(encoding="utf-8")
            data = json.loads(text)
            if not isinstance(data, dict):
                raise ValueError("Root element is not a JSON object")
            return data
        except (json.JSONDecodeError, ValueError, OSError) as exc:
            log.warning(
                "pr-watcher-state.json is corrupt or unreadable (%s): %s -- "
                "resetting to empty; all PRs will be re-baselined this tick.",
                self._path,
                exc,
            )
            return {}

    def _persist(self) -> None:
        assert self._data is not None
        self._path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path: Optional[Path] = None
        try:
            fd, tmp_str = tempfile.mkstemp(
                dir=self._path.parent,
                prefix=".pr-watcher-state.tmp.",
            )
            tmp_path = Path(tmp_str)
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                json.dump(self._data, fh, indent=2)
                fh.write("\n")
            os.replace(tmp_path, self._path)
        except Exception:
            if tmp_path and tmp_path.exists():
                try:
                    tmp_path.unlink()
                except OSError:
                    pass
            raise
