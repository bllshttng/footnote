"""`fno backlog encounter` - the agent demand signal, and its four refusals.

An encounter is a thing that happened, not an opinion. So the refusals ARE the
feature: evidence is required, identity must be provable, over-length evidence
is refused rather than truncated, and one session votes once. A verb that
permits all four is a popularity contest between agents the operator spawned.

Every refusal here is proven by a MAKE-IT-FAIL probe that runs the real verb in
a real subprocess and asserts the exact exit code plus a positive stderr marker.
Two habits are deliberate.

An exact code, never merely non-zero. Typer answers 2 for a usage error, so a
probe asserting "non-zero" also passes when the invocation has a typo in a flag.
`test_a_typo_is_exit_2_and_the_identity_refusal_is_not` is the positive control
that makes the distinction real: it aims the same probe at a deliberately
malformed invocation and shows the two codes differ.

A positive marker, never an absence. An exit code alone cannot separate a
duplicate refusal from a crash, so each probe also matches a string only that
refusal produces.

Identity is injected in the probe rather than resolved. `resolve_self_identity`
proves ownership by walking the process tree for a harness ancestor, which a
pytest subprocess does not have in CI and DOES have on a developer's machine.
Reading it live would make these tests pass or fail on where they run. The probe
replaces that one prover and leaves the verb, typer's exit codes, the graph
lock, and the render fanout running for real.
"""
from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest


# The probe entry point. `python -c SCRIPT a b c` leaves sys.argv as
# ["-c", "a", "b", "c"], which is exactly the shape typer reads.
_PROBE = r"""
import os, sys, types
import fno.claims.self_identity as si

sid = os.environ.get("PROBE_SESSION_ID", "")
if sid == "__none__":
    si.resolve_self_identity = lambda *a, **k: None
elif sid == "":
    si.resolve_self_identity = lambda *a, **k: types.SimpleNamespace(
        session_id=None, harness=None
    )
else:
    si.resolve_self_identity = lambda *a, **k: types.SimpleNamespace(
        session_id=sid, harness=os.environ.get("PROBE_HARNESS", "claude")
    )

from fno.cli import app
app()
"""

SESSION_A = "aaaaaaaa-1111-4111-8111-111111111111"
SESSION_B = "bbbbbbbb-2222-4222-8222-222222222222"


def _words(n: int) -> str:
    """``n`` masked words that break no style rule except, possibly, rule 7."""
    full, remainder = divmod(n, 5)
    sentences = ["word word word word word." for _ in range(full)]
    if remainder:
        sentences.append(" ".join("word" for _ in range(remainder)) + ".")
    return " ".join(sentences)


@pytest.fixture
def probe(tmp_path: Path):
    """A hermetic state dir plus a runner for the real verb in a real process."""
    state = tmp_path / "state"
    state.mkdir()
    settings = tmp_path / "settings.yaml"
    settings.write_text(f"schema_version: 1\nconfig:\n  state_dir: {state}\n", encoding="utf-8")

    graph = state / "graph.json"
    graph.write_text('{"entries": []}\n', encoding="utf-8")

    def run(
        *args: str,
        session_id: str = SESSION_A,
        harness: str = "claude",
        extra_env: dict | None = None,
    ):
        env = dict(os.environ)
        env["FNO_CONFIG"] = str(settings)
        env["FNO_REPO_ROOT"] = str(tmp_path)
        env["PROBE_SESSION_ID"] = session_id
        env["PROBE_HARNESS"] = harness
        env.pop("FNO_STYLE_ENFORCE", None)
        env.update(extra_env or {})
        return subprocess.run(
            [sys.executable, "-c", _PROBE, *args],
            capture_output=True,
            text=True,
            env=env,
            cwd=str(tmp_path),
        )

    run.graph = graph  # type: ignore[attr-defined]
    run.md = state / "graph.md"  # type: ignore[attr-defined]
    run.settings = settings  # type: ignore[attr-defined]
    run.state = state  # type: ignore[attr-defined]
    return run


