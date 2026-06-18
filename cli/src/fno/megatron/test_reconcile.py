"""Tests for fno megatron reconcile (pure-module surface)."""
from __future__ import annotations

import json

import pytest


@pytest.fixture(autouse=True)
def _passthrough_resolver(monkeypatch):
    """Make resolve_project_name a passthrough so tests don't depend on settings.yaml."""
    import fno.megatron.reconcile as rc_mod

    monkeypatch.setattr(rc_mod, "resolve_project_name", lambda s: s)


@pytest.fixture
def manifest_2x2(tmp_path):
    """Two waves of two projects each. No completion files seeded."""
    from fno.megatron.manifest import load_manifest

    manifest_path = tmp_path / "fleet" / "2026-05-13-rc" / "00-INDEX.md"
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(
        """---
mission_type: fleet
mission_id: ab-rc00001
waves:
  - wave: 1
    mode: parallel
    projects:
      - {name: alpha, body: x}
      - {name: beta, body: y}
  - wave: 2
    mode: parallel
    projects:
      - {name: alpha, body: x}
      - {name: beta, body: y}
---
""",
        encoding="utf-8",
    )
    return load_manifest(manifest_path), manifest_path.parent


def test_classify_drift_state_no_pr():
    from fno.megatron.reconcile import _classify_drift_state

    assert _classify_drift_state([]) == "missing-no-pr"


def test_classify_drift_state_single_merged():
    from fno.megatron.reconcile import PrState, _classify_drift_state

    p = PrState(number=1, url="u", state="MERGED", merged_at="t", merge_commit_sha="abc")
    assert _classify_drift_state([p]) == "missing-pr-merged"


def test_classify_drift_state_single_open():
    from fno.megatron.reconcile import PrState, _classify_drift_state

    p = PrState(number=1, url="u", state="OPEN", merged_at=None, merge_commit_sha=None)
    assert _classify_drift_state([p]) == "missing-pr-open"


def test_classify_drift_state_ambiguous_two_merged():
    from fno.megatron.reconcile import PrState, _classify_drift_state

    p1 = PrState(number=1, url="u1", state="MERGED", merged_at="t", merge_commit_sha="a")
    p2 = PrState(number=2, url="u2", state="MERGED", merged_at="t", merge_commit_sha="b")
    assert _classify_drift_state([p1, p2]) == "ambiguous"


def test_classify_drift_state_picks_unique_merged_amongst_others():
    """Single MERGED + assorted non-merged → missing-pr-merged."""
    from fno.megatron.reconcile import PrState, _classify_drift_state

    p_merged = PrState(number=1, url="u", state="MERGED", merged_at="t", merge_commit_sha="a")
    p_open = PrState(number=2, url="u", state="OPEN", merged_at=None, merge_commit_sha=None)
    p_closed = PrState(number=3, url="u", state="CLOSED", merged_at=None, merge_commit_sha=None)
    assert (
        _classify_drift_state([p_merged, p_open, p_closed]) == "missing-pr-merged"
    )


def test_scan_drift_clean_mission_makes_no_pr_queries(manifest_2x2):
    """All completion files present → has_drift False and no gh queries."""
    from fno.megatron.reconcile import scan_drift

    manifest, fleet_dir = manifest_2x2
    for wave in (1, 2):
        for proj in ("alpha", "beta"):
            d = fleet_dir / "completions" / f"wave-{wave}"
            d.mkdir(parents=True, exist_ok=True)
            (d / f"{proj}.json").write_text(f'{{"project":"{proj}","wave":{wave},"mission_id":"x"}}')

    calls = []

    def no_call_query(project, branch):
        calls.append((project, branch))
        return []

    report = scan_drift(manifest, fleet_dir, "2026-05-13-rc", query_pr=no_call_query)
    assert calls == []
    assert not report.has_drift
    assert all(d.state == "no-drift" for d in report.drift)


