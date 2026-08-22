"""The king session manifest and the freshness predicate that proves a walk ran."""
from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from fno.king.state import (
    KingManifestExists,
    last_run_is_fresh,
    parse_manifest,
    write_manifest,
)


def test_scope_keyed_manifest_paths_are_independent(tmp_path):
    import fno.king.state as state

    path_for = getattr(state, "king_manifest_path", None)
    assert path_for is not None, "scope-keyed king manifest path is missing"

    epic = path_for("x-f3d0", state_root=tmp_path / ".fno")
    project = path_for("fno", state_root=tmp_path / ".fno")
    write_manifest(epic, scope="x-f3d0", harness_session_id="epic-session")
    write_manifest(project, scope="fno", harness_session_id="project-session")

    assert epic == tmp_path / ".fno" / "kings" / "x-f3d0.md"
    assert project == tmp_path / ".fno" / "kings" / "fno.md"
    assert parse_manifest(epic)["harness_session_id"] == "epic-session"
    assert parse_manifest(project)["harness_session_id"] == "project-session"


@pytest.mark.parametrize("scope", ["../escape", "a/b", r"a\b", ".."])
def test_scope_keyed_manifest_path_refuses_escape_spellings(tmp_path, scope):
    import fno.king.state as state

    path_for = getattr(state, "king_manifest_path", None)
    assert path_for is not None, "scope-keyed king manifest path is missing"
    with pytest.raises(ValueError, match="scope"):
        path_for(scope, state_root=tmp_path / ".fno")


def test_live_registry_crown_resolves_scope_path_after_session_id_changes(tmp_path):
    import fno.king.state as state

    resolve = getattr(state, "resolve_king_manifest_path", None)
    assert resolve is not None, "live crown manifest resolver is missing"
    row = SimpleNamespace(
        status="live",
        crown_scope="x-f3d0",
        harness="codex",
        harness_session_id="new-session",
        cc_session_id=None,
        short_id=None,
    )
    path = state.king_manifest_path("x-f3d0", state_root=tmp_path / ".fno")
    write_manifest(path, scope="x-f3d0", harness_session_id="old-session")

    assert resolve(
        "new-session",
        "codex",
        state_root=tmp_path / ".fno",
        registry=[row],
    ) == path


def test_stale_scope_file_without_a_live_crown_resolves_nothing(tmp_path):
    import fno.king.state as state

    resolve = getattr(state, "resolve_king_manifest_path", None)
    assert resolve is not None, "live crown manifest resolver is missing"
    path = tmp_path / ".fno" / "kings" / "x-f3d0.md"
    write_manifest(path, scope="x-f3d0", harness_session_id="old-session")
    row = SimpleNamespace(
        status="exited",
        crown_scope="x-f3d0",
        harness="codex",
        harness_session_id="old-session",
        cc_session_id=None,
        short_id=None,
    )

    assert resolve(
        "old-session",
        "codex",
        state_root=tmp_path / ".fno",
        registry=[row],
    ) is None


def test_coronation_refreshes_scope_manifest_for_a_successor(monkeypatch, tmp_path):
    import fno.king.state as state

    arm = getattr(state, "arm_king_manifest", None)
    assert arm is not None, "coronation manifest arming is missing"
    monkeypatch.setattr(state, "king_loop_enabled", lambda: True)
    state_root = tmp_path / ".fno"

    first = arm("x-f3d0", "old-session", state_root=state_root)
    first_id = parse_manifest(first)["fno_id"]
    second = arm("x-f3d0", "new-session", state_root=state_root)

    assert second == first
    fields = parse_manifest(second)
    assert fields["harness_session_id"] == "new-session"
    assert fields["fno_id"] != first_id


def test_disabled_king_loop_arms_no_manifest(monkeypatch, tmp_path):
    import fno.king.state as state

    arm = getattr(state, "arm_king_manifest", None)
    assert arm is not None, "coronation manifest arming is missing"
    monkeypatch.setattr(state, "king_loop_enabled", lambda: False)

    assert arm("x-f3d0", "session", state_root=tmp_path / ".fno") is None
    assert not (tmp_path / ".fno" / "kings" / "x-f3d0.md").exists()


def test_disabled_coronation_neutralizes_stale_scope_state(monkeypatch, tmp_path):
    import fno.king.state as state

    root = tmp_path / "repo" / ".fno"
    stale = state.king_manifest_path("x-f3d0", state_root=root)
    write_manifest(stale, scope="x-f3d0", harness_session_id="old-session")
    monkeypatch.setattr(state, "king_loop_enabled", lambda: False)

    assert state.arm_king_manifest(
        "x-f3d0", "new-session", owner_cwd=str(tmp_path / "repo")
    ) is None
    assert not stale.exists()


