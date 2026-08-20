"""Tests for `fno outstanding` - the operator-facing read of what is piled up.

Two legs, both over stores that already exist: unharvested carve-outs and open
operator questions. The verb exists because the carve-out ledger reached 39 rows
over 29 days with nothing ever asking a human to look at it - `consume_carveouts`
was wired the whole time, but a wire nobody is told to pull is not a surface.

Every assertion here names a POSITIVE marker rather than an absence. An
absence has two explanations (the real outcome, or the instrument never ran)
and a test built on one cannot tell them apart; test_hook_block_positive_control
is the pair that closes that gap for the silent-on-zero case.
"""

from __future__ import annotations

import json
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.harness_identity import HarnessIdentity
from fno.king.lane import LaneItem
from fno.outstanding.cli import outstanding_app
from fno.outstanding.core import RENDER_CAP, Outstanding, Question, render

runner = CliRunner()


def _write_carveouts(root: Path, rows: list[dict]) -> Path:
    ledger = root / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        "".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8"
    )
    return ledger


def _carveout(cid: str, kind: str, ts: str = "2026-07-14T04:27:55Z") -> dict:
    return {
        "id": cid,
        "ts": ts,
        "session_id": "sess-a",
        "kind": kind,
        "priority": None,
        "need": None,
        "description": f"{cid} description",
        "truncated": False,
    }


@pytest.fixture()
def root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A hermetic canonical root; both legs resolve through it."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
    monkeypatch.delenv("FNO_BG", raising=False)
    # Presence resolves through classify_presence, whose documented default is
    # "away" when no human signal is present - correct for a cron/script, and
    # what a bare pytest process looks like. Declare the attended case here so
    # each test states the presence it is exercising instead of inheriting it.
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    # resolve_repo_root is @cache'd and is warmed with the REAL worktree root
    # before this fixture runs. Without the clear, session resolution reads the
    # live target-state.md instead of this sandbox, and the ownership tests
    # would pass on the real session id rather than the one they set.
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    # The clear path projects decisions onto the subject node's graph entry,
    # resolving GRAPH_JSON through the module attribute. Pin it to a
    # nonexistent path so these tests never read or write the real machine
    # graph when a --node happens to resolve there.
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", tmp_path / "graph.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", tmp_path / "graph.json")
    # Sidecar/metadata seam readers resolve through paths.graph_json at call
    # time; pin the resolver too or the seam reads the real machine graph.
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(
        "fno.paths.questions_jsonl",
        lambda: tmp_path / "questions.jsonl",
        raising=False,
    )
    # Every test in this module that calls `collect()` without an explicit
    # `lane=` must never touch the real machine's ~/.fno/my-priorities.md -
    # pin it to a sandbox path that starts out missing (an empty lane).
    monkeypatch.setattr(
        "fno.paths.operator_lane",
        lambda: tmp_path / "my-priorities.md",
        raising=False,
    )
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    return tmp_path


# --- 2.1 both legs -----------------------------------------------------------


def test_both_legs_report_a_count_each(root: Path):
    _write_carveouts(
        root,
        [
            _carveout("cv-1", "oos-bug"),
            _carveout("cv-2", "oos-bug"),
            _carveout("cv-3", "deferred"),
        ],
    )
    assert runner.invoke(outstanding_app, ["ask", "should the gate refuse?"]).exit_code == 0
    assert runner.invoke(outstanding_app, ["ask", "which base do we rebase on?"]).exit_code == 0

    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0, result.output

    # The carve-out leg names the split, not just a total: only `deferred`
    # blocks a node close, so a bare total hides which rows have a gate.
    assert "3 carve-out" in result.output
    assert "2 oos-bug" in result.output
    assert "1 deferred" in result.output
    assert "2 open question" in result.output


def test_carveout_leg_names_the_verb_that_clears_it(root: Path):
    _write_carveouts(root, [_carveout("cv-1", "oos-bug")])
    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0
    # The whole fix for "nothing clears them": the ledger sat 29 days because
    # nobody was ever told what to run.
    assert "fno retro sweep-carveouts" in result.output


def test_carveout_leg_reports_the_age_of_the_oldest_row(root: Path):
    """Pins the DATE, not the word "oldest".

    The label is emitted for any row carrying a ts, so an `or "oldest" in ...`
    disjunct stays green even if the date slice regressed. Assert the thing
    measured, which is this file's own stated rule.
    """
    _write_carveouts(
        root,
        [
            _carveout("cv-old", "oos-bug", ts="2026-01-01T00:00:00Z"),
            _carveout("cv-new", "oos-bug", ts="2026-08-01T00:00:00Z"),
        ],
    )
    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0
    assert "oldest 2026-01-01" in result.output
    # The newer row's date must NOT be the one reported as oldest.
    assert "oldest 2026-08-01" not in result.output


# --- 2.2 an unreadable ledger is a stated failure ----------------------------


def test_unreadable_ledger_is_a_stated_failure_not_silence(root: Path):
    ledger = root / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    # Present but undecodable. read_carveouts raises rather than returning []
    # for exactly this case; the new verb must not re-introduce the masquerade.
    ledger.write_bytes(b"\xff\xfe not utf-8 at all\n")

    result = runner.invoke(outstanding_app, [])

    assert result.exit_code != 0
    # Positive marker in the FAILURE text, pinned to the thing measured.
    assert "cannot read" in result.output or "failed to read" in result.output
    # And never the success line: a failed read reported as "nothing
    # outstanding" is the exact absence-as-success trap.
    assert "nothing outstanding" not in result.output.lower()


# --- 2.3 ask then clear ------------------------------------------------------


def test_ask_clear_round_trip_and_idempotence(root: Path):
    asked = runner.invoke(outstanding_app, ["ask", "do we widen the fold window?"])
    assert asked.exit_code == 0, asked.output
    qid = asked.stdout.strip().splitlines()[-1].strip()
    assert qid, "ask must print the new question id on stdout"

    listed = runner.invoke(outstanding_app, ["--json"])
    assert listed.exit_code == 0
    payload = json.loads(listed.stdout)
    assert [q["id"] for q in payload["questions"]] == [qid]

    cleared = runner.invoke(outstanding_app, ["clear", qid, "--answer", "yes, widen it"])
    assert cleared.exit_code == 0, cleared.output
    assert "1" in cleared.stdout

    after = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)
    assert after["questions"] == []

    # Idempotent: a second clear, and an id that was never open, are exit-0
    # no-ops that report a count of 0 rather than failing.
    again = runner.invoke(outstanding_app, ["clear", qid])
    assert again.exit_code == 0
    assert "0" in again.stdout

    unknown = runner.invoke(outstanding_app, ["clear", "q-neverexisted"])
    assert unknown.exit_code == 0
    assert "0" in unknown.stdout


