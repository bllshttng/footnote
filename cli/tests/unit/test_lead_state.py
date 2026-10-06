"""The lead session manifest and the freshness predicate that proves a walk ran."""
from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from fno.lead.state import (
    LeadManifestExists,
    last_run_is_fresh,
    parse_manifest,
    write_manifest,
)


def test_scope_keyed_manifest_paths_are_independent(tmp_path):
    import fno.lead.state as state

    path_for = getattr(state, "lead_manifest_path", None)
    assert path_for is not None, "scope-keyed lead manifest path is missing"

    epic = path_for("x-f3d0", state_root=tmp_path / ".fno")
    project = path_for("fno", state_root=tmp_path / ".fno")
    write_manifest(epic, scope="x-f3d0", harness_session_id="epic-session")
    write_manifest(project, scope="fno", harness_session_id="project-session")

    assert epic == tmp_path / ".fno" / "leads" / "x-f3d0.md"
    assert project == tmp_path / ".fno" / "leads" / "fno.md"
    assert parse_manifest(epic)["harness_session_id"] == "epic-session"
    assert parse_manifest(project)["harness_session_id"] == "project-session"


@pytest.mark.parametrize("scope", ["../escape", "a/b", r"a\b", ".."])
def test_scope_keyed_manifest_path_refuses_escape_spellings(tmp_path, scope):
    import fno.lead.state as state

    path_for = getattr(state, "lead_manifest_path", None)
    assert path_for is not None, "scope-keyed lead manifest path is missing"
    with pytest.raises(ValueError, match="scope"):
        path_for(scope, state_root=tmp_path / ".fno")


def test_live_registry_role_resolves_scope_path_after_session_id_changes(tmp_path):
    import fno.lead.state as state

    resolve = getattr(state, "resolve_lead_manifest_path", None)
    assert resolve is not None, "live role manifest resolver is missing"
    row = SimpleNamespace(
        status="live",
        role_scope="x-f3d0",
        harness="codex",
        harness_session_id="new-session",
        cc_session_id=None,
        short_id=None,
    )
    path = state.lead_manifest_path("x-f3d0", state_root=tmp_path / ".fno")
    write_manifest(path, scope="x-f3d0", harness_session_id="old-session")

    resolved, reason = resolve(
        "new-session",
        "codex",
        state_root=tmp_path / ".fno",
        registry=[row],
    )
    assert resolved == path
    assert reason == ""


def test_stale_scope_file_without_a_live_role_resolves_nothing(tmp_path):
    import fno.lead.state as state

    resolve = getattr(state, "resolve_lead_manifest_path", None)
    assert resolve is not None, "live role manifest resolver is missing"
    path = tmp_path / ".fno" / "leads" / "x-f3d0.md"
    write_manifest(path, scope="x-f3d0", harness_session_id="old-session")
    row = SimpleNamespace(
        status="exited",
        role_scope="x-f3d0",
        harness="codex",
        harness_session_id="old-session",
        cc_session_id=None,
        short_id=None,
    )

    resolved, reason = resolve(
        "old-session",
        "codex",
        state_root=tmp_path / ".fno",
        registry=[row],
    )
    assert resolved is None
    assert "exited" in reason and "terminal" in reason


def test_promotion_refreshes_scope_manifest_for_a_successor(monkeypatch, tmp_path):
    import fno.lead.state as state

    arm = getattr(state, "arm_lead_manifest", None)
    assert arm is not None, "promotion manifest arming is missing"
    monkeypatch.setattr(state, "lead_loop_enabled", lambda: True)
    state_root = tmp_path / ".fno"

    first = arm(
        "x-f3d0", "11111111-1111-4111-8111-111111111111", state_root=state_root
    )
    first_id = parse_manifest(first)["fno_id"]
    second = arm(
        "x-f3d0", "22222222-2222-4222-8222-222222222222", state_root=state_root
    )

    assert second == first
    fields = parse_manifest(second)
    assert fields["harness_session_id"] == "22222222-2222-4222-8222-222222222222"
    assert fields["fno_id"] != first_id


