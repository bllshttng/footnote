"""CLI-wiring coverage for `fno observer` (x-57a5, Task 7).

Sweep and replay are exercised with injected seams (fake corpus, fake gh, fake
spawn, no real worktree) so no test touches the network, real ~/.fno state, or
spawns a session. Asserts the anti-silent contract (a state word + the right
events), the insufficient guard (no run_complete), replay's A2 rejection, the
replay tool-fault path, and the one hard failure: an isolation violation.
"""

from __future__ import annotations

import json

import pytest
import typer
from typer.testing import CliRunner

from fno.observer import cli

runner = CliRunner()


def _item(session_id, node_id, plan_path):
    return {
        "session_id": session_id,
        "graph_node_id": node_id,
        "plan_path": str(plan_path) if plan_path else None,
        "skill_id": "fno:blueprint",
        "skill_version": "abc1234",
        "method": "phase-proxy",
        "shipped": True,
        "termination_reason": "DonePRGreen",
        "judgeable": False,
        "outcome": None,
        "attribution_class": None,
    }


def _wire(monkeypatch, tmp_path, items):
    """Redirect events + digest into tmp; inject the corpus."""
    events_path = tmp_path / "events.jsonl"
    monkeypatch.setattr(cli, "_events_paths", lambda: [events_path])
    monkeypatch.setattr(cli, "_load_corpus", lambda skill, since: ({"items": items, "attributed": len(items)}, {}))
    import fno.paths as paths
    monkeypatch.setattr(paths, "observer_reports_dir", lambda *a, **k: tmp_path / "reports")
    return events_path


def _events(path):
    if not path.exists():
        return []
    return [json.loads(ln) for ln in path.read_text().splitlines() if ln.strip()]


def _good_plan(tmp_path, name):
    p = tmp_path / name
    p.write_text(
        "# Plan\n\n## Failure Modes\n\nboundaries\n\n"
        "## Execution Strategy\n\n```yaml\ntasks:\n- id: '1'\n  surface: ['a.py']\n```\n"
    )
    return p


# --------------------------------------------------------------------------- #
# help + A2 rejection
# --------------------------------------------------------------------------- #

def test_digest_dir_keyed_to_skill_owner(monkeypatch, tmp_path):
    """The digest lands under the skill's owning project (its skill-id
    namespace), not the ambient checkout - so a sweep fired from a worktree
    or a sibling repo never spawns an internal/<basename>/ folder."""
    import fno.paths as paths

    seen = {}

    def _capture(project_root=None, project_id=None):
        seen["project_id"] = project_id
        return tmp_path

    monkeypatch.setattr(paths, "observer_reports_dir", _capture)
    summary = {
        "skill_id": "fno:blueprint", "run_id": "obs-x", "coverage_pct": 100,
        "corpus_size": 1, "pass_count": 1, "degraded_count": 0, "fail_count": 0,
        "failure_ranking": [],
    }
    cli._write_digest(summary, "blueprint", mode="sweep")
    assert seen["project_id"] == "fno"


def test_observer_dir_rejects_traversal_project_id(monkeypatch, tmp_path):
    """A caller-supplied project_id carrying path separators / traversal must
    not escape internal/<project>/ - it falls back to the configured id."""
    import fno.paths as paths

    class _S:
        class paths:
            observer_reports_dir = None

        class project:
            id = "safeproj"

        class obsidian:
            enabled = False
            vault = None

    monkeypatch.setattr(paths, "_settings", lambda: _S())
    monkeypatch.setattr(paths, "state_dir", lambda: tmp_path)
    d = paths.observer_reports_dir(project_id="../../etc")
    assert ".." not in str(d)
    assert d == tmp_path / "observer-reports" / "safeproj"


def test_help_lists_sweep_and_replay():
    r = runner.invoke(cli.observer_app, ["--help"])
    assert r.exit_code == 0
    assert "sweep" in r.output and "replay" in r.output