def test_scan_drift_missing_file_with_merged_pr(manifest_2x2):
    """Missing file + merged PR → drift record with merged state."""
    from fno.megatron.reconcile import PrState, scan_drift

    manifest, fleet_dir = manifest_2x2

    def stub_query(project, branch):
        if project == "beta" and "wave-1" in branch:
            return [
                PrState(
                    number=42,
                    url="https://github.com/x/y/pull/42",
                    state="MERGED",
                    merged_at="2026-05-13T20:00:00Z",
                    merge_commit_sha="abc1234",
                )
            ]
        return []

    report = scan_drift(manifest, fleet_dir, "2026-05-13-rc", query_pr=stub_query)
    assert report.has_drift
    beta_w1 = next(d for d in report.drift if d.wave == 1 and d.project == "beta")
    assert beta_w1.state == "missing-pr-merged"
    assert len(beta_w1.pr_candidates) == 1


def test_scan_drift_query_failure_marks_record(manifest_2x2):
    """When query_pr raises ReconcileError, record is marked query-failed."""
    from fno.megatron.reconcile import ReconcileError, scan_drift

    manifest, fleet_dir = manifest_2x2

    def failing_query(project, branch):
        raise ReconcileError("gh broke")

    report = scan_drift(manifest, fleet_dir, "2026-05-13-rc", query_pr=failing_query)
    assert all(d.state == "query-failed" for d in report.drift)
    assert all(d.backfill_skipped_reason == "gh broke" for d in report.drift)


def test_backfill_writes_for_merged_pr(manifest_2x2):
    """Backfill writes the missing JSON for a merged PR."""
    from fno.megatron.reconcile import PrState, backfill_completion, scan_drift

    manifest, fleet_dir = manifest_2x2

    def stub_query(project, branch):
        if project == "beta":
            return [
                PrState(
                    number=42,
                    url="https://github.com/x/y/pull/42",
                    state="MERGED",
                    merged_at="2026-05-13T20:00:00Z",
                    merge_commit_sha="abc1234",
                )
            ]
        return []

    report = scan_drift(manifest, fleet_dir, "2026-05-13-rc", query_pr=stub_query)
    beta_w1 = next(d for d in report.drift if d.wave == 1 and d.project == "beta")
    written = backfill_completion(beta_w1, mission_id=manifest.mission_id)
    assert written is True
    assert beta_w1.completion_path.exists()
    payload = json.loads(beta_w1.completion_path.read_text())
    assert payload["source"] == "reconcile-backfill"
    assert payload["pr_url"] == "https://github.com/x/y/pull/42"
    assert payload["commit_sha"] == "abc1234"
    assert payload["schema_version"] == 1
    assert beta_w1.backfill_written is True


def test_backfill_refuses_to_clobber(tmp_path):
    """Existing completion file is never overwritten even when record claims missing."""
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    existing = tmp_path / "completions" / "wave-1" / "beta.json"
    existing.parent.mkdir(parents=True, exist_ok=True)
    existing.write_text('{"sentinel": "pre-existing"}')
    mtime_before = existing.stat().st_mtime

    record = DriftRecord(
        wave=1,
        project="beta",
        completion_exists=False,  # lie: file exists on disk
        completion_path=existing,
        branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=99,
                url="u",
                state="MERGED",
                merged_at="2026-05-13T20:00:00Z",
                merge_commit_sha="def5678",
            )
        ],
        state="missing-pr-merged",
    )
    written = backfill_completion(record, mission_id="ab-test1")
    assert written is False
    assert "already present" in (record.backfill_skipped_reason or "")
    assert existing.stat().st_mtime == mtime_before
    assert json.loads(existing.read_text())["sentinel"] == "pre-existing"


