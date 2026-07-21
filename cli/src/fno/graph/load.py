"""graph/load.py - Hash-validated graph reader.

Public API:
    load_graph(path)    - Read graph.json with SHA256 sidecar validation.
    GraphCorruptionError - Raised on hash mismatch.

The sidecar lives at {path}.sha256.  On first run (sidecar absent), load_graph
writes the sidecar lazily so subsequent reads are validated.
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
import time
from pathlib import Path

from fno.graph._constants import GRAPH_JSON

# The graph and its sidecar are two sequential atomic replaces under the write
# lock; a lock-free reader can land between them and see new graph bytes against
# the old sidecar. Re-read BOTH files a bounded number of times before raising:
# the window is milliseconds, so a retry lands consistent, while a genuine
# corruption still raises once the attempts are spent. Bounded, never a
# wait-until-consistent loop: worst case is (_ATTEMPTS - 1) * _SLEEP_S.
_RETRY_ATTEMPTS = 5
_RETRY_SLEEP_S = 0.01


class GraphCorruptionError(Exception):
    """Raised when graph.json SHA256 does not match the stored sidecar hash.

    Attributes:
        path     - Path to graph.json
        actual   - SHA256 hex digest of the on-disk bytes
        expected - SHA256 hex digest stored in the sidecar
        hint     - Human-readable recovery instruction
    """

    def __init__(self, path: Path, actual: str, expected: str, hint: str | None = None):
        self.path = path
        self.actual = actual
        self.expected = expected
        self.hint = hint or (
            "Run `fno backlog rehash` to acknowledge + rehash, "
            "or `fno backlog rehash --revert` to restore from latest backup."
        )
        super().__init__(
            f"graph.json hash mismatch at {path}: "
            f"expected {expected[:8]}, got {actual[:8]}. {self.hint}"
        )


def _sha256_file(path: Path) -> str:
    """Return SHA256 hex digest of file contents."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _sidecar_path(path: Path) -> Path:
    """Return the .sha256 sidecar path for a graph.json path."""
    return Path(str(path) + ".sha256")


def _is_sha256(s: str) -> bool:
    """True for a well-formed 64-char lowercase-hex digest.

    A sidecar that is not one (empty, truncated, garbage) carries no usable
    baseline, so it is treated as absent rather than as evidence of corruption.
    """
    if len(s) != 64:
        return False
    try:
        int(s, 16)
    except ValueError:
        return False
    return True


def load_graph(path: Path | None = None) -> list[dict]:
    """Read and validate graph.json against its SHA256 sidecar.

    Behavior:
    - If graph.json does not exist: returns [].
    - If sidecar is absent (first run): writes sidecar with current hash,
      returns parsed entries (trusting the file on first contact).
    - If sidecar present and matches: returns parsed entries.
    - If sidecar present and mismatches: raises GraphCorruptionError.

    Args:
        path: Path to graph.json. Defaults to ~/.fno/graph.json.

    Returns:
        List of graph entry dicts (raw, without defaults applied).
    """
    if path is None:
        path = GRAPH_JSON

    if not path.exists():
        return []

    sidecar = _sidecar_path(path)
    actual_hash = expected_hash = ""
    for attempt in range(_RETRY_ATTEMPTS):
        # Re-read BOTH files every attempt: caching either would freeze the
        # mismatch and convert a transient window into a guaranteed raise.
        raw_bytes = path.read_bytes()
        actual_hash = hashlib.sha256(raw_bytes).hexdigest()

        sidecar_present = sidecar.exists()
        expected_hash = sidecar.read_text().strip() if sidecar_present else ""
        if not _is_sha256(expected_hash):
            # Absent, empty, or truncated sidecar: no baseline to validate
            # against, so trust the file and (re)write the sidecar -- the same
            # first-contact stance as before, NOT graph corruption. But a sidecar
            # that EXISTS yet is not a valid digest is anomalous (a damaged or
            # partially-written sidecar disables corruption detection), so warn
            # before re-blessing it -- unlike a legitimately-absent first run.
            if sidecar_present:
                print(
                    f"Warning: {sidecar} is present but not a valid sha256; "
                    f"rewriting from current graph bytes (corruption detection was disabled)",
                    file=sys.stderr,
                )
            sidecar.write_text(actual_hash + "\n")
            return _entries(json.loads(raw_bytes))

        if actual_hash == expected_hash:
            return _entries(json.loads(raw_bytes))

        # Mismatch: likely the two-write window. Retry after a short sleep.
        if attempt < _RETRY_ATTEMPTS - 1:
            if os.environ.get("FNO_DEBUG"):
                print(
                    f"load_graph: hash mismatch on {path} (attempt {attempt + 1}), retrying",
                    file=sys.stderr,
                )
            time.sleep(_RETRY_SLEEP_S)

    raise GraphCorruptionError(path, actual_hash, expected_hash)


def _entries(data: object) -> list[dict]:
    """Extract the entry list, folding the pre-rename `_status` key into `status`.

    A key rename, not a default: raw callers here bypass ``_apply_graph_defaults``
    but must still read a graph.json an older fno wrote.
    """
    entries = data.get("entries", []) if isinstance(data, dict) else []
    for e in entries:
        if isinstance(e, dict) and "_status" in e:
            e.setdefault("status", e["_status"])
            del e["_status"]
    return entries


def query_by_source_inbox_msg(msg_id: str, path: Path | None = None) -> list[dict]:
    """Return entries whose source_inbox_msg matches msg_id.

    Uses read_graph (defaults applied) so provenance fields are guaranteed
    to be present even on legacy entries written before Phase 01.
    """
    from fno.graph.store import read_graph

    entries = read_graph(path) if path is not None else read_graph()
    return [e for e in entries if e.get("source_inbox_msg") == msg_id]
