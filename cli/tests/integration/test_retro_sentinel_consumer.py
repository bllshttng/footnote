"""Integration tests for the retro-sentinel consumer (Wave 4.2, US6)."""
from __future__ import annotations

import json
from pathlib import Path

from fno.retro.cli import process_sentinel_file


def _write_sentinel(tmp_path: Path, node_id="ab-12345678", pr=343, mode=None) -> Path:
    payload = {
        "node_id": node_id,
        "pr_number": pr,
        "pr_url": f"https://github.com/o/r/pull/{pr}",
        "merged_at": "2026-05-24T00:00:00Z",
        "plan_path": None,
        "closed_by": "backlog-reconcile",
        "closed_at": "2026-05-24T00:01:00Z",
    }
    if mode:
        payload["mode"] = mode
    sd = tmp_path / "retro-pending"
    sd.mkdir(parents=True, exist_ok=True)
    p = sd / f"{node_id}.json"
    p.write_text(json.dumps(payload), encoding="utf-8")
    return p


class _Rec:
    def __init__(self):
        self.created, self.queued, self.inbox = [], [], []
        self._n = 0

    def create(self, **kw):
        self._n += 1
        nid = f"ab-new{self._n}"
        self.created.append({"id": nid, **kw})
        return nid

    def inbox_append(self, candidate):
        self.inbox.append(candidate)


def test_ac6_edge_sentinel_consumed_and_removed(tmp_path: Path):
    """AC6-EDGE: a reconcile retro sentinel is consumed; a node is filed; sentinel removed."""
    sentinel = _write_sentinel(tmp_path)
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] real declined finding", "reviewer": "gemini[bot]"}]
    report, removed = process_sentinel_file(
        sentinel,
        repo_root=tmp_path,
        existing_nodes=[],
        comments=comments,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    assert len(rec.created) == 1  # node filed from the declined finding
    # default mode (no mode in sentinel) is interactive -> queued
    assert rec.created[0].get("queued") is True
    assert removed is True
    assert not sentinel.exists()


def test_partial_harvest_retains_sentinel(tmp_path: Path):
    """A gh failure RETAINS the sentinel for retry. The unreadable resolved
    state is caught at the reviewThreads fetch (before any review harvest), so
    no review nodes are filed and the sentinel is kept."""
    sentinel = _write_sentinel(tmp_path)
    rec = _Rec()

    def failing_gh(args):
        return 1, "", "HTTP 403 rate limit"

    report, removed = process_sentinel_file(
        sentinel,
        repo_root=tmp_path,
        existing_nodes=[],
        comments=None,           # force a gh fetch
        gh_runner=failing_gh,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    assert rec.created == []   # review harvest skipped (resolved state unknown)
    assert removed is False
    assert sentinel.exists()  # retained for retry


def test_land_failure_retains_sentinel(tmp_path: Path):
    """A land failure RETAINS the sentinel (crash mid-run re-triages)."""
    sentinel = _write_sentinel(tmp_path)

    def boom(**kw):
        raise TimeoutError("lock timeout")

    comments = [{"id": "c1", "body": "![high] x", "reviewer": "gemini[bot]"}]
    report, removed = process_sentinel_file(
        sentinel,
        repo_root=tmp_path,
        existing_nodes=[],
        comments=comments,
        create_fn=boom,
        inbox_fn=lambda s: None,
    )
    assert report.failed is True
    assert removed is False
    assert sentinel.exists()


def test_dedup_against_existing_nodes(tmp_path: Path):
    """AC6-FR/AC5-HP: a finding already filed (trailer on a live node) is not re-created."""
    from fno.retro.dedup import content_hash, trailer

    sentinel = _write_sentinel(tmp_path)
    rec = _Rec()
    body = "![high] already filed finding"
    # The live node carries the trailer for this finding's hash on PR 343.
    h = content_hash(body)
    existing = [{"id": "ab-old", "details": f"x\n\n{trailer(343, h)}"}]
    comments = [{"id": "c1", "body": body, "reviewer": "gemini[bot]"}]
    report, removed = process_sentinel_file(
        sentinel,
        repo_root=tmp_path,
        existing_nodes=existing,
        comments=comments,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    assert rec.created == []          # deduped: nothing new filed
    assert report.skipped_dupes == 1
    assert removed is True            # clean run, sentinel consumed


def test_fast_path_triage_pending_shape(tmp_path: Path):
    """Wave 4.3: the .triage-pending fast-path file uses the same sentinel shape/routine."""
    abil = tmp_path / ".fno"
    abil.mkdir()
    tp = abil / ".triage-pending"
    tp.write_text(
        json.dumps(
            {"pr_number": 50, "pr_url": "https://github.com/o/r/pull/50",
             "mode": "autonomous", "plan_path": ""}
        ),
        encoding="utf-8",
    )
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] declined finding", "reviewer": "gemini[bot]"}]
    report, removed = process_sentinel_file(
        tp, repo_root=tmp_path, existing_nodes=[], comments=comments,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert len(rec.created) == 1
    assert rec.created[0].get("queued") is False  # mode autonomous -> active
    assert removed is True
    assert not tp.exists()


def test_ac2_ui_ac4_ui_operator_feedback_strings(tmp_path: Path, capsys):
    """AC2-UI + AC4-UI: source-count line (stderr) + queued line + pick hint (stdout)."""
    sentinel = _write_sentinel(tmp_path)  # no mode -> interactive (queued)
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] a real finding", "reviewer": "gemini[bot]"}]
    process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=comments,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    out = capsys.readouterr()
    # AC2-UI: a source-count line naming each source.
    assert "carve-outs=" in out.err and "reviews=1" in out.err
    # AC4-UI: queued line + closing pick hint on stdout.
    assert "queued" in out.out and "for review" in out.out
    assert "fno backlog pick" in out.out


def test_autonomous_mode_creates_active(tmp_path: Path):
    """A sentinel carrying mode=autonomous lands active (no queue)."""
    sentinel = _write_sentinel(tmp_path, mode="autonomous")
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] x", "reviewer": "gemini[bot]"}]
    report, removed = process_sentinel_file(
        sentinel,
        repo_root=tmp_path,
        existing_nodes=[],
        comments=comments,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    assert len(rec.created) == 1
    assert rec.created[0].get("queued") is False  # autonomous -> active
    assert removed is True


# --- derive resolved/skipped from real PR data (ab-bb7fa74f) ---------------

import json as _json


def _dispatch_gh(*, resolved_ids, inline, issue_comments="[[]]"):
    """A gh runner that answers graphql / inline-comments / issue-comments calls.

    `resolved_ids` -> databaseIds returned in a single RESOLVED reviewThread.
    `inline` -> the REST pulls/N/comments payload (list of comment dicts).
    """
    threads = [
        {
            "isResolved": True,
            "path": "src/x.py",
            "comments": {"nodes": [{"databaseId": int(i), "body": "b"} for i in resolved_ids]},
        }
    ]
    graphql = _json.dumps(
        {"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": threads}}}}}
    )

    def gh(args):
        if args[:2] == ["api", "graphql"]:
            return 0, graphql, ""
        if "issues" in (args[1] if len(args) > 1 else ""):
            return 0, issue_comments, ""
        if "pulls" in (args[1] if len(args) > 1 else ""):
            return 0, _json.dumps(inline), ""
        return 0, "[]", ""

    return gh


