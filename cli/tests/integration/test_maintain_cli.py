"""Integration tests for `fno backlog maintain` (ab-9c144a4c).

Covers the AC set from internal/fno/design/2026-06-08-backlog-maintenance-ritual.md:
deterministic apply legs (re-scope, leak-prune), judgment legs as propose-only,
claimed-node skip, empty-graph no-op, idempotency, and the health-history report.

Filter: `python -m pytest tests/ -k maintain_cli`
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    # No live claims unless a test says so.
    import fno.graph.cli as gcli

    monkeypatch.setattr(gcli, "_live_claimed_node_ids", lambda: set())
    # Hermetic workspace map (no settings.yaml leakage); tests that exercise
    # re-scope override this.
    import fno.graph.maintain as gm

    monkeypatch.setattr(gm, "load_workspaces", lambda: {})
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _node(node_id: str, **over) -> dict:
    base = {
        "id": node_id,
        "title": f"Node {node_id}",
        "priority": "p2",
        "project": None,
        "cwd": None,
        "plan_path": None,
        "completed_at": None,
        "blocked_by": [],
    }
    base.update(over)
    return base


def _invoke(args: list[str]):
    return runner.invoke(app, ["backlog", "maintain"] + args, catch_exceptions=False)


# --- AC1-EDGE: empty graph is a clean no-op --------------------------------


def test_maintain_cli_empty_graph_noop(tmp_graph):
    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph) == []


# --- AC1-HP: --apply prunes temp leaks -------------------------------------


def test_maintain_cli_apply_prunes_temp_leak(tmp_graph):
    _seed(
        tmp_graph,
        [
            _node("ab-keep01", cwd="/home/u/code/abilities", project="fno"),
            _node("ab-leak01", cwd="/tmp/pytest-of-x/pytest-1/p"),
        ],
    )
    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    ids = {n["id"] for n in _read(tmp_graph)}
    assert ids == {"ab-keep01"}  # leak removed
    assert "pruned 1" in result.output


# --- AC1-HP: --apply re-scopes a worktree-cwd node -------------------------


def test_maintain_cli_apply_rescopes_worktree_cwd(tmp_graph, monkeypatch):
    import fno.graph.maintain as gm

    monkeypatch.setattr(gm, "load_workspaces", lambda: {"proj": "/canonical/proj"})
    _seed(
        tmp_graph,
        [_node("ab-scope01", project="proj", cwd="/canonical/proj/worktrees/x")],
    )
    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    node = _read(tmp_graph)[0]
    assert node["cwd"] == "/canonical/proj"  # cwd rewritten to canonical
    assert node["project"] == "proj"


# --- AC1-FR: idempotent re-run --------------------------------------------


def test_maintain_cli_idempotent_rerun(tmp_graph, monkeypatch):
    import fno.graph.maintain as gm

    monkeypatch.setattr(gm, "load_workspaces", lambda: {"proj": "/canonical/proj"})
    _seed(tmp_graph, [_node("ab-scope02", project="proj", cwd="/canonical/proj/wt/y")])

    first = _invoke(["--apply", "--json"])
    assert first.exit_code == 0
    assert json.loads(first.output)["rescope"]["applied"] == ["ab-scope02"]

    second = _invoke(["--apply", "--json"])
    assert second.exit_code == 0
    payload = json.loads(second.output)
    assert payload["rescope"]["applied"] == []
    assert payload["rescope"]["candidates"] == []


# --- AC2-FR: a live-claimed node is not mutated ----------------------------


def test_maintain_cli_skips_claimed_node(tmp_graph, monkeypatch):
    import fno.graph.cli as gcli

    _seed(tmp_graph, [_node("ab-leak02", cwd="/tmp/pytest-of-x/pytest-9/p")])
    monkeypatch.setattr(gcli, "_live_claimed_node_ids", lambda: {"ab-leak02"})

    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    # The leak node is claimed -> NOT pruned.
    assert {n["id"] for n in _read(tmp_graph)} == {"ab-leak02"}
    assert "skipped-claimed 1" in result.output


# --- AC2-HP / AC2-ERR: judgment legs propose, never mutate -----------------


def test_maintain_cli_judgment_legs_propose_only(tmp_graph):
    now = datetime.now(timezone.utc)
    old = (now - timedelta(days=99)).isoformat()
    _seed(
        tmp_graph,
        [
            # two near-duplicate ideas (no plan_path -> _status idea)
            _node("ab-dup01", title="Same idea", created_at=old),
            _node("ab-dup02", title="same  idea!", created_at=old),
        ],
    )
    before = _read(tmp_graph)
    # Even WITH --apply, dedup + drain must not mutate.
    result = _invoke(["--apply", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert len(payload["dedup_groups"]) == 1
    assert {s["node_id"] for s in payload["stale_ideas"]} == {"ab-dup01", "ab-dup02"}
    # Graph unchanged: no defer, no removal.
    after = _read(tmp_graph)
    assert {n["id"] for n in after} == {"ab-dup01", "ab-dup02"}
    assert all(n.get("deferred_at") is None for n in after)


# --- AC1-UI: per-leg counts printed (no-op vs active distinguishable) -------


def test_maintain_cli_prints_per_leg_counts(tmp_graph):
    result = _invoke([])  # report mode, empty graph
    assert result.exit_code == 0, result.output
    assert "re-scope candidates 0" in result.output
    assert "prune candidates 0" in result.output


# --- AC3-HP: summary appended to health-history ----------------------------


def test_maintain_cli_appends_health_history(tmp_graph):
    _seed(tmp_graph, [_node("ab-keep02", cwd="/home/u/code/abilities", project="fno")])
    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output

    hist = Path.home() / ".fno" / "health-history.jsonl"
    assert hist.exists()
    lines = [json.loads(ln) for ln in hist.read_text().splitlines() if ln.strip()]
    maintain_records = [r for r in lines if r.get("scope") == "maintain"]
    assert maintain_records, "expected a maintain record in health-history"
    assert maintain_records[-1]["report"]["applied"] is True


# --- auto-defer apply-leg (#34, task 2.1) ----------------------------------

# The streak reader and the undefer emitter both use paths.state_dir() /
# events.jsonl (== $HOME/.fno/events.jsonl under the conftest HOME
# redirect), so seed/assert against that single file.


def _events_file() -> Path:
    return Path.home() / ".fno" / "events.jsonl"


def _seed_events(records: list[dict]) -> None:
    p = _events_file()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("".join(json.dumps(r) + "\n" for r in records))


def _ev_fail(nid: str) -> dict:
    return {"type": "node_failed", "data": {"unit_id": nid}}


def _ev_parked(nid: str) -> dict:
    return {"type": "node_closed", "data": {"unit_id": nid, "close": "parked"}}


def _append_events(records: list[dict]) -> None:
    p = _events_file()
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a", encoding="utf-8") as fh:
        for r in records:
            fh.write(json.dumps(r) + "\n")


@pytest.fixture(autouse=True)
def _clean_events():
    # No event-log leakage between tests (the log lives in the shared session
    # HOME, not tmp_path).
    p = _events_file()
    if p.exists():
        p.unlink()
    yield
    if p.exists():
        p.unlink()


def _ready(node_id: str, **over) -> dict:
    # A node with a plan_path and no completed/deferred state derives _status:
    # ready (the auto-defer candidate filter only considers ready nodes).
    return _node(node_id, plan_path=f"plans/{node_id}.md", **over)


def test_maintain_cli_auto_defer_at_threshold(tmp_graph):
    # AC1-HP: a ready node with N consecutive failures is deferred under --apply.
    _seed(tmp_graph, [_ready("ab-fail01")])
    _seed_events([_ev_fail("ab-fail01"), _ev_fail("ab-fail01"), _ev_fail("ab-fail01")])

    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output

    node = _read(tmp_graph)[0]
    assert node["deferred_at"] is not None
    assert node["deferred_reason"] == "auto-failure: 3 consecutive failed attempts"
    assert node["_status"] == "deferred"  # no longer surfaced by `backlog next`
    assert "auto-deferred ab-fail01" in result.output  # named in the run summary


def test_maintain_cli_auto_defer_threshold_boundary(tmp_graph):
    # AC4-EDGE: a node at exactly N-1 consecutive failures is NOT deferred.
    _seed(tmp_graph, [_ready("ab-fail02")])
    _seed_events([_ev_fail("ab-fail02"), _ev_fail("ab-fail02")])  # 2, default N=3

    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0].get("deferred_at") is None


def test_maintain_cli_auto_defer_skips_live_claim(tmp_graph, monkeypatch):
    # AC6-FR: a node holding a live node:<id> claim is skipped, never deferred.
    import fno.graph.cli as gcli

    _seed(tmp_graph, [_ready("ab-fail03")])
    _seed_events([_ev_fail("ab-fail03")] * 4)
    monkeypatch.setattr(gcli, "_live_claimed_node_ids", lambda: {"ab-fail03"})

    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0].get("deferred_at") is None
    assert "skipped-claimed 1" in result.output


def test_maintain_cli_auto_defer_propose_only_without_apply(tmp_graph):
    # Auto-defer is an apply-leg: without --apply it only proposes (no mutation).
    _seed(tmp_graph, [_ready("ab-fail04")])
    _seed_events([_ev_fail("ab-fail04")] * 3)

    result = _invoke(["--json"])  # no --apply
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert [c["node_id"] for c in payload["auto_defer"]["candidates"]] == ["ab-fail04"]
    assert payload["auto_defer"]["applied"] == []
    assert _read(tmp_graph)[0].get("deferred_at") is None


def test_maintain_cli_auto_defer_parked_counts_as_failure(tmp_graph):
    # A node_closed{close=parked} counts as a failure toward the streak.
    _seed(tmp_graph, [_ready("ab-fail05")])
    _seed_events(
        [_ev_fail("ab-fail05"), _ev_parked("ab-fail05"), _ev_fail("ab-fail05")]
    )
    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["deferred_at"] is not None


def test_maintain_cli_auto_defer_blast_cap(tmp_graph):
    # Blast-radius guard: a mass-failure defers at most AUTO_DEFER_BLAST_CAP
    # nodes in one run, and logs the truncation (no silent cap).
    from fno.graph.maintain import AUTO_DEFER_BLAST_CAP

    n = AUTO_DEFER_BLAST_CAP + 2
    nodes = [_ready(f"ab-mass{i:02d}") for i in range(n)]
    _seed(tmp_graph, nodes)
    events: list[dict] = []
    for node in nodes:
        events += [_ev_fail(node["id"])] * 3
    _seed_events(events)

    result = _invoke(["--apply"])
    assert result.exit_code == 0, result.output
    deferred = [e for e in _read(tmp_graph) if e.get("deferred_at")]
    assert len(deferred) == AUTO_DEFER_BLAST_CAP
    assert "blast cap hit" in result.output


# --- triage health "stranded by failed blocker" section (#34, task 2.2) -----

# --all keeps the project-inference filter from dropping these project-null
# fixtures; the stranded section itself is project-agnostic.


def _invoke_health(args: list[str]):
    return runner.invoke(
        app, ["backlog", "triage", "health"] + args, catch_exceptions=False
    )


def test_health_stranded_lists_dependents_of_auto_deferred_blocker(tmp_graph):
    # AC3-UI: each auto-failure-deferred node lists its dependents.
    _seed(
        tmp_graph,
        [
            _node(
                "ab-blk01",
                _status="deferred",
                deferred_at="2026-06-10T00:00:00Z",
                deferred_reason="auto-failure: 3 consecutive failed attempts",
            ),
            _node("ab-dep01", _status="blocked", blocked_by=["ab-blk01"]),
            _node("ab-dep02", _status="blocked", blocked_by=["ab-blk01"]),
        ],
    )
    result = _invoke_health(["--json", "--all"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    section = payload["stranded_by_failed_blocker"]
    assert len(section) == 1
    assert section[0]["blocker"] == "ab-blk01"
    assert {d["id"] for d in section[0]["dependents"]} == {"ab-dep01", "ab-dep02"}
    # AC3-UI: dependents' own _status is unchanged (surfacing only).
    assert all(d["status"] == "blocked" for d in section[0]["dependents"])
    assert payload["totals"]["stranded_by_failed_blocker"] == 2
    # The deferred blocker was not mutated either.
    blk = next(e for e in _read(tmp_graph) if e["id"] == "ab-blk01")
    assert blk["_status"] == "deferred"


def test_health_stranded_ignores_manual_defer(tmp_graph):
    # A hand-deferred blocker (no auto-failure sentinel) is NOT strand-reported.
    _seed(
        tmp_graph,
        [
            _node(
                "ab-blk02",
                _status="deferred",
                deferred_at="2026-06-10T00:00:00Z",
                deferred_reason="parked by hand",
            ),
            _node("ab-dep03", _status="blocked", blocked_by=["ab-blk02"]),
        ],
    )
    result = _invoke_health(["--json", "--all"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.output)["stranded_by_failed_blocker"] == []


def test_health_stranded_section_always_runs(tmp_graph):
    # The section always runs (read-only): an absent entry means "none
    # stranded", never "not checked" - the key is present with an empty list.
    _seed(tmp_graph, [_node("ab-r01", _status="ready", plan_path="p.md")])
    result = _invoke_health(["--json", "--all"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["stranded_by_failed_blocker"] == []
    assert payload["totals"]["stranded_by_failed_blocker"] == 0


# --- end-to-end recovery (#34, task 3.1) -----------------------------------


def test_e2e_undefer_gives_fresh_slate(tmp_graph):
    # AC5-FR: auto-defer, undefer, then a SINGLE fresh failure -> NOT
    # re-deferred. The undefer reset the streak; it needs N fresh failures.
    _seed(tmp_graph, [_ready("ab-rec01")])
    _seed_events([_ev_fail("ab-rec01")] * 3)

    r1 = _invoke(["--apply"])  # 1) auto-defer at threshold
    assert r1.exit_code == 0, r1.output
    assert _read(tmp_graph)[0].get("deferred_at") is not None

    # 2) human recovers it (this emits the node_undeferred reset boundary)
    r2 = runner.invoke(app, ["backlog", "undefer", "ab-rec01"], catch_exceptions=False)
    assert r2.exit_code == 0, r2.output
    assert _read(tmp_graph)[0].get("deferred_at") is None

    _append_events([_ev_fail("ab-rec01")])  # 3) one fresh failure

    r3 = _invoke(["--apply"])  # 4) streak reset -> single failure is below N
    assert r3.exit_code == 0, r3.output
    assert _read(tmp_graph)[0].get("deferred_at") is None  # NOT re-deferred


def test_e2e_blocker_done_auto_readies_dependents(tmp_graph):
    # AC5-FR: when the blocker is fixed and reaches done, its dependents become
    # ready automatically via normal blocked_by resolution (no special code).
    _seed(
        tmp_graph,
        [
            _node("ab-blkE2E"),  # blocker (no plan_path -> done skips the stamp)
            _ready("ab-depE2E", blocked_by=["ab-blkE2E"]),
        ],
    )
    r = runner.invoke(
        app,
        ["backlog", "done", "ab-blkE2E", "--force", "--reason", "e2e recovery test"],
        catch_exceptions=False,
    )
    assert r.exit_code == 0, r.output
    by_id = {e["id"]: e for e in _read(tmp_graph)}
    assert by_id["ab-blkE2E"]["_status"] == "done"
    assert by_id["ab-depE2E"]["_status"] == "ready"  # auto-unblocked


# --- leg 8: validity sweep ----------------------------------------


def _old_idea(node_id: str, age_days: int, **over) -> dict:
    created = (datetime.now(timezone.utc) - timedelta(days=age_days)).isoformat()
    # plan_path None -> idea status; created_at old enough to be eligible.
    return _node(node_id, created_at=created, **over)


@pytest.fixture
def validity_env(tmp_path, monkeypatch):
    """Point deck output at tmp and install a keep-everything analyzer stub."""
    deck_root = tmp_path / "state"
    import fno.paths as p

    monkeypatch.setattr(p, "state_dir", lambda: deck_root)
    stub = tmp_path / "stub.py"
    stub.write_text(
        "#!/usr/bin/env python3\n"
        "import sys, json\n"
        "ctx = json.loads(sys.stdin.read().split('CONTEXT:\\n', 1)[1])\n"
        "print(json.dumps({'results': [\n"
        "  {'node_id': pk['node_id'], 'classification': 'keep',\n"
        "   'confidence': 0.8, 'rationale': 'stub keep', 'evidence_ids': []}\n"
        "  for pk in ctx['packets']]}))\n"
    )
    stub.chmod(0o755)
    monkeypatch.setenv("FNO_VALIDITY_STUB", str(stub))
    return deck_root


def test_validity_ac1_hp_bounded_deck(tmp_graph, validity_env):
    # AC1-HP: >25 ideas older than 60d -> reviews the 25 oldest, writes a deck.
    _seed(tmp_graph, [_old_idea(f"ab-i{i:03d}", 100 + i) for i in range(30)])
    r = _invoke([])
    assert r.exit_code == 0, r.output
    assert "validity: reviewed 25 ideas" in r.output
    decks = list((validity_env / "validity-decks").glob("*.json"))
    assert len(decks) == 1
    side = json.loads(decks[0].read_text())
    assert len(side["rows"]) == 25
    # Oldest-first: ab-i029 (age 129) selected, freshest ab-i000..i004 not.
    reviewed = {row["node_id"] for row in side["rows"]}
    assert "ab-i029" in reviewed and "ab-i000" not in reviewed


def test_validity_ac3_ui_no_eligible(tmp_graph, validity_env):
    # AC3-UI: nothing older than threshold -> clean no-work, no deck.
    _seed(tmp_graph, [_old_idea("ab-fresh", 5)])
    r = _invoke([])
    assert r.exit_code == 0, r.output
    assert "validity: 0 eligible ideas" in r.output
    assert not (validity_env / "validity-decks").exists()


def test_validity_ac2_err_analyzer_failure_degraded_deck(tmp_graph, tmp_path, monkeypatch):
    # AC2-ERR: analyzer fails -> evidence-only deck, all needs-human, degraded.
    deck_root = tmp_path / "state"
    import fno.paths as p

    monkeypatch.setattr(p, "state_dir", lambda: deck_root)
    boom = tmp_path / "boom.sh"
    boom.write_text("#!/usr/bin/env bash\nexit 1\n")
    boom.chmod(0o755)
    monkeypatch.setenv("FNO_VALIDITY_STUB", str(boom))
    _seed(tmp_graph, [_old_idea(f"ab-d{i}", 90 + i) for i in range(3)])
    r = _invoke([])
    assert r.exit_code == 0, r.output
    assert "DEGRADED" in r.output
    side = json.loads(next((deck_root / "validity-decks").glob("*.json")).read_text())
    assert side["degraded"] is True
    assert all(row["classification"] == "needs-human" for row in side["rows"])
    # Degraded rows never watermark -> the same batch retries next sweep.
    assert all(row["watermark"] is False for row in side["rows"])


def test_validity_is_proposal_only_under_apply(tmp_graph, validity_env):
    # US3: --apply must NOT mutate a validity candidate's graph state.
    _seed(tmp_graph, [_old_idea("ab-keepme", 90)])
    before = _read(tmp_graph)
    r = _invoke(["--apply"])
    assert r.exit_code == 0, r.output
    assert _read(tmp_graph) == before  # graph untouched by the validity leg


def test_validity_watermark_then_recheck(tmp_graph, validity_env):
    # AC5-FR: a second sweep skips watermarked ideas; --recheck re-reviews them.
    _seed(tmp_graph, [_old_idea("ab-wm1", 90)])
    assert "reviewed 1 ideas" in _invoke([]).output
    # Second run: same idea is watermarked -> 0 eligible.
    assert "validity: 0 eligible ideas" in _invoke([]).output
    # --recheck bypasses the watermark.
    assert "reviewed 1 ideas" in _invoke(["--recheck"]).output


def test_validity_skipped_with_no_validity_flag(tmp_graph, validity_env):
    _seed(tmp_graph, [_old_idea("ab-skip", 90)])
    r = _invoke(["--no-validity"])
    assert r.exit_code == 0, r.output
    assert "validity:" not in r.output
    assert not (validity_env / "validity-decks").exists()


# --- leg 2b: pr_url backfill ------------------------------------------------


@pytest.fixture
def stub_slugs(monkeypatch):
    """Map a node cwd to a repo slug without shelling out to git/gh."""
    import fno.graph.maintain as gm

    mapping: dict[str, str] = {}
    monkeypatch.setattr(gm, "_slug_from_node_cwd", lambda cwd: mapping.get(cwd))
    return mapping


def test_pr_url_backfill_applies(tmp_graph, stub_slugs):
    stub_slugs["/repo/a"] = "o/a"
    _seed(tmp_graph, [_node("ab-url0001", cwd="/repo/a", pr_number=7)])

    result = _invoke(["--apply"])

    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["pr_url"] == "https://github.com/o/a/pull/7"
    assert "pr-url written 1" in result.output


def test_pr_url_backfill_dry_run_proposes_without_writing(tmp_graph, stub_slugs):
    stub_slugs["/repo/a"] = "o/a"
    _seed(tmp_graph, [_node("ab-url0001", cwd="/repo/a", pr_number=7)])

    result = _invoke([])

    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0].get("pr_url") is None
    assert "pr-url proposed 1" in result.output


def test_pr_url_backfill_reports_unresolvable(tmp_graph, stub_slugs):
    _seed(tmp_graph, [_node("ab-url0002", cwd="/gone", pr_number=7)])

    result = _invoke(["--apply"])

    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0].get("pr_url") is None
    assert "pr-url unresolvable 1" in result.output
    assert "ab-url0002" in result.output


def test_pr_url_backfill_counts_print_when_zero(tmp_graph, stub_slugs):
    """A silent category reads as "nothing to do" when it may be "nothing resolved"."""
    result = _invoke(["--apply"])

    assert "pr-url written 0" in result.output
    assert "pr-url unresolvable 0" in result.output


def test_pr_url_backfill_is_idempotent(tmp_graph, stub_slugs):
    stub_slugs["/repo/a"] = "o/a"
    _seed(tmp_graph, [_node("ab-url0001", cwd="/repo/a", pr_number=7)])

    _invoke(["--apply"])
    second = _invoke(["--apply"])

    assert "pr-url written 0" in second.output
    assert _read(tmp_graph)[0]["pr_url"] == "https://github.com/o/a/pull/7"


def test_pr_url_backfill_never_overwrites_a_present_url(tmp_graph, stub_slugs):
    stub_slugs["/repo/a"] = "o/a"
    _seed(tmp_graph, [_node(
        "ab-url0003", cwd="/repo/a", pr_number=7,
        pr_url="https://github.com/other/repo/pull/7",
    )])

    _invoke(["--apply"])

    assert _read(tmp_graph)[0]["pr_url"] == "https://github.com/other/repo/pull/7"


def test_pr_url_backfill_keys_off_each_nodes_own_cwd(tmp_graph, stub_slugs):
    """No node may end with a url derived from a different node's cwd."""
    stub_slugs.update({"/repo/a": "o/a", "/repo/b": "o/b"})
    _seed(tmp_graph, [
        _node("ab-url0001", cwd="/repo/a", pr_number=7),
        _node("ab-url0002", cwd="/repo/b", pr_number=7),
    ])

    _invoke(["--apply"])

    urls = {n["id"]: n["pr_url"] for n in _read(tmp_graph)}
    assert urls == {
        "ab-url0001": "https://github.com/o/a/pull/7",
        "ab-url0002": "https://github.com/o/b/pull/7",
    }