def test_backfill_refuses_open_unmerged_pr(tmp_path):
    """An open-unmerged PR is not safe to backfill."""
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    completion_path = tmp_path / "completions" / "wave-1" / "beta.json"
    record = DriftRecord(
        wave=1,
        project="beta",
        completion_exists=False,
        completion_path=completion_path,
        branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=99, url="u", state="OPEN", merged_at=None, merge_commit_sha=None
            )
        ],
        state="missing-pr-open",
    )
    written = backfill_completion(record, mission_id="ab-test2")
    assert written is False
    assert "not safe" in (record.backfill_skipped_reason or "")
    assert not completion_path.exists()


def test_backfill_refuses_null_merge_commit_sha(tmp_path):
    """A merged PR with null merge_commit_sha is rejected."""
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    completion_path = tmp_path / "completions" / "wave-1" / "beta.json"
    record = DriftRecord(
        wave=1,
        project="beta",
        completion_exists=False,
        completion_path=completion_path,
        branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=99,
                url="u",
                state="MERGED",
                merged_at="2026-05-13T20:00:00Z",
                merge_commit_sha=None,
            )
        ],
        state="missing-pr-merged",
    )
    written = backfill_completion(record, mission_id="ab-test3")
    assert written is False
    assert "merge_commit_sha" in (record.backfill_skipped_reason or "")
    assert not completion_path.exists()


def test_render_report_json_mode_is_parseable(manifest_2x2):
    """--json output is parseable structured JSON."""
    from fno.megatron.reconcile import render_drift_report, scan_drift

    manifest, fleet_dir = manifest_2x2
    report = scan_drift(manifest, fleet_dir, "2026-05-13-rc", query_pr=lambda p, b: [])
    out = render_drift_report(report, as_json=True)
    parsed = json.loads(out)
    assert parsed["mission_id"] == manifest.mission_id
    assert isinstance(parsed["drift"], list)
    assert len(parsed["drift"]) == 4  # 2 waves x 2 projects


def test_render_report_markdown_hides_no_drift_without_verbose(manifest_2x2):
    """Markdown rendering hides no-drift rows unless --verbose."""
    from fno.megatron.reconcile import render_drift_report, scan_drift

    manifest, fleet_dir = manifest_2x2
    for wave in (1, 2):
        for proj in ("alpha", "beta"):
            d = fleet_dir / "completions" / f"wave-{wave}"
            d.mkdir(parents=True, exist_ok=True)
            (d / f"{proj}.json").write_text(f'{{"project":"{proj}","wave":{wave},"mission_id":"x"}}')

    report = scan_drift(manifest, fleet_dir, "2026-05-13-rc", query_pr=lambda p, b: [])
    quiet = render_drift_report(report, as_json=False, verbose=False)
    assert "No drift detected." in quiet
    assert "alpha" not in quiet

    verbose = render_drift_report(report, as_json=False, verbose=True)
    assert "alpha" in verbose
    assert "beta" in verbose


def test_expected_branch_pattern_strips_ab_prefix():
    """Mission-id short form drops the ab- prefix."""
    from fno.megatron.reconcile import _expected_branch_pattern

    pattern = _expected_branch_pattern("2026-05-13-foo", "ab-deadbeef", 2, "example-pipeline")
    assert pattern == "feature/2026-05-13-foo-mission-deadbeef-wave-2-example-pipeline"


def test_backfill_rejects_ambiguous_all_open_candidates(tmp_path):
    """When --pr picks an OPEN candidate from an ambiguous record, refuse."""
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    completion_path = tmp_path / "completions" / "wave-1" / "beta.json"
    record = DriftRecord(
        wave=1,
        project="beta",
        completion_exists=False,
        completion_path=completion_path,
        branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=1, url="u1", state="OPEN", merged_at=None, merge_commit_sha=None
            ),
            PrState(
                number=2, url="u2", state="OPEN", merged_at=None, merge_commit_sha=None
            ),
        ],
        state="ambiguous",
    )
    written = backfill_completion(record, mission_id="ab-amb", pr_choice_index=0)
    assert written is False
    assert "not MERGED" in (record.backfill_skipped_reason or "")
    assert not completion_path.exists()


