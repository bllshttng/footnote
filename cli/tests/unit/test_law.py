"""One-step law recording, and the two refusals that survived the collapse.

Every refusal here asserts an EXACT exit code. `typer` already spends exit 2 on
usage errors and `fno.graph.cli` spends it 66 more times, so a bare non-zero
assertion proves the command failed and nothing about WHY. Each refusal also
carries a make-it-fail probe: the same call with the one refused input removed
records a `d-` id, which is what proves the gate is the thing refusing.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.rust_binary import find_dev_binary

LAW_RECORDED_EXIT = 0
LAW_REFUSED_EXIT = 3

# Every recording path now routes its statement classification through the
# `law-match` crate verb (mode validate), so the whole module needs this
# checkout's own build.
pytestmark = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents)`",
)


@pytest.fixture(autouse=True)
def quiet_sweep(request, monkeypatch):
    """law set's rule-time sweep is best-effort stderr side work over the
    machine-wide question store; the suite keeps it silent except in the
    tests that pin its contract (x-cf6a)."""
    from fno import law as law_mod

    request.node._real_sweep = law_mod._sweep_open_questions
    monkeypatch.setattr(law_mod, "_sweep_open_questions", lambda *a, **k: None)


@pytest.fixture
def real_sweep(request, monkeypatch):
    """Re-arm the real sweep for the contract tests."""
    from fno import law as law_mod

    monkeypatch.setattr(law_mod, "_sweep_open_questions", request.node._real_sweep)



def _rows(index):
    from tests._event_rows import event_rows

    return event_rows(index)


def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    from fno import paths

    (tmp_path / ".fno").mkdir(parents=True)
    index = tmp_path / "state" / "decisions.jsonl"
    index.parent.mkdir()
    index.touch()
    import fno.decide

    monkeypatch.setattr(fno.decide, "_decisions_index_path", lambda: index)
    # The law door stamps the recording project and refuses an unmapped one,
    # so the fixture provisiones a hermetic work map naming the pytest cwd
    # itself (a direct match, layout-independent). The project is fno so the
    # seeded legacy rows (no scope, read as project:fno) stay visible.
    map_file = tmp_path / "settings.yaml"
    map_file.write_text(
        "work:\n"
        "  workspaces:\n"
        "    main:\n"
        "      projects:\n"
        "        - name: fno\n"
        f"          path: {Path.cwd()}\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(map_file))
    return index


def _as_chat_session(monkeypatch: pytest.MonkeyPatch) -> None:
    """Make the resolver see a session someone typed into."""
    from types import SimpleNamespace

    from fno.agents import self_stamp

    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id="a" * 32, harness="claude"),
    )


def _run(args: list[str]):
    """Invoke through a mounted parent, the way `fno inbox law` reaches it.

    `inbox_app.add_typer(law_app, name="law")` is the real mount, and it always
    builds a group. Invoking the bare `law_app` instead would test a shape no
    caller has.
    """
    import typer

    from fno.law import law_app

    parent = typer.Typer()
    parent.add_typer(law_app, name="law")
    return CliRunner().invoke(parent, ["law", *args])


# ── the acceptance: one call, one d- id ───────────────────────────────────────


def test_one_call_records_and_prints_a_decision_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            "merge-authority",
            "Merges belong to the operator",
            "--rationale",
            "The operator owns durable policy.",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    decision_id = result.output.strip().splitlines()[-1]
    assert decision_id.startswith("d-"), result.output

    rows = _rows(index)
    assert len(rows) == 1
    data = rows[0]["data"]
    assert data["decision_id"] == decision_id
    # The honest attribution is what survived the trade: a chat recording never
    # claims the superuser lane.
    assert data["authority_source"] == "chat_attested"


def test_no_staged_proposal_surface_remains() -> None:
    """prepare / enact / resume / inspect are gone, hash and receipt with them."""
    from fno import law
    from fno.law import law_app

    commands = {command.name for command in law_app.registered_commands}
    assert commands == {"set"}
    for retired in (
        "prepare_proposal",
        "enact_proposal",
        "load_proposal",
        "proposal_lock",
        "validate_operator_consent",
    ):
        assert not hasattr(law, retired), retired

    from fno import paths

    assert not hasattr(paths, "law_proposals_dir")


# ── refusal 1: the statement is not durable law ───────────────────────────────


@pytest.mark.parametrize(
    "decision",
    [
        "Merges belong to the operator for this change",
        "This PR merges without review",
    ],
)
def test_coordination_statement_is_refused_with_exit_3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, decision: str
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(["set", "merge-authority", decision, "--rationale", "why"])

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "coordination" in result.output
    assert _rows(index) == []


def test_missing_rationale_is_refused_with_exit_3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(["set", "merge-authority", "Merges belong to the operator"])

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "rationale is required" in result.output
    assert _rows(index) == []


def test_durable_law_probe_records_where_the_refused_shapes_did_not(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The make-it-fail control for both refusals above.

    Same command, same session, the one refused input removed. It records. So
    the refusals are the validator firing, not an unrelated failure.
    """
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        ["set", "merge-authority", "Merges belong to the operator", "--rationale", "why"]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    assert result.output.strip().splitlines()[-1].startswith("d-")
    assert _rows(index)


