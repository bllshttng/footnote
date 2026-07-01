"""Retro harvest hardening (ab-158ab951): --pr scope + project attribution.

Two reported failures of `fno retro run --pr N`:
  - the global pending-sentinel sweep ran ALONGSIDE the --pr harvest, pulling an
    unrelated repo's carve-outs into the same run (cross-PR spillover);
  - every filed node landed with project=None (retro's create path does not
    auto-derive project the way `fno backlog idea` does).
"""
from __future__ import annotations

import json
from pathlib import Path

import typer

from fno.retro.cli import _sentinel_pr_number, process_sentinel_file


class _Rec:
    def __init__(self):
        self.created: list = []
        self._n = 0

    def create(self, **kw):
        self._n += 1
        nid = f"ab-new{self._n}"
        self.created.append({"id": nid, **kw})
        return nid

    def inbox_append(self, candidate):
        pass


def _write_sentinel(sd: Path, node_id: str, pr: int) -> Path:
    sd.mkdir(parents=True, exist_ok=True)
    p = sd / f"{node_id}.json"
    p.write_text(json.dumps({
        "node_id": node_id,
        "pr_number": pr,
        "pr_url": f"https://github.com/o/r/pull/{pr}",
    }), encoding="utf-8")
    return p


# ── _sentinel_pr_number unit ──────────────────────────────────────────


def test_sentinel_pr_number(tmp_path):
    p = _write_sentinel(tmp_path, "ab-x", 409)
    assert _sentinel_pr_number(p) == 409
    bad = tmp_path / "bad.json"
    bad.write_text("not json", encoding="utf-8")
    assert _sentinel_pr_number(bad) is None
    nope = tmp_path / "nope.json"
    nope.write_text(json.dumps({"node_id": "x"}), encoding="utf-8")  # no pr_number
    assert _sentinel_pr_number(nope) is None


# ── #3: filed nodes carry the derived project ─────────────────────────


def test_filed_nodes_carry_derived_project(tmp_path, monkeypatch):
    # sentinel pr_url is github.com/o/r/... -> attribute only when the current
    # repo matches that slug.
    sentinel = _write_sentinel(tmp_path / "retro-pending", "ab-12345678", 343)
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] real declined finding", "reviewer": "gemini[bot]"}]

    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "detect_project_from_settings", lambda cwd=None: "myproj")

    report, removed = process_sentinel_file(
        sentinel,
        repo_root=tmp_path,
        existing_nodes=[],
        comments=comments,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
        current_repo_slug="o/r",  # matches the sentinel's repo -> attribute
    )
    assert len(rec.created) == 1, rec.created
    assert rec.created[0].get("project") == "myproj", rec.created[0]


def test_cross_repo_sentinel_not_attributed_to_cwd(tmp_path, monkeypatch):
    """A sentinel for repo B processed from repo A must NOT inherit repo A's
    project; it lands project=None (codex P2)."""
    sentinel = _write_sentinel(tmp_path / "retro-pending", "ab-33333333", 50)  # repo o/r
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] real declined finding", "reviewer": "gemini[bot]"}]
    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "detect_project_from_settings", lambda cwd=None: "repo-A-proj")
    process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=comments,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
        current_repo_slug="someone/else",  # foreign repo -> do not attribute
    )
    assert len(rec.created) == 1
    assert rec.created[0].get("project") is None, rec.created[0]


def test_project_none_when_unattributed(tmp_path, monkeypatch):
    """A repo not in the workspace registry derives project=None, not a crash."""
    sentinel = _write_sentinel(tmp_path / "retro-pending", "ab-22222222", 344)
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] real declined finding", "reviewer": "gemini[bot]"}]
    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "detect_project_from_settings", lambda cwd=None: None)
    process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=comments,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert len(rec.created) == 1
    assert rec.created[0].get("project") is None


# ── #2: --pr is strictly scoped to that PR ────────────────────────────