def test_replay_review_rejected_nonzero():
    r = runner.invoke(cli.observer_app, ["replay", "--skill", "review", "--corpus-item", "s1"])
    assert r.exit_code != 0
    assert "A2" in r.output


# --------------------------------------------------------------------------- #
# sweep
# --------------------------------------------------------------------------- #

def test_sweep_insufficient_emits_no_run_complete(monkeypatch, tmp_path):
    items = [_item(f"s{i}", f"x-{i}", _good_plan(tmp_path, f"p{i}.md")) for i in range(5)]
    events_path = _wire(monkeypatch, tmp_path, items)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint"])
    assert r.exit_code == 0
    assert "insufficient" in r.output and "5" in r.output
    # AC1-UI: nothing to complete -> no terminal event
    assert not any(e["type"] == "skill_eval_run_complete" for e in _events(events_path))


def test_sweep_ok_emits_findings_run_complete_and_digest(monkeypatch, tmp_path):
    items = [_item(f"s{i}", f"x-{i}", _good_plan(tmp_path, f"p{i}.md")) for i in range(10)]
    events_path = _wire(monkeypatch, tmp_path, items)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint"])
    assert r.exit_code == 0, r.output
    assert r.output.startswith("ok:")
    evs = _events(events_path)
    findings = [e for e in evs if e["type"] == "skill_eval_finding"]
    completes = [e for e in evs if e["type"] == "skill_eval_run_complete"]
    assert len(completes) == 1
    assert completes[0]["data"]["coverage_pct"] == 100
    assert findings and all(f["data"]["verdict"] in ("pass", "degraded", "fail") for f in findings)
    # digest written
    assert list((tmp_path / "reports").glob("blueprint-*.md"))


def test_sweep_partial_when_an_item_is_unscorable(monkeypatch, tmp_path):
    items = [_item(f"s{i}", f"x-{i}", _good_plan(tmp_path, f"p{i}.md")) for i in range(9)]
    items.append(_item("s9", "x-9", tmp_path / "missing.md"))  # no plan on disk, no node -> gap
    _wire(monkeypatch, tmp_path, items)  # side effects only (monkeypatch); events unread here
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint", "--json"])
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert payload["state"] == "partial"
    assert payload["coverage_pct"] == 90  # 9 of 10 scored


# --------------------------------------------------------------------------- #
# gh fan-out cap (x-dbdf US4 / AC2-EDGE)
# --------------------------------------------------------------------------- #

def test_capped_gh_bounds_invocations_to_cap():
    """A sweep with more gh-needing nodes than the cap fetches exactly `cap`
    times; every call past the cap short-circuits to a gh-unavailable result
    (which the caller degrades to a coverage gap, not a crash)."""
    calls: list[list[str]] = []

    def fake_gh(args):
        calls.append(args)
        return 0, "diff", ""

    capped = cli._capped_gh(fake_gh, cap=10)
    results = [capped(["pr", "diff", str(n)]) for n in range(15)]

    assert len(calls) == 10  # exactly the cap reached the real runner
    assert all(rc == 0 for rc, _o, _e in results[:10])
    assert all(rc == 1 and "cap reached" in err for rc, _o, err in results[10:])


