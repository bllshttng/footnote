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

pytestmark = pytest.mark.usefixtures("_no_global_tick_events")


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


def _check(behind: int = 0, ahead: int = 0, notes=None):
    """A `canonical-check` verb stand-in: payload in, answer out."""
    built = notes
    if built is None:
        built = []
        if behind:
            built.append(f"local default branch {behind} behind origin")
        if ahead:
            built.append(f"local default branch {ahead} ahead of origin")

    def fake(payload):
        fake.payloads.append(dict(payload))
        return {"behind": behind, "ahead": ahead, "notes": built}

    fake.payloads = []
    return fake


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
        settings=_pm(), canonical_root=tmp_path, check=_check(), gh_list=_gh(rows)
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
        settings=_pm(), canonical_root=tmp_path, check=_check(), gh_list=_gh(rows)
    )
    assert st.state == "stale"
    assert "#49" in st.detail
    assert [r["sha"] for r in st.markerless] == ["bbb"]


def test_staleness_fetches_only_when_asked(tmp_path):
    """The 5-minute tick must not fetch; the human-facing doctor must."""
    tick = _check()
    sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, check=tick, gh_list=_gh([])
    )
    assert all(p.get("fetch") is False for p in tick.payloads)

    doctor = _check()
    sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, check=doctor,
        gh_list=_gh([]), fetch=True,
    )
    assert any(p.get("fetch") is True for p in doctor.payloads)


def test_staleness_stale_when_newest_merge_is_old_and_unmarked(tmp_path):
    rows = [_merged(50, "aaa", 48)]
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, check=_check(), gh_list=_gh(rows)
    )
    assert st.state == "stale"
    assert "#50" in st.detail  # AC2: the offending PR is named


def test_staleness_fresh_when_newest_merge_is_recent(tmp_path):
    """A merge from two minutes ago is not an outage - the tick has not run yet."""
    st = sc.sync_staleness(
        settings=_pm(),
        canonical_root=tmp_path,
        check=_check(),
        gh_list=_gh([_merged(50, "aaa", 0.03)]),
    )
    assert st.state == "fresh"
    assert st.markerless  # still swept eagerly, just not alarmed on


def test_staleness_stale_when_behind_origin(tmp_path):
    _stamp(tmp_path, "aaa")
    st = sc.sync_staleness(
        settings=_pm(),
        canonical_root=tmp_path,
        check=_check(behind=7),
        gh_list=_gh([_merged(50, "aaa", 1)]),
    )
    assert st.state == "stale"
    assert "7 behind" in st.detail


def test_staleness_stale_when_ahead_of_origin(tmp_path):
    """A clean canonical that is AHEAD blocks every fast-forward (x-a150)."""
    _stamp(tmp_path, "aaa")
    st = sc.sync_staleness(
        settings=_pm(),
        canonical_root=tmp_path,
        check=_check(ahead=1),
        gh_list=_gh([_merged(50, "aaa", 1)]),
    )
    assert st.state == "stale"
    assert "1 ahead" in st.detail


def test_staleness_unknown_when_gh_unavailable(tmp_path):  # AC3-ERR
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, check=_check(), gh_list=_gh(None)
    )
    assert st.state == "unknown"
    assert st.markerless == ()


def test_staleness_fresh_on_zero_merges(tmp_path):
    st = sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, check=_check(), gh_list=_gh([])
    )
    assert st.state == "fresh"


def test_staleness_never_fetches_on_the_tick(tmp_path):
    """A predicate that mutates the repo is not a predicate."""
    tick = _check()
    sc.sync_staleness(
        settings=_pm(), canonical_root=tmp_path, check=tick, gh_list=_gh([])
    )
    assert all(not p.get("fetch") for p in tick.payloads)


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


def test_canonical_check_unavailable_reads_as_clean(tmp_path, capsys, monkeypatch):
    """Fail-open: an unavailable verb reads as "not dirty, not ahead", with
    one stderr line and never a refusal."""
    import fno.rust_binary as rb

    def boom(*_a, **_k):
        raise rb.VerbUnavailable("fno-agents canonical-check exited 2: unknown verb")

    monkeypatch.setattr(rb, "verb_call", boom)
    assert sc._canonical_check({"canonical": str(tmp_path)}) == {}
    assert "canonical check unavailable" in capsys.readouterr().err


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
        settings=_pm(), canonical_root=tmp_path, check=_check(),
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
        settings=_pm(), canonical_root=tmp_path, check=_check(),
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
        settings=_pm(), canonical_root=tmp_path, check=_check(),
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
        check=_check(), gh_list=_gh(rows), sync=path_gated_sync,
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
        check=_check(), gh_list=_gh(rows), sync=real_sync,
    )
    assert res2.outcome == "synced" and res2.pr_number == 51


def test_catchup_reports_a_lying_marker_set(tmp_path):
    """Every marker present but the canonical still behind: nothing to sweep, so
    the outcome has to carry the reason rather than read as a flat 'fresh'."""
    _stamp(tmp_path, "aaa")
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, check=_check(behind=4),
        gh_list=_gh([_merged(50, "aaa", 30)]),
        sync=lambda pr, **_kw: pytest.fail("nothing markerless to sync"),
    )
    assert res.outcome == "fresh"
    assert "4 behind" in res.detail


def test_catchup_inert_when_auto_run_off(tmp_path):  # AC6-EDGE
    calls: list[int] = []
    res = sc.run_sync_catchup(
        settings=_pm(auto_run=False), canonical_root=tmp_path, check=_check(),
        gh_list=_gh([_merged(52, "ccc", 30)]),
        sync=lambda pr, **_kw: calls.append(pr) or 0,
    )
    assert res.outcome == "disabled"
    assert calls == []


