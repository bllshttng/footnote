"""In-package Python parallel to the ``verify_child_promise`` verb.

The canonical implementation is the bundled ``fno-agents`` binary's
``verify-evidence child-promise`` verb (folded out of the deleted
``scripts/lib/verify-event-evidence.sh`` in US1, ab-58645f63). This
module mirrors its semantics so a CLI consumer (e.g. when megawalk
promotes off the bash hooks) can verify a child target session's
``child_promise`` event using the same diagnostic vocabulary, without
shelling out to the binary.

Design notes
============
* **Vocabulary symmetry.** Error keys returned by this module overlap
  with the ``fno-agents`` verb's stderr substrings so a consumer can map
  between them without per-language branching:

  ====================================  =============================================
  Python error key                       fno-agents verb stderr substring
  ====================================  =============================================
  ``child_promise_missing``              ``child_promise missing for session``
  ``child_promise_nonce_mismatch``       ``nonce mismatch``
  ``events_unreadable``                  ``unreadable``
  ====================================  =============================================

* **Tolerant envelope parsing.** Both legacy ``{timestamp, ...}`` and
  canonical ``{ts, type, source, data}`` envelopes carry ``data.session_id``
  and ``data.nonce``, so the parser only inspects the data block.
* **Truncated lines are skipped.** A malformed JSON line is not a fatal
  substrate failure - the helper continues scanning. This mirrors the
  ``fno-agents`` verb's pre-filter-then-parse path which silently drops
  un-parseable lines. If no other line matches, the helper reports
  ``child_promise_missing`` rather than ``events_unreadable``.

The function is exported via ``fno.events.verify_child_promise``
for ergonomic imports::

    from fno.events import verify_child_promise

There is no caller in-tree today; this lands ahead of the CLI megawalk
promotion so the protocol is unified the moment that wiring flips on.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Literal, TypeAlias, Union

ChildPromiseError: TypeAlias = Literal[
    "child_promise_missing",
    "child_promise_nonce_mismatch",
    "events_unreadable",
]

# Sum-type return shape: success carries no error, failure always carries an
# error key. Spelled as a union of two narrowed tuples (rather than the looser
# tuple[bool, ChildPromiseError | None]) so type-checkers can narrow the
# second element after a check on the first - callers do not need to handle
# `None` in the success branch.
VerifyResult: TypeAlias = Union[
    tuple[Literal[True], None],
    tuple[Literal[False], ChildPromiseError],
]


def verify_child_promise(
    session_id: str,
    nonce: str,
    events_path: Path,
) -> VerifyResult:
    """Verify a ``child_promise`` event matches ``(session_id, nonce)``.

    Args:
        session_id: target session id to look up.
        nonce: provenance nonce expected on the matching event.
        events_path: path to the events.jsonl log to scan.

    Returns:
        ``(True, None)`` when a ``child_promise`` event with the given
        ``session_id`` and a ``nonce`` equal to the expected one is found.
        ``(False, error_key)`` otherwise. ``error_key`` is one of:

        * ``"events_unreadable"`` - the events file does not exist, is not
          a regular file, or cannot be decoded as UTF-8.
        * ``"child_promise_missing"`` - no ``child_promise`` event matches
          the requested session id (event absent or for a different session).
        * ``"child_promise_nonce_mismatch"`` - a matching session id was
          found, but its nonce differs from the expected value.
    """
    events_path = Path(events_path)
    if not events_path.exists() or not events_path.is_file():
        return False, "events_unreadable"

    # Stream line-by-line rather than read_text(): events.jsonl is
    # append-only and can grow large in long-running deployments;
    # loading the whole file into memory caused an OOM concern in PR
    # #220 review feedback (Gemini, MEDIUM). The first matching event
    # short-circuits the loop, so the worst case is bounded by the
    # position of the matching record, not the file size.
    matched_data: dict | None = None
    try:
        with events_path.open(encoding="utf-8", errors="strict") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    evt = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(evt, dict) or evt.get("type") != "child_promise":
                    continue
                data = evt.get("data")
                if not isinstance(data, dict) or data.get("session_id") != session_id:
                    continue
                matched_data = data
                break
    except (OSError, UnicodeDecodeError):
        return False, "events_unreadable"

    if matched_data is None:
        return False, "child_promise_missing"
    if matched_data.get("nonce") != nonce:
        return False, "child_promise_nonce_mismatch"
    return True, None
