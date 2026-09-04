"""Tests for classified session-id transitions (x-dfe7).

The discriminator is liveness, never a preference: a positively unreachable
predecessor retires in place (succession, one row plus a chain), a positively
reachable predecessor forks into a second row (branch, two rows and one
operator question), and unknown evidence parks the new id additively
(deferred, no question, no overwrite). Every result emits a positive
transition event to the daemon lifecycle log, joinable with the predecessor's
eventual reap.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir

BIRTH = "e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
REMINT = "08054b1d-a907-47ab-a3d2-4a1e7a87eb4e"


def _spawned_row(name: str = "target-x-f0c2", session_id: str = BIRTH, **extra):
    """A footnote-spawned claude worker row as `fno agents spawn` writes it."""
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            harness="claude",
            harness_session_id=session_id,
            short_id=session_id.split("-", 1)[0],
            cwd="/proj",
            log_path="",
            status="live",
            **extra,
        )
    ])


def _reading(verdict: str, basis: str):
    from fno.agents.reachability import Reachability

    return Reachability(verdict=verdict, basis=basis, age_s=None)


def _isolate_daemon_log(monkeypatch, tmp_path: Path) -> None:
    """Point the daemon lifecycle log at this test's tmp tree.

    `agents_home_dir` deliberately ignores state_dir (it resolves
    FNO_AGENTS_HOME, else $HOME), so a test that reads the log must pin the
    env var; chdir keeps the question's project journal in tmp too.
    """
    monkeypatch.setenv("FNO_AGENTS_HOME", str(tmp_path / ".fno" / "agents"))
    monkeypatch.chdir(tmp_path)


def _observe(name: str, session_id: str, *, predecessor_reachable=None, **kwargs):
    from fno.agents.registry import record_session_observation

    return record_session_observation(
        name=name,
        harness="claude",
        session_id=session_id,
        predecessor_reachable=predecessor_reachable,
        **kwargs,
    )


def _transition_events(tmp_path: Path) -> list[dict]:
    """The daemon lifecycle log, where births, deaths, and transitions join."""
    path = tmp_path / ".fno" / "agents" / "events.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


# ---------------------------------------------------------------------------
# Registry-level classification (record_session_observation)
# ---------------------------------------------------------------------------


def test_observation_succession_advances_the_primary_and_chains_a(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP: a positively unreachable predecessor retires in place. One row
    keeps the stable fno_id, the primary advances to B, and A lands in the
    predecessor chain exactly once."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    entry, outcome = _observe("target-x-f0c2", REMINT, predecessor_reachable=False)
    assert outcome == "succession"
    rows = load_registry()
    assert len(rows) == 1, "succession is one row, never two"
    assert rows[0].harness_session_id == REMINT
    assert rows[0].predecessor_session_ids == [BIRTH]
    assert entry.forked_from_session_id is None, "succession is not a fork edge"


def test_observation_succession_refuses_successor_already_owned_by_another_row(
    tmp_path: Path, monkeypatch
) -> None:
    """A succession cannot overwrite into a session id already owned by a row."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    successor = "08054b1d-a907-47ab-a3d2-4a1e7a87eb4e"
    write_registry([
        AgentEntry(
            name="target-x-f0c2",
            harness="claude",
            harness_session_id=BIRTH,
            short_id=BIRTH.split("-", 1)[0],
            cwd="/proj",
            log_path="",
            status="live",
        ),
        AgentEntry(
            name="target-existing",
            harness="claude",
            harness_session_id=successor,
            short_id=successor.split("-", 1)[0],
            cwd="/proj",
            log_path="",
            status="live",
        ),
    ])

    with pytest.raises(ValueError, match="succession session .* already has a registry row"):
        _observe("target-x-f0c2", successor, predecessor_reachable=False)

    rows = load_registry()
    assert [row.harness_session_id for row in rows] == [BIRTH, successor]