def test_backfill_refuses_to_clobber_under_race_window(tmp_path, monkeypatch):
    """Even if the initial exists()-check passes, a concurrent producer that
    writes the file before our os.link() must not be clobbered."""
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    completion_path = tmp_path / "completions" / "wave-1" / "beta.json"
    completion_path.parent.mkdir(parents=True, exist_ok=True)
    # Stash the original payload we want to protect.
    sentinel_payload = '{"sentinel": "concurrent-producer-wrote-this"}'

    record = DriftRecord(
        wave=1,
        project="beta",
        completion_exists=False,
        completion_path=completion_path,
        branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=1,
                url="u",
                state="MERGED",
                merged_at="2026-05-13T20:00:00Z",
                merge_commit_sha="abc",
            )
        ],
        state="missing-pr-merged",
    )

    # Race injection: after the initial exists() check inside backfill_completion
    # returns False, simulate a concurrent producer dropping the final file
    # in before the os.link() call. We monkeypatch Path.write_text to drop the
    # competing file as a side effect; the tmp write still happens normally.
    import pathlib

    real_write_text = pathlib.Path.write_text

    def write_text_then_race(self_path, *args, **kwargs):
        rc = real_write_text(self_path, *args, **kwargs)
        if str(self_path).endswith(".tmp"):
            # competing producer races in here
            completion_path.write_text(sentinel_payload)
        return rc

    monkeypatch.setattr(pathlib.Path, "write_text", write_text_then_race)

    written = backfill_completion(record, mission_id="ab-race")
    assert written is False
    assert "mid-write" in (record.backfill_skipped_reason or "")
    # Original sentinel must be preserved.
    assert completion_path.read_text() == sentinel_payload
    # tmp must have been cleaned up.
    leftover = list(completion_path.parent.glob(f".{completion_path.name}.*.tmp"))
    assert leftover == [], f"orphan tmp left behind: {leftover}"


def test_scan_drift_corrupt_completion_treated_as_drift(manifest_2x2):
    """A JSON-decode-failing completion file does NOT count as no-drift."""
    from fno.megatron.reconcile import scan_drift

    manifest, fleet_dir = manifest_2x2
    # Seed alpha/wave-1 with corrupt content (not JSON).
    bad_dir = fleet_dir / "completions" / "wave-1"
    bad_dir.mkdir(parents=True, exist_ok=True)
    (bad_dir / "alpha.json").write_text("this is not json")

    report = scan_drift(
        manifest, fleet_dir, "2026-05-13-rc", query_pr=lambda p, b: []
    )
    alpha_w1 = next(d for d in report.drift if d.wave == 1 and d.project == "alpha")
    assert alpha_w1.completion_exists is False
    assert "corrupt completion file present" in (alpha_w1.backfill_skipped_reason or "")


def test_scan_drift_non_object_completion_treated_as_drift(manifest_2x2):
    """A JSON array at the top level does not satisfy the completion schema."""
    from fno.megatron.reconcile import scan_drift

    manifest, fleet_dir = manifest_2x2
    bad_dir = fleet_dir / "completions" / "wave-1"
    bad_dir.mkdir(parents=True, exist_ok=True)
    (bad_dir / "alpha.json").write_text('["not", "an", "object"]')

    report = scan_drift(
        manifest, fleet_dir, "2026-05-13-rc", query_pr=lambda p, b: []
    )
    alpha_w1 = next(d for d in report.drift if d.wave == 1 and d.project == "alpha")
    assert alpha_w1.completion_exists is False