def _seed(probe, *entries: dict) -> None:
    probe.graph.write_text(json.dumps({"entries": list(entries)}, indent=2) + "\n", "utf-8")


def _node(node_id: str = "zz-0001", **over) -> dict:
    entry = {
        "id": node_id,
        "slug": f"slug-{node_id}",
        "title": f"node {node_id}",
        "status": "ready",
        "priority": "p2",
        "_kanban_column": "Next",
    }
    entry.update(over)
    return entry


def _entries(probe) -> list[dict]:
    return json.loads(probe.graph.read_text(encoding="utf-8"))["entries"]


def _encounters(probe, node_id: str = "zz-0001") -> list[dict]:
    for entry in _entries(probe):
        if entry["id"] == node_id:
            return entry.get("encounters") or []
    raise AssertionError(f"no node {node_id} in the graph")


# --- the four refusals, each by exact code and positive marker ----------------


def test_evidence_is_mandatory(probe):
    """AC7. An encounter with no evidence is the poll this feature refuses to be."""
    _seed(probe, _node())
    result = probe("backlog", "encounter", "zz-0001", "--evidence", "   ")
    assert result.returncode == 1, result.stderr
    assert "evidence" in result.stderr
    assert _encounters(probe) == []


def test_an_unprovable_identity_cannot_vote(probe):
    """AC5. No provenance means no falsifiability, which means no vote."""
    _seed(probe, _node())
    result = probe("backlog", "encounter", "zz-0001", "--evidence", "hit a wall.", session_id="")
    assert result.returncode == 5, result.stderr
    assert "fno whoami" in result.stderr
    assert _encounters(probe) == []


def test_operator_vote_does_not_require_session_identity(probe):
    """AC1-HP: the explicit operator voter works from a plain terminal."""
    _seed(probe, _node())

    result = probe(
        "backlog",
        "encounter",
        "zz-0001",
        "--operator",
        "--evidence",
        "the operator hit the same seam.",
        session_id="",
    )

    assert result.returncode == 0, result.stderr
    assert "operator" in result.stdout
    assert _encounters(probe) == [
        {
            "ts": _encounters(probe)[0]["ts"],
            "voter_key": "operator",
            "voter_kind": "operator",
            "evidence": "the operator hit the same seam.",
        }
    ]


def test_operator_vote_is_deduped_by_voter_key(probe):
    """AC4-EDGE: operator votes use one shared, stable dedupe key."""
    _seed(probe, _node())
    first = probe(
        "backlog",
        "encounter",
        "zz-0001",
        "--operator",
        "--evidence",
        "first operator encounter.",
        session_id="",
    )
    assert first.returncode == 0, first.stderr
    first_ts = _encounters(probe)[0]["ts"]

    second = probe(
        "backlog",
        "encounter",
        "zz-0001",
        "--operator",
        "--evidence",
        "second operator encounter.",
        session_id="another-session",
    )

    assert second.returncode == 3, second.stderr
    assert first_ts in second.stderr


def test_operator_vote_keeps_the_casting_session_on_the_record(probe):
    """The operator lane is a declaration, so the record still names who cast it.

    session_id is the provenance key the falsifiability contract names and the
    one every pre-operator record carried. voter_key staying ``operator`` is
    what keeps the lane one-per-node and split-visible, so both ride along.
    """
    _seed(probe, _node())

    result = probe(
        "backlog",
        "encounter",
        "zz-0001",
        "--operator",
        "--evidence",
        "the operator hit the same seam.",
    )

    assert result.returncode == 0, result.stderr
    record = _encounters(probe)[0]
    assert record["voter_key"] == "operator"
    assert record["session_id"] == SESSION_A