def test_pr_flag_skips_unrelated_sentinels(tmp_path, monkeypatch):
    import fno.carveout.core as cocore
    import fno.graph.store as store
    import fno.paths as paths
    import fno.retro.cli as rcli

    sd = tmp_path / "retro-pending"
    _write_sentinel(sd, "ab-aaaaaaaa", 409)   # the PR we ask for
    _write_sentinel(sd, "ab-bbbbbbbb", 999)   # an unrelated PR
    (tmp_path / "graph.json").write_text("[]", encoding="utf-8")

    monkeypatch.setattr(paths, "resolve_repo_root", lambda: tmp_path)
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sd)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(cocore, "resolve_carveout_root", lambda: tmp_path)
    monkeypatch.setattr(store, "read_graph", lambda p: [])

    processed: list = []
    synth: list = []

    def fake_psf(sentinel_path, **kw):
        processed.append(Path(sentinel_path).name)
        return (object(), True)

    def fake_pp(payload, **kw):
        synth.append(payload.get("pr_number"))
        return (object(), True)

    monkeypatch.setattr(rcli, "process_sentinel_file", fake_psf)
    monkeypatch.setattr(rcli, "_process_payload", fake_pp)
    monkeypatch.setattr(rcli, "_current_repo_slug", lambda *a, **k: None)

    try:
        rcli.run(node=None, pr=409, session=None, repo=None)
    except typer.Exit:
        pass

    assert processed == ["ab-aaaaaaaa.json"], (
        f"--pr 409 must process ONLY the matching sentinel, got {processed}"
    )
    assert synth == [409], f"synthetic --pr harvest must run for 409, got {synth}"


def test_pr_flag_scopes_by_repo(tmp_path, monkeypatch):
    """Same PR number in two repos: --pr N --repo o/r processes only o/r's (codex P2)."""
    import fno.carveout.core as cocore
    import fno.graph.store as store
    import fno.paths as paths
    import fno.retro.cli as rcli

    sd = tmp_path / "retro-pending"
    sd.mkdir(parents=True, exist_ok=True)
    (sd / "ab-local.json").write_text(json.dumps(
        {"node_id": "ab-local", "pr_number": 77, "pr_url": "https://github.com/o/r/pull/77"}),
        encoding="utf-8")
    (sd / "ab-foreign.json").write_text(json.dumps(
        {"node_id": "ab-foreign", "pr_number": 77, "pr_url": "https://github.com/x/y/pull/77"}),
        encoding="utf-8")
    (tmp_path / "graph.json").write_text("[]", encoding="utf-8")

    monkeypatch.setattr(paths, "resolve_repo_root", lambda: tmp_path)
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sd)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(cocore, "resolve_carveout_root", lambda: tmp_path)
    monkeypatch.setattr(store, "read_graph", lambda p: [])

    processed: list = []

    def fake_psf(sentinel_path, **kw):
        processed.append(Path(sentinel_path).name)
        return (object(), True)

    monkeypatch.setattr(rcli, "process_sentinel_file", fake_psf)
    monkeypatch.setattr(rcli, "_process_payload", lambda payload, **kw: (object(), True))

    try:
        rcli.run(node=None, pr=77, session=None, repo="o/r")
    except typer.Exit:
        pass

    assert processed == ["ab-local.json"], (
        f"--pr 77 --repo o/r must process only the o/r sentinel, got {processed}"
    )


# ── ab-b4da4664: filed nodes root at CANONICAL, not the worktree ──────


def test_filed_node_cwd_and_project_root_at_canonical(tmp_path, monkeypatch):
    """The filed node's cwd + project derive from the CANONICAL root (node_root),
    not the worktree (repo_root). A node outlives the worktree it was captured in,
    and detect_project_from_settings only matches canonical roots."""
    worktree = tmp_path / "wt"
    canonical = tmp_path / "canonical"
    worktree.mkdir()
    canonical.mkdir()
    sentinel = _write_sentinel(worktree / "retro-pending", "ab-44444444", 410)
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] real declined finding", "reviewer": "gemini[bot]"}]

    import fno.graph._intake as intake
    seen = {}

    def fake_detect(cwd=None):
        seen["cwd"] = cwd
        return "canon-proj"

    monkeypatch.setattr(intake, "detect_project_from_settings", fake_detect)

    report, removed = process_sentinel_file(
        sentinel,
        repo_root=worktree,    # worktree: used only for worktree-local artifacts
        node_root=canonical,   # canonical: the node's durable home + attribution
        existing_nodes=[],
        comments=comments,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
        current_repo_slug="o/r",  # matches the sentinel's repo -> attribute
    )
    assert len(rec.created) == 1, rec.created
    assert rec.created[0].get("cwd") == str(canonical), rec.created[0]
    assert rec.created[0].get("project") == "canon-proj", rec.created[0]
    # project must be derived from the CANONICAL root, not the worktree.
    assert seen.get("cwd") == str(canonical), seen