def test_sweep_wires_the_gh_cap(monkeypatch, tmp_path):
    """End-to-end: 15 nodes with no on-disk plan (all fall to gh) under a cap of
    3 -> the real gh runner is invoked at most 3 times for the whole run."""
    monkeypatch.setenv("FNO_OBSERVER_GH_CAP", "3")
    import importlib
    importlib.reload(cli)

    calls = {"n": 0}

    def counting_gh(args):
        calls["n"] += 1
        return 0, "# Plan\n\n## Failure Modes\n\nx\n\n## Execution Strategy\n\n```yaml\ntasks: []\n```\n", ""

    monkeypatch.setattr(cli, "_default_gh", counting_gh)
    items = [_item(f"s{i}", f"x-{i}", None) for i in range(15)]
    by_id = {f"x-{i}": {"pr_number": 100 + i, "pr_url": f"https://github.com/o/r/pull/{100 + i}"} for i in range(15)}
    events_path = tmp_path / "events.jsonl"
    monkeypatch.setattr(cli, "_events_paths", lambda: [events_path])
    monkeypatch.setattr(cli, "_load_corpus", lambda skill, since: ({"items": items, "attributed": len(items)}, by_id))
    import fno.paths as paths
    monkeypatch.setattr(paths, "observer_reports_dir", lambda *a, **k: tmp_path / "reports")

    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint"])
    assert r.exit_code == 0, r.output
    assert calls["n"] == 3  # cap held across the whole sweep

    monkeypatch.delenv("FNO_OBSERVER_GH_CAP", raising=False)
    importlib.reload(cli)


# --------------------------------------------------------------------------- #
# target sweep (x-6ff0)
# --------------------------------------------------------------------------- #

def _wire_target(monkeypatch, tmp_path, corpus, meta=None):
    """Inject the PR-anchored corpus + redirect events/digest into tmp."""
    events_path = tmp_path / "events.jsonl"
    monkeypatch.setattr(cli, "_events_paths", lambda: [events_path])
    monkeypatch.setattr(cli, "_load_target_corpus", lambda since, repo, gh: (corpus, meta or {"repos_scoped": 1, "dropped_repos": 0, "truncated_repos": []}))
    import fno.paths as paths
    monkeypatch.setattr(paths, "observer_reports_dir", lambda *a, **k: tmp_path / "reports")
    return events_path


def _target_item(pr_number, node_id, *, outcome="merged_clean", merged=True, ci_reds=0, loop_fires=1, terminal="DonePRGreen"):
    return {
        "pr_number": pr_number, "pr_url": f"https://github.com/o/r/pull/{pr_number}", "repo": "o/r",
        "merged": merged, "graph_node_id": node_id, "session_ids": [f"s{pr_number}"],
        "signals": {"promises": 1, "loop_fires": loop_fires, "ci_reds": ci_reds,
                    "terminal_reasons": [terminal] if terminal else [], "tokens_total": None,
                    "duration_minutes": None, "turns": None, "toolcalls": None},
        "terminal_reasons": [terminal] if terminal else [],
        "judgeable": merged and outcome is not None, "outcome": outcome, "attribution_class": None,
    }


def _target_corpus(items, *, unattributed=0, no_pr=0, stop_cause=None, prs_total=None):
    n = prs_total if prs_total is not None else len(items) + unattributed
    return {
        "items": items,
        "unattributed": [{"pr_number": 900 + i, "pr_url": None, "repo": None, "merged": True, "graph_node_id": None} for i in range(unattributed)],
        "no_pr": [{"session_ids": [f"np{i}"], "termination_reason": "NoProgress", "graph_node_id": None} for i in range(no_pr)],
        "coverage": {"prs_total": n, "attributed": len(items), "unattributed_pr": unattributed,
                     "no_pr_attempts": no_pr, "no_pr_stop_cause": stop_cause or ({"NoProgress": no_pr} if no_pr else {})},
    }


def test_target_no_data_emits_no_run_complete(monkeypatch, tmp_path):
    corpus = _target_corpus([], prs_total=0)
    events_path = _wire_target(monkeypatch, tmp_path, corpus)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "target"])
    assert r.exit_code == 0
    assert "no_data" in r.output
    assert not any(e["type"] == "skill_eval_run_complete" for e in _events(events_path))


def test_target_insufficient_below_ten(monkeypatch, tmp_path):
    items = [_target_item(i, f"x-{i}") for i in range(5)]
    events_path = _wire_target(monkeypatch, tmp_path, _target_corpus(items))
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "target"])
    assert r.exit_code == 0
    assert "insufficient" in r.output and "5" in r.output
    assert not any(e["type"] == "skill_eval_run_complete" for e in _events(events_path))