def test_backfill_refuses_null_merged_at_timestamp(tmp_path):
    """Refuse to fabricate completed_at when merged_at is null."""
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    completion_path = tmp_path / "completions" / "wave-1" / "beta.json"
    record = DriftRecord(
        wave=1, project="beta", completion_exists=False,
        completion_path=completion_path, branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=1, url="u", state="MERGED",
                merged_at=None,  # degenerate
                merge_commit_sha="abc1234",
            )
        ],
        state="missing-pr-merged",
    )
    written = backfill_completion(record, mission_id="ab-tnull")
    assert written is False
    assert "merged_at" in (record.backfill_skipped_reason or "")
    assert not completion_path.exists()


def test_backfill_payload_includes_discoveries(tmp_path):
    """Documented completion schema includes discoveries; backfill must too."""
    import json as _json
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    completion_path = tmp_path / "completions" / "wave-1" / "beta.json"
    record = DriftRecord(
        wave=1, project="beta", completion_exists=False,
        completion_path=completion_path, branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=1, url="u", state="MERGED",
                merged_at="2026-05-14T07:00:00Z", merge_commit_sha="abc",
            )
        ],
        state="missing-pr-merged",
    )
    assert backfill_completion(record, mission_id="ab-disco") is True
    payload = _json.loads(completion_path.read_text())
    assert "discoveries" in payload
    assert payload["completed_at"] == "2026-05-14T07:00:00Z"  # no fabrication


def test_query_pr_state_passes_repo_slug_to_gh(monkeypatch):
    """--repo OWNER/NAME must reach gh when repo_slug is provided."""
    import subprocess as _sp
    from fno.megatron import reconcile as rc

    captured = {}

    class _FakeResult:
        returncode = 0
        stdout = "[]"
        stderr = ""

    def fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        captured["timeout"] = kwargs.get("timeout")
        return _FakeResult()

    monkeypatch.setattr(rc.shutil, "which", lambda _: "/usr/bin/gh")
    monkeypatch.setattr(rc.subprocess, "run", fake_run)
    rc.query_pr_state("foo", "feature/bar", repo_slug="acme/foo")
    assert "--repo" in captured["cmd"]
    idx = captured["cmd"].index("--repo")
    assert captured["cmd"][idx + 1] == "acme/foo"
    assert captured["timeout"] == rc.GH_QUERY_TIMEOUT_S


def test_query_pr_state_raises_on_timeout(monkeypatch):
    """subprocess.TimeoutExpired becomes ReconcileError."""
    import subprocess as _sp
    from fno.megatron import reconcile as rc

    monkeypatch.setattr(rc.shutil, "which", lambda _: "/usr/bin/gh")

    def fake_run(*a, **kw):
        raise _sp.TimeoutExpired(cmd="gh", timeout=kw.get("timeout", 30))

    monkeypatch.setattr(rc.subprocess, "run", fake_run)
    with pytest.raises(rc.ReconcileError, match="timed out"):
        rc.query_pr_state("foo", "feature/bar")


def test_backfill_glob_reaper_escapes_metacharacters(tmp_path):
    """A project name with [ ] must not break the orphan-tmp glob."""
    from fno.megatron.reconcile import DriftRecord, PrState, backfill_completion

    # Project name has [ ] which would be glob metachars unescaped.
    completion_path = tmp_path / "completions" / "wave-1" / "weird[name].json"
    completion_path.parent.mkdir(parents=True, exist_ok=True)
    # Plant an orphan tmp matching our naming pattern.
    orphan = completion_path.parent / ".weird[name].json.99999-deadbeef.tmp"
    orphan.write_text("stale")

    record = DriftRecord(
        wave=1, project="weird[name]", completion_exists=False,
        completion_path=completion_path, branch_pattern="feature/test",
        pr_candidates=[
            PrState(
                number=1, url="u", state="MERGED",
                merged_at="2026-05-14T07:00:00Z", merge_commit_sha="abc",
            )
        ],
        state="missing-pr-merged",
    )
    assert backfill_completion(record, mission_id="ab-meta") is True
    # Orphan must be reaped by the escaped-glob.
    assert not orphan.exists()
    # The real file landed.
    assert completion_path.exists()