def test_ask_records_the_answer_text_on_clear(root: Path):
    qid = runner.invoke(outstanding_app, ["ask", "which lane?"]).stdout.strip().splitlines()[-1]
    runner.invoke(outstanding_app, ["clear", qid, "--answer", "the codex lane"])
    lines = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    closed = [e for e in lines if e["type"] == "operator_question_closed"]
    assert len(closed) == 1
    assert closed[0]["data"]["answer"] == "the codex lane"
    assert closed[0]["data"]["question_id"] == qid


def test_asker_ask_field_options_and_blocks_are_recorded(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setattr("fno.outstanding.cli._session_id", lambda: "ledger-run-id")
    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity",
        lambda: HarnessIdentity(session_id="01234567-full-session", harness="codex"),
    )

    asked = runner.invoke(
        outstanding_app,
        [
            "ask",
            "which implementation should land?",
            "--ask",
            "pick one",
            "--option",
            "index",
            "--option",
            "journal",
            "--blocks",
            "x-one",
            "--blocks",
            "x-two",
        ],
    )

    assert asked.exit_code == 0, asked.output
    event = json.loads((root / ".fno" / "events.jsonl").read_text().splitlines()[-1])
    assert event["data"]["asker"] == "01234567"
    assert event["data"]["ask"] == "pick one"
    assert event["data"]["options"] == ["index", "journal"]
    assert event["data"]["blocks"] == ["x-one", "x-two"]
    assert event["data"]["session_id"] == "ledger-run-id"
    assert "live" not in event["data"], "liveness is computed, never stored"

    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable",
        lambda asker: (object(), []) if asker == "01234567" else (None, []),
    )
    question = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)["questions"][0]
    assert question == {
        "id": event["data"]["question_id"],
        "ts": event["ts"],
        "question": "which implementation should land?",
        "session_id": "ledger-run-id",
        "cwd": str(Path.cwd()),
        "node": None,
        "asker": "01234567",
        "ask": "pick one",
        "options": ["index", "journal"],
        "blocks": ["x-one", "x-two"],
        "live": True,
        "rank": 1,
    }


def test_live_is_computed_for_json_and_missing_asker_is_stale(
    root: Path,
):
    from fno.outstanding.core import read_open_questions

    def resolve(asker: str):
        return (object(), []) if asker == "live-ask" else (None, [])

    _write_indexed_questions(
        root,
        [
            _indexed_question(
                "q-live", "2026-08-19T00:00:00Z", asker="live-ask", blocks=[]
            ),
            _indexed_question(
                "q-stale", "2026-08-18T00:00:00Z", asker="gone-ask", blocks=[]
            ),
            {
                "ts": "2026-08-17T00:00:00Z",
                "type": "operator_question",
                "source": "test",
                "data": {"question_id": "q-legacy", "question": "legacy?"},
            },
        ],
    )
    questions = read_open_questions(
        root,
        liveness_budget_seconds=1.0,
        clock=lambda: 0.0,
        resolver=resolve,
    )
    report = Outstanding(
        carveout_total=0,
        carveout_by_kind={},
        carveout_oldest_ts=None,
        questions=questions,
        captures=[],
    )

    payload = report.as_dict()["questions"]

    assert {row["id"]: row["live"] for row in payload} == {
        "q-live": True,
        "q-stale": False,
        "q-legacy": False,
    }


def test_liveness_budget_expiry_is_unknown_and_does_not_block_report(
    root: Path,
):
    """AC-ERR: a blocked durable-store scan expires as explicit unknown."""
    import multiprocessing
    import threading

    from fno.outstanding.core import read_open_questions

    _write_indexed_questions(
        root,
        [
            _indexed_question(
                "q-slow", "2026-08-19T00:00:00Z", asker="slow-ask", blocks=[]
            )
        ],
    )
    process_context = multiprocessing.get_context("fork")
    started = process_context.Event()

    def slow_resolver(_asker):
        started.set()
        time.sleep(1)
        return object(), []

    began = time.perf_counter()
    questions = read_open_questions(
        root, liveness_budget_seconds=0.1, resolver=slow_resolver
    )
    elapsed = time.perf_counter() - began
    lingering = [
        thread.name
        for thread in threading.enumerate()
        if thread.name == "fno-question-liveness" and thread.is_alive()
    ]

    assert started.is_set()
    assert elapsed < 0.4
    assert lingering == []
    assert questions[0].as_dict()["live"] is None
    output = render(
        Outstanding(0, {}, None, questions, [])
    )
    assert "Questions with unknown liveness" in output
    assert "Stale questions" not in output


def test_liveness_within_budget_keeps_resolver_fidelity_and_live_first(
    root: Path,
):
    """AC-HP: completed reachable and stale answers retain exact semantics."""
    from fno.outstanding.core import read_open_questions

    _write_indexed_questions(
        root,
        [
            _indexed_question(
                "q-stale", "2026-08-19T00:00:00Z", asker="stale", blocks=[]
            ),
            _indexed_question(
                "q-live", "2026-08-19T01:00:00Z", asker="live", blocks=[]
            ),
        ],
    )

    def resolver(asker):
        return (object(), []) if asker == "live" else (None, [])

    questions = read_open_questions(
        root,
        liveness_budget_seconds=1.0,
        clock=lambda: 0.0,
        resolver=resolver,
    )

    assert [(q.id, q.live) for q in questions] == [
        ("q-live", True),
        ("q-stale", False),
    ]


def _write_indexed_questions(root: Path, rows: list[dict]) -> None:
    (root / "questions.jsonl").write_text(
        "".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8"
    )