def test_target_ok_emits_findings_run_complete_digest(monkeypatch, tmp_path):
    items = [_target_item(i, f"x-{i}") for i in range(10)]
    events_path = _wire_target(monkeypatch, tmp_path, _target_corpus(items, no_pr=3))
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "target"])
    assert r.exit_code == 0, r.output
    assert r.output.startswith("ok:")
    evs = _events(events_path)
    assert len([e for e in evs if e["type"] == "skill_eval_run_complete"]) == 1
    findings = [e for e in evs if e["type"] == "skill_eval_finding"]
    assert findings and all(f["data"]["skill_id"] == "fno:target" for f in findings)
    assert all(f["data"]["corpus_item_id"].startswith("pr-") for f in findings)
    digest = next((tmp_path / "reports").glob("target-*.md")).read_text()
    # AC3-HP: unconditional no_pr line + attempt->PR formula
    assert "no_pr_attempts: 3" in digest
    assert "attempt -> PR:" in digest
    # AC2-FR: the cross-tab renders in the summary block (above the failure ranking)
    assert digest.index("Falsifier cross-tab") < digest.index("Failure ranking")


def test_target_no_pr_line_renders_even_at_zero(monkeypatch, tmp_path):
    """AC3-HP bypass guard: the no_pr line + formula print even when count is 0."""
    items = [_target_item(i, f"x-{i}") for i in range(10)]
    _wire_target(monkeypatch, tmp_path, _target_corpus(items, no_pr=0))
    runner.invoke(cli.observer_app, ["sweep", "--skill", "target"])
    digest = next((tmp_path / "reports").glob("target-*.md")).read_text()
    assert "no_pr_attempts: 0" in digest
    assert "attempt -> PR: 100%" in digest  # 10 PRs / 10 attempts


def test_target_falsifier_cell_cited(monkeypatch, tmp_path):
    """AC2-FR: a good-process PR that bounced surfaces as a falsifier, not
    collapsed into the headline pass counts."""
    items = [_target_item(i, f"x-{i}") for i in range(9)]
    # one PR: clean process (0 ci_reds, ship terminal) but bounced outcome
    items.append(_target_item(99, "x-bad", outcome="bounced", ci_reds=0, terminal="DonePRGreen"))
    _wire_target(monkeypatch, tmp_path, _target_corpus(items))
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "target", "--json"])
    payload = json.loads(r.output)
    assert 99 in payload["falsifier_prs"]
    digest = next((tmp_path / "reports").glob("target-*.md")).read_text()
    assert "FALSIFIER" in digest and "#99" in digest


def test_target_partial_when_unattributed_present(monkeypatch, tmp_path):
    items = [_target_item(i, f"x-{i}") for i in range(10)]
    _wire_target(monkeypatch, tmp_path, _target_corpus(items, unattributed=2))
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "target", "--json"])
    payload = json.loads(r.output)
    assert payload["state"] == "partial"
    digest = next((tmp_path / "reports").glob("target-*.md")).read_text()
    assert "unattributed PR" in digest  # AC1-EDGE surfaced as a coverage gap


def test_target_fatal_when_canonical_event_unwritable(monkeypatch, tmp_path):
    items = [_target_item(i, f"x-{i}") for i in range(10)]
    _wire_target(monkeypatch, tmp_path, _target_corpus(items))
    monkeypatch.setattr(cli, "_emit_run_complete", lambda summary, paths: False)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "target"])
    assert r.exit_code == 5
    assert "not recorded" in r.output
    assert not list((tmp_path / "reports").glob("target-*.md"))