def test_coronation_defaults_to_the_owner_repositories_state_root(monkeypatch, tmp_path):
    import fno.king.state as state

    owner = tmp_path / "repo"
    owner.mkdir()
    monkeypatch.setattr(state, "king_loop_enabled", lambda: True)

    path = state.arm_king_manifest("x-f3d0", "session", owner_cwd=str(owner))

    assert path == owner / ".fno" / "kings" / "x-f3d0.md"


def test_cleanup_does_not_delete_a_successors_refreshed_manifest(tmp_path):
    import fno.king.state as state

    root = tmp_path / ".fno"
    path = state.king_manifest_path("alpha", state_root=root)
    write_manifest(path, scope="alpha", harness_session_id="successor")

    assert state.remove_king_manifest(
        "alpha", state_root=root, expected_harness_session_id="vacating-owner"
    ) is False
    assert parse_manifest(path)["harness_session_id"] == "successor"


def test_best_effort_cleanup_removes_only_the_named_scope(tmp_path):
    import fno.king.state as state

    cleanup = getattr(state, "remove_king_manifest", None)
    assert cleanup is not None, "best-effort crown cleanup is missing"
    root = tmp_path / ".fno"
    first = state.king_manifest_path("alpha", state_root=root)
    second = state.king_manifest_path("beta", state_root=root)
    write_manifest(first, scope="alpha", harness_session_id="a")
    write_manifest(second, scope="beta", harness_session_id="b")

    assert cleanup("alpha", state_root=root) is True
    assert not first.exists()
    assert second.exists()


def test_init_writes_a_manifest_carrying_the_fields_the_loop_reads(tmp_path):
    path = tmp_path / "king-state.md"
    write_manifest(path, scope="board drain", harness_session_id="sess-1")

    fields = parse_manifest(path)
    assert fields["scope"] == "board drain"
    assert fields["harness_session_id"] == "sess-1"
    assert fields["fno_id"]
    assert fields["created_at"].endswith("Z")
    assert int(fields["budget_max_iterations"]) > 0


def test_the_manifest_is_immutable_after_init(tmp_path):
    """Same rule as the target manifest: write-once, and a second init refuses
    rather than silently forking one session's identity in place."""
    path = tmp_path / "king-state.md"
    write_manifest(path, scope="first", harness_session_id="sess-1")
    before = path.read_text(encoding="utf-8")

    with pytest.raises(KingManifestExists):
        write_manifest(path, scope="second", harness_session_id="sess-2")

    assert path.read_text(encoding="utf-8") == before


def test_force_replaces_the_manifest_for_a_deliberate_re_init(tmp_path):
    path = tmp_path / "king-state.md"
    write_manifest(path, scope="first", harness_session_id="sess-1")
    write_manifest(path, scope="second", harness_session_id="sess-2", force=True)
    assert parse_manifest(path)["scope"] == "second"


def test_a_scope_with_a_quote_survives_the_round_trip(tmp_path):
    path = tmp_path / "king-state.md"
    write_manifest(path, scope='drain "x-e747" and friends', harness_session_id="s")
    assert parse_manifest(path)["scope"] == 'drain "x-e747" and friends'


def test_parsing_a_missing_manifest_returns_nothing(tmp_path):
    assert parse_manifest(tmp_path / "absent.md") == {}


# --- the freshness predicate ------------------------------------------------


def _journal(tmp_path, *events):
    path = tmp_path / "events.jsonl"
    path.write_text(
        "".join(json.dumps(e) + "\n" for e in events), encoding="utf-8"
    )
    return path


def _terminated(ts, *, driver="king", reason="NoWork"):
    return {
        "ts": ts,
        "type": "loop_terminated",
        "source": "loop",
        "data": {"driver": driver, "reason": reason},
    }


NOW = "2026-08-18T12:00:00Z"


def test_a_king_termination_inside_the_window_is_fresh(tmp_path):
    path = _journal(tmp_path, _terminated("2026-08-18T02:00:00Z"))
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is True


def test_a_king_termination_outside_the_window_is_stale(tmp_path):
    path = _journal(tmp_path, _terminated("2026-08-01T02:00:00Z"))
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is False