def test_resolved_findings_not_refiled(tmp_path: Path):
    """ab-bb7fa74f: an IMPLEMENTED (resolved-thread) finding is NOT filed; a
    declined one IS. Proves resolved_ids is derived from real PR data."""
    sentinel = _write_sentinel(tmp_path, mode="autonomous")
    rec = _Rec()
    inline = [
        {"id": 111, "body": "![high] implemented later", "html_url": "u1",
         "user": {"login": "gemini[bot]"}},
        {"id": 222, "body": "![high] genuinely declined", "html_url": "u2",
         "user": {"login": "gemini[bot]"}},
    ]
    gh = _dispatch_gh(resolved_ids=["111"], inline=inline)
    report, removed = process_sentinel_file(
        sentinel,
        repo_root=tmp_path,
        existing_nodes=[],
        comments=None,            # force live derivation
        gh_runner=gh,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    assert report.partial is False
    assert len(rec.created) == 1                       # only the declined one
    assert "declined" in rec.created[0]["details"].lower() or \
           "declined" in (rec.created[0].get("title", "").lower())
    assert report.source_counts["reviews"] == 1        # 111 filtered out
    assert removed is True


def test_resolved_fetch_unavailable_retains_sentinel(tmp_path: Path):
    """If the resolved-thread state can't be read, retain the sentinel (do not
    risk re-filing implemented findings)."""
    sentinel = _write_sentinel(tmp_path, mode="autonomous")
    rec = _Rec()

    def gh(args):
        if args[:2] == ["api", "graphql"]:
            return 1, "", "HTTP 502"        # resolved state unreadable
        return 0, "[]", ""                  # inline/issue empty but ok

    report, removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=None,
        gh_runner=gh, create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert removed is False
    assert sentinel.exists()


def _skipped_reply(path):
    return (
        "## Code Review Response\n\n### Skipped\n\n"
        "| Reviewer | File | Issue | Reason |\n|---|---|---|---|\n"
        f"| Gemini | `{path}` | MEDIUM: thing | out of scope |\n"
    )


def test_ac2_fr_discrepancy_warning_end_to_end(tmp_path: Path):
    """A finding both resolved (fix evidence) AND in the author Skipped table
    surfaces the AC2-FR discrepancy warning through live derivation."""
    sentinel = _write_sentinel(tmp_path, mode="autonomous")
    rec = _Rec()
    inline = [{"id": 111, "body": "![high] thing", "html_url": "u",
               "user": {"login": "gemini[bot]"}}]
    issue_comments = _json.dumps([[{"body": _skipped_reply("src/x.py")}]])
    gh = _dispatch_gh(resolved_ids=["111"], inline=inline, issue_comments=issue_comments)
    report, removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=None,
        gh_runner=gh, create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert rec.created == []                       # 111 resolved -> not filed
    assert any("fix evidence wins" in w for w in report.warnings)
    assert removed is True


def test_skipped_fetch_failure_does_not_retain(tmp_path: Path):
    """GraphQL succeeds but the Skipped-table fetch fails: the cross-check is
    cosmetic, so the sentinel is still consumed (asymmetry vs resolved state)."""
    sentinel = _write_sentinel(tmp_path, mode="autonomous")
    rec = _Rec()
    threads = _json.dumps({"data": {"repository": {"pullRequest": {"reviewThreads": {
        "pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": []}}}}})
    inline = _json.dumps([{"id": 222, "body": "![high] declined", "html_url": "u",
                           "user": {"login": "gemini[bot]"}}])

    def gh(args):
        if args[:2] == ["api", "graphql"]:
            return 0, threads, ""
        if "issues" in (args[1] if len(args) > 1 else ""):
            return 1, "", "HTTP 500"           # skipped-table fetch fails
        return 0, inline, ""

    report, removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=None,
        gh_runner=gh, create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert len(rec.created) == 1                   # declined finding still filed
    assert removed is True                         # NOT retained for cosmetic miss


def test_missing_pr_url_retains_without_crash(tmp_path: Path):
    """A sentinel with no pr_url -> unresolvable repo -> resolved state
    unavailable -> retained, not crashed, not re-filed."""
    sd = tmp_path / "retro-pending"
    sd.mkdir(parents=True, exist_ok=True)
    p = sd / "ab-nourl.json"
    p.write_text(_json.dumps({"pr_number": 7, "mode": "autonomous"}), encoding="utf-8")
    rec = _Rec()
    report, removed = process_sentinel_file(
        p, repo_root=tmp_path, existing_nodes=[], comments=None,
        gh_runner=lambda a: (0, "[]", ""), create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert removed is False
    assert p.exists()


def test_graphql_down_rest_up_files_no_review_nodes(tmp_path: Path):
    """Codex P1: GraphQL (resolved state) fails but REST inline comments
    succeed. We must NOT harvest reviews (can't tell implemented from
    declined), so zero review nodes are filed and the sentinel is retained."""
    sentinel = _write_sentinel(tmp_path, mode="autonomous")
    rec = _Rec()
    inline = _json.dumps([{"id": 222, "body": "![high] would-be candidate",
                           "html_url": "u", "user": {"login": "gemini[bot]"}}])

    def gh(args):
        if args[:2] == ["api", "graphql"]:
            return 1, "", "HTTP 403 (GraphQL permission)"   # resolved unreadable
        return 0, inline, ""                                # REST still works

    report, removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=None,
        gh_runner=gh, create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert rec.created == []                # no false re-file despite REST up
    assert report.source_counts["reviews"] == 0
    assert removed is False and sentinel.exists()


# -- ab-44408b6e (holes #2 + #3) + ab-d4e8f852: synthetic --pr harvest, the
#    canonical carveout_root, and a non-silent consume. --

def _write_carveout(root: Path, cv_id: str, session_id: str = "sX") -> Path:
    """Seed a one-line carveouts.jsonl under `root`/.fno/."""
    ledger = root / ".fno" / "carveouts.jsonl"
    ledger.parent.mkdir(parents=True, exist_ok=True)
    ledger.write_text(
        json.dumps(
            {
                "id": cv_id,
                "kind": "deferred",
                "description": "left SSO wiring undone",
                "session_id": session_id,
                "need": "which auth provider",
            }
        )
        + "\n",
        encoding="utf-8",
    )
    return ledger


def test_pr_payload_harvests_and_consumes_carveouts(tmp_path: Path):
    """A synthetic --pr payload (no sentinel) harvests carve-outs from
    carveout_root and consumes them on a clean run (ab-44408b6e holes #2/#3)."""
    from fno.retro.cli import _process_payload

    ledger = _write_carveout(tmp_path, "cv-aaa")
    rec = _Rec()
    payload = {"pr_number": 500, "session_ids": ["sX"], "mode": "autonomous"}
    report, clean = _process_payload(
        payload,
        repo_root=tmp_path,
        existing_nodes=[],
        comments=[],  # skip the gh review-derivation path
        carveout_root=tmp_path,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    assert report.source_counts["carveouts"] == 1
    assert len(rec.created) == 1               # carve-out filed as a node
    assert clean is True
    # Consumed: the processed id is gone from the canonical ledger.
    assert "cv-aaa" not in ledger.read_text(encoding="utf-8")


def test_consume_partial_removal_warns(tmp_path: Path, monkeypatch, capsys):
    """ab-d4e8f852: a consume that removes fewer ids than processed must WARN,
    not silently leave them to churn back in."""
    import fno.carveout.core as _core
    from fno.retro.cli import _process_payload

    _write_carveout(tmp_path, "cv-bbb")
    monkeypatch.setattr(_core, "consume_carveouts", lambda root, ids: 0)
    rec = _Rec()
    payload = {"pr_number": 501, "session_ids": ["sX"], "mode": "autonomous"}
    report, clean = _process_payload(
        payload, repo_root=tmp_path, existing_nodes=[], comments=[],
        carveout_root=tmp_path, create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert clean is True
    assert "consume_carveouts removed only 0/1" in capsys.readouterr().err


def test_consume_exception_warns(tmp_path: Path, monkeypatch, capsys):
    """ab-d4e8f852: a consume that RAISES must WARN, not be swallowed by the
    old bare `except: pass`."""
    import fno.carveout.core as _core
    from fno.retro.cli import _process_payload

    _write_carveout(tmp_path, "cv-ccc")

    def boom(root, ids):
        raise RuntimeError("disk full")

    monkeypatch.setattr(_core, "consume_carveouts", boom)
    rec = _Rec()
    payload = {"pr_number": 502, "session_ids": ["sX"], "mode": "autonomous"}
    report, clean = _process_payload(
        payload, repo_root=tmp_path, existing_nodes=[], comments=[],
        carveout_root=tmp_path, create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert clean is True  # a consume failure must NOT block the clean result
    err = capsys.readouterr().err
    assert "consume_carveouts failed" in err and "disk full" in err


def test_run_pr_builds_synthetic_payload(tmp_path: Path, monkeypatch):
    """`fno retro run --pr N --session sX --repo o/r` builds a synthetic payload
    and runs the harvest even with no sentinel present (ab-44408b6e hole #3)."""
    from typer.testing import CliRunner

    import fno.paths as _paths
    import fno.retro.cli as _cli
    from fno.cli import app

    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    # Isolate the global sentinel/graph lookups so no real state is touched.
    monkeypatch.setattr(_paths, "retro_pending_dir", lambda: tmp_path / "no-sentinels")
    monkeypatch.setattr(_paths, "graph_json", lambda: tmp_path / "graph.json")

    captured: dict = {}

    def fake_process(payload, **kwargs):
        captured["payload"] = payload
        captured["carveout_root"] = kwargs.get("carveout_root")
        return object(), True

    monkeypatch.setattr(_cli, "_process_payload", fake_process)

    result = CliRunner().invoke(
        app, ["retro", "run", "--pr", "777", "--session", "sX", "--repo", "o/r"]
    )
    assert "no retro-pending sentinels" not in result.output  # did not early-exit
    assert captured["payload"]["pr_number"] == 777
    assert captured["payload"]["session_ids"] == ["sX"]
    assert captured["payload"]["pr_url"] == "https://github.com/o/r/pull/777"
    assert captured["carveout_root"] == tmp_path  # canonical root threaded through


def test_current_repo_slug_handles_raising_runner():
    """gemini MEDIUM (PR #405): a gh runner that raises must yield None, not
    crash `fno retro run`."""
    from fno.retro.cli import _current_repo_slug

    def boom(args):
        raise FileNotFoundError("gh not on PATH")

    assert _current_repo_slug(gh_runner=boom) is None


def test_run_pr_no_slug_passes_empty_comments(tmp_path: Path, monkeypatch):
    """gemini HIGH (PR #405): with no resolvable repo slug, `run --pr` passes
    comments=[] so the carve-out-first path consumes rather than retaining
    (which would re-file duplicate nodes next run)."""
    from typer.testing import CliRunner

    import fno.paths as _paths
    import fno.retro.cli as _cli
    from fno.cli import app

    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setattr(_paths, "retro_pending_dir", lambda: tmp_path / "no-sentinels")
    monkeypatch.setattr(_paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(_cli, "_current_repo_slug", lambda *a, **k: None)

    captured: dict = {}

    def fake_process(payload, **kwargs):
        captured["comments"] = kwargs.get("comments")
        captured["payload"] = payload
        return object(), True

    monkeypatch.setattr(_cli, "_process_payload", fake_process)

    CliRunner().invoke(app, ["retro", "run", "--pr", "888"])
    assert captured["comments"] == []  # review derivation skipped, carve-outs still consume
    assert "pr_url" not in captured["payload"]  # no slug -> no url