def test_target_gh_list_windows_and_flags_truncation():
    """_gh_pr_list windows by merged/closed date and flags a repo that returned
    exactly the list limit (possible more PRs)."""
    from datetime import datetime as dt
    now = dt(2026, 7, 18, 12, 0, 0)
    cutoff = now - __import__("datetime").timedelta(days=28)
    in_win = {"number": 1, "headRefName": "feature/x-1", "mergedAt": "2026-07-10T10:00:00Z", "closedAt": None, "url": "https://github.com/o/r/pull/1", "state": "MERGED"}
    out_win = {"number": 2, "headRefName": "feature/x-2", "mergedAt": "2026-01-01T10:00:00Z", "closedAt": None, "url": "https://github.com/o/r/pull/2", "state": "MERGED"}

    def fake_gh(args):
        return 0, json.dumps([in_win, out_win]), ""

    prs, truncated = cli._gh_pr_list("o/r", cutoff, now, fake_gh)
    assert [p["number"] for p in prs] == [1]  # out-of-window dropped
    assert truncated is False


def test_target_gh_failure_is_repo_coverage_gap():
    """AC1-ERR: a gh failure for a repo yields None (a coverage gap), never a crash."""
    from datetime import datetime as dt
    now = dt(2026, 7, 18, 12, 0, 0)
    prs, truncated = cli._gh_pr_list("o/r", now, now, lambda args: (1, "", "rate limited"))
    assert prs is None


# --------------------------------------------------------------------------- #
# replay
# --------------------------------------------------------------------------- #

def _wire_replay(monkeypatch, tmp_path, item):
    events_path = _wire(monkeypatch, tmp_path, [item])
    monkeypatch.setattr(cli, "_read_plan_text", lambda *a, **k: "# Doc\n\nbuild a widget\n")
    import fno.claims.core as claims
    monkeypatch.setattr(claims, "acquire_claim", lambda **k: object())
    monkeypatch.setattr(claims, "release_claim", lambda *a, **k: None)
    return events_path


def test_replay_happy_path_emits_tagged_finding(monkeypatch, tmp_path):
    item = _item("s-rep", "x-rep", None)
    events_path = _wire_replay(monkeypatch, tmp_path, item)
    fresh = "## Failure Modes\nx\n\n## Execution Strategy\n```yaml\ntasks:\n- id: '1'\n  surface: ['a.py']\n```\n"
    cli._replay(
        skill="blueprint", corpus_item="s-rep", skill_ref="cand-branch",
        run_id="obs-fno:blueprint-test", since=90,
        spawn=lambda name, prompt, *, cwd, timeout: (0, fresh, ""),
        run_worktree=False,
    )
    evs = _events(events_path)
    findings = [e for e in evs if e["type"] == "skill_eval_finding"]
    assert findings and all(f["data"].get("skill_ref") == "cand-branch" for f in findings)
    # A1: replay never emits shipped_outcome
    assert all(f["data"]["dimension"] != "shipped_outcome" for f in findings)
    completes = [e for e in evs if e["type"] == "skill_eval_run_complete"]
    assert completes and completes[0]["data"].get("skill_ref") == "cand-branch"
    # truthful single-item batch numbers (not a padded corpus_size=10)
    assert completes[0]["data"]["corpus_size"] == 1
    assert completes[0]["data"]["coverage_pct"] == 100


def test_already_scored_tolerates_non_dict_json_lines(tmp_path):
    # A bare scalar/array line must not AttributeError on e.get("type").
    events = tmp_path / "events.jsonl"
    events.write_text(
        "123\n"
        '"a string"\n'
        "[1, 2]\n"
        + json.dumps({"type": "skill_eval_finding", "data": {"run_id": "r1", "corpus_item_id": "s1", "skill_ref": None}})
        + "\n"
    )
    assert cli._already_scored("r1", None, "s1", [events]) is True
    assert cli._already_scored("r1", None, "other", [events]) is False


