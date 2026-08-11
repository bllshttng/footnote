"""Which model a session's transcript says answered, and where that file is.

A provenance fact, not a runtime one: the question "what did this session
ACTUALLY answer as" is asked by the agent runtime (``fno agents truth``) AND by
the graph writer that stamps a node's session rows, which sit on opposite sides
of the runtime boundary. It lives here, next to :mod:`fno.provenance.resolver`,
because a transcript pointer and what the transcript says about itself are the
same concern; ``fno.agents.session_truth`` re-exports both names so its own
callers are unaffected.

A route stamped at spawn records INTENT, so it reports the intended model in
exactly the case an operator suspects a silent fallback (an ``ANTHROPIC_MODEL``
surviving without its ``ANTHROPIC_BASE_URL``). The transcript reports what the
vendor answered as. Read-only; never writes; never raises.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Callable, Iterable, Optional


def resolve_transcript_path(
    agent: str,
    session_id: str,
    cwd: str,
    projects_root: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
) -> Optional[Path]:
    """The session's own transcript FILE, for the harnesses that keep one.

    Only claude and codex are file-backed; opencode keeps a shared SQLite store
    with no per-session file, and an unsupported harness resolves to nothing.
    Resolved once per call and handed to every reader below, so the tail read,
    the age probe, and the model read all agree on which file they are looking
    at instead of each running its own discovery.

    Public because :func:`observed_model` takes a path rather than finding one,
    so every caller outside this module needs this to reach it -- see
    ``fno.graph.store._observe_model``. Never raises; an unresolvable pointer is
    ``None``, which ``observed_model`` reports as ``no-transcript``.
    """
    if agent not in {"claude", "codex"}:
        return None
    try:
        from fno.provenance.resolver import resolve_transcript

        rt = resolve_transcript(
            agent,
            session_id,
            cwd,
            projects_root=projects_root,
            codex_sessions_dir=codex_sessions_dir,
        )
    except Exception:  # noqa: BLE001 — discovery failure -> no path, never raise
        return None
    if not rt.resolved or not rt.transcript_path:
        return None
    return Path(rt.transcript_path)


# Bounded window for the observed-model read, taken from the END of the file so
# a multi-MB transcript costs the same fixed read as a fresh one (the record
# tail streams the whole file; this one seeks).
_MODEL_TAIL_BYTES = 256 * 1024


def _models_in(
    lines: Iterable[str], reader: Callable[[dict], Optional[str]]
) -> tuple[Optional[str], int]:
    """``(most recent model, how many records carried one)`` over ``lines``.

    A line that is not parseable JSON is skipped rather than fatal: inside the
    window it is a stray, and the one place a torn line MATTERS (the end of the
    file) is checked by the caller before it gets here.
    """
    last: Optional[str] = None
    samples = 0
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if not isinstance(rec, dict):
            continue
        model = reader(rec)
        if isinstance(model, str) and model:
            last = model
            samples += 1
    return last, samples


# claude writes its OWN notices (API errors, interrupts, refusals) as
# `type: assistant` records carrying this placeholder instead of a model id. No
# vendor answered one, so it is not an observation -- and it lands LAST on a
# worker that just errored, which is precisely when an operator is checking the
# route. Measured on 200 local transcripts: 13 of them end on one.
_CLAUDE_SYNTHETIC_MODEL = "<synthetic>"


def _claude_model(rec: dict) -> Optional[str]:
    """claude stamps the answering model on every assistant message it received.

    Its own synthetic notices are skipped, so the reading falls back to the last
    record a vendor actually answered rather than reporting ``<synthetic>`` as
    the model.
    """
    if rec.get("type") != "assistant":
        return None
    msg = rec.get("message")
    if not isinstance(msg, dict):
        return None
    model = msg.get("model")
    return None if model == _CLAUDE_SYNTHETIC_MODEL else model


def _codex_model(rec: dict) -> Optional[str]:
    """codex stamps it on the per-turn ``turn_context``, not on the message."""
    if rec.get("type") != "turn_context":
        return None
    payload = rec.get("payload")
    return payload.get("model") if isinstance(payload, dict) else None


_MODEL_READERS: dict[str, Callable[[dict], Optional[str]]] = {
    "claude": _claude_model,
    "codex": _codex_model,
}


def observed_model(agent: str, transcript_path: Optional[Path]) -> dict[str, Any]:
    """What the session ACTUALLY answered as, read from its own transcript.

    Derived, never recorded: a route stamped at spawn records INTENT, so it
    reports the intended model in precisely the case an operator suspects a
    silent fallback. A worker that fell back to Anthropic reports ``claude-*``
    here and the disagreement with the requested route is visible without
    anyone having to trust the spawn.

    Five outcomes, deliberately not collapsed into one "unknown" -- the whole
    point of the ``no-model-yet`` variant is that a worker which came up and
    never processed a turn is a different thing from one whose transcript does
    not exist yet::

        {"kind": "observed", "model": str, "samples": int}
        {"kind": "no-transcript"}
        {"kind": "not-file-backed"}
        {"kind": "no-model-yet"}
        {"kind": "unreadable", "reason": str}

    ``not-file-backed`` is separate from ``no-transcript`` for the same reason
    the others are separate: an opencode worker keeps no per-session file and so
    would report "no transcript yet" forever, which reads as a worker that just
    spawned. Permanently-not-available and not-available-yet are different
    facts, and collapsing them is what this shape exists to avoid.

    ``samples`` counts the model-bearing records inside the tail window, not
    the whole session. Read-only, takes no lock (the transcript is append-only,
    so a torn final line is ``unreadable`` for this read and correct on the
    next), and never raises: this is a reporting field and must not break
    liveness reporting for the caller.
    """
    reader = _MODEL_READERS.get(agent)
    if reader is None:
        return {"kind": "not-file-backed"}
    if transcript_path is None:
        return {"kind": "no-transcript"}
    try:
        with open(transcript_path, "rb") as fh:
            size = fh.seek(0, os.SEEK_END)
            windowed = size > _MODEL_TAIL_BYTES
            fh.seek(max(0, size - _MODEL_TAIL_BYTES))
            blob = fh.read()
    except FileNotFoundError:
        return {"kind": "no-transcript"}
    except OSError as exc:  # unreadable dir, EIO, EACCES...
        # A file that exists but cannot be read must stay distinguishable from
        # one that is absent: invisibility is not absence.
        return {"kind": "unreadable", "reason": str(exc)}

    text = blob.decode("utf-8", "replace")
    if not text:
        return {"kind": "no-model-yet"}
    # Both harnesses terminate every record with a newline, so a window that
    # does not end in one caught the writer mid-line.
    if not text.endswith("\n"):
        return {"kind": "unreadable", "reason": "torn final line (mid-write)"}
    lines = text.split("\n")
    if windowed:
        # Our own window boundary cut the first line, not the writer.
        lines = lines[1:]

    last, samples = _models_in(lines, reader)
    if last is None and windowed:
        # The tail was inconclusive, and "no model yet" read off a window we
        # never looked past is a claim rather than an observation. claude
        # stamps the model on every assistant message so its tail always
        # answers; codex stamps it once per TURN on `turn_context`, which one
        # tool-heavy turn pushes clean out of the window (measured: a 622 KB
        # rollout whose only turn_context sits at byte 126850). Escalate to a
        # full streaming scan before claiming absence -- paid only on the rare
        # inconclusive tail, never on the common read.
        #
        # Ceiling, measured 2026-08-04: a full scan of the largest local rollout
        # (15 MB) is 56 ms. That matters because on the Rust list path this whole
        # verb runs inside the family-1 probe's 5s budget, and a probe timeout
        # costs `status` too -- the row would render `unknown` and drop out of
        # `--status live`, so a reporting field would have degraded the liveness
        # field it rides along with. At ~1% of the budget there is headroom; if
        # transcripts ever grow an order of magnitude, pre-filter this loop on the
        # marker substring before json.loads rather than widening the budget.
        try:
            with open(transcript_path, "r", encoding="utf-8", errors="replace") as fh:
                last, samples = _models_in(fh, reader)
        except OSError as exc:
            return {"kind": "unreadable", "reason": str(exc)}
    if last is None:
        return {"kind": "no-model-yet"}
    return {"kind": "observed", "model": last, "samples": samples}


def observed_model_for_session(agent: str, session_id: str, cwd: str) -> dict[str, Any]:
    """:func:`observed_model` for a session POINTER, keeping a broken resolver
    distinguishable from an absent transcript.

    :func:`resolve_transcript_path` answers "which file" and collapses every
    failure to ``None``, which the reader then reports as ``no-transcript``.
    That is right for a caller who only wants a path, and wrong for a caller
    recording durable evidence: a resolver that failed on permissions, a
    schema-drifted store, or an unreadable directory becomes indistinguishable
    from a session that simply has no transcript yet. An absence has two
    explanations and a row built on one cannot tell them apart, which is the
    whole reason the variant vocabulary exists.

    So this resolves with the reason in hand and maps it::

        reason "error" / a raised resolver  -> {"kind": "unreadable", "reason": ...}
        harness-not-supported               -> {"kind": "not-file-backed"}
        anything else unresolved            -> {"kind": "no-transcript"}

    Never raises.
    """
    if agent not in _MODEL_READERS:
        return {"kind": "not-file-backed"}
    try:
        from fno.provenance.resolver import resolve_transcript

        rt = resolve_transcript(agent, session_id, cwd)
    except Exception as exc:  # noqa: BLE001 - a broken resolver is a NAMED unknown
        return {"kind": "unreadable", "reason": f"resolver raised: {exc}"}
    if not rt.resolved or not rt.transcript_path:
        if rt.reason == "error":
            return {"kind": "unreadable", "reason": "transcript resolution failed"}
        if rt.reason == "harness-not-supported":
            return {"kind": "not-file-backed"}
        return {"kind": "no-transcript"}
    return observed_model(agent, Path(rt.transcript_path))
