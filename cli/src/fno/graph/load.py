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
from pathlib import Path

from fno.graph._constants import GRAPH_JSON


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

    raw_bytes = path.read_bytes()
    actual_hash = hashlib.sha256(raw_bytes).hexdigest()
    sidecar = _sidecar_path(path)

    if not sidecar.exists():
        # First run: write sidecar lazily, trust the file
        sidecar.write_text(actual_hash + "\n")
        data = json.loads(raw_bytes)
        return data.get("entries", []) if isinstance(data, dict) else []

    expected_hash = sidecar.read_text().strip()
    if actual_hash != expected_hash:
        raise GraphCorruptionError(path, actual_hash, expected_hash)

    data = json.loads(raw_bytes)
    return data.get("entries", []) if isinstance(data, dict) else []


def query_by_source_inbox_msg(msg_id: str, path: Path | None = None) -> list[dict]:
    """Return entries whose source_inbox_msg matches msg_id.

    Uses read_graph (defaults applied) so provenance fields are guaranteed
    to be present even on legacy entries written before Phase 01.
    """
    from fno.graph.store import read_graph

    entries = read_graph(path) if path is not None else read_graph()
    return [e for e in entries if e.get("source_inbox_msg") == msg_id]