def test_replay_concurrent_holder_refuses_without_stomping(monkeypatch, tmp_path):
    item = _item("s-rep", "x-rep", None)
    _wire_replay(monkeypatch, tmp_path, item)
    import fno.claims.core as claims
    def _held(**k):
        raise claims.ClaimHeldByOther(holder="peer:999", pid=999, host="h", key=k.get("key", "obs"))
    monkeypatch.setattr(claims, "acquire_claim", _held)
    with pytest.raises(typer.Exit) as exc:
        cli._replay(
            skill="blueprint", corpus_item="s-rep", skill_ref=None,
            run_id="obs-fno:blueprint-conc", since=90,
            spawn=lambda *a, **k: (0, "x", ""), run_worktree=False,
        )
    assert exc.value.exit_code == 4  # refused cleanly, no worktree touched


def test_replay_spawn_failure_is_tool_fault(monkeypatch, tmp_path):
    item = _item("s-rep", "x-rep", None)
    events_path = _wire_replay(monkeypatch, tmp_path, item)
    with pytest.raises(typer.Exit) as exc:
        cli._replay(
            skill="blueprint", corpus_item="s-rep", skill_ref=None,
            run_id="obs-fno:blueprint-tf", since=90,
            spawn=lambda name, prompt, *, cwd, timeout: (1, "", "boom"),
            run_worktree=False,
        )
    assert exc.value.exit_code == 1
    findings = [e for e in _events(events_path) if e["type"] == "skill_eval_finding"]
    assert findings and "tool-fault" in findings[0]["data"]["evidence"]
    # machine-readable marker: never conflated with a skill-quality fail (AC2-ERR)
    assert findings[0]["data"].get("tool_fault") is True


def test_sweep_fatal_when_canonical_event_unwritable(monkeypatch, tmp_path):
    # codex P2: the CLI must not report ok/write a digest when the canonical
    # skill_eval_run_complete could not be recorded.
    items = [_item(f"s{i}", f"x-{i}", _good_plan(tmp_path, f"p{i}.md")) for i in range(10)]
    _wire(monkeypatch, tmp_path, items)
    monkeypatch.setattr(cli, "_emit_run_complete", lambda summary, paths: False)
    r = runner.invoke(cli.observer_app, ["sweep", "--skill", "blueprint"])
    assert r.exit_code == 5
    assert "not recorded" in r.output
    assert not list((tmp_path / "reports").glob("*.md"))  # no digest on failure


def test_replay_isolation_violation_hard_fails(monkeypatch, tmp_path):
    item = _item("s-rep", "x-rep", None)
    events_path = _wire_replay(monkeypatch, tmp_path, item)
    monkeypatch.setattr(cli, "_write_workdir_settings", lambda wd: None)

    class _P:
        returncode, stdout, stderr = 0, "", ""

    monkeypatch.setattr(cli.subprocess, "run", lambda *a, **k: _P())
    from fno.observer import isolation
    monkeypatch.setattr(isolation, "collect_eval_session_ids", lambda wd, **k: ({"leak"}, None))
    monkeypatch.setattr(
        isolation, "check_isolation",
        lambda ids, paths: isolation.IsolationResult(
            verdict="violated",
            violations=[isolation.Violation(path=tmp_path / "ledger.json", session_id="leak", line_number=1, detail="x")],
        ),
    )
    fresh = "## Failure Modes\nx\n\n## Execution Strategy\n```yaml\ntasks:\n- id: '1'\n  surface: ['a.py']\n```\n"
    with pytest.raises(typer.Exit) as exc:
        cli._replay(
            skill="blueprint", corpus_item="s-rep", skill_ref=None,
            run_id="obs-fno:blueprint-iso", since=90,
            spawn=lambda name, prompt, *, cwd, timeout: (0, fresh, ""),
            run_worktree=True,
        )
    assert exc.value.exit_code == 3  # the ONE non-advisory failure
    # codex P1: the scan runs BEFORE any emit, so a violated run voids all output
    assert not any(e["type"] == "skill_eval_run_complete" for e in _events(events_path))
    assert not any(e["type"] == "skill_eval_finding" for e in _events(events_path))
