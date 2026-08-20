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
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Literal, Optional, TypeAlias, Union

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


# ── fan-in completeness ─────────────────────────────────────────────────────
#
# The deterministic completeness count a barrier consumes before it lets
# synthesis proceed. Kept OUTSIDE any model-generated summary: a summary that
# claims "all done" is not evidence; this tally is. It is contract-agnostic on
# purpose - the caller classifies each worker return (via the canonical
# ``parse_task_result``) into ``completed`` / ``failed`` / malformed and hands
# the classified pairs here, so the single source of the status enum stays in
# ``skills/execute/orchestrator.py`` and this module does pure set math.

FanInKind: TypeAlias = Literal["completed", "failed"]

# One observed worker return: (node_id, kind). A ``None`` node_id OR a ``None``
# kind means the return could not be attributed/classified - i.e. malformed.
Observation: TypeAlias = tuple[Optional[str], Optional[FanInKind]]


@dataclass(frozen=True)
class FanInTally:
    """Expected-versus-observed counts at a fan-in barrier."""

    expected: int
    completed: int
    failed: int
    duplicate: int
    malformed: int
    missing: int

    def __post_init__(self) -> None:
        for name in ("expected", "completed", "failed", "duplicate", "malformed", "missing"):
            v = getattr(self, name)
            if isinstance(v, bool) or not isinstance(v, int) or v < 0:
                raise ValueError(f"FanInTally.{name} must be a non-negative int, got {v!r}")

    @property
    def complete(self) -> bool:
        """True only when every expected node completed with nothing unresolved.

        Refuses on ANY missing, failed, or malformed return: a barrier that
        released on a partial fan-in is the exact bug this count prevents.
        """
        return (
            self.missing == 0
            and self.failed == 0
            and self.malformed == 0
            and self.completed == self.expected
        )

    def to_dict(self) -> dict[str, int | bool]:
        return {
            "expected": self.expected,
            "completed": self.completed,
            "failed": self.failed,
            "duplicate": self.duplicate,
            "malformed": self.malformed,
            "missing": self.missing,
            "complete": self.complete,
        }


def tally_fan_in(
    expected: Iterable[str],
    observed: Iterable[Observation],
) -> FanInTally:
    """Count observed worker returns against the expected node set.

    Args:
        expected: node ids that MUST resolve for the barrier to release.
        observed: ``(node_id, kind)`` pairs, one per worker return. ``kind`` is
            ``"completed"`` (SUCCESS/DONE_WITH_CONCERNS) or ``"failed"``
            (FAILED/BLOCKED). A ``None`` node_id or ``None``/unknown kind is
            counted as ``malformed`` and attributed to no id.

    Counting rules (all scoped to ``expected`` - a return for an id outside the
    expected set is ignored, since it belongs to no barrier this tally gates):
        * ``duplicate``  - a second+ observation for an expected id already seen.
        * ``missing``    - an expected id with no attributed observation.
        * ``completed``  - distinct expected ids observed completing.
        * ``failed``     - attributed expected returns in a non-success state.
        * ``malformed``  - returns that could not be parsed/attributed at all.
    """
    expected_set = {e for e in expected if e}
    seen: set[str] = set()
    completed_ids: set[str] = set()
    failed = 0
    duplicate = 0
    malformed = 0

    for node_id, kind in observed:
        if not node_id or kind not in ("completed", "failed"):
            malformed += 1
            continue
        if node_id not in expected_set:
            # A return for a node this barrier never expected (a stale id, a
            # typo, a sibling barrier's return crossing streams) is irrelevant
            # to THIS fan-in's completeness - ignore it rather than let it
            # permanently skew `completed` and block the barrier with no flag.
            continue
        if node_id in seen:
            duplicate += 1
            continue
        seen.add(node_id)
        if kind == "completed":
            completed_ids.add(node_id)
        else:
            failed += 1

    return FanInTally(
        expected=len(expected_set),
        completed=len(completed_ids),
        failed=failed,
        duplicate=duplicate,
        malformed=malformed,
        missing=len(expected_set - seen),
    )
