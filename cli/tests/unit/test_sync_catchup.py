"""Catch-up sweep + staleness alarm for the post-merge canonical sync.

The outage this covers was "the daemon runs but does nothing" for five straight
merges, so every assertion here is on ground truth (marker files on disk, call
counts) rather than on log prose - a passing log line is exactly what lied last
time.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.pr import _sync_canonical as sc


def _pm(**over):
    base = dict(
        sync_command="true",
        sync_paths=[],
        auto_run=True,
        catchup_window_days=3,
        sync_stale_hours=24,
    )
    base.update(over)
    return SimpleNamespace(post_merge=SimpleNamespace(**base))


def _merged(number: int, sha: str, hours_ago: float) -> dict:
    return {
        "number": number,
        "sha": sha,
        "merged_at": datetime.now(timezone.utc) - timedelta(hours=hours_ago),
    }


def _gh(rows):
    return lambda _canonical, _window: rows


def _git(behind: int | None):
    """A runner standing in for git: symbolic-ref then rev-list."""

    def runner(cmd, **_kw):
        if behind is None:
            return sc.Result(returncode=1, stdout="", stderr="no upstream")
        if cmd[:2] == ["git", "symbolic-ref"]:
            return sc.Result(returncode=0, stdout="origin/main\n", stderr="")
        return sc.Result(returncode=0, stdout=f"{behind}\n", stderr="")

    return runner


def _marker(root: Path, sha: str) -> Path:
    return root / ".fno" / "post-merge-synced" / sha


def _stamp(root: Path, sha: str) -> None:
    m = _marker(root, sha)
    m.parent.mkdir(parents=True, exist_ok=True)
    m.touch()


# --- sync_staleness ---------------------------------------------------------


def test_staleness_fresh_when_every_merge_is_marked(tmp_path):
    rows = [_merged(50, "aaa", 48), _merged(49, "bbb", 72)]
    _stamp(tmp_path, "aaa")
    _stamp(tmp_path, "bbb")
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh(rows)
    )
    assert st.state == "fresh"
    assert st.markerless == ()


def test_staleness_stale_on_an_older_markerless_merge(tmp_path):
    """A marked newest merge does NOT vouch for the merges behind it.

    run_sync_canonical marks a merge that misses the sync_paths globs without
    pulling, so treating an older markerless merge as cosmetic-because-the-head-
    is-marked would hide a code merge that was never pulled.
    """
    rows = [_merged(50, "aaa", 1), _merged(49, "bbb", 72)]
    _stamp(tmp_path, "aaa")  # newest marked - possibly by a path-gate skip
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh(rows)
    )
    assert st.state == "stale"
    assert "#49" in st.detail
    assert [r["sha"] for r in st.markerless] == ["bbb"]


def test_staleness_fetches_only_when_asked(tmp_path):
    """The 5-minute tick must not fetch; the human-facing doctor must."""
    seen: list[list[str]] = []

    def runner(cmd, **_kw):
        seen.append(list(cmd))
        return sc.Result(returncode=0, stdout="origin/main\n0\n", stderr="")

    sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=runner, gh_list=_gh([])
    )
    assert not any("fetch" in c for cmd in seen for c in cmd)

    seen.clear()
    sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=runner,
        gh_list=_gh([]), fetch=True,
    )
    assert any("fetch" in c for cmd in seen for c in cmd)


def test_staleness_stale_when_newest_merge_is_old_and_unmarked(tmp_path):
    rows = [_merged(50, "aaa", 48)]
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh(rows)
    )
    assert st.state == "stale"
    assert "#50" in st.detail  # AC2: the offending PR is named


def test_staleness_fresh_when_newest_merge_is_recent(tmp_path):
    """A merge from two minutes ago is not an outage - the tick has not run yet."""
    st = sc.sync_staleness(
        settings=_pm(),
        canonical_root=tmp_path,
        runner=_git(0),
        gh_list=_gh([_merged(50, "aaa", 0.03)]),
    )
    assert st.state == "fresh"
    assert st.markerless  # still swept eagerly, just not alarmed on


def test_staleness_stale_when_behind_origin(tmp_path):
    _stamp(tmp_path, "aaa")
    st = sc.sync_staleness(
        settings=_pm(),
        canonical_root=tmp_path,
        runner=_git(7),
        gh_list=_gh([_merged(50, "aaa", 1)]),
    )
    assert st.state == "stale"
    assert "7 behind" in st.detail


def test_staleness_unknown_when_gh_unavailable(tmp_path):  # AC3-ERR
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh(None)
    )
    assert st.state == "unknown"
    assert st.markerless == ()


def test_staleness_fresh_on_zero_merges(tmp_path):
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0), gh_list=_gh([])
    )
    assert st.state == "fresh"


def test_staleness_never_fetches(tmp_path):
    """A predicate that mutates the repo is not a predicate."""
    seen: list[list[str]] = []

    def runner(cmd, **_kw):
        seen.append(list(cmd))
        return sc.Result(returncode=0, stdout="origin/main\n0\n", stderr="")

    sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, runner=runner, gh_list=_gh([])
    )
    assert not any("fetch" in c for cmd in seen for c in cmd)


def test_gh_list_filters_by_window(tmp_path, monkeypatch):
    import json

    payload = json.dumps([
        {"number": 9, "mergedAt": "2020-01-01T00:00:00Z", "mergeCommit": {"oid": "old"}},
        {"number": 10, "mergedAt": None, "mergeCommit": {"oid": "nodate"}},
        {"number": 11, "mergedAt": _iso(1), "mergeCommit": {"oid": "new"}},
        {"number": 12, "mergedAt": _iso(2), "mergeCommit": None},
    ])
    monkeypatch.setattr(
        sc, "_run", lambda *_a, **_k: sc.Result(returncode=0, stdout=payload, stderr="")
    )
    rows = sc._default_gh_list(tmp_path, 3)
    assert [r["sha"] for r in rows] == ["new"]


def test_parse_iso_is_always_tz_aware():
    """A naive datetime would TypeError against the aware `now` on every compare."""
    for raw in ("2026-07-21T10:00:00Z", "  2026-07-21T10:00:00Z  ", "2026-07-21T10:00:00"):
        dt = sc._parse_iso(raw)
        assert dt is not None and dt.tzinfo is not None, raw
        assert (datetime.now(timezone.utc) - dt).total_seconds() > 0  # comparable
    assert sc._parse_iso("not-a-date") is None
    assert sc._parse_iso(None) is None


def test_behind_count_ignores_empty_symbolic_ref(tmp_path):
    """A zero exit with empty stdout is not an answer; it would malform the range."""
    ranges: list[str] = []

    def runner(cmd, **_kw):
        if cmd[:2] == ["git", "symbolic-ref"]:
            return sc.Result(returncode=0, stdout="   \n", stderr="")
        ranges.append(cmd[-1])
        return sc.Result(returncode=0, stdout="3\n", stderr="")

    assert sc._behind_count(tmp_path, runner) == 3
    assert ranges == ["main..origin/main"]


def _iso(hours_ago: float) -> str:
    return (
        datetime.now(timezone.utc) - timedelta(hours=hours_ago)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- run_sync_catchup -------------------------------------------------------


def test_catchup_syncs_newest_and_stamps_the_rest(tmp_path):  # AC1-HP
    rows = [_merged(52, "ccc", 30), _merged(51, "bbb", 40), _merged(50, "aaa", 50)]
    calls: list[int] = []

    def sync(pr, shell_runner=None, **_kw):
        calls.append(pr)
        shell_runner("git pull", str(tmp_path))  # a real sync enters the shell
        _stamp(tmp_path, "ccc")
        return 0

    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(rows), sync=sync,
    )
    assert calls == [52]  # newest only, one pull covers the rest
    assert res.outcome == "synced" and res.swept == 2
    for sha in ("aaa", "bbb", "ccc"):
        assert _marker(tmp_path, sha).exists()


def test_catchup_failure_stamps_nothing_and_retries(tmp_path):  # AC4-ERR
    rows = [_merged(52, "ccc", 30), _merged(51, "bbb", 40)]
    calls: list[int] = []

    def sync(pr, **_kw):
        calls.append(pr)
        return 1

    kw = dict(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(rows), sync=sync,
    )
    assert sc.run_sync_catchup(**kw).outcome == "failed"
    assert not _marker(tmp_path, "ccc").exists()
    assert not _marker(tmp_path, "bbb").exists()

    sc.run_sync_catchup(**kw)  # a later firing retries the same merge
    assert calls == [52, 52]


def test_catchup_declined_sync_stamps_nothing(tmp_path):  # AC7-EDGE
    """A claim-held loser exits 0 without syncing; it must not backdate markers."""
    rows = [_merged(52, "ccc", 30), _merged(51, "bbb", 40)]
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(rows), sync=lambda pr, **_kw: 0,  # exits 0, writes no marker
    )
    assert res.outcome == "skipped"
    assert not _marker(tmp_path, "bbb").exists()


def test_catchup_does_not_stamp_when_newest_merge_needed_no_pull(tmp_path):
    """The regression this feature would otherwise have re-introduced.

    run_sync_canonical writes a marker and returns 0 for a merge that misses the
    sync_paths globs, having pulled nothing. Stamping the older merges off that
    marker would mark real code merges synced without ever pulling them - the
    exact silent skip the catch-up exists to end. Proof-of-pull is whether
    sync_command's shell was entered, so a path-gated newest leaves the rest
    markerless for the next sweep.
    """
    rows = [_merged(52, "docs", 30), _merged(51, "code", 40)]

    def path_gated_sync(pr, shell_runner=None, **_kw):
        _stamp(tmp_path, "docs")  # marker written, shell never entered
        return 0

    res = sc.run_sync_catchup(
        settings=_pm(sync_paths=["cli/**"]), canonical_root=tmp_path,
        runner=_git(0), gh_list=_gh(rows), sync=path_gated_sync,
    )
    assert res.outcome == "marked"
    assert not _marker(tmp_path, "code").exists()

    # The next sweep picks the newest REMAINING merge and pulls for real.
    def real_sync(pr, shell_runner=None, **_kw):
        shell_runner("git pull", str(tmp_path))
        _stamp(tmp_path, "code")
        return 0

    res2 = sc.run_sync_catchup(
        settings=_pm(sync_paths=["cli/**"]), canonical_root=tmp_path,
        runner=_git(0), gh_list=_gh(rows), sync=real_sync,
    )
    assert res2.outcome == "synced" and res2.pr_number == 51


def test_catchup_reports_a_lying_marker_set(tmp_path):
    """Every marker present but the canonical still behind: nothing to sweep, so
    the outcome has to carry the reason rather than read as a flat 'fresh'."""
    _stamp(tmp_path, "aaa")
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(4),
        gh_list=_gh([_merged(50, "aaa", 30)]),
        sync=lambda pr, **_kw: pytest.fail("nothing markerless to sync"),
    )
    assert res.outcome == "fresh"
    assert "4 behind" in res.detail


def test_catchup_inert_when_auto_run_off(tmp_path):  # AC6-EDGE
    calls: list[int] = []
    res = sc.run_sync_catchup(
        settings=_pm(auto_run=False), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh([_merged(52, "ccc", 30)]),
        sync=lambda pr, **_kw: calls.append(pr) or 0,
    )
    assert res.outcome == "disabled"
    assert calls == []


def test_catchup_skips_on_gh_failure(tmp_path, capsys):  # AC3-ERR
    calls: list[int] = []
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh(None), sync=lambda pr, **_kw: calls.append(pr) or 0,
    )
    assert res.outcome == "unknown"
    assert calls == []
    assert len(capsys.readouterr().err.strip().splitlines()) == 1  # exactly one warning


def test_catchup_fresh_when_all_marked(tmp_path):
    _stamp(tmp_path, "ccc")
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh([_merged(52, "ccc", 30)]),
        sync=lambda pr, **_kw: pytest.fail("must not sync a current canonical"),
    )
    assert res.outcome == "fresh"


def test_catchup_survives_a_wedged_events_bus(tmp_path, monkeypatch):  # AC5-FR
    """The cure must survive the disease: the outage was an events-bus deadlock."""
    import fno.events as events

    monkeypatch.setattr(
        events, "append_event",
        lambda *_a, **_k: (_ for _ in ()).throw(RuntimeError("lock timeout")),
        raising=False,
    )
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, runner=_git(0),
        gh_list=_gh([_merged(52, "ccc", 30), _merged(51, "bbb", 40)]),
        sync=lambda pr, shell_runner=None, **_kw: (
            shell_runner("git pull", str(tmp_path)), _stamp(tmp_path, "ccc"), 0
        )[2],
    )
    assert res.outcome == "synced"
    assert _marker(tmp_path, "bbb").exists()


# --- the three firing legs --------------------------------------------------


def _tick_settings(**over):
    s = _pm(**over)
    s.pr_watch = SimpleNamespace(max_age_days=7, retries=3)
    s.recovery = SimpleNamespace(enabled=False)
    return s


def _run_tick(monkeypatch, catchup_result):
    """Invoke `fno pr-watch tick` with everything but the catch-up leg stubbed."""
    from typer.testing import CliRunner

    from fno.pr_watch import cli as pw
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(pw, "load_settings", _tick_settings)
    monkeypatch.setattr(pw, "load_settings_for_repo", lambda _root: _tick_settings())
    # The daemon is global, so the leg sweeps roots from the graph, not cwd.
    monkeypatch.setattr(pw, "_catchup_roots", lambda: [Path("/tmp/proj-alpha")])
    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick",
        lambda **_kw: SimpleNamespace(
            lock_held=False, lock_holder=None, open_prs=0, acted=0, skipped=0
        ),
    )
    calls: list[str] = []
    monkeypatch.setattr(pw, "_notify_parked", lambda msg: calls.append(msg))
    monkeypatch.setattr(sc_mod, "run_sync_catchup", lambda **_kw: catchup_result)
    result = CliRunner().invoke(pw.cli, ["tick"])
    return result, calls


def test_catchup_roots_come_from_the_graph_deduped(tmp_path, monkeypatch):
    """launchd starts the daemon in `/`, so there is no ambient project.

    A bare load_settings() there reads global config, where post_merge is
    unset - which silently disabled this whole leg.
    """
    from fno.pr_watch import cli as pw

    alpha = tmp_path / "alpha"
    alpha.mkdir()
    monkeypatch.setattr(
        "fno.graph.store.read_graph",
        lambda _p: [
            {"cwd": str(alpha)},
            {"cwd": str(alpha)},              # duplicate project
            {"cwd": str(tmp_path / "gone")},  # deleted checkout
            {},                                # node with no cwd
        ],
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    assert pw._catchup_roots() == [alpha]


def test_catchup_roots_survive_an_unreadable_graph(monkeypatch):
    from fno.pr_watch import cli as pw

    monkeypatch.setattr(
        "fno.graph.store.read_graph",
        lambda _p: (_ for _ in ()).throw(RuntimeError("corrupt")),
    )
    assert pw._catchup_roots() == []


def test_tick_sweeps_each_project_independently(monkeypatch, tmp_path):
    from typer.testing import CliRunner

    from fno.pr_watch import cli as pw
    from fno.pr import _sync_canonical as sc_mod

    roots = []
    for name in ("alpha", "beta"):
        d = tmp_path / name
        d.mkdir()
        roots.append(d)

    seen: list[Path] = []
    monkeypatch.setattr(pw, "load_settings", _tick_settings)
    monkeypatch.setattr(pw, "load_settings_for_repo", lambda _r: _tick_settings())
    monkeypatch.setattr(pw, "_catchup_roots", lambda: roots)
    monkeypatch.setattr(pw, "_notify_parked", lambda _m: None)
    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick",
        lambda **_kw: SimpleNamespace(
            lock_held=False, lock_holder=None, open_prs=0, acted=0, skipped=0
        ),
    )

    def catchup(*, canonical_root=None, **_kw):
        seen.append(canonical_root)
        return sc.CatchupResult("synced", 1)

    monkeypatch.setattr(sc_mod, "run_sync_catchup", catchup)
    res = CliRunner().invoke(pw.cli, ["tick"])
    assert res.exit_code == 0
    assert seen == roots  # scoped per project, not once against the daemon's cwd
    assert "sync catch-up [alpha]" in res.output
    assert "sync catch-up [beta]" in res.output


def test_tick_continues_after_one_project_raises(monkeypatch, tmp_path):
    from typer.testing import CliRunner

    from fno.pr_watch import cli as pw
    from fno.pr import _sync_canonical as sc_mod

    bad, good = tmp_path / "bad", tmp_path / "good"
    bad.mkdir()
    good.mkdir()
    monkeypatch.setattr(pw, "load_settings", _tick_settings)
    monkeypatch.setattr(pw, "load_settings_for_repo", lambda _r: _tick_settings())
    monkeypatch.setattr(pw, "_catchup_roots", lambda: [bad, good])
    monkeypatch.setattr(pw, "_notify_parked", lambda _m: None)
    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick",
        lambda **_kw: SimpleNamespace(
            lock_held=False, lock_holder=None, open_prs=0, acted=0, skipped=0
        ),
    )

    def catchup(*, canonical_root=None, **_kw):
        if canonical_root == bad:
            raise RuntimeError("boom")
        return sc.CatchupResult("synced", 1)

    monkeypatch.setattr(sc_mod, "run_sync_catchup", catchup)
    res = CliRunner().invoke(pw.cli, ["tick"])
    assert res.exit_code == 0
    assert "sync catch-up [good]: synced" in res.output


def test_tick_alarms_on_detected_and_unresolved(monkeypatch):  # AC8-HP
    res, notes = _run_tick(
        monkeypatch,
        sc.CatchupResult("failed", 52, detail="exit 1", stale=True),
    )
    assert res.exit_code == 0
    assert "ALARM" in res.output and "proj-alpha" in res.output
    assert len(notes) == 1


def test_tick_does_not_alarm_on_a_failure_that_is_not_yet_stale(monkeypatch):
    """A merge from two minutes ago whose retry is seconds away is not an outage."""
    res, notes = _run_tick(
        monkeypatch, sc.CatchupResult("failed", 52, detail="exit 1", stale=False)
    )
    assert "sync catch-up [proj-alpha]: failed" in res.output  # still reported
    assert "ALARM" not in res.output
    assert notes == []


def test_tick_alarms_when_markers_lie(monkeypatch):
    """Nothing to sweep, yet the canonical is proven behind. Detected, unresolved."""
    res, notes = _run_tick(
        monkeypatch,
        sc.CatchupResult("fresh", detail="local default branch 5 behind origin", stale=True),
    )
    assert "ALARM" in res.output
    assert len(notes) == 1


def test_tick_is_silent_when_catchup_succeeds(monkeypatch):  # AC8-HP, no cry-wolf
    res, notes = _run_tick(
        monkeypatch, sc.CatchupResult("synced", 52, swept=2, stale=True)
    )
    assert res.exit_code == 0
    assert "sync catch-up [proj-alpha]: synced" in res.output  # the leg ran
    assert "ALARM" not in res.output
    assert notes == []


def test_tick_survives_a_catchup_exception(monkeypatch):  # US2 non-fatal
    from typer.testing import CliRunner

    from fno.pr_watch import cli as pw
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(pw, "load_settings", _tick_settings)
    monkeypatch.setattr(
        "fno.pr_watch._dispatch.tick",
        lambda **_kw: SimpleNamespace(
            lock_held=False, lock_holder=None, open_prs=0, acted=0, skipped=0
        ),
    )
    monkeypatch.setattr(
        sc_mod, "run_sync_catchup",
        lambda **_kw: (_ for _ in ()).throw(RuntimeError("boom")),
    )
    res = CliRunner().invoke(pw.cli, ["tick"])
    assert res.exit_code == 0
    assert "pr-watch tick:" in res.output  # the tick itself still reported


def test_tick_prints_nothing_when_disabled(monkeypatch):  # AC6-EDGE
    res, notes = _run_tick(monkeypatch, sc.CatchupResult("disabled"))
    assert "sync catch-up" not in res.output
    assert notes == []


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    for mod, attr, val in (
        (gc, "GRAPH_JSON", g),
        (gc, "GRAPH_MD", tmp_path / "graph.md"),
        (gc, "GRAPH_HTML", tmp_path / "graph.html"),
        (gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json"),
        (gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock"),
        (gs, "GRAPH_JSON", g),
        (gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock"),
    ):
        monkeypatch.setattr(mod, attr, val)
    return g


def _reconcile_json(monkeypatch, catchup_result):
    """`fno backlog reconcile --json` with only the catch-up leg live."""
    import json

    from typer.testing import CliRunner

    from fno.graph import cli as gcli
    from fno.pr import _sync_canonical as sc_mod

    if catchup_result is None:
        monkeypatch.setattr(
            sc_mod, "run_sync_catchup",
            lambda **_kw: (_ for _ in ()).throw(RuntimeError("gh exploded")),
        )
    else:
        monkeypatch.setattr(sc_mod, "run_sync_catchup", lambda **_kw: catchup_result)
    res = CliRunner().invoke(gcli.cli, ["reconcile", "--json"])
    assert res.exit_code == 0, res.output
    return json.loads(res.stdout)


def test_reconcile_reports_catchup_in_json(tmp_graph, monkeypatch):  # US3
    """The SessionStart hook runs reconcile --json and discards stderr, so the
    outcome has to ride the payload or it is unobservable."""
    payload = _reconcile_json(
        monkeypatch, sc.CatchupResult("synced", 52, swept=3)
    )
    assert payload["sync_catchup"] == {
        "outcome": "synced", "pr_number": 52, "swept": 3, "detail": ""
    }


def test_reconcile_survives_a_catchup_exception(tmp_graph, monkeypatch):
    payload = _reconcile_json(monkeypatch, None)
    assert payload["sync_catchup"]["outcome"] == "error"
    assert "gh exploded" in payload["sync_catchup"]["detail"]


def test_reconcile_dry_run_never_syncs(tmp_graph, monkeypatch):  # AC6-EDGE
    import json

    from typer.testing import CliRunner

    from fno.graph import cli as gcli
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(
        sc_mod, "run_sync_catchup",
        lambda **_kw: pytest.fail("a preview must mutate nothing"),
    )
    res = CliRunner().invoke(gcli.cli, ["reconcile", "--json", "--dry-run"])
    assert res.exit_code == 0
    assert json.loads(res.stdout)["sync_catchup"]["outcome"] == "not-run"


def test_doctor_reports_staleness(monkeypatch):  # AC2-HP
    from fno import doctor
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(
        sc_mod, "sync_staleness",
        lambda **_kw: sc.SyncStaleness("stale", (), 7, "PR #50 merged 48h ago"),
    )
    health = doctor._post_merge_sync_health()
    assert health["stale"] is True
    assert "#50" in health["detail"]


def test_doctor_health_never_raises(monkeypatch):
    from fno import doctor
    from fno.pr import _sync_canonical as sc_mod

    monkeypatch.setattr(
        sc_mod, "sync_staleness",
        lambda **_kw: (_ for _ in ()).throw(RuntimeError("gh exploded")),
    )
    assert doctor._post_merge_sync_health() == {
        "state": "unknown", "stale": False, "behind": None, "detail": ""
    }
