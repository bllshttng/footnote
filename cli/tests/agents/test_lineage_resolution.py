"""Resolution semantics over classified lineage (x-dfe7).

One row's ids serve different jobs. Delivery (mail, inject, pane send) always
consumes the row's CURRENT address, even when the caller named a predecessor.
Exact-id resume is the one consumer allowed to keep the historical id it
actually spelled. A token that could name either of two live workers refuses
and names both full ids, never picking a winner.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional

import pytest

A = "e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
B = "08054b1d-a907-47ab-a3d2-4a1e7a87eb4e"
C = "cafef00d-a907-47ab-a3d2-4a1e7a87eb4e"


@dataclass
class _Row:
    """Duck-typed AgentEntry with just what the resolver reads."""

    name: str
    harness: str = "claude"
    cwd: str = "/proj"
    log_path: str = ""
    short_id: str = ""
    provider: Optional[str] = None
    effort: Optional[str] = None
    session_id: Optional[str] = None
    harness_session_id: Optional[str] = None
    related_session_id: Optional[str] = None
    aliases: list = None  # type: ignore[assignment]
    fno_id: Optional[str] = None
    predecessor_session_ids: list = None  # type: ignore[assignment]
    forked_from_session_id: Optional[str] = None

    def __post_init__(self):
        if self.aliases is None:
            self.aliases = []
        if self.predecessor_session_ids is None:
            self.predecessor_session_ids = []

    def __getattr__(self, item):
        # Duck-typed fake: any registry field this file does not care about
        # reads as None, exactly like the getattr-based consumers see.
        if item.startswith("__"):
            raise AttributeError(item)
        return None


def _successor_row() -> _Row:
    """A row after succession: B current, A retained in the chain."""
    return _Row(
        name="worker",
        harness_session_id=B,
        short_id=B.split("-", 1)[0],
        fno_id="thread-a",
        predecessor_session_ids=[A],
    )


# ---------------------------------------------------------------------------
# AC6-HP: addressing follows B; the match records which id was named
# ---------------------------------------------------------------------------


def test_predecessor_full_id_resolves_the_current_row() -> None:
    from fno.agents.registry import resolve_agent_in

    row = _successor_row()
    resolved = resolve_agent_in([row], A)
    assert resolved.entry is row
    assert resolved.matched_by == "full_session_id"
    assert resolved.matched_session_id == A, "the match names the id called"


def test_current_full_id_resolves_with_its_own_match() -> None:
    from fno.agents.registry import resolve_agent_in

    row = _successor_row()
    resolved = resolve_agent_in([row], B)
    assert resolved.entry is row
    assert resolved.matched_session_id == B


def test_name_match_carries_no_exact_id() -> None:
    from fno.agents.registry import resolve_agent_in

    row = _successor_row()
    resolved = resolve_agent_in([row], "worker")
    assert resolved.matched_session_id is None, (
        "only a full-id match separates the two identity jobs"
    )


def test_delivery_resolves_to_the_current_address() -> None:
    """The resolved row's harness_session_id is B: every delivery verb reads
    the CURRENT address off the row, so mail naming A lands on B's inbox."""
    from fno.agents.registry import resolve_agent_in

    resolved = resolve_agent_in([_successor_row()], A)
    assert resolved.entry.harness_session_id == B


def test_predecessor_short_form_stays_retired() -> None:
    """A's 8-hex short retired with A. The successor's own short answers;
    the predecessor's does not resurrect a retired namespace."""
    from fno.agents.registry import AgentResolutionError, resolve_agent_in

    row = _successor_row()
    with pytest.raises(AgentResolutionError):
        resolve_agent_in([row], A.split("-", 1)[0])


# ---------------------------------------------------------------------------
# AC6-ERR: a live branch never lets a token pick one worker
# ---------------------------------------------------------------------------


def test_token_matching_two_live_branch_rows_refuses_and_names_both() -> None:
    from fno.agents.registry import AgentResolutionError, resolve_agent_in

    rows = [
        _Row(name="worker-one", harness_session_id=B, short_id="shared001",
             fno_id=B, forked_from_session_id=A),
        _Row(name="worker-two", harness_session_id=C, short_id="shared001",
             fno_id=C, forked_from_session_id=A),
    ]
    with pytest.raises(AgentResolutionError) as excinfo:
        resolve_agent_in(rows, "shared001")
    message = str(excinfo.value)
    assert B in message and C in message, "the refusal names both full ids"
    assert excinfo.value.ambiguous


# ---------------------------------------------------------------------------
# AC6-HP resume half: an exact predecessor id is the resume target
# ---------------------------------------------------------------------------


class _CodexSuccessorRow(_Row):
    """A codex-shaped succession row (harness-specific id mapping reads
    harness_session_id through HARNESS_SESSION_ID_FIELDS)."""


def test_resume_by_predecessor_full_uuid_keeps_that_exact_id() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _CodexSuccessorRow(
        name="alpha",
        harness="codex",
        cwd="/path/to/workdir",
        harness_session_id=B,
        predecessor_session_ids=[A],
    )
    res = resume_logic(
        name=A,  # the operator resumed the HISTORICAL session by full uuid
        registry_loader=lambda: [entry],
        path_checker=lambda _bin: True,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        emit_event=lambda kind, **kw: None,
        execvp=lambda *_a, **_k: None,
    )
    assert res.exit_code == 0
    assert res.exec_argv[-2:] == ["resume", A], (
        "exact-id resume reopens A, not the row's current B"
    )


def test_resume_by_name_keeps_the_current_session() -> None:
    from fno.agents.resume_cli import resume_logic

    entry = _CodexSuccessorRow(
        name="alpha",
        harness="codex",
        cwd="/path/to/workdir",
        harness_session_id=B,
        predecessor_session_ids=[A],
    )
    res = resume_logic(
        name="alpha",
        registry_loader=lambda: [entry],
        path_checker=lambda _bin: True,
        cwd_checker=lambda _c: True,
        claim_fn=lambda _s: None,
        emit_event=lambda kind, **kw: None,
        execvp=lambda *_a, **_k: None,
    )
    assert res.exit_code == 0
    assert res.exec_argv[-2:] == ["resume", B], (
        "a current address (name) selects the current session"
    )


# ---------------------------------------------------------------------------
# AC7: the list row states both identity axes
# ---------------------------------------------------------------------------


def test_serialize_entry_states_thread_and_current_beside_each_other() -> None:
    """AC7-HP/AC7-ERR: after succession the row reports current_session_id B
    POSITIVELY, with the stable thread id and the predecessor chain beside it.
    A renderer sourcing current identity from fno_id, pane metadata, or the
    first predecessor fails this assertion instead of passing silently."""
    from fno.agents.format import serialize_entry

    entry = _Row(
        name="worker",
        harness_session_id=B,
        short_id=B.split("-", 1)[0],
        fno_id="thread-a",
        predecessor_session_ids=[A],
    )
    row = serialize_entry(entry, live_status=None)  # type: ignore[arg-type]

    assert row["current_session_id"] == B, "current identity is B, never A"
    assert row["harness_session_id"] == B
    assert row["thread_id"] == "thread-a", "the thread key stays visible"
    assert row["predecessor_session_ids"] == [A]
    assert row["forked_from_session_id"] is None, "succession is not a fork edge"