def test_arming_refuses_an_id_no_transcript_can_ever_match(monkeypatch, tmp_path):
    """A short id or row name arms a manifest the owner guard always rejects.

    The stop hook matches the manifest id against the transcript basename, and
    every harness names transcripts with a full uuid, so an 8-hex short id or
    a row name is a gate that arms dead. Arming must refuse it loudly.
    """
    import fno.lead.state as state

    arm = state.arm_lead_manifest
    monkeypatch.setattr(state, "lead_loop_enabled", lambda: True)
    for bogus in ("7c5dcf5d", "t-x1069-glm", ""):
        with pytest.raises(ValueError, match="transcript basename|harness session id"):
            arm("x-f3d0", bogus, state_root=tmp_path / ".fno")
    assert not (tmp_path / ".fno" / "leads" / "x-f3d0.md").exists()


def test_disabled_lead_loop_arms_no_manifest(monkeypatch, tmp_path):
    import fno.lead.state as state

    arm = getattr(state, "arm_lead_manifest", None)
    assert arm is not None, "promotion manifest arming is missing"
    monkeypatch.setattr(state, "lead_loop_enabled", lambda: False)

    assert arm("x-f3d0", "session", state_root=tmp_path / ".fno") is None
    assert not (tmp_path / ".fno" / "leads" / "x-f3d0.md").exists()


def test_disabled_promotion_neutralizes_stale_scope_state(monkeypatch, tmp_path):
    import fno.lead.state as state

    from fno.paths import space_dir

    # Seed the manifest where THIS owner resolves it: the space keyed on the
    # owner cwd's canonical root, the same derivation arm_lead_manifest uses.
    stale = state.lead_manifest_path("x-f3d0", state_root=space_dir(tmp_path / "repo"))
    write_manifest(stale, scope="x-f3d0", harness_session_id="old-session")
    monkeypatch.setattr(state, "lead_loop_enabled", lambda: False)

    assert state.arm_lead_manifest(
        "x-f3d0", "new-session", owner_cwd=str(tmp_path / "repo")
    ) is None
    assert not stale.exists()


def test_promotion_defaults_to_the_owner_repositories_state_root(monkeypatch, tmp_path):
    import fno.lead.state as state

    owner = tmp_path / "repo"
    owner.mkdir()
    monkeypatch.setattr(state, "lead_loop_enabled", lambda: True)

    path = state.arm_lead_manifest(
        "x-f3d0", "33333333-3333-4333-8333-333333333333", owner_cwd=str(owner)
    )

    from fno.paths import space_dir

    assert path == space_dir(owner) / "leads" / "x-f3d0.md"
    # owner_pid named the promoting CLI's pid and read false in both
    # directions; a manifest keys its holder on harness_session_id alone.
    assert "owner_pid" not in path.read_text()


def test_cleanup_does_not_delete_a_successors_refreshed_manifest(tmp_path):
    import fno.lead.state as state

    root = tmp_path / ".fno"
    path = state.lead_manifest_path("alpha", state_root=root)
    write_manifest(path, scope="alpha", harness_session_id="successor")

    assert state.remove_lead_manifest(
        "alpha", state_root=root, expected_harness_session_id="vacating-owner"
    ) is False
    assert parse_manifest(path)["harness_session_id"] == "successor"


def test_best_effort_cleanup_removes_only_the_named_scope(tmp_path):
    import fno.lead.state as state

    cleanup = getattr(state, "remove_lead_manifest", None)
    assert cleanup is not None, "best-effort role cleanup is missing"
    root = tmp_path / ".fno"
    first = state.lead_manifest_path("alpha", state_root=root)
    second = state.lead_manifest_path("beta", state_root=root)
    write_manifest(first, scope="alpha", harness_session_id="a")
    write_manifest(second, scope="beta", harness_session_id="b")

    assert cleanup("alpha", state_root=root) is True
    assert not first.exists()
    assert second.exists()


