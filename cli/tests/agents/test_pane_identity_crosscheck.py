"""x-b029: an unstamped spawn reports itself, and the pane/registry
cross-check reports both mismatch directions with its counts.

Two defects, one file:

- AC4-ERR (task 2.2): a codex spawn with an OPTIONAL session binding whose
  stamp never landed used to fall through the reconcile pass and return zero
  with an id-less row and no journal entry. Pane 46 vs pane 50 measured
  character-identical argv with only one stamped, so the stamp step CAN fail;
  the failure is now an event plus a ``stamp_failure`` receipt field, never a
  silent None.
- AC5-HP / AC5-EDGE (task 3.1): ``fno agents pane-identity`` cross-checks the
  mux listing against the registry in BOTH directions - every row with a mux
  ref resolves to a pane carrying its id, and every pane carrying fno's spawn
  signature is referenced by a row - and prints the counts compared so a
  zero-mismatch result is a reading, not a check that never ran.

Direction 2 reads argv only to RAISE a mismatch for an operator; it never
mints an identity from argv (the AGENTS.md trap: argv can outlive the process
it describes).
"""
from __future__ import annotations

import ast
import inspect
import textwrap

import pytest

from fno.agents import mux_spawn
from fno.agents.mux_spawn import MuxSpawnResult
from fno.agents.reachability import (
    FNO_SPAWN_SIGNATURE_FLAGS,
    pane_identity_crosscheck,
    render_pane_identity_crosscheck,
)


# ---------------------------------------------------------------------------
# Task 2.2: the stamp step reports when it does not write


def test_optional_binding_fall_through_reports_itself() -> None:
    """The silent fall-through is gone: the codex reconcile branch emits the
    uncaptured-id event and sets stamp_failure when the binding is optional
    and the backfill found nothing."""
    src = textwrap.dedent(inspect.getsource(mux_spawn.dispatch_spawn_pane))
    tree = ast.parse(src)
    # Find the codex reconcile `if` chain by its sentinel string, then require
    # the emit and the assignment inside the SAME chain.
    emits = [
        n
        for n in ast.walk(tree)
        if isinstance(n, ast.Call)
        and isinstance(n.func, ast.Attribute)
        and n.func.attr == "emit"
        and any(
            isinstance(a, ast.Constant) and a.value == "agent_session_id_uncaptured"
            for a in n.args
        )
    ]
    assert emits, "the fall-through no longer emits agent_session_id_uncaptured"
    assert "stamp_failure = stamp_failure_detail" in src, (
        "the fall-through no longer records stamp_failure"
    )
    assert "stamp_failure=stamp_failure" in src, (
        "MuxSpawnResult no longer carries stamp_failure"
    )


def test_spawn_result_defaults_to_no_stamp_failure() -> None:
    """The field is additive: a bound (or structurally unbound) spawn carries
    None, so every existing receipt stays byte-stable."""
    assert MuxSpawnResult.__dataclass_fields__["stamp_failure"].default is None


def _result(**overrides) -> MuxSpawnResult:
    base = dict(
        name="t-x",
        provider="codex",
        session="main",
        pane_id=46,
        child_pid=None,
        session_uuid=None,
        bound=False,
        unbound_reason="binding-window-expired",
    )
    base.update(overrides)
    return MuxSpawnResult(**base)


def test_receipt_renders_stamp_failure_when_set() -> None:
    """A set stamp_failure reaches the receipt object the operator reads."""
    from fno.agents import cli as agents_cli

    src = textwrap.dedent(inspect.getsource(agents_cli))
    assert 'receipt_obj["stamp_failure"]' in src, (
        "cli.py no longer renders stamp_failure on the spawn receipt"
    )


# ---------------------------------------------------------------------------
# Task 3.1: the cross-check, in both directions


def _pane(pane_id, *, fno_id=None, state=None, child_pid=None, pristine=False, **extra):
    p = {
        "pane_id": pane_id,
        "fno_id": fno_id,
        "fno_id_state": state or ("resolved" if fno_id else "unresolved:spawned-name"),
        "child_pid": child_pid,
        "pristine_idle_shell": pristine,
    }
    p.update(extra)
    return p


class _Row:
    def __init__(self, name, mux=None, harness_session_id=None):
        self.name = name
        self.mux = mux
        self.harness_session_id = harness_session_id


SIG_ARGV = f"codex --model gpt {FNO_SPAWN_SIGNATURE_FLAGS[0]}"
PLAIN_ARGV = "/bin/zsh"