def test_run_threads_canonical_node_root(tmp_path, monkeypatch):
    """run() roots node scoping at resolve_canonical_repo_root() while keeping
    repo_root = resolve_repo_root() (the worktree) for artifact lookups."""
    import fno.carveout.core as cocore
    import fno.graph.store as store
    import fno.paths as paths
    import fno.retro.cli as rcli

    worktree = tmp_path / "wt"
    canonical = tmp_path / "canonical"
    worktree.mkdir()
    canonical.mkdir()
    sd = worktree / "retro-pending"
    _write_sentinel(sd, "ab-cccccccc", 411)
    (tmp_path / "graph.json").write_text("[]", encoding="utf-8")

    monkeypatch.setattr(paths, "resolve_repo_root", lambda: worktree)
    monkeypatch.setattr(paths, "resolve_canonical_repo_root", lambda: canonical)
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sd)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(cocore, "resolve_carveout_root", lambda: canonical)
    monkeypatch.setattr(store, "read_graph", lambda p: [])

    captured: dict = {}

    def fake_psf(sentinel_path, **kw):
        captured.update(kw)
        return (object(), True)

    monkeypatch.setattr(rcli, "process_sentinel_file", fake_psf)
    monkeypatch.setattr(rcli, "_current_repo_slug", lambda *a, **k: None)

    try:
        rcli.run(node=None, pr=None, session=None, repo=None)
    except typer.Exit:
        pass

    assert captured.get("node_root") == canonical, captured
    assert captured.get("repo_root") == worktree, captured


def test_explicit_foreign_repo_not_attributed_to_local(tmp_path, monkeypatch):
    """`fno retro run --pr N --repo other/repo` from a different checkout must
    gate project attribution on the ACTUAL local repo, not the --repo override
    (codex P2). Otherwise the foreign PR's node inherits the caller's project."""
    import fno.carveout.core as cocore
    import fno.graph.store as store
    import fno.paths as paths
    import fno.retro.cli as rcli

    (tmp_path / "graph.json").write_text("[]", encoding="utf-8")
    sd = tmp_path / "retro-pending"
    sd.mkdir(parents=True, exist_ok=True)

    monkeypatch.setattr(paths, "resolve_repo_root", lambda: tmp_path)
    monkeypatch.setattr(paths, "resolve_canonical_repo_root", lambda: tmp_path)
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sd)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(cocore, "resolve_carveout_root", lambda: tmp_path)
    monkeypatch.setattr(store, "read_graph", lambda p: [])
    # The ACTUAL local repo, independent of the --repo override.
    monkeypatch.setattr(rcli, "_current_repo_slug", lambda *a, **k: "acme/local")

    captured: dict = {}

    def fake_pp(payload, **kw):
        captured.update(kw)
        captured["pr_url"] = payload.get("pr_url")
        return (object(), True)

    monkeypatch.setattr(rcli, "_process_payload", fake_pp)

    try:
        rcli.run(node=None, pr=5, session=None, repo="other/repo")
    except typer.Exit:
        pass

    # Attribution gate is the LOCAL repo, so _is_local fails for the foreign
    # PR and the node stays unattributed - not mis-scoped to acme/local.
    assert captured.get("current_repo_slug") == "acme/local", captured
    # The harvest target URL still honors --repo.
    assert captured.get("pr_url") == "https://github.com/other/repo/pull/5", captured


# ── x-90b8: --pr-number with no owning session is carve-out READ-ONLY ──
#
# A hotfix / manual PR has no session<->PR ledger link. Without a guard, the
# synthetic `retro run --pr-number N` harvests EVERY unconsumed carve-out,
# stamps it `Source: PR #N`, files a node under the wrong lineage, and consumes
# it - exactly cv-0932fa60 minted onto PR #522. The fix mirrors the post-merge
# Step 4b guard: no owning session -> carve-outs are listed read-only, never
# filed or consumed. Reviews/COMPLETION (inherently PR-scoped) are untouched.

from fno.retro.cli import _resolve_pr_session_ids
from fno.retro.routine import triage_pr


def _write_carveout(root: Path, **rec) -> None:
    d = root / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    base = {
        "id": "cv-stray",
        "description": "stray deferred work from another session",
        "session_id": "other-sess",
        "kind": "deferred",
    }
    base.update(rec)
    (d / "carveouts.jsonl").write_text(json.dumps(base) + "\n", encoding="utf-8")