def test_a_none_identity_is_the_same_refusal(probe):
    """The prover may answer None outright; that is the same missing provenance."""
    _seed(probe, _node())
    result = probe(
        "backlog", "encounter", "zz-0001", "--evidence", "hit a wall.", session_id="__none__"
    )
    assert result.returncode == 5, result.stderr
    assert "fno whoami" in result.stderr


def test_a_typo_is_exit_2_and_the_identity_refusal_is_not(probe):
    """The positive control for AC5.

    Typer answers 2 for a usage error. A probe that asserted "non-zero" for the
    identity refusal would pass on this line too, and would be proving nothing.
    """
    _seed(probe, _node())
    typo = probe("backlog", "encounter", "zz-0001", "--evidenc", "hit a wall.")
    assert typo.returncode == 2
    identity = probe(
        "backlog", "encounter", "zz-0001", "--evidence", "hit a wall.", session_id=""
    )
    assert identity.returncode != typo.returncode


def test_over_length_evidence_is_refused_and_never_truncated(probe):
    """AC3. The refusal names the cap enforced and the real count."""
    _seed(probe, _node())
    result = probe("backlog", "encounter", "zz-0001", "--evidence", _words(120))
    assert result.returncode == 4, result.stderr
    assert "120" in result.stderr
    assert "80" in result.stderr
    assert _encounters(probe) == []


def test_a_configured_encounter_cap_is_the_one_enforced(probe):
    """The cap the refusal names comes from config, not from the module default."""
    probe.settings.write_text(
        "schema_version: 1\n"
        f"config:\n  state_dir: {probe.state}\n"
        "  style:\n    word_cap:\n      encounter: 20\n",
        encoding="utf-8",
    )
    _seed(probe, _node())
    result = probe("backlog", "encounter", "zz-0001", "--evidence", _words(30))
    assert result.returncode == 4, result.stderr
    assert "20" in result.stderr
    assert _encounters(probe) == []


def test_the_style_kill_switch_reaches_this_surface(probe):
    """`docs/style-rules.md` says rule 7 inherits the existing escapes.

    A surface that quietly opts out makes that sentence false. What stays
    unescapable is the falsifiability contract: evidence is still required and
    identity is still proven, whatever this switch says.
    """
    _seed(probe, _node())
    result = probe(
        "backlog",
        "encounter",
        "zz-0001",
        "--evidence",
        _words(120),
        extra_env={"FNO_STYLE_ENFORCE": "0"},
    )
    assert result.returncode == 0, result.stderr
    assert len(_encounters(probe)) == 1


def test_the_kill_switch_does_not_escape_evidence_or_identity(probe):
    """The positive control for the test above: the switch is a LENGTH escape."""
    _seed(probe, _node())
    empty = probe(
        "backlog", "encounter", "zz-0001", "--evidence", "  ",
        extra_env={"FNO_STYLE_ENFORCE": "0"},
    )
    assert empty.returncode == 1, empty.stderr
    anonymous = probe(
        "backlog", "encounter", "zz-0001", "--evidence", "hit a wall.",
        session_id="", extra_env={"FNO_STYLE_ENFORCE": "0"},
    )
    assert anonymous.returncode == 5, anonymous.stderr
    assert _encounters(probe) == []


def test_a_style_exception_line_also_escapes_the_cap(probe):
    _seed(probe, _node())
    result = probe(
        "backlog",
        "encounter",
        "zz-0001",
        "--evidence",
        "style-exception: pasted a measured table\n" + _words(120),
    )
    assert result.returncode == 0, result.stderr
    assert len(_encounters(probe)) == 1


def test_one_session_votes_once(probe):
    """AC1. The second vote is REFUSED, never silently idempotent.

    A silent success is indistinguishable from a working write, which is what
    makes it the wrong answer here: the caller learns nothing and the probe
    proves nothing.
    """
    _seed(probe, _node())
    first = probe("backlog", "encounter", "zz-0001", "--evidence", "cost two wrong diagnoses.")
    assert first.returncode == 0, first.stderr
    recorded = _encounters(probe)
    assert len(recorded) == 1

    second = probe("backlog", "encounter", "zz-0001", "--evidence", "hit the same wall again.")
    assert second.returncode == 3, second.stderr
    assert recorded[0]["ts"] in second.stderr
    assert len(_encounters(probe)) == 1