def _indexed_question(
    qid: str,
    ts: str,
    *,
    asker: str,
    blocks: list[str],
) -> dict:
    return {
        "ts": ts,
        "type": "operator_question",
        "source": "test",
        "data": {
            "question_id": qid,
            "question": f"question {qid}",
            "asker": asker,
            "blocks": blocks,
        },
    }


def test_question_rank_orders_live_then_blocks_then_oldest_and_id(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable",
        lambda asker: (object(), []) if asker == "live" else (None, []),
    )
    _write_indexed_questions(
        root,
        [
            _indexed_question(
                "q-zero", "2026-08-19T03:00:00Z", asker="live", blocks=[]
            ),
            _indexed_question(
                "q-two", "2026-08-19T02:00:00Z", asker="live", blocks=["x-a", "x-b"]
            ),
            _indexed_question(
                "q-one", "2026-08-19T01:00:00Z", asker="live", blocks=["x-a"]
            ),
            _indexed_question(
                "q-stale", "2026-08-19T00:00:00Z", asker="stale", blocks=["x-a", "x-b", "x-c"]
            ),
        ],
    )

    payload = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)["questions"]

    assert [row["id"] for row in payload] == ["q-two", "q-one", "q-zero", "q-stale"]
    assert [row["rank"] for row in payload] == [1, 2, 3, 4]


def test_question_rank_uses_oldest_then_id_within_a_block_count(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable", lambda _asker: (object(), [])
    )
    _write_indexed_questions(
        root,
        [
            _indexed_question(
                "q-new", "2026-08-19T03:00:00Z", asker="live", blocks=["x-a"]
            ),
            _indexed_question(
                "q-z", "2026-08-19T01:00:00Z", asker="live", blocks=["x-a"]
            ),
            _indexed_question(
                "q-a", "2026-08-19T01:00:00Z", asker="live", blocks=["x-a"]
            ),
        ],
    )

    payload = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)["questions"]

    assert [row["id"] for row in payload] == ["q-a", "q-z", "q-new"]
    assert [row["rank"] for row in payload] == [1, 2, 3]


def test_question_render_cap_shows_ten_and_names_true_total(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setattr("fno.agents.discover.resolve_reachable", lambda _asker: (None, []))
    rows = [
        _indexed_question(
            f"q-{i:02d}", f"2026-08-19T{i:02d}:00:00Z", asker="stale", blocks=[]
        )
        for i in range(11)
    ]
    _write_indexed_questions(root, rows)

    out = runner.invoke(outstanding_app, []).stdout

    assert sum(row["data"]["question_id"] in out for row in rows) == 10
    assert "Showing 10 of 11 open questions" in out


def test_stale_questions_render_under_visible_heading_with_age(
):
    stale_ts = (datetime.now(timezone.utc) - timedelta(days=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
    report = Outstanding(
        carveout_total=0,
        carveout_by_kind={},
        carveout_oldest_ts=None,
        questions=[
            Question(
                "q-stale",
                stale_ts,
                "which lane?",
                asker="gone-ask",
                ask="pick a lane",
                live=False,
            )
        ],
        captures=[],
    )

    output = render(report)

    assert "Stale questions" in output
    assert "2 days old" in output
    assert "records the decision but reaches nobody" in output


def test_manual_question_liveness_tristate_renders_and_serializes_explicitly():
    questions = [
        Question("q-live", "2026-08-19T00:00:00Z", "live?", live=True),
        Question("q-stale", "2026-08-19T00:00:01Z", "stale?", live=False),
        Question("q-unknown", "2026-08-19T00:00:02Z", "unknown?", live=None),
    ]
    report = Outstanding(0, {}, None, questions, [])

    assert [row["live"] for row in report.as_dict()["questions"]] == [
        True,
        False,
        None,
    ]
    output = render(report)
    assert "Live open questions" in output
    assert "Stale questions" in output
    assert "Questions with unknown liveness" in output


def test_clear_preserves_asker_as_the_best_answer_provenance(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity",
        lambda: HarnessIdentity(session_id="89abcdef-full-session", harness="codex"),
    )
    qid = runner.invoke(outstanding_app, ["ask", "which lane?"]).stdout.strip().splitlines()[-1]
    recorded: dict[str, object] = {}

    def record_decision(**kwargs):
        recorded.update(kwargs)
        return {"decision_id": "d-recorded"}

    monkeypatch.setattr("fno.decide.record_decision", record_decision)

    cleared = runner.invoke(outstanding_app, ["clear", qid, "--answer", "coord"])

    assert cleared.exit_code == 0, cleared.output
    assert recorded["asked_by"] == "89abcdef"


def test_clear_with_answer_emits_operator_decision(root: Path):
    """An answered close records the decision, not just the closure.

    The closed event stays; the decision event lands beside it carrying the
    recovery fields. A close with NO answer is a withdrawal and must not
    mint a decision.
    """
    asked = runner.invoke(
        outstanding_app, ["ask", "fold or migrate?", "--node", "x-7d94"]
    )
    qid = asked.stdout.strip().splitlines()[-1]
    cleared = runner.invoke(outstanding_app, ["clear", qid, "--answer", "fold"])
    assert cleared.exit_code == 0, cleared.output

    lines = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    decisions = [e for e in lines if e["type"] == "operator_decision"]
    assert len(decisions) == 1, "an answered close must emit exactly one decision"
    data = decisions[0]["data"]
    assert data["decision"] == "fold"
    assert data["question_id"] == qid
    assert data["question"] == "fold or migrate?"
    assert data["subject"] == "x-7d94"
    assert data["decision_id"].startswith("d-")
    assert data["asked_at"], "asked_at must inherit the question's timestamp"
    assert data["decided_by"]
    # NOT authority_source == "operator". This runs with no session identity
    # and no terminal, which is the fail-closed third state: the answer is
    # recorded and claims no authority, rather than inheriting law by silence.
    # Answering a question does not by itself prove the operator answered it.
    assert "authority_source" not in data

    # A withdrawal (no --answer) decides nothing: positive control is the
    # closed event itself, the decision count stays at one from the ask above.
    qid2 = runner.invoke(outstanding_app, ["ask", "second question?"]).stdout.strip().splitlines()[-1]
    runner.invoke(outstanding_app, ["clear", qid2])
    lines = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert len([e for e in lines if e["type"] == "operator_decision"]) == 1
    assert len([e for e in lines if e["type"] == "operator_question_closed"]) == 2


def test_clear_with_answer_records_operator_authority_when_stated_at_a_terminal(
    root: Path, monkeypatch
):
    """The operator lane stays reachable on this path; only the silent default
    closed. An agent clearing a question on the operator's behalf must not be
    indistinguishable from the operator answering it."""
    from fno import decide as decide_mod

    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)

    asked = runner.invoke(
        outstanding_app, ["ask", "fold or migrate?", "--node", "x-7d94"]
    )
    qid = asked.stdout.strip().splitlines()[-1]
    cleared = runner.invoke(
        outstanding_app,
        ["clear", qid, "--answer", "fold", "--authority", "operator"],
    )
    assert cleared.exit_code == 0, cleared.output

    lines = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
        if line.strip()
    ]
    data = [e for e in lines if e["type"] == "operator_decision"][0]["data"]
    assert data["decided_by"] == "operator"
    assert data["attested_by"] == "operator", "a person was at the terminal"
    # The assertion the name promises. Without --authority on `clear`, this
    # path could not produce law at all, and the operator answering a question
    # they were formally asked filed into the same lane as the pre-cutover rows
    # the cutover exists to quarantine.
    assert data["authority_source"] == "operator"


def test_a_refused_answer_leaves_the_question_open(root: Path, monkeypatch):
    """Closing on a refused answer would retire the question with nothing on
    record, which is worse than refusing the close. The refusal covers both."""
    from fno import decide as decide_mod

    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: False)

    asked = runner.invoke(
        outstanding_app, ["ask", "fold or migrate?", "--node", "x-7d94"]
    )
    qid = asked.stdout.strip().splitlines()[-1]
    refused = runner.invoke(
        outstanding_app,
        ["clear", qid, "--answer", "fold", "--authority", "operator"],
    )
    assert refused.exit_code != 0, refused.output
    assert "Nothing was closed" in refused.output

    still_open = runner.invoke(outstanding_app, [])
    assert still_open.exit_code == 0, still_open.output
    assert qid in still_open.output, "the question must survive its refused close"

    # A batch refuses whole, never halfway. The refusal turns on the ambient
    # identity and the one --authority value, both invocation-wide, so it
    # fires on the first id before anything closes. Asserted rather than
    # reasoned, because a partial batch is a data-integrity bug and the
    # argument for why it cannot happen is exactly the kind that stops being
    # true when someone moves the record_decision call.
    qid2 = runner.invoke(
        outstanding_app, ["ask", "second question?", "--node", "x-7d94"]
    ).stdout.strip().splitlines()[-1]
    batch = runner.invoke(
        outstanding_app,
        ["clear", qid, qid2, "--answer", "fold", "--authority", "operator"],
    )
    assert batch.exit_code != 0, batch.output
    after = runner.invoke(outstanding_app, [])
    assert qid in after.output and qid2 in after.output, "neither may close"


