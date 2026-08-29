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


LAW_RECORDED_EXIT = 0
LAW_REFUSED_EXIT = 3


def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    from fno import paths

    paths.resolve_repo_root.cache_clear()
    (tmp_path / ".fno").mkdir(parents=True)
    index = tmp_path / "state" / "decisions.jsonl"
    index.parent.mkdir()
    index.touch()
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)
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
            "x-12ba",
            "Merges belong to the operator",
            "--rationale",
            "The operator owns durable policy.",
        ]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    decision_id = result.output.strip().splitlines()[-1]
    assert decision_id.startswith("d-"), result.output

    rows = [json.loads(line) for line in index.read_text().splitlines() if line.strip()]
    assert len(rows) == 1
    data = rows[0]["data"]
    assert data["decision_id"] == decision_id
    # The honest attribution is what survived the trade: a chat recording never
    # claims the operator lane.
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

    result = _run(["set", "x-12ba", decision, "--rationale", "why"])

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "coordination" in result.output
    assert index.read_text() == ""


def test_missing_rationale_is_refused_with_exit_3(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(["set", "x-12ba", "Merges belong to the operator"])

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "rationale is required" in result.output
    assert index.read_text() == ""


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
        ["set", "x-12ba", "Merges belong to the operator", "--rationale", "why"]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    assert result.output.strip().splitlines()[-1].startswith("d-")
    assert index.read_text().strip()


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
        ["set", "x-12ba", "Merges belong to the operator", "--rationale", "why"]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "nothing here marks a decider" in result.output
    assert index.read_text() == ""


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
            subject="x-12ba",
            decision="Merges belong to the operator",
            rationale="why",
            authority_source="chat_attested",
        )
    assert index.read_text() == ""


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
            subject="x-12ba",
            decision="This PR merges without review",
            rationale="why",
            authority_source="chat_attested",
        )
    assert index.read_text() == ""


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
        ["set", "x-12ba", "Merges belong to the operator", "--rationale", "why"]
    )

    assert result.exit_code == LAW_RECORDED_EXIT, result.output
    rows = [json.loads(line) for line in index.read_text().splitlines() if line.strip()]
    assert rows[0]["data"]["authority_source"] == "operator"


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
    # claim the operator lane just because someone happened to be at a tty.
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
            "x-12ba",
            "Merges belong to the operator",
            "--rationale",
            "why",
            "--supersedes",
            "d-deadbeef",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "not recoverable" in result.output
    assert index.read_text() == ""


def test_supersedes_must_be_a_decision_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index = _isolate(tmp_path, monkeypatch)
    _as_chat_session(monkeypatch)

    result = _run(
        [
            "set",
            "x-12ba",
            "Merges belong to the operator",
            "--rationale",
            "why",
            "--supersedes",
            "x-12ba",
        ]
    )

    assert result.exit_code == LAW_REFUSED_EXIT, result.output
    assert "supersedes must be a decision id" in result.output
    assert index.read_text() == ""
