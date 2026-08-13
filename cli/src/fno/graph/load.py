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
# the window is normally milliseconds, so a retry lands consistent, while a
# genuine corruption still raises once the attempts are spent (a corrupt file
# mismatches on EVERY attempt, so a larger ceiling never masks real corruption).
# The budget must exceed the worst-case window: on a saturated host a
# GIL-starved writer (or a slow atomic replace) can hold the two-write window
# open well past a few milliseconds. Bounded, never a wait-until-consistent
# loop: worst case is (_ATTEMPTS - 1) * _SLEEP_S (~0.25s), paid only by a
# genuine corruption; a consistent read returns on the first attempt.
_RETRY_ATTEMPTS = 12
_RETRY_SLEEP_S = 0.023
# Cap on the wait-out-the-writer loop, in `_RETRY_SLEEP_S` units (~0.9s). The
# bound exists so a writer that dies holding the lock costs one slow read, never
# a hung one. It is reached only after every ordinary retry has already failed.
_WRITER_WAIT_ATTEMPTS = 40


def _writer_active(path: Path) -> bool:
    """True when a writer holds the graph lock at this instant.

    A NON-BLOCKING probe, and the reason this is safe to call from the read
    path. It never waits and never keeps the lock, so a caller that already
    owns it cannot deadlock against itself. Anything unreadable answers False,
    which leaves the caller's verdict exactly as it was without this call.
    """
    fd = None
    try:
        import fcntl

        from fno.graph.store import _graph_lock_path

        lock_path = _graph_lock_path(path)
        if not lock_path.exists():
            return False
        fd = os.open(str(lock_path), os.O_RDWR)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(fd, fcntl.LOCK_UN)
        return False
    except BlockingIOError:
        return True
    except (OSError, ImportError, RuntimeError):
        return False
    finally:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass


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
    - If sidecar present and mismatches: raises GraphCorruptionError.

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
            return _entries(json.loads(raw_bytes), keep_malformed=keep_malformed)

        if actual_hash == expected_hash:
            return _entries(json.loads(raw_bytes), keep_malformed=keep_malformed)

        # Mismatch: likely the two-write window. Retry after a short sleep.
        #
        # On the LAST attempt, WAIT OUT the writer rather than retrying once more.
        # A mismatch under a held lock is a live two-file update
        # (`store._write_json` then `store._write_sha256_sidecar`), never
        # corruption, and the fixed budget above cannot outlast a writer
        # descheduled between those two statements. That is measured, not
        # theoretical: three concurrent readers each reported exactly one false
        # corruption over 10k-15k loads on a loaded CI runner, agreeing on the
        # instant, which is one starved writer rather than random noise.
        #
        # The wait is keyed on the writer RELEASING, so it scales with how long
        # that writer was starved. One extra sleep does not: it widens a 0.28s
        # budget by 8% and still raises on the 300ms starvation this exists for.
        # `_writer_active` never blocks and never holds, so a caller that
        # already owns the lock cannot deadlock against itself, and a lock it
        # cannot read leaves the verdict exactly as it was. The iteration cap is
        # what keeps a wedged writer from hanging the read path forever.
        if attempt == _RETRY_ATTEMPTS - 1:
            for _ in range(_WRITER_WAIT_ATTEMPTS):
                if not _writer_active(path):
                    break
                time.sleep(_RETRY_SLEEP_S)
            raw_bytes = path.read_bytes()
            actual_hash = hashlib.sha256(raw_bytes).hexdigest()
            expected_hash = sidecar.read_text().strip() if sidecar.exists() else ""
            if actual_hash == expected_hash:
                return _entries(json.loads(raw_bytes), keep_malformed=keep_malformed)
        if attempt < _RETRY_ATTEMPTS - 1:
            if os.environ.get("FNO_DEBUG"):
                print(
                    f"load_graph: hash mismatch on {path} (attempt {attempt + 1}), retrying",
                    file=sys.stderr,
                )
            time.sleep(_RETRY_SLEEP_S)

    raise GraphCorruptionError(path, actual_hash, expected_hash)


def _entries(data: object, *, keep_malformed: bool = False) -> list[dict]:
    """Extract the entry list and run the canonical migration/defaults pass.

    One seam: this is the same ``_apply_graph_defaults`` ``read_graph`` uses, so
    a row whose on-disk ``status`` predates a rename (``claimed`` -> ``in_progress``)
    reads identically no matter which reader a caller reached for. A local fold of
    only the ``_status`` key rename used to live here, and left the ~10 hash-validated
    callers one migration behind the canonical readers.

    Imported function-locally to keep this module free of a load-time dependency on
    ``store`` (which is the write path), matching ``query_by_source_inbox_msg`` below.
    """
    from fno.graph.store import _apply_graph_defaults

    return _apply_graph_defaults(
        data.get("entries", []) if isinstance(data, dict) else [],
        keep_malformed=keep_malformed,
    )


def query_by_source_inbox_msg(msg_id: str, path: Path | None = None) -> list[dict]:
    """Return entries whose source_inbox_msg matches msg_id.

    Uses read_graph (defaults applied) so provenance fields are guaranteed
    to be present even on legacy entries written before Phase 01.
    """
    from fno.graph.store import read_graph

    entries = read_graph(path) if path is not None else read_graph()
    return [e for e in entries if e.get("source_inbox_msg") == msg_id]
