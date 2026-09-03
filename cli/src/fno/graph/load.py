"""graph/load.py - Hash-validated graph reader.

Public API:
    load_graph(path)    - Read graph.json with SHA256 sidecar validation.
    GraphCorruptionError - Raised on hash mismatch.

The sidecar lives at {path}.sha256.  On first run (sidecar absent), load_graph
writes the sidecar lazily so subsequent reads are validated.

The retry loop this module used to carry is gone with the port: the store
keeper publishes the graph bytes and their sidecar as two atomic replaces
under one bounded lock, and `read_file_bytes` is served under that same
gate, so a reader can no longer observe new bytes against an old sidecar.
A hash mismatch now means real corruption (or an out-of-band editor), not
the transient two-write window that was measured at one false positive per
10k-15k loads on a loaded CI runner.
"""
from __future__ import annotations

import hashlib
import json
import sys
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


def load_graph(path: Path | None = None, *, keep_malformed: bool = False) -> list[dict]:
    """Read and validate graph.json against its SHA256 sidecar.

    Behavior:
    - If graph.json does not exist: returns [].
    - If sidecar is absent (first run): writes sidecar with current hash,
      returns parsed entries (trusting the file on first contact).
    - If sidecar present and matches: returns parsed entries.
    - If sidecar present and mismatches: raises GraphCorruptionError. No
      retry: the keeper's gated read already rules out the transient
      two-write window this check used to race against.

    Args:
        path: Path to graph.json. Defaults to ~/.fno/graph.json.

    Returns:
        List of graph entry dicts with the canonical migration/defaults pass
        applied (see :func:`_entries`) -- the same vocabulary ``read_graph``
        returns, not the raw on-disk rows.
    """
    if path is None:
        path = GRAPH_JSON

    if not path.exists():
        return []

    from fno.graph.store import read_file_bytes

    raw_bytes = read_file_bytes(Path(path))
    actual_hash = hashlib.sha256(raw_bytes).hexdigest()

    sidecar = _sidecar_path(path)
    expected_hash = ""
    if sidecar.exists():
        expected_hash = sidecar.read_text().strip()
    if not _is_sha256(expected_hash):
        # Absent, empty, or truncated sidecar: no baseline to validate
        # against, so trust the file and (re)write the sidecar -- the same
        # first-contact stance as before, NOT graph corruption. But a sidecar
        # that EXISTS yet is not a valid digest is anomalous (a damaged or
        # partially-written sidecar disables corruption detection), so warn
        # before re-blessing it -- unlike a legitimately-absent first run.
        if sidecar.exists():
            print(
                f"Warning: {sidecar} is present but not a valid sha256; "
                f"rewriting from current graph bytes (corruption detection was disabled)",
                file=sys.stderr,
            )
        sidecar.write_text(actual_hash + "\n")
        return _entries(json.loads(raw_bytes), keep_malformed=keep_malformed)

    if actual_hash != expected_hash:
        raise GraphCorruptionError(path, actual_hash, expected_hash)
    return _entries(json.loads(raw_bytes), keep_malformed=keep_malformed)


def _entries(data: object, *, keep_malformed: bool = False) -> list[dict]:
    """Extract the entry list and run the canonical migration/defaults pass.

    One seam: this is the same ``_apply_graph_defaults`` ``read_graph`` uses
    (the ported store's defaults pipeline), so a row whose on-disk ``status``
    predates a rename (``claimed`` -> ``in_progress``) reads identically no
    matter which reader a caller reached for.

    Imported function-locally to keep this module free of a load-time dependency on
    ``store`` (which is the write path), matching ``query_by_source_inbox_msg`` below.
    """
    from fno.graph.store import _apply_graph_defaults

    return _apply_graph_defaults(
        data.get("entries", []) if isinstance(data, dict) else [],
        keep_malformed=keep_malformed,
    )


def query_by_source_inbox_msg(msg_id: str, path: Path | None = None) -> list[dict]:
    """Return sidecar rows whose source_inbox_msg matches msg_id.

    source_inbox_msg is footnote-owned provenance (the same family as
    source_node_id / source_plan_path), so the scan runs over the sidecar
    projection and works on any tracker backend. An explicit ``path`` (a
    hermetic-test redirect) is still honored by reading that file directly.
    """
    if path is not None:
        from fno.graph.store import read_graph

        return [e for e in read_graph(path) if e.get("source_inbox_msg") == msg_id]
    from fno.tracker import sidecar as sidecar_store

    return [
        {"id": nid, "source_inbox_msg": sc.source_inbox_msg}
        for nid, sc in sidecar_store.load_all().items()
        if sc.source_inbox_msg == msg_id
    ]