def test_a_crashed_lead_s_leftover_manifest_captures_nobody(tmp_path):
    """Expiry must not depend on the verb being called, and it does not.

    A lead that crashes never runs the expire verb, so its scope manifest
    stays on disk. The registry row is authority: the dead lead's own read
    resolves nothing (terminal row), and a successor session resolving over
    the same file also gets nothing (its row holds no role over that
    scope). If either read ever returned the path, a leftover file would
    capture a session that was never promoted by it.
    """
    import fno.lead.state as state

    resolve = state.resolve_lead_manifest_path
    root = tmp_path / ".fno"
    path = state.lead_manifest_path("x-f3d0", state_root=root)
    write_manifest(path, scope="x-f3d0", harness_session_id="dead-session")
    crashed_lead = SimpleNamespace(
        # A reconcile flipped the row terminal; the role fields stay stamped
        # because nothing ran to vacate them.
        status="orphaned",
        role_scope="x-f3d0",
        harness="claude",
        harness_session_id="dead-session",
        cc_session_id=None,
        short_id=None,
    )
    successor = SimpleNamespace(
        status="live",
        role_scope=None,
        harness="claude",
        harness_session_id="successor-session",
        cc_session_id=None,
        short_id=None,
    )

    dead_resolved, dead_reason = resolve(
        "dead-session", "claude", state_root=root, registry=[crashed_lead]
    )
    assert dead_resolved is None
    assert "orphaned" in dead_reason and "terminal" in dead_reason
    successor_resolved, successor_reason = resolve(
        "successor-session", "claude", state_root=root, registry=[successor]
    )
    assert successor_resolved is None
    assert "role_scope" in successor_reason and "unstamped" in successor_reason
    # The file is still there - inert, not deleted. Crash safety is row
    # authority, not cleanup.
    assert path.is_file()


def test_a_wrong_state_root_names_the_path_it_looked_for(tmp_path):
    """The defect this node is: a stamped role with --state-root pointing
    nowhere read as the same bare silence as an unpromoted row. The reason
    must name the path it looked for and the remedy, and the same row over
    the right root stays a clean resolve."""
    import fno.lead.state as state

    resolve = state.resolve_lead_manifest_path
    row = SimpleNamespace(
        status="live",
        role_scope="x-f3d0",
        harness="claude",
        harness_session_id="promoted-session",
        cc_session_id=None,
        short_id=None,
    )
    right_root = tmp_path / "space"
    path = state.lead_manifest_path("x-f3d0", state_root=right_root)
    write_manifest(path, scope="x-f3d0", harness_session_id="promoted-session")

    resolved, reason = resolve(
        "promoted-session",
        "claude",
        state_root=tmp_path / "elsewhere",
        registry=[row],
    )
    assert resolved is None
    assert str(tmp_path / "elsewhere" / "leads" / "x-f3d0.md") in reason
    assert "--state-root" in reason

    ok, ok_reason = resolve(
        "promoted-session", "claude", state_root=right_root, registry=[row]
    )
    assert ok == path
    assert ok_reason == ""


def test_a_missing_manifest_without_the_flag_skips_the_flag_advice(monkeypatch, tmp_path):
    """The 'omit --state-root' remedy only makes sense when one was passed.
    With the default root taken, the reason names the absent manifest and
    stops there instead of advising a flag the caller never used."""
    import fno.lead.state as state

    monkeypatch.setattr(state, "lead_state_root", lambda cwd=None: tmp_path)
    row = SimpleNamespace(
        status="live",
        role_scope="x-f3d0",
        harness="claude",
        harness_session_id="promoted-session",
        cc_session_id=None,
        short_id=None,
    )

    resolved, reason = state.resolve_lead_manifest_path(
        "promoted-session", "claude", registry=[row]
    )
    assert resolved is None
    assert "no manifest exists" in reason
    assert "--state-root" not in reason


def test_init_writes_a_manifest_carrying_the_fields_the_loop_reads(tmp_path):
    path = tmp_path / "lead-state.md"
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
    path = tmp_path / "lead-state.md"
    write_manifest(path, scope="first", harness_session_id="sess-1")
    before = path.read_text(encoding="utf-8")

    with pytest.raises(LeadManifestExists):
        write_manifest(path, scope="second", harness_session_id="sess-2")

    assert path.read_text(encoding="utf-8") == before


def test_force_replaces_the_manifest_for_a_deliberate_re_init(tmp_path):
    path = tmp_path / "lead-state.md"
    write_manifest(path, scope="first", harness_session_id="sess-1")
    write_manifest(path, scope="second", harness_session_id="sess-2", force=True)
    assert parse_manifest(path)["scope"] == "second"


def test_a_scope_with_a_quote_survives_the_round_trip(tmp_path):
    path = tmp_path / "lead-state.md"
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