def test_a_second_session_votes_freely(probe):
    """The refusal is per session, not per node."""
    _seed(probe, _node())
    assert probe("backlog", "encounter", "zz-0001", "--evidence", "one.").returncode == 0
    assert (
        probe(
            "backlog", "encounter", "zz-0001", "--evidence", "two.", session_id=SESSION_B
        ).returncode
        == 0
    )
    assert len(_encounters(probe)) == 2


def test_an_unknown_node_refuses_with_its_own_marker(probe):
    _seed(probe, _node())
    result = probe("backlog", "encounter", "nope-9999", "--evidence", "hit a wall.")
    assert result.returncode == 1, result.stderr
    assert "no node resolves" in result.stderr


# --- what a recorded encounter carries ---------------------------------------


def test_a_vote_is_readable_back_to_a_transcript(probe):
    """AC2. session_id plus harness is what makes the count falsifiable."""
    _seed(probe, _node())
    result = probe("backlog", "encounter", "zz-0001", "--evidence", "cost a CI cycle.")
    assert result.returncode == 0, result.stderr
    record = _encounters(probe)[0]
    assert record["session_id"] == SESSION_A
    assert record["voter_key"] == SESSION_A
    assert record["voter_kind"] == "agent"
    assert record["harness"] == "claude"
    assert record["evidence"] == "cost a CI cycle."
    assert record["ts"].endswith("+00:00") or record["ts"].endswith("Z")

    from fno.harness_identity import canonical_handle

    assert record["fno_id"] == canonical_handle(SESSION_A)


def test_the_success_line_is_a_receipt(probe):
    _seed(probe, _node())
    result = probe("backlog", "encounter", "zz-0001", "--evidence", "cost a CI cycle.")
    assert "zz-0001" in result.stdout
    assert "1" in result.stdout


def test_json_output_carries_the_record(probe):
    _seed(probe, _node())
    result = probe("backlog", "encounter", "zz-0001", "--evidence", "cost a CI cycle.", "--json")
    payload = json.loads(result.stdout)
    assert payload["id"] == "zz-0001"
    assert payload["encounter"]["session_id"] == SESSION_A
    assert payload["total"] == 1


# --- the ordering contract ---------------------------------------------------


def test_the_board_does_not_move(probe):
    """AC4. The board IS the work order, so a vote must not re-rank it.

    Bytes, not a field read. A rank recomputed identically and a rank never
    touched are the same board; a rank changed by one row is a different one,
    and the digest is what tells them apart.
    """
    _seed(
        probe,
        _node("zz-0001", priority="p3"),
        _node("zz-0002", priority="p0"),
        _node("zz-0003", priority="p2"),
    )
    # Render the board through a real mutation on an unrelated node.
    assert probe("backlog", "encounter", "zz-0003", "--evidence", "seeded.").returncode == 0
    before = hashlib.sha256(probe.md.read_bytes()).hexdigest()
    ranks_before = {e["id"]: e.get("rank") for e in _entries(probe)}
    columns_before = {e["id"]: e.get("_kanban_column") for e in _entries(probe)}

    assert probe("backlog", "encounter", "zz-0001", "--evidence", "cost a wrong fix.").returncode == 0

    after = hashlib.sha256(probe.md.read_bytes()).hexdigest()
    assert after == before
    assert {e["id"]: e.get("rank") for e in _entries(probe)} == ranks_before
    assert {e["id"]: e.get("_kanban_column") for e in _entries(probe)} == columns_before