def test_catchup_skips_on_gh_failure(tmp_path, capsys):  # AC3-ERR
    calls: list[int] = []
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, check=_check(),
        gh_list=_gh(None), sync=lambda pr, **_kw: calls.append(pr) or 0,
    )
    assert res.outcome == "unknown"
    assert calls == []
    assert len(capsys.readouterr().err.strip().splitlines()) == 1  # exactly one warning


def test_catchup_fresh_when_all_marked(tmp_path):
    _stamp(tmp_path, "ccc")
    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, check=_check(),
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
        settings=_pm(), canonical_root=tmp_path, check=_check(),
        gh_list=_gh([_merged(52, "ccc", 30), _merged(51, "bbb", 40)]),
        sync=lambda pr, shell_runner=None, **_kw: (
            shell_runner("git pull", str(tmp_path)), _stamp(tmp_path, "ccc"), 0
        )[2],
    )
    assert res.outcome == "synced"
    assert _marker(tmp_path, "bbb").exists()


def test_catchup_roots_come_from_the_graph_deduped(tmp_path, monkeypatch):
    """launchd starts the daemon in `/`, so there is no ambient project.

    A bare load_settings() there reads global config, where post_merge is
    unset - which silently disabled this whole leg. Roots come from every
    sidecar's cwd via one ``load_all()`` scan (task 2.1's guarded seam), not
    a direct graph.json read, and not filtered to open tracker state - a
    project whose nodes are all done/closed must still get swept.
    """
    import json

    from fno.pr_watch import cli as pw
    from fno.tracker import sidecar as sidecar_store

    alpha = tmp_path / "alpha"
    alpha.mkdir()
    sidecar_dir = tmp_path / "sidecars"
    sidecar_dir.mkdir()
    rows = {
        "a": str(alpha),
        "b": str(alpha),              # duplicate project
        "c": str(tmp_path / "gone"),  # deleted checkout
        "d": None,                     # node with no cwd
    }
    for node_id, cwd in rows.items():
        payload = {"id": node_id}
        if cwd is not None:
            payload["cwd"] = cwd
        (sidecar_dir / f"{node_id}.json").write_text(json.dumps(payload))
    monkeypatch.setattr(
        sidecar_store, "sidecar_path", lambda i: sidecar_dir / f"{i}.json"
    )
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    assert pw._catchup_roots() == [alpha]


def test_catchup_roots_survive_an_unreadable_sidecar_store(monkeypatch):
    from fno.pr_watch import cli as pw
    from fno.tracker import sidecar as sidecar_store

    def _blow_up():
        raise RuntimeError("corrupt")

    monkeypatch.setattr(sidecar_store, "load_all", _blow_up)
    assert pw._catchup_roots() == []


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
        (gs, "GRAPH_JSON", g),
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
        "outcome": "synced", "stale": False, "pr_number": 52, "swept": 3, "detail": ""
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


def test_catchup_progress_goes_to_stderr_not_stdout(tmp_path, capsys):
    """A --json caller parses stdout; the sync's progress echoes must not ride
    along with the document."""
    import typer

    rows = [_merged(52, "ccc", 30)]

    def sync(pr, shell_runner=None, **_kw):
        typer.echo(f"post-merge sync: running in {tmp_path} for ccc")
        shell_runner("git pull", str(tmp_path))
        _stamp(tmp_path, "ccc")
        return 0

    res = sc.run_sync_catchup(
        settings=_pm(), canonical_root=tmp_path, check=_check(),
        gh_list=_gh(rows), sync=sync,
    )
    captured = capsys.readouterr()
    assert res.outcome == "synced"
    assert captured.out == ""
    assert f"running in {tmp_path} for ccc" in captured.err


def test_reconcile_json_stdout_is_parseable_with_catchup_firing(tmp_graph, monkeypatch, tmp_path):
    """The daemon's merge_close arm parses reconcile --json stdout; a progress
    line printed there made every merged node read as an unparseable failure."""
    import json

    import typer
    from typer.testing import CliRunner

    import fno.config as config_mod
    import fno.paths as paths_mod
    from fno.graph import cli as gcli
    from fno.pr import _sync_canonical as sc_mod

    real_load = config_mod.load_settings

    def fake_load(*a, **kw):
        s = real_load(*a, **kw)
        pm = s.post_merge.model_copy(update={"auto_run": True})
        return s.model_copy(update={"post_merge": pm})

    monkeypatch.setattr(config_mod, "load_settings", fake_load)
    monkeypatch.setattr(paths_mod, "resolve_canonical_repo_root", lambda: tmp_path)
    st = sc.SyncStaleness("stale", ({"number": 52, "sha": "abc123def456"},), 0, "window")
    monkeypatch.setattr(sc_mod, "sync_staleness", lambda **_kw: st)

    def fake_sync_canonical(number, settings=None, canonical_root=None, shell_runner=None, **_kw):
        typer.echo(f"post-merge sync: running in {canonical_root} for abc123def456")
        if shell_runner is not None:
            shell_runner("git pull", str(canonical_root))
        _stamp(tmp_path, "abc123def456")
        return 0

    monkeypatch.setattr(sc_mod, "run_sync_canonical", fake_sync_canonical)

    res = CliRunner().invoke(gcli.cli, ["reconcile", "--json"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.stdout)
    assert payload["sync_catchup"]["outcome"] == "synced"
    assert payload["sync_catchup"]["pr_number"] == 52


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