@pytest.mark.parametrize(
    "url,expected",
    [
        ("git@github.com:org/repo.name.git", "org/repo.name"),
        ("https://github.com/foo/bar.git", "foo/bar"),
        ("https://github.com/foo/my.project", "foo/my.project"),
        ("git@github.com:acme/svc-1.git", "acme/svc-1"),
        ("https://github.com/owner/repo.with.many.dots", "owner/repo.with.many.dots"),
    ],
)
def test_github_origin_re_allows_dots_in_repo(url, expected):
    """Regression: github.com regex must not drop dots from repo names (P2 codex + HIGH gemini round 3)."""
    from fno.megatron.reconcile import _GITHUB_ORIGIN_RE

    m = _GITHUB_ORIGIN_RE.search(url.strip())
    assert m is not None, f"regex failed to match {url}"
    assert f"{m.group(1)}/{m.group(2)}" == expected


def test_completion_validator_rejects_null_or_non_string_project_key(tmp_path):
    """P2 codex round 3: {'project': null} and {'project': 12} must not pass."""
    from fno.megatron.reconcile import _completion_payload_valid

    bad_null = tmp_path / "null.json"
    bad_null.write_text('{"project": null}')
    ok, reason = _completion_payload_valid(bad_null)
    assert ok is False
    assert "non-empty string" in (reason or "")

    bad_int = tmp_path / "int.json"
    bad_int.write_text('{"project": 12}')
    ok, reason = _completion_payload_valid(bad_int)
    assert ok is False
    assert "non-empty string" in (reason or "")

    bad_empty = tmp_path / "empty.json"
    bad_empty.write_text('{"project": ""}')
    ok, reason = _completion_payload_valid(bad_empty)
    assert ok is False
    assert "non-empty string" in (reason or "")

    good = tmp_path / "ok.json"
    good.write_text('{"project": "foo"}')
    ok, reason = _completion_payload_valid(good)
    assert ok is True and reason is None


def test_repo_slug_handles_list_shape_projects(tmp_path, monkeypatch):
    """P1 codex round 3: settings.yaml has projects as a LIST, not a dict.
    The resolver must walk the list, not silently skip."""
    import fno.megatron.reconcile as rc

    settings = tmp_path / "settings.yaml"
    fake_repo = tmp_path / "fake-repo"
    fake_repo.mkdir()
    # Make it a git repo with a github origin
    import subprocess as _sp
    _sp.run(["git", "-C", str(fake_repo), "init", "-q"], check=True)
    _sp.run(
        ["git", "-C", str(fake_repo), "remote", "add", "origin",
         "git@github.com:acme/widget.git"],
        check=True,
    )
    settings.write_text(
        f"""work:
  workspaces:
    acme:
      projects:
        - name: widget
          path: {fake_repo}
"""
    )
    monkeypatch.setattr(rc, "SETTINGS_PATH", settings)
    rc._clear_repo_slug_cache()

    assert rc._project_repo_slug("widget") == "acme/widget"


def test_repo_slug_refuses_cwd_fallback_when_path_missing(tmp_path, monkeypatch):
    """P2 codex round 3: a project with missing/empty `path` must NOT default to '.' (cwd)."""
    import fno.megatron.reconcile as rc

    settings = tmp_path / "settings.yaml"
    settings.write_text(
        """work:
  workspaces:
    acme:
      projects:
        - name: no_path_widget
        - name: empty_path_widget
          path: ""
        - name: whitespace_path_widget
          path: "   "
"""
    )
    monkeypatch.setattr(rc, "SETTINGS_PATH", settings)
    rc._clear_repo_slug_cache()

    assert rc._project_repo_slug("no_path_widget") is None
    rc._clear_repo_slug_cache()
    assert rc._project_repo_slug("empty_path_widget") is None
    rc._clear_repo_slug_cache()
    assert rc._project_repo_slug("whitespace_path_widget") is None