def test_direction_1_flags_a_row_whose_pane_vanished() -> None:
    rows = [_Row("gone", mux={"session": "main", "pane_id": 999}, harness_session_id="a" * 36)]
    out = pane_identity_crosscheck([], rows, "main", argv_of=lambda pid: None)
    assert out["row_mismatches"] == [
        {"row": "gone", "pane": 999, "reason": "pane missing from session listing"}
    ]
    assert out["panes_compared"] == 0 and out["rows_with_mux_compared"] == 1


def test_direction_1_flags_an_id_mismatch_and_an_idless_row() -> None:
    uuid = "01a05fce-0000-7ccc-8000-000000000000"
    rows = [
        _Row("stale", mux={"session": "main", "pane_id": 1}, harness_session_id=uuid),
        _Row("idless", mux={"session": "main", "pane_id": 2}, harness_session_id=None),
    ]
    panes = [
        _pane(1, fno_id="119e3c52-a4b3-4f7e-8a1c-2d3e4f5a6b7c"),
        _pane(2),
    ]
    out = pane_identity_crosscheck(panes, rows, "main", argv_of=lambda pid: None)
    reasons = [m["reason"] for m in out["row_mismatches"]]
    assert len(reasons) == 2
    assert f"row expects {uuid}" in reasons[0]
    assert "no session id" in reasons[1]


def test_direction_1_matching_row_is_clean() -> None:
    uuid = "01a05fce-0000-7ccc-8000-000000000000"
    rows = [_Row("bound", mux={"session": "main", "pane_id": 3}, harness_session_id=uuid)]
    panes = [_pane(3, fno_id=uuid)]
    out = pane_identity_crosscheck(panes, rows, "main", argv_of=lambda pid: None)
    assert out["row_mismatches"] == []
    assert out["pane_mismatches"] == []


def test_direction_2_flags_signed_argv_with_no_row() -> None:
    panes = [_pane(9, child_pid=11258)]
    rows: list = []
    out = pane_identity_crosscheck(
        panes, rows, "main", argv_of=lambda pid: SIG_ARGV
    )
    assert out["pane_mismatches"] == [
        {"pane": 9, "pid": 11258, "reason": "fno spawn signature in argv, no registry row"}
    ]


def test_direction_2_spares_referenced_plain_and_pristine_panes() -> None:
    panes = [
        _pane(1, child_pid=101),  # referenced by a row
        _pane(2, child_pid=102),  # plain shell argv
        _pane(3, child_pid=103, pristine=True),  # positively idle shell
        _pane(4, child_pid=None),  # nothing to probe
        _pane(5, fno_id="119e3c52-a4b3-4f7e-8a1c-2d3e4f5a6b7c", child_pid=105),
    ]
    rows = [_Row("held", mux={"session": "main", "pane_id": 1}, harness_session_id="x")]
    argvs = {101: SIG_ARGV, 102: PLAIN_ARGV, 103: SIG_ARGV, 105: SIG_ARGV}
    out = pane_identity_crosscheck(panes, rows, "main", argv_of=argvs.get)
    assert out["pane_mismatches"] == []


def test_direction_2_skips_rows_in_other_sessions_from_direction_1() -> None:
    rows = [
        _Row("elsewhere", mux={"session": "other", "pane_id": 1}, harness_session_id="y")
    ]
    out = pane_identity_crosscheck([], rows, "main", argv_of=lambda pid: None)
    assert out["rows_with_mux_compared"] == 0
    assert out["rows_other_session"] == 1
    assert out["row_mismatches"] == []


def test_counts_are_print_even_when_clean() -> None:
    """AC5-EDGE: a zero-mismatch result is a reading, never a silence."""
    out = pane_identity_crosscheck([], [], "main", argv_of=lambda pid: None)
    text = render_pane_identity_crosscheck(out)
    assert "panes compared: 0" in text
    assert "rows with mux ref compared: 0" in text
    assert "clean: no mismatch in either direction" in text


def test_render_lists_both_direction_names() -> None:
    rows = [
        _Row("gone", mux={"session": "main", "pane_id": 999}, harness_session_id="a")
    ]
    panes = [_pane(9, child_pid=11258)]
    out = pane_identity_crosscheck(panes, rows, "main", argv_of=lambda pid: SIG_ARGV)
    text = render_pane_identity_crosscheck(out)
    assert "row -> pane mismatches: 1" in text
    assert "pane -> row mismatches: 1" in text
    assert "fno spawn signature in argv, no registry row" in text


@pytest.mark.parametrize("flag", FNO_SPAWN_SIGNATURE_FLAGS)
def test_every_signature_flag_alone_raises_the_mismatch(flag) -> None:
    panes = [_pane(7, child_pid=1)]
    out = pane_identity_crosscheck(
        panes, [], "main", argv_of=lambda pid: f"claude --model x {flag}"
    )
    assert len(out["pane_mismatches"]) == 1