def test_an_empty_journal_is_not_fresh(tmp_path):
    """The predicate has to be a real freshness read, not a vacuous file test:
    an absent run is exactly what it exists to report."""
    path = _journal(tmp_path)
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is False


def test_a_target_termination_does_not_satisfy_the_king_predicate(tmp_path):
    path = _journal(tmp_path, _terminated("2026-08-18T02:00:00Z", driver="target"))
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is False


def test_the_newest_king_termination_wins_over_an_older_one(tmp_path):
    path = _journal(
        tmp_path,
        _terminated("2026-08-18T02:00:00Z"),
        _terminated("2026-08-01T02:00:00Z"),
    )
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is True


def test_a_corrupt_line_does_not_hide_a_real_termination(tmp_path):
    path = tmp_path / "events.jsonl"
    path.write_text(
        "{not json\n" + json.dumps(_terminated("2026-08-18T02:00:00Z")) + "\n",
        encoding="utf-8",
    )
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is True


def test_the_in_session_arms_termination_also_counts_as_a_walk(tmp_path):
    """Both arms end a king walk. Reading only the runtime's event would report
    no king walk right after a king drained its board and exited."""
    path = _journal(
        tmp_path,
        {
            "ts": "2026-08-18T02:00:00Z",
            "type": "termination",
            "source": "hook",
            "data": {"driver": "king", "reason": "NoWork", "session_id": "k-1"},
        },
    )
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is True


def test_a_target_termination_event_does_not_count(tmp_path):
    path = _journal(
        tmp_path,
        {
            "ts": "2026-08-18T02:00:00Z",
            "type": "termination",
            "source": "hook",
            "data": {"reason": "DonePRGreen", "session_id": "t-1"},
        },
    )
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is False


def test_a_missing_journal_is_not_fresh(tmp_path):
    assert last_run_is_fresh(tmp_path / "absent.jsonl", since_s=3600, now_iso=NOW) is False


@pytest.mark.parametrize(
    "window,seconds",
    [("24h", 24 * 3600), ("90m", 90 * 60), ("7d", 7 * 86400), ("30s", 30), ("3600", 3600)],
)
def test_window_parsing(window, seconds):
    from fno.king.state import parse_window

    assert parse_window(window) == seconds


def test_an_unparseable_window_is_refused():
    from fno.king.state import parse_window

    with pytest.raises(ValueError):
        parse_window("soon")


# --- the two refusals that make a crown real -------------------------------


def _init(monkeypatch, tmp_path, *, enabled=True, harness_id="sess-1"):
    """Run `fno agents king init` in tmp_path and return (exit_code, stderr)."""
    import fno.king.state as state
    from typer.testing import CliRunner

    from fno.king.cli import king_app

    monkeypatch.setattr(state, "king_loop_enabled", lambda: enabled)
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".fno").mkdir(exist_ok=True)
    result = CliRunner().invoke(
        king_app,
        ["init", "--scope", "drain", "--harness-session-id", harness_id],
    )
    return result.exit_code, result.output


def test_a_disabled_king_loop_writes_no_manifest(monkeypatch, tmp_path):
    """`config.king.enabled` must gate the loop, not just describe it.

    Every arm - the stop hook, `loop-check --driver king`, and `KingQueue` -
    arms on this manifest existing. So the manifest is the one chokepoint where
    the flag can gate all three. Before this, the flag was read ONLY by
    `fno agents autonomy status`: the corpus's "guard on one of N reachable paths"
    with N of zero, and a default-off king still held sessions open.
    """
    code, _ = _init(monkeypatch, tmp_path, enabled=False)

    assert code == 3
    assert not (tmp_path / ".fno" / "kings" / "drain.md").exists()


def test_a_manifest_that_names_nobody_is_refused(monkeypatch, tmp_path):
    """The hook gates the session the manifest NAMES, so it must name one.

    An id-less manifest can be matched against no session. It then either gates
    every session in the checkout or none of them, and both readings are wrong.
    """
    code, out = _init(monkeypatch, tmp_path, harness_id="")

    assert code == 2
    assert "harness-session-id" in out
    assert not (tmp_path / ".fno" / "kings" / "drain.md").exists()


def test_an_enabled_named_king_is_crowned(monkeypatch, tmp_path):
    """The positive control: both guards pass and the manifest lands."""
    code, _ = _init(monkeypatch, tmp_path)

    assert code == 0
    manifest = tmp_path / ".fno" / "kings" / "drain.md"
    assert manifest.exists()
    assert "harness_session_id: sess-1" in manifest.read_text()