def _terminated(ts, *, driver="lead", reason="NoWork"):
    return {
        "ts": ts,
        "type": "loop_terminated",
        "source": "loop",
        "data": {"driver": driver, "reason": reason},
    }


NOW = "2026-08-18T12:00:00Z"


def test_a_lead_termination_inside_the_window_is_fresh(tmp_path):
    path = _journal(tmp_path, _terminated("2026-08-18T02:00:00Z"))
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is True


def test_a_lead_termination_outside_the_window_is_stale(tmp_path):
    path = _journal(tmp_path, _terminated("2026-08-01T02:00:00Z"))
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is False


def test_an_empty_journal_is_not_fresh(tmp_path):
    """The predicate has to be a real freshness read, not a vacuous file test:
    an absent run is exactly what it exists to report."""
    path = _journal(tmp_path)
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is False


def test_a_target_termination_does_not_satisfy_the_lead_predicate(tmp_path):
    path = _journal(tmp_path, _terminated("2026-08-18T02:00:00Z", driver="target"))
    assert last_run_is_fresh(path, since_s=24 * 3600, now_iso=NOW) is False


def test_the_newest_lead_termination_wins_over_an_older_one(tmp_path):
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
    """Both arms end a lead walk. Reading only the runtime's event would report
    no lead walk right after a lead drained its board and exited."""
    path = _journal(
        tmp_path,
        {
            "ts": "2026-08-18T02:00:00Z",
            "type": "termination",
            "source": "hook",
            "data": {"driver": "lead", "reason": "NoWork", "session_id": "k-1"},
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
    from fno.lead.state import parse_window

    assert parse_window(window) == seconds


def test_an_unparseable_window_is_refused():
    from fno.lead.state import parse_window

    with pytest.raises(ValueError):
        parse_window("soon")


# --- the two refusals that make a role real -------------------------------


def _init(
    monkeypatch,
    tmp_path,
    *,
    enabled=True,
    harness_id="sess-1",
    scopes=("drain",),
    readiness_error=None,
    readiness_calls=None,
    popen_calls=None,
):
    """Run `fno agents lead init` in tmp_path and return (exit_code, stderr)."""
    import fno.lead.state as state
    from typer.testing import CliRunner

    from fno.lead.cli import lead_app

    def readiness(verb, args):
        if readiness_calls is not None:
            readiness_calls.append((verb, args))
        if readiness_error:
            return readiness_error, None
        return None, {"ready": True}

    class recording_popen:
        # Records only the term-hold arm; every other Popen (the graph
        # store's unstubbed primitive, store.py:568) delegates to the real
        # thing so init's settled-children read keeps working.
        def __new__(cls, argv, *args, **kwargs):
            argv = [str(a) for a in argv]
            if popen_calls is not None and argv[1:4] == ["agents", "mail", "hold"]:
                popen_calls.append(argv)
                import subprocess as _sp

                return _sp.Popen(
                    ["true"],
                    stdout=_sp.DEVNULL,
                    stderr=_sp.DEVNULL,
                )
            return real_popen(argv, *args, **kwargs)

    real_popen = __import__("subprocess").Popen
    monkeypatch.setattr("subprocess.Popen", recording_popen)
    monkeypatch.setattr(state, "lead_loop_enabled", lambda: enabled)
    monkeypatch.setattr("fno.rust_binary.call_binary_json", readiness)
    monkeypatch.chdir(tmp_path)
    (tmp_path / ".fno").mkdir(exist_ok=True)
    result = CliRunner().invoke(
        lead_app,
        ["init", *[arg for scope in scopes for arg in ("--scope", scope)],
         "--harness-session-id", harness_id],
    )
    return result.exit_code, result.output


def test_a_disabled_lead_loop_writes_no_manifest(monkeypatch, tmp_path):
    """`config.lead.enabled` must gate the loop, not just describe it.

    Every arm - the stop hook, `loop-check --driver lead`, and `LeadQueue` -
    arms on this manifest existing. So the manifest is the one chokepoint where
    the flag can gate all three. Before this, the flag was read ONLY by
    `fno agents autonomy status`: the corpus's "guard on one of N reachable paths"
    with N of zero, and a default-off lead still held sessions open.
    """
    code, _ = _init(monkeypatch, tmp_path, enabled=False)

    assert code == 3
    assert not (tmp_path / ".fno" / "leads" / "drain.md").exists()


def test_a_manifest_that_names_nobody_is_refused(monkeypatch, tmp_path):
    """The hook gates the session the manifest NAMES, so it must name one.

    An id-less manifest can be matched against no session. It then either gates
    every session in the checkout or none of them, and both readings are wrong.
    """
    code, out = _init(monkeypatch, tmp_path, harness_id="")

    assert code == 2
    assert "harness-session-id" in out
    assert not (tmp_path / ".fno" / "leads" / "drain.md").exists()


def test_an_enabled_named_lead_is_promoted(monkeypatch, tmp_path):
    """The positive control: both guards pass and the manifest lands."""
    code, _ = _init(monkeypatch, tmp_path)

    assert code == 0
    import fno.lead.state as state

    manifest = state.lead_manifest_path("drain")
    assert manifest.exists()
    assert "harness_session_id: sess-1" in manifest.read_text()


def test_rust_readiness_refusal_writes_no_role_manifest(monkeypatch, tmp_path):
    import fno.lead.state as state

    calls = []
    monkeypatch.setenv("FNO_HARNESS", "codex")
    code, out = _init(
        monkeypatch,
        tmp_path,
        readiness_error="Stop readiness is blocked: session-refresh-unverified",
        readiness_calls=calls,
    )

    assert code == 2
    assert "session-refresh-unverified" in out
    assert calls == [
        (
            "loop",
            ["readiness", "--scope", "drain", "--session", "sess-1", "--ensure-goal"],
        )
    ]
    assert not state.lead_manifest_path("drain", state_root=tmp_path / ".fno").exists()


def test_a_repeated_scope_roles_one_epic_set(monkeypatch, tmp_path):
    code, out = _init(monkeypatch, tmp_path, scopes=("x-4d9b", "x-119e"))

    assert code == 0, out
    import fno.lead.state as state

    assert state.lead_manifest_path("x-119e,x-4d9b").exists()
    assert "scope:  x-119e,x-4d9b" in out


def test_init_does_not_arm_a_between_beat_hold(monkeypatch, tmp_path):
    """Promoting a session must not put its mail on hold between check-ins."""
    popen_calls = []
    code, out = _init(monkeypatch, tmp_path, popen_calls=popen_calls)

    assert code == 0, out
    holds = [
        argv
        for argv in popen_calls
        if argv[1:4] == ["agents", "mail", "hold"]
    ]
    assert not holds, f"init armed a between-beat hold: {holds}"


def test_cancel_does_not_clear_a_user_mail_hold(monkeypatch, tmp_path):
    """Cancelling a role must preserve a conversation or user-set DND hold."""
    import fno.lead.state as state
    from typer.testing import CliRunner

    from fno.lead.cli import lead_app

    session = "22222222-2222-4222-8222-222222222222"
    monkeypatch.setattr(state, "lead_loop_enabled", lambda: True)
    state.arm_lead_manifest("drain", session, state_root=tmp_path / ".fno")
    # cancel resolves the manifest under the ambient state root; pin it.
    real_path = state.lead_manifest_path
    monkeypatch.setattr(
        state,
        "lead_manifest_path",
        lambda scope, **kw: real_path(scope, **{**kw, "state_root": tmp_path / ".fno"}),
    )
    popen_calls = []
    real_popen = __import__("subprocess").Popen

    class recording_popen:
        # Mail-hold changes are recorded; every other Popen runs for real.
        def __new__(cls, argv, *args, **kwargs):
            argv = [str(a) for a in argv]
            if popen_calls is not None and argv[1:2] == ["mail-hold"]:
                popen_calls.append(argv)
                import subprocess as _sp

                return _sp.Popen(["true"], stdout=_sp.DEVNULL, stderr=_sp.DEVNULL)
            return real_popen(argv, *args, **kwargs)

    monkeypatch.setattr("subprocess.Popen", recording_popen)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary", lambda: tmp_path / "fno-agents"
    )
    monkeypatch.chdir(tmp_path)
    result = CliRunner().invoke(lead_app, ["cancel", "--scope", "drain"])

    assert result.exit_code == 0, result.output
    assert not popen_calls, f"cancel changed mail holds: {popen_calls}"
