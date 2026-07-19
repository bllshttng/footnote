"""Unit tests for the maintain legs (ab-9c144a4c).

Pure-function coverage of the six-leg sweep's detectors. The CLI-level
orchestration (apply under one lock, claimed-skip, health-history) is covered in
tests/integration/test_maintain_cli.py.

Filter: `python -m pytest tests/ -k maintain`
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

from fno.graph import maintain as m


WS = {
    "fno": "/home/u/code/abilities",
    "etl": "/home/u/code/etl",
}


def _n(node_id: str, **over) -> dict:
    base = {"id": node_id, "title": node_id, "project": None, "cwd": None, "_status": "ready"}
    base.update(over)
    return base


# --- leg 1: re-scope -------------------------------------------------------


def test_rescope_project_null_cwd_maps_to_project():
    fixes = m.detect_rescope_fixes(
        [_n("ab-1", project=None, cwd="/home/u/code/abilities")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "fno"
    assert fixes[0].new_cwd == "/home/u/code/abilities"


def test_rescope_worktree_cwd_with_correct_project_fixes_cwd():
    fixes = m.detect_rescope_fixes(
        [_n("ab-2", project="fno", cwd="/home/u/conductor/workspaces/abilities/foo")],
        WS,
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "fno"
    assert fixes[0].new_cwd == "/home/u/code/abilities"


def test_rescope_unknown_project_name_cwd_maps_elsewhere():
    fixes = m.detect_rescope_fixes(
        [_n("ab-3", project="bogus", cwd="/home/u/code/etl")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "etl"


def test_rescope_project_null_conductor_worktree_repo_hint():
    fixes = m.detect_rescope_fixes(
        [_n("ab-4", project=None, cwd="/x/conductor/workspaces/etl/bar")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "etl"
    assert fixes[0].new_cwd == "/home/u/code/etl"


def test_rescope_project_null_harness_native_worktree_repo_hint():
    """Harness-native worktree layout <repo>/.claude/worktrees/<name> (x-33e9):
    the segment before .claude/worktrees/ is the repo hint."""
    fixes = m.detect_rescope_fixes(
        [_n("ab-4b", project=None, cwd="/home/u/code/etl/.claude/worktrees/bar")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "etl"
    assert fixes[0].new_cwd == "/home/u/code/etl"


def test_worktree_repo_hint_custom_base(monkeypatch):
    """A custom worktrees_base <base>/<repo>/<name> is recognized (codex P1, PR #67)."""
    monkeypatch.setattr(m, "_configured_worktrees_base", lambda: "/custom/wt")
    fixes = m.detect_rescope_fixes(
        [_n("ab-4c", project=None, cwd="/custom/wt/etl/bar")], WS
    )
    assert len(fixes) == 1
    assert fixes[0].new_project == "etl"
    assert fixes[0].new_cwd == "/home/u/code/etl"


def test_worktree_repo_hint_custom_base_unset_declines(monkeypatch):
    """With no configured base, a node under an arbitrary root is left untouched."""
    monkeypatch.setattr(m, "_configured_worktrees_base", lambda: None)
    fixes = m.detect_rescope_fixes(
        [_n("ab-4d", project=None, cwd="/custom/wt/etl/bar")], WS
    )
    assert fixes == []


def test_rescope_correct_node_is_noop():
    fixes = m.detect_rescope_fixes(
        [_n("ab-5", project="fno", cwd="/home/u/code/abilities")], WS
    )
    assert fixes == []


def test_rescope_unmappable_cwd_left_alone():
    fixes = m.detect_rescope_fixes(
        [_n("ab-6", project=None, cwd="/somewhere/unmapped")], WS
    )
    assert fixes == []


def test_rescope_empty_workspaces_yields_nothing():
    assert m.detect_rescope_fixes([_n("ab-7", project=None, cwd="/home/u/code/etl")], {}) == []


# --- leg 2: leak-prune -----------------------------------------------------


def test_is_temp_cwd_variants():
    # Carries a pytest/test marker -> a real leak.
    assert m.is_temp_cwd("/tmp/pytest-of-bob/pytest-3/x")
    assert m.is_temp_cwd("/private/var/folders/aa/bb/T/fno-test-home-xyz")
    assert m.is_temp_cwd("/Users/u/x/pytest-of-u/pytest-12")
    # Not a leak: a real cwd.
    assert not m.is_temp_cwd("/home/u/code/abilities")
    assert not m.is_temp_cwd(None)
    assert not m.is_temp_cwd("")
    # A legitimate checkout / scratch worktree under a bare temp ROOT (no
    # pytest marker) must NOT be pruned (codex P2 on PR #474): matching the
    # whole /tmp or /var/folders prefix would delete a real node.
    assert not m.is_temp_cwd("/tmp/my-scratch-project")
    assert not m.is_temp_cwd("/var/folders/aa/bb/T/some-worktree")


def test_detect_temp_leaks():
    entries = [
        _n("ab-good", cwd="/home/u/code/abilities"),
        _n("ab-leak", cwd="/tmp/pytest-of-x/pytest-1/proj"),
    ]
    assert m.detect_temp_leaks(entries) == ["ab-leak"]


# --- leg 3: dedup ----------------------------------------------------------


def test_detect_dup_groups_idea_only():
    entries = [
        _n("ab-a", title="Fix the thing", _status="idea"),
        _n("ab-b", title="fix the  thing!", _status="idea"),  # normalizes same
        _n("ab-c", title="Fix the thing", _status="ready"),   # ready -> ignored
        _n("ab-d", title="Unrelated", _status="idea"),
    ]
    groups = m.detect_dup_groups(entries)
    assert len(groups) == 1
    assert set(groups[0]) == {"ab-a", "ab-b"}


# --- leg 4: drain stale ----------------------------------------------------


def test_detect_stale_ideas_strictly_older_than():
    now = datetime(2026, 6, 8, tzinfo=timezone.utc)
    old = (now - timedelta(days=40)).isoformat()
    exactly = (now - timedelta(days=30)).isoformat()
    fresh = (now - timedelta(days=5)).isoformat()
    entries = [
        _n("ab-old", _status="idea", created_at=old),
        _n("ab-edge", _status="idea", created_at=exactly),  # exactly N -> NOT stale
        _n("ab-fresh", _status="idea", created_at=fresh),
        _n("ab-ready", _status="ready", created_at=old),    # not an idea
        _n("ab-nots", _status="idea"),                       # no created_at
    ]
    stale = m.detect_stale_ideas(entries, 30, now=now)
    assert [s.node_id for s in stale] == ["ab-old"]
    assert stale[0].age_days == 40


# --- leg 5: cap Now --------------------------------------------------------


def test_now_overflow():
    entries = [_n(f"ab-{i}", col="Now") for i in range(3)] + [_n("ab-x", col="Next")]

    def col(e):
        return e.get("col")

    assert m.now_overflow(entries, 2, col) == (3, 2)
    assert m.now_overflow(entries, 3, col) is None


# --- config block ----------------------------------------------------------


def test_maintain_config_default_staleness():
    from fno.config import ConfigBlock

    assert ConfigBlock().backlog.maintain.staleness_days == 30


def test_maintain_config_custom_staleness():
    from fno.config import BacklogBlock

    assert BacklogBlock(maintain={"staleness_days": 7}).maintain.staleness_days == 7


def test_maintain_config_rejects_non_positive_staleness():
    import pytest
    from pydantic import ValidationError

    from fno.config import MaintainBlock

    with pytest.raises(ValidationError):
        MaintainBlock(staleness_days=0)


def test_maintain_config_default_max_failed_attempts():
    from fno.config import ConfigBlock

    assert ConfigBlock().backlog.maintain.max_failed_attempts == 3


def test_maintain_config_custom_max_failed_attempts():
    from fno.config import BacklogBlock

    assert (
        BacklogBlock(maintain={"max_failed_attempts": 5}).maintain.max_failed_attempts
        == 5
    )


def test_maintain_config_rejects_non_positive_max_failed_attempts():
    import pytest
    from pydantic import ValidationError

    from fno.config import MaintainBlock

    with pytest.raises(ValidationError):
        MaintainBlock(max_failed_attempts=0)


# --- failure-streak helper (ab-5b7cf63a / #34, task 1.2) -------------------

from fno.graph import failure as f  # noqa: E402


def _fail(nid: str) -> dict:
    return {"type": "node_failed", "data": {"unit_id": nid}}


def _parked(nid: str) -> dict:
    return {"type": "node_closed", "data": {"unit_id": nid, "close": "parked"}}


def _closed(nid: str) -> dict:
    return {"type": "node_closed", "data": {"unit_id": nid, "close": "closed"}}


def _refused(nid: str) -> dict:
    return {"type": "node_closed", "data": {"unit_id": nid, "close": "refused"}}


def _undefer(nid: str) -> dict:
    # Flat agents-emitter envelope (fno.agents.events.emit shape).
    return {"kind": "node_undeferred", "unit_id": nid}


def test_streak_zero_with_no_events():
    assert f.consecutive_failures("ab-x", []) == 0


def test_streak_counts_consecutive_failures():
    events = [_fail("ab-x"), _fail("ab-x"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 3


def test_streak_parked_close_counts_as_failure():
    events = [_fail("ab-x"), _parked("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 2


def test_streak_only_counts_target_node():
    events = [_fail("ab-x"), _fail("ab-y"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 2


def test_streak_reset_on_success_close():
    # AC4-EDGE: two failures then a success ship -> streak 0.
    events = [_fail("ab-x"), _fail("ab-x"), _closed("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 0
    # ... and a later failure after the success counts from the boundary.
    events2 = events + [_fail("ab-x")]
    assert f.consecutive_failures("ab-x", events2) == 1


def test_streak_reset_on_undefer_event():
    # AC5-FR: undefer is a reset boundary; one fresh failure after it -> 1.
    events = [_fail("ab-x"), _fail("ab-x"), _fail("ab-x"), _undefer("ab-x"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 1


def test_streak_refused_close_ignored():
    # A dispatch refusal neither counts nor resets the streak.
    events = [_fail("ab-x"), _refused("ab-x"), _fail("ab-x")]
    assert f.consecutive_failures("ab-x", events) == 2


def test_streak_malformed_event_skipped(tmp_path):
    # AC2-ERR: a truncated/non-JSON line is skipped, valid lines still read.
    log = tmp_path / "events.jsonl"
    log.write_text(
        '{"type":"node_failed","data":{"unit_id":"ab-x"}}\n'
        '{"type":"node_failed","data":{"unit_id":"ab-x"\n'  # truncated
        "not json at all\n"
        '{"type":"node_failed","data":{"unit_id":"ab-x"}}\n'
    )
    events = f.read_events(log)
    assert len(events) == 2  # two well-formed lines survive
    assert f.consecutive_failures("ab-x", events) == 2


def test_read_events_absent_file_is_empty(tmp_path):
    assert f.read_events(tmp_path / "nope.jsonl") == []


def test_stranded_dependents_maps_auto_failure_deferred():
    entries = [
        _n("ab-block", _status="deferred", deferred_at="t",
           deferred_reason="auto-failure: 3 consecutive failed attempts"),
        _n("ab-dep1", _status="blocked", blocked_by=["ab-block"]),
        _n("ab-dep2", _status="blocked", blocked_by=["ab-block"]),
    ]
    stranded = f.stranded_dependents(entries)
    assert set(stranded["ab-block"]) == {"ab-dep1", "ab-dep2"}


def test_stranded_dependents_ignores_manual_defer():
    # A hand-deferred blocker (no auto-failure sentinel) is NOT strand-reported.
    entries = [
        _n("ab-block", _status="deferred", deferred_at="t",
           deferred_reason="parked by hand"),
        _n("ab-dep1", _status="blocked", blocked_by=["ab-block"]),
    ]
    assert f.stranded_dependents(entries) == {}


def test_stranded_dependents_omits_blocker_with_no_dependents():
    entries = [
        _n("ab-block", _status="deferred", deferred_at="t",
           deferred_reason="auto-failure: 4 consecutive failed attempts"),
    ]
    assert f.stranded_dependents(entries) == {}


# --- auto-defer detector (maintain.py, task 2.1) ---------------------------


def test_detect_failure_defers_threshold_boundary():
    # N-1 must not trigger; >= N must (Boundaries).
    events = [_fail("ab-x"), _fail("ab-x")]
    node = _n("ab-x")  # _status ready
    assert m.detect_failure_defers([node], events, 3) == []
    cands = m.detect_failure_defers([node], events + [_fail("ab-x")], 3)
    assert [(c.node_id, c.streak) for c in cands] == [("ab-x", 3)]


def test_detect_failure_defers_skips_non_ready_and_deferred():
    events = [_fail("ab-x")] * 5
    assert m.detect_failure_defers([_n("ab-x", _status="idea")], events, 3) == []
    assert (
        m.detect_failure_defers(
            [_n("ab-x", _status="deferred", deferred_at="t")], events, 3
        )
        == []
    )


def test_detect_failure_defers_event_for_absent_node_noops():
    events = [_fail("ab-ghost")] * 5
    assert m.detect_failure_defers([_n("ab-real")], events, 3) == []


# --- leg 8: validity sweep - selection + fingerprint ---------------


def _idea(node_id: str, age_days: int, now: datetime, **over) -> dict:
    created = (now - timedelta(days=age_days)).isoformat()
    return _n(node_id, _status="idea", created_at=created, **over)


def test_clamp_validity_bounds_defaults_and_hard_cap():
    days, size, warns = m.clamp_validity_bounds(0, 0)
    assert (days, size) == (m.VALIDITY_DAYS_DEFAULT, m.VALIDITY_BATCH_DEFAULT)
    assert len(warns) == 2
    # Over-large batch is clamped to the hard max (Locked Decision #7).
    _, size2, warns2 = m.clamp_validity_bounds(60, 5000)
    assert size2 == m.VALIDITY_BATCH_HARD_MAX
    assert warns2
    # A bool is not a valid int here (True == 1 must not sneak through).
    days3, _, warns3 = m.clamp_validity_bounds(True, 25)
    assert days3 == m.VALIDITY_DAYS_DEFAULT and warns3


def test_select_validity_strict_age_and_oldest_first():
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    entries = [
        _idea("ab-90", 90, now),
        _idea("ab-70", 70, now),
        _idea("ab-60", 60, now),   # exactly 60 -> excluded (strict)
        _idea("ab-30", 30, now),   # too fresh
        _n("ab-ready", _status="ready", created_at=(now - timedelta(days=99)).isoformat()),
    ]
    cands = m.select_validity_candidates(entries, 60, 25, now=now)
    assert [c["id"] for c in cands] == ["ab-90", "ab-70"]  # oldest first


def test_select_validity_batch_cap_and_pagination():
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    entries = [_idea(f"ab-{i:03d}", 100 + i, now) for i in range(10)]
    first = m.select_validity_candidates(entries, 60, 3, now=now)
    assert [c["id"] for c in first] == ["ab-009", "ab-008", "ab-007"]  # oldest = biggest age


def test_select_validity_excludes_claimed_and_watermarked():
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    claimed = _idea("ab-claim", 90, now)
    watermarked = _idea("ab-wm", 85, now)
    fresh = _idea("ab-fresh-idea", 80, now)
    entries = [claimed, watermarked, fresh]
    cands = m.select_validity_candidates(
        entries, 60, 25, now=now,
        claimed_ids=frozenset({"ab-claim"}),
        seen_fingerprints=frozenset({m.node_fingerprint(watermarked)}),
    )
    assert [c["id"] for c in cands] == ["ab-fresh-idea"]


def test_node_fingerprint_changes_on_edit():
    base = _n("ab-x", title="original", details="d")
    fp1 = m.node_fingerprint(base)
    assert fp1 == m.node_fingerprint(dict(base))          # stable
    edited = dict(base, title="rewritten premise")
    assert m.node_fingerprint(edited) != fp1              # edit requalifies


# --- leg 8: validity sweep - evidence collection ----------------------------


def test_collect_evidence_allowlisted_ids_and_graph_dup():
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    node = _idea(
        "ab-idea", 90, now,
        title="Add spinner",
        details="touches `Spinner` in cli/src/fno/ui.py",
        plan_path="internal/plans/p.md",
        blocked_by=["ab-dep"],
    )
    entries = [node, _n("ab-done", title="add spinner", _status="done")]
    pkt = m.collect_evidence(
        node, entries, now=now,
        exists=lambda p: p == "cli/src/fno/ui.py",
        search=lambda s: 4,
    )
    ids = set(pkt.items)
    assert "graph:blocked_by" in ids
    assert "graph:title-match:ab-done" in ids and pkt.items["graph:title-match:ab-done"] == "done"
    assert pkt.items["path:cli/src/fno/ui.py"] == "exists"
    assert pkt.items["pr:plan"] == "internal/plans/p.md"
    assert pkt.items["git:Spinner"] == "4 matches"
    # Every id carries an allowlisted prefix (injection boundary).
    assert all(any(i.startswith(p) for p in m.ALLOWED_EVIDENCE_PREFIXES) for i in ids)


def test_collect_evidence_records_unavailable_sources():
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    node = _idea("ab-idea", 90, now, title="t", details="touches a/b.py and `Sym`")
    # No exists/search seams -> repo + git unavailable, recorded not fabricated.
    pkt = m.collect_evidence(node, [node], now=now)
    assert "path" in pkt.unavailable and "git" in pkt.unavailable
    assert not any(i.startswith("path:") for i in pkt.items)


def test_contained_path_exists_blocks_traversal(tmp_path):
    (tmp_path / "in.py").write_text("x")
    root = str(tmp_path)
    assert m.contained_path_exists(root, "in.py") is True
    assert m.contained_path_exists(root, "nope.py") is False
    # CWE-22: an untrusted `../` or absolute path must not escape root -> missing.
    assert m.contained_path_exists(root, "../../etc/passwd") is False
    assert m.contained_path_exists(root, "/etc/passwd") is False


def test_collect_evidence_missing_path_is_missing_not_dropped():
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    node = _idea("ab-idea", 90, now, title="t", details="cli/gone/removed.py")
    pkt = m.collect_evidence(node, [node], now=now, exists=lambda p: False)
    assert pkt.items["path:cli/gone/removed.py"] == "missing"


def test_collect_evidence_caps_packet_size():
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    node = _idea("ab-big", 90, now, title="t", details="x" * 100_000)
    pkt = m.collect_evidence(node, [node], now=now)
    size = len(json.dumps(pkt.to_json()).encode("utf-8"))
    assert size <= m.PACKET_MAX_BYTES


# --- leg 8: validity sweep - validation + command rendering --------


def _packet(node_id: str, **over) -> m.EvidencePacket:
    base = dict(
        node_id=node_id, fingerprint=f"fp-{node_id}", title="t", details="d",
        project=None, cwd=None, age_days=90, items={}, unavailable=[],
    )
    base.update(over)
    return m.EvidencePacket(**base)


def test_validate_row_supersede_requires_evidenced_target():
    pkt = _packet("ab-x", items={"graph:title-match:ab-new": "done"})
    ok = m.validate_row(
        {"classification": "supersede", "confidence": 0.9,
         "rationale": "dup", "evidence_ids": ["graph:title-match:ab-new"],
         "target": "ab-new"},
        pkt,
    )
    assert ok.classification == "supersede" and ok.target == "ab-new"
    assert ok.command == (
        "fno backlog supersede ab-new --replaces ab-x "
        "--reason 'validity sweep: superseded by ab-new'"
    )
    # A target the graph evidence does not name -> needs-human, no command.
    bad = m.validate_row(
        {"classification": "supersede", "confidence": 0.9, "rationale": "dup",
         "evidence_ids": ["graph:title-match:ab-new"], "target": "ab-ghost"},
        pkt,
    )
    assert bad.classification == "needs-human" and bad.command is None


def test_validate_row_uncited_or_low_conf_destructive_downgrades():
    pkt = _packet("ab-x", items={"path:a/b.py": "missing"})
    uncited = m.validate_row(
        {"classification": "promote", "confidence": 0.9, "rationale": "r", "evidence_ids": []},
        pkt,
    )
    assert uncited.classification == "needs-human"
    lowconf = m.validate_row(
        {"classification": "promote", "confidence": 0.2, "rationale": "r",
         "evidence_ids": ["path:a/b.py"]},
        pkt,
    )
    assert lowconf.classification == "needs-human"


def test_validate_row_unknown_class_and_bad_citation():
    pkt = _packet("ab-x", items={"path:a/b.py": "exists"})
    assert m.validate_row({"classification": "delete"}, pkt).classification == "needs-human"
    # A citation not present in the packet is dropped (injection boundary).
    row = m.validate_row(
        {"classification": "keep", "confidence": 0.8, "rationale": "r",
         "evidence_ids": ["path:a/b.py", "git:__import__('os')"]},
        pkt,
    )
    assert row.classification == "keep" and row.evidence_ids == ["path:a/b.py"]


def test_validate_row_missing_result_is_needs_human():
    assert m.validate_row(None, _packet("ab-x")).classification == "needs-human"


def test_render_command_never_embeds_rationale():
    # Trusted-render only: no analyzer text reaches the command string.
    cmd = m.render_command("promote", "ab-x", None)
    assert cmd == "fno backlog update ab-x --priority p3"
    assert m.render_command("keep", "ab-x", None) is None
    assert m.render_command("needs-human", "ab-x", None) is None


# --- leg 8: validity sweep - JSON-last deck + watermark ---------------------


def test_write_deck_json_last_and_grouped(tmp_path):
    pkts = {"ab-a": _packet("ab-a", title="Alpha"), "ab-b": _packet("ab-b", title="Beta")}
    rows = [
        m.ValidityRow("ab-a", "fp-a", "keep", 0.8, "still good", ["path:x"]),
        m.ValidityRow("ab-b", "fp-b", "needs-human", 0.0, "unclear", []),
    ]
    md, js = m.write_validity_deck(
        rows, pkts, tmp_path, deck_id="validity-1", created_iso="2026-07-12T00:00:00Z"
    )
    md_text = (tmp_path / "validity-1.md").read_text()
    assert "## Keep / Cool-Later (1)" in md_text and "## Needs Human (1)" in md_text
    side = json.loads((tmp_path / "validity-1.json").read_text())
    assert side["counts"]["keep"] == 1 and side["counts"]["needs-human"] == 1
    assert side["md_hash"] and len(side["rows"]) == 2


def test_watermark_only_from_valid_committed_rows(tmp_path):
    pkts = {"ab-a": _packet("ab-a")}
    good = [m.ValidityRow("ab-a", "fp-good", "keep", 0.8, "r", [])]
    m.write_validity_deck(good, pkts, tmp_path, deck_id="d1", created_iso="t")
    # A degraded (analyzer-failure) deck must NOT watermark, so its batch retries.
    degraded = m.evidence_only_rows([_packet("ab-b", fingerprint="fp-degraded")])
    m.write_validity_deck(degraded, {}, tmp_path, deck_id="d2", created_iso="t", degraded=True)
    seen = m.read_watermarked_fingerprints(tmp_path)
    assert "fp-good" in seen and "fp-degraded" not in seen


def test_read_watermarks_skips_malformed_sidecar(tmp_path):
    (tmp_path / "broken.json").write_text("{not json")
    (tmp_path / "ok.json").write_text(
        json.dumps({"rows": [{"fingerprint": "fp-1", "watermark": True}]})
    )
    assert m.read_watermarked_fingerprints(tmp_path) == frozenset({"fp-1"})


def test_apply_aggregate_budget_drops_overflow_tail():
    # Each packet ~ >200 KiB of details; 3 of them exceed the 512 KiB aggregate,
    # so the oldest-first prefix is kept and the tail dropped (never silent).
    big = "y" * 200_000
    pkts = [_packet(f"ab-{i}", details=big) for i in range(4)]
    kept, dropped = m._apply_aggregate_budget(pkts)
    total = sum(len(json.dumps(p.to_json()).encode()) for p in kept)
    assert total <= m.AGGREGATE_MAX_BYTES
    assert dropped == len(pkts) - len(kept) and dropped > 0
    # At least one packet always survives (no permanent starvation).
    assert len(kept) >= 1


def test_run_validity_sweep_reread_marks_stale_after_analysis(tmp_path):
    now = datetime(2026, 7, 12, tzinfo=timezone.utc)
    node = _idea("ab-race", 90, now, title="t", plan_path=None)
    entries = [node]

    def analyze(packets):
        return {
            p.node_id: {"classification": "keep", "confidence": 0.8,
                        "rationale": "still good", "evidence_ids": []}
            for p in packets
        }

    # reread (called AFTER analyze) returns the node now claimed -> no longer idea.
    def reread():
        return [dict(node, _status="claimed", locked_by="someone")]

    res = m.run_validity_sweep(
        entries, validity_days=60, batch_size=25, out_dir=tmp_path,
        now=now, analyze=analyze, reread=reread,
    )
    assert res.eligible == 1 and res.stale == 1
    side = json.loads(next(tmp_path.glob("*.json")).read_text())
    assert side["rows"][0]["stale"] is True and side["rows"][0]["command"] is None


def test_run_validity_analysis_refuses_real_call_under_pytest(monkeypatch):
    import pytest
    monkeypatch.delenv("FNO_VALIDITY_STUB", raising=False)
    with pytest.raises(RuntimeError, match="refusing real claude"):
        m._run_validity_analysis([_packet("ab-x")])


def test_run_validity_analysis_parses_stub(tmp_path, monkeypatch):
    stub = tmp_path / "stub.sh"
    stub.write_text(
        '#!/usr/bin/env bash\n'
        'echo \'{"results":[{"node_id":"ab-x","classification":"keep",'
        '"confidence":0.8,"rationale":"good","evidence_ids":[]}]}\'\n'
    )
    stub.chmod(0o755)
    monkeypatch.setenv("FNO_VALIDITY_STUB", str(stub))
    out = m._run_validity_analysis([_packet("ab-x")])
    assert out["ab-x"]["classification"] == "keep"


# ---------------------------------------------------------------------------
# G1 stale-ready quarantine (x-3236)
# ---------------------------------------------------------------------------


def _sr_now():
    return datetime(2026, 7, 18, tzinfo=timezone.utc)


def test_backlog_staleness_days_default_21():
    from fno.config import BacklogBlock

    assert BacklogBlock().staleness_days == 21


def test_backlog_staleness_days_rejects_non_positive():
    import pytest
    from pydantic import ValidationError

    from fno.config import BacklogBlock

    with pytest.raises(ValidationError):
        BacklogBlock(staleness_days=0)


def test_node_has_movement_field_signals():
    now = _sr_now()
    assert m.node_has_movement({"sessions": [{"phase": "do"}]}, now, 21)
    assert m.node_has_movement({"pr_number": 5}, now, 21)
    assert m.node_has_movement({"locked_by": "sess"}, now, 21)
    assert m.node_has_movement({"claimed_at": "2026-07-18T00:00:00+00:00"}, now, 21)
    assert not m.node_has_movement({}, now, 21)


def test_node_has_movement_fresh_plan_mtime(tmp_path):
    now = datetime.now(timezone.utc)
    p = tmp_path / "plan.md"
    p.write_text("x")
    assert m.node_has_movement({"plan_path": str(p)}, now, 21)


def test_node_has_movement_stale_plan_mtime(tmp_path):
    import os
    import time

    now = datetime.now(timezone.utc)
    p = tmp_path / "plan.md"
    p.write_text("x")
    old = time.time() - 60 * 86400
    os.utime(p, (old, old))
    assert not m.node_has_movement({"plan_path": str(p)}, now, 21)


def test_is_stale_ready_old_and_unmoved():
    now = _sr_now()
    old = (now - timedelta(days=80)).isoformat()
    assert m.is_stale_ready({"created_at": old}, now, 21)


def test_is_stale_ready_recent_not_stale():
    now = _sr_now()
    recent = (now - timedelta(days=5)).isoformat()
    assert not m.is_stale_ready({"created_at": recent}, now, 21)


def test_is_stale_ready_moved_not_stale():
    now = _sr_now()
    old = (now - timedelta(days=80)).isoformat()
    assert not m.is_stale_ready({"created_at": old, "pr_number": 3}, now, 21)


def test_is_stale_ready_boundary_exactly_threshold_not_stale():
    now = _sr_now()
    at21 = (now - timedelta(days=21)).isoformat()
    assert not m.is_stale_ready({"created_at": at21}, now, 21)


def test_is_stale_ready_no_timestamp_not_quarantined():
    # AC4-EDGE (as implemented): a node whose age cannot be proven is NEVER
    # quarantined - a guard must not starve a freshly-minted node lacking a
    # stamp. The untimestamped abandoned case is left to the human/maintain leg.
    assert not m.is_stale_ready({}, _sr_now(), 21)


def test_is_stale_ready_blocked_node_not_quarantined():
    # A just-unblocked dependent carries a lingering blocked_by and legitimately
    # has no movement yet; it must never be quarantined.
    now = _sr_now()
    old = (now - timedelta(days=80)).isoformat()
    assert not m.is_stale_ready(
        {"created_at": old, "blocked_by": ["dep"]}, now, 21
    )


def test_detect_stale_ready_only_ready_and_unmoved():
    now = _sr_now()
    old = (now - timedelta(days=80)).isoformat()
    entries = [
        {"id": "a", "_status": "ready", "created_at": old},
        {"id": "b", "_status": "idea", "created_at": old},        # not ready
        {"id": "c", "_status": "ready", "created_at": old, "pr_number": 1},  # moved
        {"id": "d", "_status": "ready", "created_at": (now - timedelta(days=2)).isoformat()},
    ]
    got = {s.node_id for s in m.detect_stale_ready(entries, 21, now=now)}
    assert got == {"a"}


def test_node_has_movement_non_string_plan_path_no_crash():
    # A malformed graph could carry a non-string plan_path; getmtime would raise
    # TypeError (not OSError). The isinstance guard keeps it from crashing.
    now = _sr_now()
    assert not m.node_has_movement({"plan_path": 12345}, now, 21)
    assert not m.node_has_movement({"plan_path": ["a", "b"]}, now, 21)