def test_clear_with_answer_projects_the_decision_onto_the_node(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    """The clear-path decision has both halves a `fno decide` record has:
    findable by subject through the graph projection, not merely greppable
    in the journal."""
    graph = root / "graph.json"
    graph.write_text(
        json.dumps({"entries": [{"id": "x-7d94", "title": "t", "status": "ready"}]}) + "\n"
    )

    qid = runner.invoke(
        outstanding_app, ["ask", "fold or migrate?", "--node", "x-7d94"]
    ).stdout.strip().splitlines()[-1]
    cleared = runner.invoke(outstanding_app, ["clear", qid, "--answer", "fold"])
    assert cleared.exit_code == 0, cleared.output

    from fno.decide.cli import decide_app as decide_cli_app

    listed = runner.invoke(decide_cli_app, ["list", "--subject", "x-7d94", "--json"])
    assert listed.exit_code == 0, listed.output
    decisions = json.loads(listed.stdout)["decisions"]
    assert [d["question_id"] for d in decisions] == [qid]
    assert decisions[0]["decision"] == "fold"
    assert decisions[0]["question"] == "fold or migrate?"
    assert decisions[0]["asked_at"]


def test_a_projection_failure_no_longer_holds_the_question_open(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    """The answer is durable and recoverable without the projection.

    This inverts the old contract on purpose. Holding the question open was the
    only recall path when the graph projection was the sole read source: a
    projection failure then meant the answer existed and could not be found.
    The machine-wide index now carries recall, so the projection is the node
    VIEW alone - and keeping the question open would invite a retry that
    records one answer a second time under a new id.
    """
    qid = runner.invoke(
        outstanding_app, ["ask", "fold or migrate?", "--node", "x-7d94"]
    ).stdout.strip().splitlines()[-1]

    def fail_projection(_event):
        raise OSError("graph unavailable")

    monkeypatch.setattr("fno.decide._project", fail_projection)
    cleared = runner.invoke(outstanding_app, ["clear", qid, "--answer", "fold"])

    assert cleared.exit_code == 0, cleared.output
    assert "graph projection failed" in cleared.output
    from fno.outstanding.core import read_open_questions

    assert [q.id for q in read_open_questions(root)] == []

    from fno.decide.cli import decide_app as decide_cli_app

    listed = runner.invoke(decide_cli_app, ["list", "--json"])
    assert "fold" in [d["decision"] for d in json.loads(listed.stdout)["decisions"]]


def test_unrelated_journal_volume_does_not_slow_the_read(root: Path):
    """The shared journal is append-only and never rotated.

    Parsing every line put the hook's 3s bound in reach, and that bound firing
    does not surface an error - the block just vanishes and the operator reads
    "nothing outstanding". Asserts the positive outcome (the question is still
    found among 20k unrelated rows) plus a wall-clock ceiling.
    """
    qid = runner.invoke(outstanding_app, ["ask", "buried under noise?"]).stdout.strip().splitlines()[-1]
    events = root / ".fno" / "events.jsonl"
    noise = json.dumps(
        {"ts": "2026-08-01T00:00:00Z", "type": "phase_transition", "source": "target",
         "data": {"phase": "do", "nonce": "x" * 32, "session_id": "s", "gate_bearing": False}}
    )
    with events.open("a", encoding="utf-8") as fh:
        for _ in range(20_000):
            fh.write(noise + "\n")

    started = time.monotonic()
    result = runner.invoke(outstanding_app, ["--json"])
    elapsed = time.monotonic() - started

    assert result.exit_code == 0, result.output
    assert [q["id"] for q in json.loads(result.stdout)["questions"]] == [qid]
    assert elapsed < 1.5, f"read took {elapsed:.2f}s over 20k unrelated rows"


def test_a_malformed_events_line_is_skipped_never_raised(root: Path):
    qid = runner.invoke(outstanding_app, ["ask", "still readable?"]).stdout.strip().splitlines()[-1]
    events = root / ".fno" / "events.jsonl"
    with events.open("a", encoding="utf-8") as fh:
        fh.write("{not json at all\n")

    result = runner.invoke(outstanding_app, ["--json"])
    assert result.exit_code == 0, result.output
    assert [q["id"] for q in json.loads(result.stdout)["questions"]] == [qid]


# --- 2.2 machine-wide question index ---------------------------------------


def test_question_index_dual_writes_ask_and_close(root: Path):
    asked = runner.invoke(outstanding_app, ["ask", "which lane ships first?"])
    assert asked.exit_code == 0, asked.output
    qid = asked.stdout.strip().splitlines()[-1]

    project_path = root / ".fno" / "events.jsonl"
    index_path = root / "questions.jsonl"
    project_ask = json.loads(project_path.read_text(encoding="utf-8").splitlines()[-1])
    index_ask = json.loads(index_path.read_text(encoding="utf-8").splitlines()[-1])
    assert project_ask == index_ask
    assert project_ask["data"]["question_id"] == qid

    cleared = runner.invoke(outstanding_app, ["clear", qid])
    assert cleared.exit_code == 0, cleared.output
    project_close = json.loads(project_path.read_text(encoding="utf-8").splitlines()[-1])
    index_close = json.loads(index_path.read_text(encoding="utf-8").splitlines()[-1])
    assert project_close == index_close
    assert project_close["type"] == "operator_question_closed"
    assert project_close["data"]["question_id"] == qid


def test_question_index_failure_names_id_and_reindex(root: Path, monkeypatch: pytest.MonkeyPatch):
    from fno import paths
    from fno.events import append_event as real_append_event

    index_path = paths.questions_jsonl()
    monkeypatch.setattr("fno.outstanding.cli.secrets.token_hex", lambda _n: "feedface")

    def fail_index(event, *, events_path=None):
        if events_path == index_path:
            raise OSError("index unavailable")
        return real_append_event(event, events_path=events_path)

    monkeypatch.setattr("fno.events.append_event", fail_index)
    result = runner.invoke(outstanding_app, ["ask", "which index?"])

    assert result.exit_code == 1
    assert "q-feedface" in result.output
    assert "fno inbox outstanding reindex" in result.output
    durable = json.loads(
        (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()[-1]
    )
    assert durable["data"]["question_id"] == "q-feedface"


def test_question_close_index_failure_names_id_and_reindex(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    from fno import paths
    from fno.events import append_event as real_append_event

    asked = runner.invoke(outstanding_app, ["ask", "which close path?"])
    assert asked.exit_code == 0, asked.output
    qid = asked.stdout.strip().splitlines()[-1]
    index_path = paths.questions_jsonl()

    def fail_close_index(event, *, events_path=None):
        if event["type"] == "operator_question_closed" and events_path == index_path:
            raise OSError("index unavailable")
        return real_append_event(event, events_path=events_path)

    monkeypatch.setattr("fno.events.append_event", fail_close_index)
    result = runner.invoke(outstanding_app, ["clear", qid])

    assert result.exit_code == 1
    assert qid in result.output
    assert "fno inbox outstanding reindex" in result.output
    project_close = json.loads(
        (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()[-1]
    )
    assert project_close["type"] == "operator_question_closed"
    index_last = json.loads(index_path.read_text(encoding="utf-8").splitlines()[-1])
    assert index_last["type"] == "operator_question"


def test_missing_question_index_reports_empty_with_reindex_hint(root: Path):
    result = runner.invoke(outstanding_app, ["--json"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["questions"] == []
    assert "fno inbox outstanding reindex" in result.stderr


def test_unreadable_question_index_is_a_failed_read(root: Path):
    index_path = root / "questions.jsonl"
    index_path.mkdir()

    result = runner.invoke(outstanding_app, ["--json"])

    assert result.exit_code == 1
    assert "failed to read" in result.output
    assert "questions.jsonl" in result.output


def test_reindex_is_idempotent_and_machine_wide(
    root: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    from fno.events import append_event, operator_question, operator_question_closed
    from fno.outstanding.core import events_path

    sibling = tmp_path / "sibling"
    sibling.mkdir()
    first = operator_question(
        question_id="q-first",
        question="first project question",
        session_id="s-first",
        cwd=str(root),
    )
    first["ts"] = "2026-08-19T00:00:00Z"
    second = operator_question(
        question_id="q-second",
        question="second project question",
        session_id="s-second",
        cwd=str(sibling),
    )
    second["ts"] = "2026-08-19T00:01:00Z"
    closed = operator_question_closed(question_id="q-first", closed_by="operator")
    closed["ts"] = "2026-08-19T00:02:00Z"
    append_event(first, events_path=events_path(root))
    append_event(second, events_path=events_path(sibling))
    append_event(closed, events_path=events_path(root))
    monkeypatch.setattr(
        "fno.outstanding.core._capture_project_roots", lambda _root: [root, sibling]
    )

    indexed = runner.invoke(outstanding_app, ["reindex"])
    assert indexed.exit_code == 0, indexed.output
    assert "3 event(s) added" in indexed.output
    indexed_again = runner.invoke(outstanding_app, ["reindex"])
    assert indexed_again.exit_code == 0, indexed_again.output
    assert "0 event(s) added" in indexed_again.output

    monkeypatch.setattr("fno.outstanding.cli._storage_root", lambda: root)
    from_first = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)["questions"]
    monkeypatch.setattr("fno.outstanding.cli._storage_root", lambda: sibling)
    from_second = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)["questions"]
    assert from_first == from_second
    assert [row["id"] for row in from_first] == ["q-second"]

    indexed_events = [
        json.loads(line)
        for line in (root / "questions.jsonl").read_text(encoding="utf-8").splitlines()
    ]
    assert indexed_events == [first, closed, second]


def test_reindex_dedupes_symlinked_journals_by_inode(
    root: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    from fno.events import append_event, operator_question
    from fno.outstanding.core import events_path

    append_event(
        operator_question(
            question_id="q-control",
            question="positive control",
            session_id="s-control",
            cwd=str(root),
        ),
        events_path=events_path(root),
    )
    alias = tmp_path / "alias"
    alias.symlink_to(root, target_is_directory=True)
    monkeypatch.setattr("fno.outstanding.core._capture_project_roots", lambda _root: [root, alias])

    from fno.outstanding.core import _question_journals

    assert _question_journals(root) == [events_path(root)]


# --- 3rd leg: the capture fold -----------------------------------------------


def _write_inbox(root: Path, lines: list[str]) -> Path:
    inbox = root / "internal" / "fno" / "backlog" / "inbox.md"
    inbox.parent.mkdir(parents=True, exist_ok=True)
    inbox.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return inbox


def _capture_event(fu_id: str, ts: str) -> str:
    return json.dumps(
        {
            "ts": ts,
            "type": "capture_add",
            "source": "backlog",
            "data": {"session_id": "manual", "fu_id": fu_id, "title": "t"},
        }
    )


def test_capture_project_roots_reads_graph_without_importing_tracker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC-VERIFY: the default fold stays off the heavy tracker import path."""
    import builtins

    from fno.outstanding.core import _capture_project_roots

    this = tmp_path / "this"
    sibling = tmp_path / "sibling"
    this.mkdir()
    sibling.mkdir()
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-this", "cwd": str(this)},
                    {"id": "x-sibling", "source_cwd": str(sibling)},
                ]
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    real_import = builtins.__import__

    def refuse_tracker_import(name, *args, **kwargs):
        if name == "fno.tracker" or name.startswith("fno.tracker."):
            raise AssertionError("default graph fold imported fno.tracker")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", refuse_tracker_import)

    assert _capture_project_roots(this) == [sibling.resolve(), this.resolve()]


def test_capture_project_roots_does_not_resolve_every_graph_entry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC-VERIFY: later physical-store dedup owns filesystem resolution."""
    from fno.outstanding.core import _capture_project_roots

    this = tmp_path / "this"
    sibling = tmp_path / "sibling"
    this.mkdir()
    sibling.mkdir()
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps({"entries": [{"cwd": str(this)}, {"cwd": str(sibling)}]}),
        encoding="utf-8",
    )
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    real_resolve = Path.resolve
    resolved: list[Path] = []

    def track_resolve(path, *args, **kwargs):
        resolved.append(path)
        return real_resolve(path, *args, **kwargs)

    monkeypatch.setattr(Path, "resolve", track_resolve)

    roots = _capture_project_roots(this)

    assert roots == [sibling, this]
    assert resolved == [this]


def test_capture_project_roots_preserves_external_backend_sidecars(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC-EDGE: non-graph trackers still discover roots through sidecars."""
    from types import SimpleNamespace

    from fno.outstanding.core import _capture_project_roots
    from fno.tracker import sidecar as sidecar_store

    this = tmp_path / "this"
    sibling = tmp_path / "sibling"
    this.mkdir()
    sibling.mkdir()
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setattr(
        sidecar_store,
        "load_all",
        lambda: {
            "external-1": SimpleNamespace(cwd=None, source_cwd=str(sibling)),
        },
    )

    assert _capture_project_roots(this) == [sibling.resolve(), this.resolve()]


def test_storage_root_skips_worktree_scan_in_canonical_checkout(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC-VERIFY: a canonical checkout needs no git worktree subprocess."""
    from fno.outstanding.cli import _storage_root

    canonical = tmp_path / "canonical"
    (canonical / ".git").mkdir(parents=True)
    monkeypatch.chdir(canonical)

    def refuse_repo_probe():
        raise AssertionError("canonical checkout ran the repo resolver")

    monkeypatch.setattr("fno.paths.resolve_repo_root", refuse_repo_probe)

    def refuse_worktree_scan():
        raise AssertionError("canonical checkout ran the worktree resolver")

    monkeypatch.setattr(
        "fno.carveout.core.resolve_carveout_root", refuse_worktree_scan
    )

    assert _storage_root() == canonical


def test_session_id_reads_manifest_without_importing_state_stack(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC-VERIFY: report ownership needs two scalars, not state/filelock."""
    import builtins

    from fno.outstanding.cli import _session_id

    (root / ".fno" / "target-state.md").write_text(
        "---\nfno_id: run-fast\nsession_id: run-legacy\n---\nbody session_id: wrong\n",
        encoding="utf-8",
    )
    real_import = builtins.__import__

    def refuse_state_import(name, *args, **kwargs):
        if name == "fno.state" or name.startswith("fno.state."):
            raise AssertionError("outstanding report imported the state stack")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", refuse_state_import)

    assert _session_id() == "run-fast"


def test_capture_fold_resolves_one_inbox_per_repo_but_reads_each_journal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC-VERIFY: worktrees share config lookup, not capture-event history."""
    from fno.outstanding.core import _read_open_captures_with_counts

    canonical = tmp_path / "repo"
    worktree_a = tmp_path / "worktree-a"
    worktree_b = tmp_path / "worktree-b"
    git_dir = canonical / ".git"
    git_dir.mkdir(parents=True)
    for name, worktree in (("a", worktree_a), ("b", worktree_b)):
        worktree.mkdir()
        worktree_git_dir = git_dir / "worktrees" / name
        worktree_git_dir.mkdir(parents=True)
        (worktree / ".git").write_text(
            f"gitdir: {worktree_git_dir}\n", encoding="utf-8"
        )
    inbox = tmp_path / "inbox.md"
    inbox.write_text("- [ ] fu-aaaaaa - one\n", encoding="utf-8")
    roots = [canonical, worktree_a, worktree_b]
    monkeypatch.setattr(
        "fno.outstanding.core._capture_project_roots", lambda _root: roots
    )
    inbox_calls: list[Path] = []

    def resolve_inbox(*, project_root):
        inbox_calls.append(project_root)
        return inbox

    monkeypatch.setattr("fno.paths.inbox_path", resolve_inbox)
    journal_calls: list[Path] = []

    def added_at(project_root):
        journal_calls.append(project_root)
        return {}

    monkeypatch.setattr("fno.outstanding.core._capture_added_at", added_at)
    monkeypatch.setattr(
        "fno.outstanding.core.events_path",
        lambda project_root: project_root / ".fno" / "events.jsonl",
    )

    captures, file_total, row_total = _read_open_captures_with_counts(canonical)

    assert inbox_calls == [canonical]
    assert set(journal_calls) == set(roots)
    assert [row.fu_id for row in captures] == ["fu-aaaaaa"]
    assert (file_total, row_total) == (1, 1)


@pytest.fixture()
def capture_roots(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> "tuple[Path, Path]":
    """A hermetic THIS project plus one sibling project, both known to the
    machine-wide graph so the fold finds the sibling's inbox."""
    this = tmp_path / "this"
    other = tmp_path / "other"
    for p in (this, other):
        p.mkdir(parents=True)
    monkeypatch.setenv("FNO_REPO_ROOT", str(this))
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    (this / ".fno").mkdir(exist_ok=True)
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-0001", "title": "a", "cwd": str(this)},
                    {"id": "x-0002", "title": "b", "cwd": str(other)},
                ]
            }
        )
        + "\n"
    )
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", graph)
    monkeypatch.setattr(gs, "GRAPH_JSON", graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    return this, other


def test_capture_leg_folds_every_project_and_renders_true_count(capture_roots):
    """Three legs; the capture leg renders Showing N of M, oldest D
    days, with M folded across projects."""
    this, other = capture_roots
    _write_inbox(this, ["- [ ] fu-aaaaaa - alpha (p1)", "- [ ] fu-bbbbbb - beta"])
    _write_inbox(other, ["- [ ] fu-cccccc - gamma", "- [x] fu-dddddd - done -> ab-1"])

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert "3 captures" in res.output, res.output
    assert "Showing 3 of 3" in res.output, res.output

    as_json = runner.invoke(outstanding_app, ["--json"])
    payload = json.loads(as_json.stdout)
    assert payload["captures"]["total"] == 3
    assert payload["captures"]["resolved_files"] == 2
    assert payload["captures"]["parsed_open_rows"] == 3
    assert payload["captures"]["repeated_ids"] == 0
    assert payload["captures"]["by_project"][this.name] == 2
    assert payload["captures"]["by_project"][other.name] == 2 - 1


def test_capture_leg_reports_oldest_age_from_capture_add_events(capture_roots):
    """The inbox markdown carries no dates; age comes from capture_add in the
    events journal."""
    this, other = capture_roots
    _write_inbox(this, ["- [ ] fu-aaaaaa - alpha", "- [ ] fu-bbbbbb - beta"])
    events = this / ".fno" / "events.jsonl"
    events.parent.mkdir(exist_ok=True)
    old = (datetime.now(timezone.utc) - timedelta(days=62)).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    events.write_text(
        _capture_event("fu-aaaaaa", old) + "\n" + _capture_event("fu-bbbbbb", "2026-08-14T00:00:00Z") + "\n",
        encoding="utf-8",
    )

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert "oldest 62 days" in res.output, res.output


def test_capture_age_folds_every_distinct_project_journal(
    capture_roots, monkeypatch: pytest.MonkeyPatch
):
    """One project's event history must not hide an older sibling capture."""
    this, other = capture_roots
    shared = this / "shared-parking-lot.md"
    shared.write_text(
        "- [ ] fu-aaaaaa - alpha\n- [ ] fu-bbbbbb - beta\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(
        "fno.paths.inbox_path", lambda project_root=None: shared
    )
    old = (datetime.now(timezone.utc) - timedelta(days=80)).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    recent = (datetime.now(timezone.utc) - timedelta(days=2)).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    def added_at(project: Path) -> dict[str, str]:
        if project.name == "this":
            return {"fu-aaaaaa": old}
        if project.name == "other":
            return {"fu-bbbbbb": recent}
        return {}

    monkeypatch.setattr("fno.outstanding.core._capture_added_at", added_at)
    monkeypatch.setattr(
        "fno.outstanding.core.events_path", lambda project: project / ".fno/events.jsonl"
    )

    res = runner.invoke(outstanding_app, [])

    assert res.exit_code == 0, res.output
    assert "oldest 80 days" in res.output, res.output


def test_unreadable_inbox_contributes_zero_and_never_raises(capture_roots):
    """A missing or unreadable sibling inbox contributes zero captures
    and the other projects still report."""
    this, other = capture_roots
    _write_inbox(this, ["- [ ] fu-aaaaaa - alpha"])
    # other/ has no inbox at all; also plant a graph cwd that does not exist.
    import fno.graph._constants as gc
    import json as _json

    graph = gc.GRAPH_JSON
    data = _json.loads(graph.read_text())
    data["entries"].append({"id": "x-dead", "title": "d", "cwd": "/nonexistent/project"})
    graph.write_text(_json.dumps(data))

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert "1 capture" in res.output, res.output


def test_capture_render_caps_rows_but_keeps_the_true_count(capture_roots):
    """Ruling 2: never the full list; the count beside the cap is the honesty."""
    this, _ = capture_roots
    _write_inbox(
        this, [f"- [ ] fu-{i:06x} - item {i}" for i in range(10)]
    )

    res = runner.invoke(outstanding_app, [])
    assert res.exit_code == 0, res.output
    assert f"Showing {RENDER_CAP} of 10" in res.output, res.output


def test_capture_leg_prints_its_counting_rule_beside_the_count(capture_roots):
    """The total is auditable when three plausible counting methods disagree."""
    this, other = capture_roots
    _write_inbox(
        this,
        [
            "- [ ] fu-aaaaaa - alpha",
            "- [ ] fu-aaaaaa - duplicate id",
            "- [ ] ab-11111111 - filed node is not a capture",
        ],
    )
    _write_inbox(other, ["- [ ] fu-bbbbbb - beta"])

    res = runner.invoke(outstanding_app, [])

    assert res.exit_code == 0, res.output
    assert "Showing 2 of 2" in res.output, res.output
    assert (
        "Count rule: 2 unique open fu-* IDs across 2 resolved capture files "
        "(3 parsed open rows minus 1 repeated ID); "
        "post_merge.parking_lot_path is used when configured."
    ) in res.output, res.output


# --- 2.5 the SessionStart block ---------------------------------------------


def test_hook_block_is_silent_on_zero(root: Path):
    result = runner.invoke(outstanding_app, [])
    assert result.exit_code == 0
    assert result.stdout.strip() == ""


def test_hook_block_positive_control(root: Path):
    """The pair for the test above.

    An empty-output assertion alone cannot distinguish a genuinely clean state
    from a block that is permanently broken and renders nothing either way.
    This is the control that makes the silence meaningful.
    """
    assert runner.invoke(outstanding_app, []).stdout.strip() == ""
    _write_carveouts(root, [_carveout("cv-1", "oos-bug")])
    assert runner.invoke(outstanding_app, []).stdout.strip() != ""


# --- 2.6 the operator lane ---------------------------------------------------


def _lane_item(text, **kw):
    return LaneItem(text=text, node=None, parked=None, done=False, line=1, **kw)


def test_lane_leads_the_block_above_carveouts(root: Path):
    """AC1-HP: any session sees the lane count and top item first."""
    (root / "my-priorities.md").write_text(
        "- [ ] ship 981 and 971 tonight\n- [ ] call the dentist\n", encoding="utf-8"
    )
    out = runner.invoke(outstanding_app, []).stdout
    assert out.startswith("## Outstanding for you\n\n2 items on your lane, top first.")
    assert "ship 981 and 971 tonight" in out


def test_render_hides_the_action_line_when_uncrowned():
    """AC6-HP: an uncrowned session gets the count and top item, no action line."""
    outstanding = Outstanding(0, {}, None, [], [], lane=[_lane_item("ship it")])
    block = render(outstanding, crowned=False)
    assert "1 item on your lane, top first." in block
    assert "ship it" in block
    assert "File one with:" not in block


def test_render_shows_the_action_line_when_crowned():
    """AC6-HP, the crowned half: the same input gets all three lines."""
    outstanding = Outstanding(0, {}, None, [], [], lane=[_lane_item("ship it")])
    block = render(outstanding, crowned=True)
    assert "File one with:" in block


def test_render_reports_the_parked_count():
    outstanding = Outstanding(0, {}, None, [], [], lane=[_lane_item("ship it")], lane_parked=1)
    block = render(outstanding, crowned=False)
    assert "1 parked" in block


def test_lane_alone_is_enough_to_break_silence():
    """AC4-EDGE positive control: a non-empty lane renders even with both other legs clear."""
    outstanding = Outstanding(0, {}, None, [], [], lane=[_lane_item("ship it")])
    assert render(outstanding, crowned=False) != ""
    assert Outstanding(0, {}, None, [], []).empty


# --- 3.3 output modes --------------------------------------------------------


def test_json_mode_emits_one_object_carrying_both_legs(root: Path):
    _write_carveouts(root, [_carveout("cv-1", "deferred")])
    runner.invoke(outstanding_app, ["ask", "a question"])
    payload = json.loads(runner.invoke(outstanding_app, ["--json"]).stdout)
    assert payload["carveouts"]["total"] == 1
    assert payload["carveouts"]["by_kind"]["deferred"] == 1
    assert len(payload["questions"]) == 1


# --- 5.2 the asking session's own questions lead -----------------------------


def test_own_first(root: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    runner.invoke(outstanding_app, ["ask", "MINE: do we ship the fold arm?"])
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-other")
    runner.invoke(outstanding_app, ["ask", "THEIRS: which base branch?"])

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-mine")
    out = runner.invoke(outstanding_app, []).stdout
    assert "MINE:" in out
    assert "THEIRS:" in out
    assert out.index("MINE:") < out.index("THEIRS:")
    # Labelled as this session's, so the agent that asked is the one prompted.
    assert "this session" in out.lower()


def test_a_worker_with_no_questions_of_its_own_stays_short(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    """The hook fires in every session, so a bg worker's share must stay small.

    The cap is what keeps it small, not a presence branch. An earlier version
    suppressed every row for a worker and produced two defects: it keyed on
    "is a human here" rather than "is this a worker", so a gemini or agy
    OPERATOR got the truncated form, and it printed "clear <id>" right after
    hiding every id. Three rows plus a count is already short.
    """
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-other")
    for i in range(6):
        runner.invoke(outstanding_app, ["ask", f"question number {i}"])

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-quiet")
    monkeypatch.setenv("FNO_AGENT_SELF", "worker-quiet")
    monkeypatch.delenv("FNO_THINK_SPAWN_PRESENCE", raising=False)
    out = runner.invoke(outstanding_app, []).stdout
    assert "6 open question" in out
    assert out.count("question number") == 6
    # Every rendered row carries an id, so the clear instruction has an operand.
    assert "fno inbox outstanding clear" in out
    assert out.count("q-") >= 6


def test_an_attended_session_does_see_other_sessions_questions(
    root: Path, monkeypatch: pytest.MonkeyPatch
):
    """The pair for the test above, and the reason ownership cannot be the gate.

    The operator owns none of these questions - workers asked them - so gating
    the render on ownership would show the one person who can answer exactly
    nothing. Attendedness is the discriminator, not ownership.
    """
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-other")
    for i in range(6):
        runner.invoke(outstanding_app, ["ask", f"question number {i}"])

    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-operator")
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
    monkeypatch.delenv("FNO_BG", raising=False)
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "attended")
    out = runner.invoke(outstanding_app, []).stdout
    assert "6 open question" in out
    assert out.count("question number") == 6


def test_render_caps_rows_and_states_the_drop_count(root: Path):
    for i in range(11):
        runner.invoke(outstanding_app, ["ask", f"q{i}"])
    out = runner.invoke(outstanding_app, []).stdout
    assert "11 open question" in out
    assert "Showing 10 of 11 open questions" in out
