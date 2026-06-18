"""Cross-agent message bus: the canonical provider-neutral message log.

A single global append-only JSONL log (``~/.fno/bus/messages.jsonl``,
path-resolver backed via ``config.paths.bus_dir``) is the system of record for
every inter-agent message - ask, send, reply - and the converged cross-project
inbox (heads-up, question, fyi). Markdown thread files are a derived render of
this log and carry zero authority (the graph.json -> graph.md pattern).

Layers:
  - :mod:`fno.bus.log`    - the versioned envelope, the locked writer
                                   (flock sidecar + O_APPEND whole-line writes
                                   at any body size), rotation, and the reader
                                   that skips malformed lines.
  - :mod:`fno.bus.cursor` - per-agent read cursors keyed by last-seen
                                   message-id (never a raw byte offset), so a
                                   rotation cannot silently reset a read position.
"""
from __future__ import annotations

from fno.bus.log import (
    Envelope,
    append,
    bus_log_path,
    from_json_line,
    iter_messages,
    iter_thread,
    to_json_line,
)
from fno.bus.cursor import (
    advance_cursor,
    cursor_path,
    read_cursor,
    scan_unread,
)

__all__ = [
    "Envelope",
    "append",
    "bus_log_path",
    "from_json_line",
    "iter_messages",
    "iter_thread",
    "to_json_line",
    "advance_cursor",
    "cursor_path",
    "read_cursor",
    "scan_unread",
]