def test_repo_slug_cache_avoids_repeated_io(tmp_path, monkeypatch):
    """MEDIUM gemini round 3: cache avoids re-reading settings.yaml + shelling out per call."""
    import fno.megatron.reconcile as rc

    settings = tmp_path / "settings.yaml"
    fake_repo = tmp_path / "fake-repo"
    fake_repo.mkdir()
    import subprocess as _sp
    _sp.run(["git", "-C", str(fake_repo), "init", "-q"], check=True)
    _sp.run(
        ["git", "-C", str(fake_repo), "remote", "add", "origin",
         "https://github.com/acme/cached.git"],
        check=True,
    )
    settings.write_text(
        f"""work:
  workspaces:
    acme:
      projects:
        - name: cached
          path: {fake_repo}
"""
    )
    monkeypatch.setattr(rc, "SETTINGS_PATH", settings)
    rc._clear_repo_slug_cache()

    calls = {"count": 0}
    real_run = rc.subprocess.run

    def counting_run(*a, **kw):
        calls["count"] += 1
        return real_run(*a, **kw)

    monkeypatch.setattr(rc.subprocess, "run", counting_run)

    # First call shells out; subsequent calls hit the cache.
    assert rc._project_repo_slug("cached") == "acme/cached"
    assert rc._project_repo_slug("cached") == "acme/cached"
    assert rc._project_repo_slug("cached") == "acme/cached"
    assert calls["count"] == 1, (
        f"expected 1 git remote call, got {calls['count']}"
    )


# Gemini leftover from PR #262: the (x or {}).get(...) chain in
# _project_repo_slug short-circuits on falsy values but lets truthy non-dict
# scalars (e.g. user wrote `work: foo` instead of a mapping) flow through and
# crash with AttributeError. The fix replaces the chain with explicit
# isinstance guards. These tests pin the no-crash behavior for each level.

def test_repo_slug_returns_none_when_root_not_dict(tmp_path, monkeypatch):
    """yaml.safe_load returns a list at the document root."""
    import fno.megatron.reconcile as rc

    settings = tmp_path / "settings.yaml"
    settings.write_text("- not\n- a\n- dict\n")
    monkeypatch.setattr(rc, "SETTINGS_PATH", settings)
    rc._clear_repo_slug_cache()

    assert rc._project_repo_slug("anything") is None


def test_repo_slug_returns_none_when_work_is_scalar(tmp_path, monkeypatch):
    """work is a string, not a mapping. Old chain crashed on .get('workspaces')."""
    import fno.megatron.reconcile as rc

    settings = tmp_path / "settings.yaml"
    settings.write_text("work: oopsie\n")
    monkeypatch.setattr(rc, "SETTINGS_PATH", settings)
    rc._clear_repo_slug_cache()

    assert rc._project_repo_slug("anything") is None


def test_repo_slug_returns_none_when_workspaces_is_scalar(tmp_path, monkeypatch):
    """workspaces is a string, not a mapping. Old chain crashed on .values()."""
    import fno.megatron.reconcile as rc

    settings = tmp_path / "settings.yaml"
    settings.write_text("work:\n  workspaces: also-oopsie\n")
    monkeypatch.setattr(rc, "SETTINGS_PATH", settings)
    rc._clear_repo_slug_cache()

    assert rc._project_repo_slug("anything") is None


def test_repo_slug_returns_none_when_settings_empty(tmp_path, monkeypatch):
    """yaml.safe_load returns None for an empty file. Used to be normalized
    via `or {}`; now caught by the isinstance guard at the same place."""
    import fno.megatron.reconcile as rc

    settings = tmp_path / "settings.yaml"
    settings.write_text("")
    monkeypatch.setattr(rc, "SETTINGS_PATH", settings)
    rc._clear_repo_slug_cache()

    assert rc._project_repo_slug("anything") is None