def test_a_graph_with_no_encounters_serializes_byte_identical(probe):
    """The field is sparse. A null on every node would break every board digest."""
    _seed(probe, _node("zz-0001"), _node("zz-0002"))
    assert probe("backlog", "note", "zz-0001", "a note.").returncode == 0
    baseline = probe.graph.read_bytes()
    assert b"encounters" not in baseline

    assert probe("backlog", "note", "zz-0002", "another note.").returncode == 0
    assert b"encounters" not in probe.graph.read_bytes()


# --- the note advisory (progress_notes stays uncapped) -----------------------


def test_a_long_note_warns_and_still_lands(probe):
    """AC10. A refusal that destroys evidence is the wrong instrument here."""
    _seed(probe, _node())
    result = probe("backlog", "note", "zz-0001", _words(400))
    assert result.returncode == 0, result.stderr
    assert "400" in result.stderr
    notes = [e for e in _entries(probe) if e["id"] == "zz-0001"][0]["progress_notes"]
    assert len(notes) == 1


def test_an_ordinary_note_says_nothing(probe):
    _seed(probe, _node())
    result = probe("backlog", "note", "zz-0001", _words(50))
    assert result.returncode == 0, result.stderr
    assert result.stderr.strip() == ""


# --- the store primitive -----------------------------------------------------


def test_append_encounter_names_the_existing_timestamp(tmp_path, monkeypatch):
    """The duplicate error must say WHEN, so the caller can decide what to do."""
    from fno.graph.store import append_encounter

    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [_node()]}), encoding="utf-8")
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")

    first = {"ts": "2026-08-29T05:00:00+00:00", "session_id": SESSION_A, "evidence": "one."}
    appended, error, reason = append_encounter(graph, "zz-0001", first)
    assert appended is True
    assert error is None
    assert reason is None

    second = {"ts": "2026-08-29T06:00:00+00:00", "session_id": SESSION_A, "evidence": "two."}
    appended, error, reason = append_encounter(graph, "zz-0001", second)
    assert appended is False
    assert reason == "duplicate"
    assert error is not None
    assert "2026-08-29T05:00:00+00:00" in error

    stored = json.loads(graph.read_text(encoding="utf-8"))["entries"][0]["encounters"]
    assert len(stored) == 1
    assert stored[0]["evidence"] == "one."


def test_append_encounter_reports_a_missing_node(tmp_path, monkeypatch):
    from fno.graph.store import append_encounter

    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": []}), encoding="utf-8")
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")

    appended, error, reason = append_encounter(graph, "zz-0001", {"ts": "x", "session_id": SESSION_A})
    assert appended is False
    assert reason == "missing"
    assert error is not None
    assert "zz-0001" in error


def test_a_reason_symbol_not_prose_picks_the_exit_code(tmp_path, monkeypatch):
    """A caller matching the error WORDING breaks the first time it improves."""
    from fno.graph.store import append_encounter

    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [_node()]}), encoding="utf-8")
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")

    appended, error, reason = append_encounter(graph, "zz-0001", {"ts": "x"})
    assert appended is False
    assert reason == "unidentified"
    assert "session_id" in error

    # The collapse this guards: without the refusal, one anonymous record
    # matches every later anonymous record and blocks all of them.
    assert json.loads(graph.read_text(encoding="utf-8"))["entries"][0].get("encounters") is None


def test_append_encounter_dedupes_operator_without_session_id(tmp_path, monkeypatch):
    from fno.graph.store import append_encounter

    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [_node()]}), encoding="utf-8")
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")

    first = {
        "ts": "2026-08-29T05:00:00+00:00",
        "voter_key": "operator",
        "voter_kind": "operator",
        "evidence": "first.",
    }
    appended, error, reason = append_encounter(graph, "zz-0001", first)
    assert appended is True
    assert error is None
    assert reason is None

    second = dict(first, ts="2026-08-29T06:00:00+00:00", evidence="second.")
    appended, error, reason = append_encounter(graph, "zz-0001", second)
    assert appended is False
    assert reason == "duplicate"
    assert error is not None
    assert "2026-08-29T05:00:00+00:00" in error
