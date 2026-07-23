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
Fall back to ``str(pr_number)`` when slug is None.

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
import tempfile
from pathlib import Path
from typing import Optional

from fno.paths import state_dir

log = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Key helper
# ---------------------------------------------------------------------------


def make_watermark_key(*, repo_slug: Optional[str], pr_number: int) -> str:
    """Return the watermark dict key for a single PR.

    Uses ``"{slug}#{n}"`` when slug is available; falls back to ``str(n)``
    when the slug is None (e.g. unparseable PR URL).
    """
    if repo_slug:
        return f"{repo_slug}#{pr_number}"
    return str(pr_number)


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