def test_resolve_pr_session_ids_matches_and_misses(tmp_path):
    led = tmp_path / "ledger.json"
    led.write_text(json.dumps({"entries": [
        {"session_id": "s1", "pr": 522, "pr_url": "https://github.com/o/r/pull/522"},
        {"session_id": "s2", "pr": 999, "pr_url": "https://github.com/o/r/pull/999"},
        {"sessions": ["s3", "s4"], "pr": 522, "pr_url": "https://github.com/o/r/pull/522"},
    ]}), encoding="utf-8")
    # matched entries: session_id + the sessions[] list, order-preserving + deduped
    assert _resolve_pr_session_ids(led, 522, "o/r") == ["s1", "s3", "s4"]
    # no entry for this PR -> [] (the read-only case)
    assert _resolve_pr_session_ids(led, 777, "o/r") == []
    # same number in a different repo -> [] (the ledger is global, slug-scoped)
    assert _resolve_pr_session_ids(led, 522, "x/y") == []
    # missing / unreadable ledger -> [] (degrades to read-only, never crashes)
    assert _resolve_pr_session_ids(tmp_path / "nope.json", 522, "o/r") == []


def test_resolve_pr_session_ids_bare_number_when_no_url(tmp_path):
    """An entry with no pr_url falls back to the bare numeric match, coercing a
    string-stored pr; a non-list `sessions` is ignored, not spread into chars."""
    led = tmp_path / "ledger.json"
    led.write_text(json.dumps({"entries": [
        {"session_id": "s1", "pr": 522},          # no url -> numeric match
        {"session_id": "s2", "pr_number": 522},   # legacy field name
        {"session_id": "s3", "pr": "522"},        # string-stored pr -> coerced
        {"session_id": "s4", "pr": 522, "sessions": "oops"},  # non-list -> ignored
    ]}), encoding="utf-8")
    assert _resolve_pr_session_ids(led, 522, "o/r") == ["s1", "s2", "s3", "s4"]
    # `sessions: "oops"` must contribute nothing beyond the entry's session_id.
    assert "o" not in _resolve_pr_session_ids(led, 522, "o/r")


def test_resolve_pr_session_ids_requires_repo_scope(tmp_path):
    """No resolvable repo (repo_slug=None) -> [] even when a global-ledger entry's
    url ends in /pull/<pr>. The ledger is global and PR numbers collide across
    repos, so an unscoped url match could consume a foreign PR's session (codex
    P2); the safe answer is read-only."""
    led = tmp_path / "ledger.json"
    led.write_text(json.dumps({"entries": [
        {"session_id": "s1", "pr": 522, "pr_url": "https://github.com/o/r/pull/522"},
        {"session_id": "s2", "pr": 522},  # bare number is also cross-repo-ambiguous
    ]}), encoding="utf-8")
    assert _resolve_pr_session_ids(led, 522, None) == []
    # ... but a known slug still resolves the matching entry.
    assert _resolve_pr_session_ids(led, 522, "o/r") == ["s1", "s2"]