def test_observation_branch_mints_a_second_row_and_leaves_a_untouched(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP: a positively reachable predecessor stays live on its own row;
    B is a distinct row with its own fno_id and the visible fork edge."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    entry, outcome = _observe("target-x-f0c2", REMINT, predecessor_reachable=True)
    assert outcome == "branch"
    rows = load_registry()
    assert len(rows) == 2
    live_a = next(row for row in rows if row.harness_session_id == BIRTH)
    branch = next(row for row in rows if row.harness_session_id == REMINT)
    assert live_a.predecessor_session_ids == [], "A's row is never overwritten"
    assert branch.forked_from_session_id == BIRTH
    assert branch.fno_id == REMINT, "a branch row carries a distinct stable id"
    assert branch.name != live_a.name
    assert entry.name == branch.name


def test_branch_mint_clears_the_related_slot(tmp_path: Path, monkeypatch) -> None:
    """A's historical ids are A's history. A branch minted from a row that
    carries a parked id starts clean - only B rides the branch row."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, restamp_harness_session_id, write_registry

    write_registry([
        AgentEntry(
            name="t-codex",
            harness="codex",
            harness_session_id=BIRTH,
            related_session_id="cafef00d-dead-47ab-a3d2-000000000000",
            cwd="/proj",
            log_path="",
            status="live",
        )
    ])
    entry = restamp_harness_session_id(
        name="t-codex", harness="codex", session_id=REMINT, predecessor_reachable=True
    )
    assert entry is not None
    branch = next(row for row in load_registry() if row.harness_session_id == REMINT)
    assert branch.forked_from_session_id == BIRTH
    assert not branch.related_session_id, "the branch inherits no parked id"


def test_claude_branch_row_is_born_bg_routable(tmp_path: Path, monkeypatch) -> None:
    """x-a457: a claude branch carries the 8-hex jobId the rv socket farm keys
    on, derived from the new session id the way the bg spawn path mints it.
    Left "" the live branch had no identity route and footprint read every
    such row as an unattributed cost, fail-closing the spawn gate."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    entry, outcome = _observe("target-x-f0c2", REMINT, predecessor_reachable=True)
    assert outcome == "branch"
    branch = next(row for row in load_registry() if row.harness_session_id == REMINT)
    assert branch.short_id == REMINT.split("-", 1)[0]


def test_codex_branch_row_keeps_no_transport_short_id(tmp_path: Path, monkeypatch) -> None:
    """First-8 of a codex id is not a transport key (time-prefixed ids collide
    across same-window sessions), so a codex branch keeps short_id empty."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import (
        AgentEntry,
        load_registry,
        restamp_harness_session_id,
        write_registry,
    )

    write_registry([
        AgentEntry(
            name="t-codex",
            harness="codex",
            harness_session_id=BIRTH,
            cwd="/proj",
            log_path="",
            status="live",
        )
    ])
    entry = restamp_harness_session_id(
        name="t-codex", harness="codex", session_id=REMINT, predecessor_reachable=True
    )
    assert entry is not None
    branch = next(row for row in load_registry() if row.harness_session_id == REMINT)
    assert branch.short_id == ""


def test_observation_unknown_evidence_parks_b_and_overwrites_nothing(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-ERR: unknown is neither death nor a question. A stays primary, B
    parks in the related slot, and the caller sees the additive outcome."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    entry, outcome = _observe("target-x-f0c2", REMINT, predecessor_reachable=None)
    assert outcome == "related"
    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].harness_session_id == BIRTH
    assert rows[0].related_session_id == REMINT
    assert rows[0].predecessor_session_ids == []


def test_observation_succession_replay_is_idempotent_and_byte_stable(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-ERR: replaying the same payload writes nothing, and the chain
    never duplicates A."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    _observe("target-x-f0c2", REMINT, predecessor_reachable=False)
    before = (tmp_path / ".fno" / "agents" / "registry.json").read_text()
    entry, outcome = _observe("target-x-f0c2", REMINT, predecessor_reachable=False)
    assert outcome == "no-op"
    after = (tmp_path / ".fno" / "agents" / "registry.json").read_text()
    assert after == before, "a replay must not rewrite the file"
    row = load_registry()[0]
    assert row.predecessor_session_ids == [BIRTH], "A appears exactly once"


def test_observation_branch_replay_is_idempotent(tmp_path: Path, monkeypatch) -> None:
    """The same fork edge observed twice answers branch again without
    minting a second B row."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    first, outcome = _observe("target-x-f0c2", REMINT, predecessor_reachable=True)
    assert outcome == "branch"
    again, outcome = _observe("target-x-f0c2", REMINT, predecessor_reachable=True)
    assert outcome == "branch"
    assert again.name == first.name
    assert len(load_registry()) == 2


def test_observation_stale_predecessor_evidence_classifies_nothing(
    tmp_path: Path, monkeypatch
) -> None:
    """Evidence sampled for an id the row no longer records must not classify
    against the CURRENT primary - the additive parking is the honest write."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    entry, outcome = _observe(
        "target-x-f0c2",
        REMINT,
        predecessor_reachable=False,
        expected_predecessor_session_id="session-sampled-before-lock",
    )
    assert outcome == "related"
    row = load_registry()[0]
    assert row.harness_session_id == BIRTH, "stale evidence never retires A"
    assert row.related_session_id == REMINT


# ---------------------------------------------------------------------------
# SessionStart end-to-end: events, questions, fail-soft
# ---------------------------------------------------------------------------


def _main(monkeypatch, tmp_path: Path, session_id: str, reading, name: str):
    from fno.agents import register_session

    if reading is not None:
        monkeypatch.setattr(
            register_session,
            "_reading_for_entry",
            lambda entry: reading,
        )
    return register_session.main([
        "--harness", "claude",
        "--session-id", session_id,
        "--cwd", "/proj",
        "--agent-self", name,
    ])


def test_main_claude_succession_emits_the_classified_event(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-HP event half: the SessionStart payload for B over a dead A emits
    session_transition_classified naming A, B, the classification, and the
    basis the verdict was read from."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry

    _spawned_row()
    assert _main(
        monkeypatch, tmp_path, REMINT, _reading("unreachable", "pane-gone"),
        "target-x-f0c2",
    ) == 0

    events = _transition_events(tmp_path)
    classified = [e for e in events if e["type"] == "session_transition_classified"]
    assert len(classified) == 1, f"exactly one classified event, got {events}"
    data = classified[0]["data"]
    assert data["classification"] == "succession"
    assert data["predecessor_session_id"] == BIRTH
    assert data["successor_session_id"] == REMINT
    assert data["basis"] == "pane-gone"
    assert data["evidence_at"]
    assert load_registry()[0].predecessor_session_ids == [BIRTH]


def test_main_claude_succession_replay_emits_already_applied(
    tmp_path: Path, monkeypatch
) -> None:
    """AC1-ERR event half: the replay names the pair with the
    already-applied marker instead of a second classified event."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)

    _spawned_row()
    assert _main(
        monkeypatch, tmp_path, REMINT, _reading("unreachable", "pane-gone"),
        "target-x-f0c2",
    ) == 0
    # The replay passes no reading seam (a live replay re-samples and finds
    # the payload id already primary); the marker must still fire.
    assert _main(monkeypatch, tmp_path, REMINT, None, "target-x-f0c2") == 0

    events = _transition_events(tmp_path)
    applied = [
        e for e in events if e["type"] == "session_transition_already_applied"
    ]
    assert len(applied) == 1
    assert applied[0]["data"]["predecessor_session_id"] == BIRTH
    assert applied[0]["data"]["successor_session_id"] == REMINT
    classified = [e for e in events if e["type"] == "session_transition_classified"]
    assert len(classified) == 1, "the replay is not a second classification"


def test_main_claude_branch_records_the_two_option_operator_question(
    tmp_path: Path, monkeypatch
) -> None:
    """AC2-HP question half + AC2-ERR: the branch asks the operator exactly
    one durable two-option question and never duplicates a claim - the mint
    touches no claim lockfile at all."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.outstanding.core import read_open_questions

    _spawned_row()
    assert _main(
        monkeypatch, tmp_path, REMINT, _reading("reachable", "transcript"),
        "target-x-f0c2",
    ) == 0

    questions = read_open_questions(tmp_path / ".fno")
    branch_qs = [q for q in questions if "session-transition-branch" in q.question]
    assert len(branch_qs) == 1, "exactly one durable question"
    assert list(branch_qs[0].options) == ["inherit node and claim", "start clean"]

    claims_dir = tmp_path / ".fno" / "claims"
    assert not claims_dir.exists() or not any(claims_dir.iterdir()), (
        "a branch mint must never create claim state"
    )

    # A replayed branch observation must not re-ask.
    assert _main(
        monkeypatch, tmp_path, REMINT, _reading("reachable", "transcript"),
        "target-x-f0c2",
    ) == 0
    questions = read_open_questions(tmp_path / ".fno")
    branch_qs = [q for q in questions if "session-transition-branch" in q.question]
    assert len(branch_qs) == 1, "the fork edge is asked once"


def test_main_claude_deferred_emits_the_deferred_event_and_asks_nothing(
    tmp_path: Path, monkeypatch
) -> None:
    """AC3-ERR event half: unavailable evidence emits session_transition_
    deferred naming both ids, and no operator question exists."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import load_registry
    from fno.outstanding.core import read_open_questions

    _spawned_row()
    assert _main(
        monkeypatch, tmp_path, REMINT, _reading("unknown", "silent"),
        "target-x-f0c2",
    ) == 0

    events = _transition_events(tmp_path)
    deferred = [e for e in events if e["type"] == "session_transition_deferred"]
    assert len(deferred) == 1
    assert deferred[0]["data"]["predecessor_session_id"] == BIRTH
    assert deferred[0]["data"]["successor_session_id"] == REMINT
    assert load_registry()[0].harness_session_id == BIRTH, "A is not overwritten"
    assert not read_open_questions(tmp_path / ".fno"), "unknown asks no question"


# ---------------------------------------------------------------------------
# The measured specimens
# ---------------------------------------------------------------------------


def test_measured_clear_specimen_classifies_as_succession(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-HP: the measured pane-27 /clear specimen. A carries the recorded
    reap as its exit falsifier; the B SessionStart must classify succession,
    leaving one current-B row with A in its chain."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    a = "01a02e51-acba-71a1-ae51-1e43d5167818"
    b = "01a031c3-92b5-7642-b210-07640fe8551b"
    write_registry([
        AgentEntry(
            name="pane-27-worker",
            harness="claude",
            harness_session_id=a,
            cwd="/proj",
            log_path="",
            status="exited",
            exited_at="2026-08-24T15:32:41Z",
        )
    ])
    from fno.agents import register_session

    monkeypatch.setattr(
        register_session,
        "_reading_for_entry",
        lambda entry: _reading("unreachable", "exit-recorded"),
    )
    assert register_session.main([
        "--harness", "claude",
        "--session-id", b,
        "--cwd", "/proj",
        "--agent-self", "pane-27-worker",
        "--source", "clear",
    ]) == 0

    rows = load_registry()
    assert len(rows) == 1
    assert rows[0].harness_session_id == b
    assert rows[0].predecessor_session_ids == [a]
    events = _transition_events(tmp_path)
    classified = [e for e in events if e["type"] == "session_transition_classified"]
    assert classified and classified[0]["data"]["classification"] == "succession"


def test_measured_specimen_with_a_live_a_flips_to_branch(
    tmp_path: Path, monkeypatch
) -> None:
    """AC5-ERR: the negative control. The SAME specimen with A positively
    reachable flips the structure to two rows - the classifier read A's
    evidence, not the command spelling or B's presence."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    a = "01a02e51-acba-71a1-ae51-1e43d5167818"
    b = "01a031c3-92b5-7642-b210-07640fe8551b"
    write_registry([
        AgentEntry(
            name="pane-27-worker",
            harness="claude",
            harness_session_id=a,
            cwd="/proj",
            log_path="",
            status="live",
        )
    ])
    from fno.agents import register_session

    monkeypatch.setattr(
        register_session,
        "_reading_for_entry",
        lambda entry: _reading("reachable", "transcript"),
    )
    assert register_session.main([
        "--harness", "claude",
        "--session-id", b,
        "--cwd", "/proj",
        "--agent-self", "pane-27-worker",
        "--source", "clear",
    ]) == 0

    rows = load_registry()
    assert len(rows) == 2
    assert {row.harness_session_id for row in rows} == {a, b}
    branch = next(row for row in rows if row.harness_session_id == b)
    assert branch.forked_from_session_id == a
    events = _transition_events(tmp_path)
    classified = [e for e in events if e["type"] == "session_transition_classified"]
    assert classified and classified[0]["data"]["classification"] == "branch"


# ---------------------------------------------------------------------------
# The codex lane emits the same event family
# ---------------------------------------------------------------------------


def test_non_claude_restamp_emits_the_classified_transition(
    tmp_path: Path, monkeypatch
) -> None:
    """The codex restamp lane's classified successions join the claude lane's
    in the daemon lifecycle log."""
    use_tmpdir(monkeypatch, tmp_path)
    _isolate_daemon_log(monkeypatch, tmp_path)
    from fno.agents import register_session
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name="t-codex",
            harness="codex",
            harness_session_id=BIRTH,
            cwd="/proj",
            log_path="",
            status="live",
        )
    ])
    monkeypatch.setattr(
        register_session,
        "_predecessor_observation",
        lambda name, harness: (BIRTH, False),
    )
    assert register_session.main([
        "--harness", "codex",
        "--session-id", REMINT,
        "--cwd", "/proj",
        "--agent-self", "t-codex",
    ]) == 0

    events = _transition_events(tmp_path)
    classified = [e for e in events if e["type"] == "session_transition_classified"]
    assert len(classified) == 1
    assert classified[0]["data"]["classification"] == "succession"
    assert classified[0]["data"]["predecessor_session_id"] == BIRTH
    assert classified[0]["data"]["successor_session_id"] == REMINT


# -- x-1ab9 task 3.1: one name store (session-names fold) --------------------


def _row_with_sid(name, sid, short):
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            harness="claude",
            harness_session_id=sid,
            short_id=short,
            cwd="/proj",
            log_path="",
            status="live",
        )
    ])


def test_reconcile_migration_folds_a_file_alias_into_its_row_ac5_hp(
    tmp_path, monkeypatch
) -> None:
    """AC5-HP: an alias the overlay file carries for a session lands on the
    row the session answers to, the mail reader resolves it from the row, and
    a second merge is a no-op (idempotent)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.discover import _alias_to_session_ids, default_name_map_path
    from fno.agents.registry import load_registry, merge_session_names_into_aliases

    _row_with_sid("target-x-f0c2", BIRTH, "f0c2abcd")
    name_map = default_name_map_path()
    name_map.parent.mkdir(parents=True, exist_ok=True)
    name_map.write_text(json.dumps({BIRTH: "legible-alias"}), encoding="utf-8")

    assert merge_session_names_into_aliases() == 1
    row = load_registry()[0]
    assert "legible-alias" in row.aliases, "the row carries the folded alias"
    ids, read_ok = _alias_to_session_ids("legible-alias", None)
    assert read_ok and BIRTH in ids, "mail resolution answers from the row"
    assert merge_session_names_into_aliases() == 0, "a replay merges nothing"


def test_append_row_alias_refuses_an_alias_another_row_answers_to(
    tmp_path, monkeypatch
) -> None:
    """An ambiguous alias is no address: the append fails closed when another
    row already answers to it, so `find` never has to guess."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.registry import append_row_alias, load_registry

    _row_with_sid("row-a", BIRTH, "aaaa1111")
    _row_with_sid("legible-alias", REMINT, "bbbb2222")
    assert append_row_alias("row-a", "legible-alias") is False
    assert "legible-alias" not in load_registry()[0].aliases, "the ambiguous alias never lands"