# ── refusal 2: nothing marks a decider ────────────────────────────────────────


def _unmarked_process(monkeypatch: pytest.MonkeyPatch) -> None:
    """No harness session identity, and no terminal on stdin."""
    from types import SimpleNamespace

    from fno import decide
    from fno.agents import self_stamp

    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id=None, harness=None),
    )
    monkeypatch.setattr(decide, "_attended_terminal", lambda: False)


def test_unmarked_process_is_refused_with_exit_3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _unmarked_process(monkeypatch)

    result = _run(
        ["set", "merge-authority", "Merges belong to the operator", "--rationale", "why"]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "nothing here marks a decider" in result.output
    assert _rows(index) == []


def test_library_refuses_chat_attested_from_an_unmarked_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The gate holds below the CLI too.

    `record_decision` is importable, so a resolver enforced only in the command
    body would be a gate anything using the library walks around.
    """
    from fno.decide import UnattributedAuthorityError, record_decision

    index = _isolate(tmp_path, monkeypatch)
    _unmarked_process(monkeypatch)

    with pytest.raises(UnattributedAuthorityError):
        record_decision(
            subject="merge-authority",
            decision="Merges belong to the operator",
            rationale="why",
            authority_source="chat_attested",
        )
    assert _rows(index) == []


def test_library_refuses_a_coordination_statement_in_the_law_lane(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The statement classifier holds below the CLI, like the session gate."""
    from fno.decide import record_decision
    from fno.law import LawValidationError

    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    with pytest.raises(LawValidationError, match="coordination"):
        record_decision(
            subject="merge-authority",
            decision="This PR merges without review",
            rationale="why",
            authority_source="chat_attested",
        )
    assert _rows(index) == []


def test_attended_terminal_probe_records_where_the_unmarked_process_did_not(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The make-it-fail control for refusal 2: give it a terminal and it lands."""
    from types import SimpleNamespace

    from fno import decide
    from fno.agents import self_stamp

    index = _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id=None, harness=None),
    )
    monkeypatch.setattr(decide, "_attended_terminal", lambda: True)

    result = _run(
        ["set", "merge-authority", "Merges belong to the operator", "--rationale", "why"]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    rows = _rows(index)
    assert rows[0]["data"]["authority_source"] == "operator"


# ── a chat recording may retire its own kind, never the operator's ───────────


def _seed_law_row(index: Path, decision_id: str, authority: str) -> None:
    """Commit one live law-lane row into the index the reader uses."""
    from fno.events.store_client import emit_envelope

    emit_envelope(
        {
            "type": "operator_decision",
            "ts": "2026-08-29T19:00:00+00:00",
            "source": "test",
            "data": {
                "decision_id": decision_id,
                "subject": "merge-authority",
                "decision": "Merges belong to the operator",
                "authority_source": authority,
                "decided_by": "operator" if authority == "operator" else "9ede2d7b",
            },
        },
        index,
    )


def test_chat_recording_cannot_supersede_an_operator_law_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Supersession IS retirement for every reader that filters live state.

    Without this refusal the retract guard is decorative: a session that cannot
    retract an operator row could still supersede it into invisibility.
    """
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)
    _seed_law_row(index, "d-0ad0ad0a", "operator")
    before = _rows(index)

    result = _run(
        [
            "set",
            "merge-authority",
            "Merges belong to whoever asks",
            "--rationale",
            "why",
            "--supersedes",
            "d-0ad0ad0a",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert _rows(index) == before


def test_chat_recording_can_supersede_another_chat_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The make-it-fail control, and the supersede happy path.

    Same call, same session, the superseded row's authority is the only thing
    that changed. It records and stamps `superseded_by` on the prior row. So the
    refusal above is the superuser-authority guard firing, not supersession being
    broken outright.
    """
    from fno.decide import list_decisions

    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)
    _seed_law_row(index, "d-c4a7c4a7", "chat_attested")

    result = _run(
        [
            "set",
            "merge-authority",
            "Merges belong to whoever asks",
            "--rationale",
            "why",
            "--supersedes",
            "d-c4a7c4a7",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    new_id = result.output.strip().splitlines()[-1]
    assert new_id.startswith("d-")

    _, rows, _ = list_decisions("merge-authority", limit=None, lane="law")
    prior = next(r for r in rows if r.get("decision_id") == "d-c4a7c4a7")
    assert prior.get("superseded_by") == new_id


# ── authority resolution reads the session, never a caller-supplied value ─────


def test_require_marked_caller_prefers_the_resolved_session(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from types import SimpleNamespace

    from fno import decide
    from fno.agents import self_stamp

    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id="a" * 32, harness="claude"),
    )
    monkeypatch.setattr(decide, "_attended_terminal", lambda: True)
    # A terminal is present too, and the session still wins: the row must not
    # claim the superuser lane just because someone happened to be at a tty.
    assert decide.require_marked_caller() == "chat_attested"


def test_supersedes_naming_no_recoverable_decision_refuses_with_exit_3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A well-formed id that resolves to nothing must not exit 1.

    Exit 1 is the code reserved for "recorded to the journal, index write
    failed, do NOT re-run". A caller reading it after a crash concludes the
    opposite of what happened.
    """
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            "merge-authority",
            "Merges belong to the operator",
            "--rationale",
            "why",
            "--supersedes",
            "d-deadbeef",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "not recoverable" in result.output
    assert _rows(index) == []


def test_supersedes_must_be_a_decision_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            "merge-authority",
            "Merges belong to the operator",
            "--rationale",
            "why",
            "--supersedes",
            "merge-authority",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "supersedes must be a decision id" in result.output
    assert _rows(index) == []


# ── the waiver-subject carve-out ──────────────────────────────────────────────
#
# Subjects under `review-coverage-waiver` are the merge gate's waiver evidence:
# they assert a person at a terminal read the diff. The door's chat_attested
# value cannot carry that fact, because any harness-descended process records
# the same value, so the write chokepoint refuses the whole family for every
# non-superuser authority (WaiverAuthorityRefusedError). The exact affirmative
# decision value is the one an agent would reach for, so it is the probe.

WAIVER_SUBJECT = "review-coverage-waiver:acme/widgets#42@" + ("c" * 40)
WAIVER_DECISION = "review coverage waived for this head"


def test_waiver_subject_refuses_a_chat_session_with_exit_3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            WAIVER_SUBJECT,
            WAIVER_DECISION,
            "--rationale",
            "the diff was read and the risk is accepted",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "coverage-waive" in result.output, result.output
    assert _rows(index) == []


def test_waiver_refusal_is_the_subject_not_the_statement(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The make-it-fail control: the same statement at a non-waiver subject
    records, so the subject family is what the guard keys on."""
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            "merge-authority",
            WAIVER_DECISION,
            "--rationale",
            "the diff was read and the risk is accepted",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    assert result.output.strip().splitlines()[-1].startswith("d-")
    assert _rows(index)


def test_operator_authority_still_records_at_a_waiver_subject(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The positive control the other direction: the guard locks agents out,
    never the operator. An attended terminal with no harness identity records
    the waiver through the same door."""
    from types import SimpleNamespace

    from fno import decide
    from fno.agents import self_stamp

    index = _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id=None, harness=None),
    )
    monkeypatch.setattr(decide, "_attended_terminal", lambda: True)

    result = _run(
        [
            "set",
            WAIVER_SUBJECT,
            WAIVER_DECISION,
            "--rationale",
            "operator read this diff by hand",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    rows = _rows(index)
    assert len(rows) == 1
    assert rows[0]["data"]["authority_source"] == "operator"


def test_library_refuses_chat_attested_at_both_waiver_subject_shapes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The gate holds below the CLI, for the scoped and the standing subject,
    and the refusal is the specific subclass whose text names the attended
    command (the generic refusal's advice points at this door, which is the
    thing being closed)."""
    from fno.decide import (
        RefusedAuthorityError,
        WaiverAuthorityRefusedError,
        record_decision,
    )

    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    for subject in (WAIVER_SUBJECT, "review-coverage-waiver"):
        with pytest.raises(WaiverAuthorityRefusedError, match="coverage-waive"):
            record_decision(
                subject=subject,
                decision=WAIVER_DECISION,
                rationale="why",
                authority_source="chat_attested",
            )
        # The subclass keeps the parent's contract, so existing handlers that
        # catch RefusedAuthorityError keep working.
        assert issubclass(WaiverAuthorityRefusedError, RefusedAuthorityError)
    assert _rows(index) == []


def test_a_lookalike_subject_is_ordinary_law(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The family is the exact standing subject and the colon-delimited
    scoped form, the two shapes the gate reads. A free-form law subject that
    merely begins with the text records under chat_attested like any other
    ordinary law, and the refusal never touches it."""
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            "review-coverage-waiver-policy",
            "Waiver requests route through the operator",
            "--rationale",
            "policy about waivers, not a waiver",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    rows = _rows(index)
    assert len(rows) == 1
    assert rows[0]["data"]["subject"] == "review-coverage-waiver-policy"
    assert rows[0]["data"]["authority_source"] == "chat_attested"


# ── the evidence gate: a code fact carries the read that produced it ──────────


def _with_advance(tmp_path: Path) -> None:
    """`advance.py` in the pinned root, so the specimen citation resolves and
    the refusal exercises the missing-read branch, not the untracked one."""
    (tmp_path / "advance.py").write_text(
        "\n".join(f"line {i}" for i in range(1, 201)) + "\n", encoding="utf-8"
    )


def _scripted_gate(monkeypatch: pytest.MonkeyPatch, answer):
    """Swap the evidence-gate transport for `answer(payload)`; the payloads
    the lanes send stay assertable on the returned call list."""
    calls: list[dict] = []

    def _gate(payload):
        calls.append(payload)
        return answer(payload)

    monkeypatch.setattr("fno.decide._evidence_gate", _gate)
    return calls


def _run_reads_answer(payload):
    """A responder that actually runs the requested reads in the payload's
    root, so the row stored on the ruling carries real command output."""
    import subprocess

    rows = []
    for cmd in payload["reads"] or []:
        done = subprocess.run(
            cmd, shell=True, cwd=payload["root"], capture_output=True, text=True
        )
        rows.append(
            {
                "cmd": cmd,
                "exit": done.returncode,
                "out_head": "\n".join(done.stdout.splitlines()[:5]),
                "ts": "2026-09-10T00:00:00Z",
                "head_sha": "",
            }
        )
    return {"ok": True, "rows": rows}


def test_code_fact_with_no_read_is_refused_with_exit_3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC11-ERR: specimen 2, replayed. A ruling naming advance.py:167 as a
    fact records nothing until the command that measured it rides along."""
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)
    _with_advance(tmp_path)
    calls = _scripted_gate(
        monkeypatch,
        lambda payload: {
            "ok": False,
            "kind": "unmeasured",
            "message": (
                "the ruling asserts a code fact ('advance.py:167') and carries "
                "no read. Attach --read with the command that produced it."
            ),
        },
    )

    result = _run(
        [
            "set",
            "territory-resolver",
            "advance.py:167 is the territory resolver",
            "--rationale",
            "port it to Rust",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "Nothing was recorded." in result.output
    assert "advance.py:167" in result.output
    assert _rows(index) == []
    assert calls[0]["reads"] is None
    assert "advance.py:167" in calls[0]["text"]


def test_code_fact_with_a_read_records_the_executed_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC12-HP: the same body with --read records, and the stored row carries
    the command, its exit code and the head of its output."""
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)
    _with_advance(tmp_path)
    calls = _scripted_gate(monkeypatch, _run_reads_answer)

    result = _run(
        [
            "set",
            "territory-resolver",
            "advance.py is 200 lines",
            "--rationale",
            "measured before ruling",
            "--read",
            "head -5 advance.py",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    assert calls[0]["reads"] == ["head -5 advance.py"]
    rows = _rows(index)
    reads = rows[0]["data"]["reads"]
    assert reads[0]["cmd"] == "head -5 advance.py"
    assert reads[0]["exit"] == 0
    assert "line 1" in reads[0]["out_head"]


def test_contradicted_citation_is_refused_even_with_a_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC13-ERR: a citation the repo contradicts is refused whatever is
    attached to it."""
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)
    _with_advance(tmp_path)
    calls = _scripted_gate(
        monkeypatch,
        lambda payload: {
            "ok": False,
            "kind": "citation",
            "message": "advance.py:99999: the file has 200 lines.",
        },
    )

    result = _run(
        [
            "set",
            "territory-resolver",
            "advance.py:99999 is the territory resolver",
            "--rationale",
            "port it to Rust",
            "--read",
            "head -5 advance.py",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "99999" in result.output
    assert _rows(index) == []
    assert calls[0]["reads"] == ["head -5 advance.py"]


def test_attended_operator_records_an_unmeasured_body_untouched(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC14-EDGE: the exemption keys on the RESOLVED authority, so a person
    at a terminal is never blocked mid-waiver by a regex."""
    from types import SimpleNamespace

    from fno import decide
    from fno.agents import self_stamp

    index = _isolate(tmp_path, monkeypatch)
    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id=None, harness=None),
    )
    monkeypatch.setattr(decide, "_attended_terminal", lambda: True)
    _with_advance(tmp_path)

    result = _run(
        [
            "set",
            "territory-resolver",
            "advance.py:167 is the territory resolver",
            "--rationale",
            "the operator measured it by hand",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    rows = _rows(index)
    assert rows[0]["data"]["authority_source"] == "operator"
    assert "reads" not in rows[0]["data"]


def test_body_with_no_code_fact_records_with_no_reads_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC15-EDGE: no claim, no change from today's behavior."""
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)
    calls = _scripted_gate(monkeypatch, lambda payload: {"ok": True, "rows": None})

    result = _run(
        [
            "set",
            "merge-authority",
            "Merges belong to the operator",
            "--rationale",
            "The operator owns durable policy.",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    rows = _rows(index)
    assert "reads" not in rows[0]["data"]
    assert calls[0]["text"] == (
        "Merges belong to the operator\nThe operator owns durable policy."
    )


# ── the rule-time sweep: a new law names the open questions it may answer ─────


class TestLawSetSweep:
    """`law set` names open questions the new law may answer (x-cf6a). The
    sweep is best-effort stderr side work: stdout stays exactly the `d-` id
    and the exit stays 0, pass or fail - exit 1 is reserved for a failed
    index write."""

    def test_sweep_lines_hit_stderr_and_stdout_stays_the_id(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, real_sweep
    ) -> None:
        index = _isolate(tmp_path, monkeypatch)
        _as_chat_session(monkeypatch)
        questions = tmp_path / "questions.jsonl"
        questions.write_text(
            json.dumps(
                {
                    "ts": "2026-09-12T17:16:00Z",
                    "type": "operator_question",
                    "data": {
                        "question_id": "q-470f40d2",
                        "question": "raise the allowance for pr-1847?",
                        "subject": "pr-1847-budget-exception",
                        "asker": "2cf809f6",
                        "session_id": "sess-9f2c",
                    },
                }
            )
            + "\n"
        )
        monkeypatch.setattr("fno.paths.questions_jsonl", lambda: questions)

        def fake_verb(verb, payload, *a, **k):
            assert verb == "law-match"
            if payload["mode"] == "record-scope":
                return {"ok": True, "scope": "project:fno"}
            if payload["mode"] == "validate":
                return {"ok": True, "refusal": None}
            assert payload["mode"] == "law"
            assert payload["law"]["subject"] == "file-budget"
            assert payload["questions"][0]["id"] == "q-470f40d2"
            return {
                "ok": True,
                "candidates": [{"question_id": "q-470f40d2"}],
                "total": 1,
                "lines": [
                    'law: d-1a2b3c4d may answer open q-470f40d2 (asker 2cf809f6): '
                    '"raise the allowance for pr-1847?"'
                ],
            }

        monkeypatch.setattr("fno.rust_binary.verb_call", fake_verb)

        result = _run(
            [
                "set",
                "file-budget",
                "The allowance is never raised.",
                "--rationale",
                "The operator owns the budget.",
            ]
        )

        assert result.exit_code == 0, result.output
        decision_id = result.stdout.strip()
        assert decision_id.startswith("d-")
        assert "may answer open q-470f40d2" in result.stderr
        rows = _rows(index)
        assert rows[0]["data"]["decision_id"] == decision_id

    def test_a_failed_sweep_keeps_exit_0_and_stdout(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, real_sweep
    ) -> None:
        index = _isolate(tmp_path, monkeypatch)
        _as_chat_session(monkeypatch)
        questions = tmp_path / "questions.jsonl"
        questions.write_text("")
        monkeypatch.setattr("fno.paths.questions_jsonl", lambda: questions)

        def broken(*a, **k):
            if len(a) > 1 and isinstance(a[1], dict):
                mode = a[1].get("mode")
                if mode == "validate":
                    return {"ok": True, "refusal": None}
                if mode == "record-scope":
                    return {"ok": True, "scope": "project:fno"}
            raise RuntimeError("matcher exploded")

        monkeypatch.setattr("fno.rust_binary.verb_call", broken)

        result = _run(
            [
                "set",
                "file-budget",
                "The allowance is never raised.",
                "--rationale",
                "The operator owns the budget.",
            ]
        )

        assert result.exit_code == 0, result.output
        assert result.stdout.strip().startswith("d-")
        assert "open-question sweep failed" in result.stderr
        rows = _rows(index)
        assert len(rows) == 1, "the law itself is recorded"


# ── the subject is a topic, never a bare node or PR id ────────────────────────


@pytest.mark.parametrize("subject", ["x-1df4", "pr-1157"])
def test_a_node_or_pr_id_is_refused_as_a_law_subject(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, subject: str
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(["set", subject, "Two rounds.", "--rationale", "r"])

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "a node or PR id is not a law subject" in result.output
    assert _rows(index) == []


def test_a_topic_subject_citing_a_node_id_records(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The make-it-fail control: the SAME shape a node-id subject would take
    is legal inside the decision text; only the SUBJECT is the key."""
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            "review-rounds-cap",
            "The node x-1df4 dispute is settled: two rounds.",
            "--rationale",
            "r",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    assert _rows(index)


def test_an_unavailable_validator_refuses_the_recording(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fail closed: a statement nobody could classify records nothing. Exit 1
    stays reserved for 'recorded, index write failed', so the unavailable
    verb must land on 3."""
    from fno.rust_binary import VerbUnavailable

    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    def down(*a, **k):
        raise VerbUnavailable("the fno-agents binary was not found")

    monkeypatch.setattr("fno.rust_binary.verb_call", down)

    result = _run(["set", "review-rounds", "Two rounds.", "--rationale", "r"])

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "law validation is unavailable" in result.output
    assert "Nothing was recorded." in result.output
    assert _rows(index) == []