def test_triage_carveouts_readonly_not_landed_or_consumed(tmp_path):
    """carveouts_readonly=True: the stray carve-out is surfaced read-only, NOT
    landed and NOT in harvested_carveout_ids (so the caller never consumes it).
    A real reviewer finding still lands."""
    _write_carveout(tmp_path)
    rec = _Rec()
    comments = [{"id": "c1", "body": "![high] real declined finding", "reviewer": "gemini[bot]"}]
    report = triage_pr(
        repo_root=tmp_path,
        pr_number=522,
        mode="autonomous",
        comments=comments,
        carveout_root=tmp_path,
        carveouts_readonly=True,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    titles = [c.get("title") or "" for c in rec.created]
    assert any("real declined finding" in t for t in titles), rec.created
    assert not any("stray" in t.lower() for t in titles), rec.created
    assert report.harvested_carveout_ids == [], report.harvested_carveout_ids
    assert report.readonly_carveout_count == 1, report.readonly_carveout_count


def test_triage_carveouts_consumed_when_not_readonly(tmp_path):
    """Control: with a resolved session (carveouts_readonly=False) the carve-out
    lands AND is reported for consumption."""
    _write_carveout(tmp_path)
    rec = _Rec()
    report = triage_pr(
        repo_root=tmp_path,
        pr_number=522,
        mode="autonomous",
        comments=[],
        carveout_root=tmp_path,
        carveouts_readonly=False,
        create_fn=rec.create,
        inbox_fn=rec.inbox_append,
    )
    assert report.harvested_carveout_ids == ["cv-stray"], report.harvested_carveout_ids
    assert report.readonly_carveout_count == 0
    assert any("stray" in (c.get("title") or "").lower() for c in rec.created), rec.created


def _run_synthetic_pr(tmp_path, monkeypatch, *, ledger_entries, local_slug="o/r"):
    """Drive run() for a synthetic --pr-number 522 with no --session-id, capturing
    the payload handed to _process_payload."""
    import fno.carveout.core as cocore
    import fno.graph.store as store
    import fno.paths as paths
    import fno.retro.cli as rcli

    (tmp_path / "graph.json").write_text("[]", encoding="utf-8")
    (tmp_path / "ledger.json").write_text(
        json.dumps({"entries": ledger_entries}), encoding="utf-8"
    )
    sd = tmp_path / "retro-pending"
    sd.mkdir(parents=True, exist_ok=True)

    monkeypatch.setattr(paths, "resolve_repo_root", lambda: tmp_path)
    monkeypatch.setattr(paths, "resolve_canonical_repo_root", lambda: tmp_path)
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sd)
    monkeypatch.setattr(paths, "graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(paths, "ledger_json", lambda: tmp_path / "ledger.json")
    monkeypatch.setattr(cocore, "resolve_carveout_root", lambda: tmp_path)
    monkeypatch.setattr(store, "read_graph", lambda p: [])
    monkeypatch.setattr(rcli, "_current_repo_slug", lambda *a, **k: local_slug)

    captured: dict = {}

    def fake_pp(payload, **kw):
        captured.update(payload)
        return (object(), True)

    monkeypatch.setattr(rcli, "_process_payload", fake_pp)
    try:
        rcli.run(node=None, pr=522, session=None, repo=None)
    except typer.Exit:
        pass
    return captured


def test_run_pr_no_owning_session_sets_readonly(tmp_path, monkeypatch):
    """No ledger entry for the PR -> payload marks carve-outs read-only and
    carries NO session scope (so triage_pr suppresses the carve-out source)."""
    captured = _run_synthetic_pr(tmp_path, monkeypatch, ledger_entries=[])
    assert captured.get("carveouts_readonly") is True, captured
    assert "session_ids" not in captured, captured


def test_run_pr_with_owning_session_scopes_not_readonly(tmp_path, monkeypatch):
    """A ledger entry linking the PR to a session -> payload scopes to that
    session and is NOT read-only (carve-outs land + consume, as before)."""
    captured = _run_synthetic_pr(tmp_path, monkeypatch, ledger_entries=[
        {"session_id": "sess-A", "pr": 522, "pr_url": "https://github.com/o/r/pull/522"},
    ])
    assert captured.get("session_ids") == ["sess-A"], captured
    assert not captured.get("carveouts_readonly"), captured


def test_run_pr_unresolvable_repo_is_readonly(tmp_path, monkeypatch):
    """gh can't resolve the repo (local_slug=None, no --repo): even a matching
    global-ledger entry must NOT scope - the run falls through to read-only so a
    same-numbered foreign PR's carve-outs are never consumed (codex P2)."""
    captured = _run_synthetic_pr(
        tmp_path, monkeypatch, local_slug=None, ledger_entries=[
            {"session_id": "sess-A", "pr": 522, "pr_url": "https://github.com/o/r/pull/522"},
        ],
    )
    assert captured.get("carveouts_readonly") is True, captured
    assert "session_ids" not in captured, captured


# ── x-23c0: the SENTINEL harvest path resolves owning session(s) too ──
#
# The synthetic --pr-number path (above) resolves + guards, but a
# reconcile-dropped sentinel (fno backlog reconcile, for merges outside the ship
# gate) arrives with NO session scope. Before the fix, process_sentinel_file ran
# harvest_carveouts with session_ids=None and DRAINED the shared ledger, stamping
# another in-flight session's carve-outs onto the merging PR (cv-5e4b9f4d,
# recorded in #123's session, swept by #121's resume reconcile -> node x-8d19
# mis-attributed). The fix gives the sentinel path the SAME resolve-or-readonly
# fallback, keyed off the ledger_path threaded in from run().


def _seed_two_session_carveouts(root: Path) -> None:
    """Two sessions drop deferred carve-outs into ONE shared ledger."""
    d = root / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    lines = [
        {"id": "cv-A", "description": "A deferred work", "session_id": "sess-A", "kind": "deferred"},
        {"id": "cv-B", "description": "B deferred work", "session_id": "sess-B", "kind": "deferred"},
    ]
    (d / "carveouts.jsonl").write_text(
        "".join(json.dumps(x) + "\n" for x in lines), encoding="utf-8"
    )


def test_sentinel_harvest_scopes_to_owning_session(tmp_path):
    """The original repro: sentinel for PR #501 (owned by sess-A) harvests ONLY
    A's carve-out and consumes it; B's stays in the shared ledger for its own
    PR's harvest. Before the fix the sentinel path drained both."""
    _seed_two_session_carveouts(tmp_path)
    led = tmp_path / "ledger.json"
    led.write_text(json.dumps({"entries": [
        {"session_id": "sess-A", "pr": 501, "pr_url": "https://github.com/o/r/pull/501"},
    ]}), encoding="utf-8")
    sentinel = _write_sentinel(tmp_path / "retro-pending", "ab-aaaa1111", 501)
    rec = _Rec()
    report, _removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=[],
        carveout_root=tmp_path, ledger_path=led,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert report.harvested_carveout_ids == ["cv-A"], report.harvested_carveout_ids
    assert report.readonly_carveout_count == 0
    # B's carve-out survives untouched in the ledger; A's was consumed.
    remaining = (tmp_path / ".fno" / "carveouts.jsonl").read_text(encoding="utf-8")
    assert "cv-B" in remaining and "cv-A" not in remaining, remaining


def test_sentinel_harvest_no_owner_is_readonly(tmp_path):
    """Sentinel for a PR with NO owning session in the ledger -> carve-outs are
    read-only: neither harvested nor consumed (x-90b8 reuse, never a 2nd guard)."""
    _seed_two_session_carveouts(tmp_path)
    led = tmp_path / "ledger.json"
    led.write_text(json.dumps({"entries": []}), encoding="utf-8")
    sentinel = _write_sentinel(tmp_path / "retro-pending", "ab-bbbb2222", 777)
    rec = _Rec()
    report, _removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=[],
        carveout_root=tmp_path, ledger_path=led,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert report.harvested_carveout_ids == [], report.harvested_carveout_ids
    assert report.readonly_carveout_count == 2, report.readonly_carveout_count
    # Nothing consumed: both carve-outs remain for their own PRs.
    remaining = (tmp_path / ".fno" / "carveouts.jsonl").read_text(encoding="utf-8")
    assert "cv-A" in remaining and "cv-B" in remaining, remaining


def test_sentinel_harvest_batch_pr_owns_two_sessions(tmp_path):
    """batch-lane contract: a batch PR's ledger entry owns MULTIPLE member
    sessions, so its sentinel harvest must collect BOTH members' carve-outs (and
    only them). Proves the plural resolution the fix must not collapse."""
    _seed_two_session_carveouts(tmp_path)
    led = tmp_path / "ledger.json"
    led.write_text(json.dumps({"entries": [
        {"sessions": ["sess-A", "sess-B"], "pr": 909,
         "pr_url": "https://github.com/o/r/pull/909"},
    ]}), encoding="utf-8")
    sentinel = _write_sentinel(tmp_path / "retro-pending", "ab-cccc3333", 909)
    rec = _Rec()
    report, _removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=[],
        carveout_root=tmp_path, ledger_path=led,
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert sorted(report.harvested_carveout_ids) == ["cv-A", "cv-B"], report.harvested_carveout_ids
    assert report.readonly_carveout_count == 0


def test_sentinel_harvest_no_ledger_is_readonly_not_drain(tmp_path):
    """Fail-SAFE: a caller that omits ledger_path (the internal-caller footgun)
    must route to read-only, NOT the old unscoped drain. Without a ledger the
    owner can't be resolved, so carve-outs are surfaced read-only and left in
    place - the drain precondition never reaches harvest_carveouts unscoped."""
    _seed_two_session_carveouts(tmp_path)
    sentinel = _write_sentinel(tmp_path / "retro-pending", "ab-dddd4444", 404)
    rec = _Rec()
    report, _removed = process_sentinel_file(
        sentinel, repo_root=tmp_path, existing_nodes=[], comments=[],
        carveout_root=tmp_path,  # NOTE: ledger_path deliberately omitted
        create_fn=rec.create, inbox_fn=rec.inbox_append,
    )
    assert report.harvested_carveout_ids == [], report.harvested_carveout_ids
    assert report.readonly_carveout_count == 2, report.readonly_carveout_count
    remaining = (tmp_path / ".fno" / "carveouts.jsonl").read_text(encoding="utf-8")
    assert "cv-A" in remaining and "cv-B" in remaining, remaining
