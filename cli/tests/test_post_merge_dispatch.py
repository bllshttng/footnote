"""Tests for post-merge-ritual auto-dispatch + merge-SHA threading (x-47be, wave 2).

Covers task 2.1 (dispatch), 2.2 (at-most-one dedup keyed on merge SHA), and 2.3
(concurrency + location + SHA threading). The spawn seam is injected so no real
`fno agents spawn` fires.
"""
from __future__ import annotations

from pathlib import Path

from fno.graph._reconcile import (
    MergeDriftRecord,
    PrMergeState,
    dispatch_post_merge_ritual,
    query_pr_merge_state,
    scan_merge_drift,
)


class _Spawn:
    def __init__(self, short_id="abc123", fail=False):
        self.short_id = short_id
        self.fail = fail
        self.calls: list[tuple[int, str]] = []

    def __call__(self, pr_number: int, cwd: str) -> str:
        self.calls.append((pr_number, cwd))
        if self.fail:
            raise RuntimeError("spawn boom")
        return self.short_id


# --- task 2.1: dispatch gating -------------------------------------------


def test_disabled_never_spawns(tmp_path):
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="sha1", auto_run=False, canonical_root=tmp_path, spawn=spawn
    )
    assert res.outcome == "disabled"
    assert spawn.calls == []


def test_dispatch_spawns_once_and_marks(tmp_path):
    spawn = _Spawn(short_id="xy")
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaA", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert res.outcome == "dispatched"
    assert res.short_id == "xy"
    assert spawn.calls == [(7, str(tmp_path))]
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaA").exists()


# --- task 2.2 / AC1-FR: at-most-one per merge SHA ------------------------


def test_second_dispatch_same_sha_is_noop(tmp_path):
    spawn = _Spawn()
    first = dispatch_post_merge_ritual(
        7, dedup_key="shaB", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    second = dispatch_post_merge_ritual(
        7, dedup_key="shaB", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert first.outcome == "dispatched"
    assert second.outcome == "already-dispatched"
    assert second.detail == "marker-exists"  # genuine completed dedup, not in-flight
    assert len(spawn.calls) == 1  # exactly one worker for the merge SHA


def test_lock_contention_is_distinguished_from_marker_exists(tmp_path, monkeypatch):
    """A concurrent holder (ClaimHeldByOther) is in-flight, NOT done: it must be
    tagged 'lock-contention' so a polling caller does not treat it as completed."""
    from fno import claims

    def _held(*a, **kw):
        raise claims.ClaimHeldByOther("other", pid=999, host="h", key="k")

    monkeypatch.setattr(claims, "acquire_claim", _held)
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaLC", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert res.outcome == "already-dispatched"
    assert res.detail == "lock-contention"
    assert spawn.calls == []  # never spawned; another holder owns the lock


def test_distinct_shas_each_dispatch(tmp_path):
    spawn = _Spawn()
    dispatch_post_merge_ritual(
        7, dedup_key="shaC", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    dispatch_post_merge_ritual(
        8, dedup_key="shaD", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert len(spawn.calls) == 2


def test_spawn_failure_drops_marker_for_retry(tmp_path):
    spawn = _Spawn(fail=True)
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaE", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert res.outcome == "spawn-failed"
    # marker removed so the next reconcile retries
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaE").exists()
    # a retry now succeeds
    ok = _Spawn()
    res2 = dispatch_post_merge_ritual(
        7, dedup_key="shaE", auto_run=True, canonical_root=tmp_path, spawn=ok
    )
    assert res2.outcome == "dispatched"
    assert len(ok.calls) == 1


def test_missing_sha_falls_back_to_pr_key(tmp_path):
    spawn = _Spawn()
    dispatch_post_merge_ritual(
        42, dedup_key=None, auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "pr-42").exists()


# --- x-490d US1: the dispatched ritual runs autonomously (no prompt) -----


def test_spawn_worker_prompt_carries_autonomous(monkeypatch):
    """A dispatched `claude --bg` worker is interactive, so it stalls at the
    ritual's first human-prompt slot unless told it has no operator. The signal
    rides the worker's initial prompt (the one channel that always reaches its
    LLM), so the constructed spawn command must end in `... merged <n> autonomous`."""
    from fno.graph import _reconcile

    captured: dict = {}

    class _Proc:
        returncode = 0
        stdout = '{"short_id": "abc123"}'
        stderr = ""

    def _fake_run(cmd, **_kw):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(_reconcile.subprocess, "run", _fake_run)
    sid = _reconcile._spawn_post_merge_worker(42, "/tmp/canon")

    assert sid == "abc123"
    assert captured["cmd"][-1] == "/fno:pr merged 42 autonomous"


def _stub_spawn_run(monkeypatch, captured):
    from fno.graph import _reconcile

    class _Proc:
        returncode = 0
        stdout = '{"short_id": "abc123"}'
        stderr = ""

    def _fake_run(cmd, **_kw):
        captured["cmd"] = cmd
        return _Proc()

    monkeypatch.setattr(_reconcile.subprocess, "run", _fake_run)


def _model_of(cmd):
    return cmd[cmd.index("--model") + 1] if "--model" in cmd else None


def test_spawn_worker_uses_config_model_default(monkeypatch):
    """The bg worker runs on config.post_merge.model (default sonnet), not the
    claude account default (Fable)."""
    import types
    import fno.config as _config
    from fno.graph import _reconcile

    captured: dict = {}
    _stub_spawn_run(monkeypatch, captured)
    settings = types.SimpleNamespace(post_merge=types.SimpleNamespace(model="claude-sonnet-5"))
    monkeypatch.setattr(_config, "load_settings_for_repo", lambda _p: settings)

    _reconcile._spawn_post_merge_worker(42, "/tmp/canon")
    assert _model_of(captured["cmd"]) == "claude-sonnet-5"


def test_spawn_worker_honors_operator_override(monkeypatch):
    """An operator's config.post_merge.model override reaches the spawn cmd."""
    import types
    import fno.config as _config
    from fno.graph import _reconcile

    captured: dict = {}
    _stub_spawn_run(monkeypatch, captured)
    settings = types.SimpleNamespace(post_merge=types.SimpleNamespace(model="claude-opus-4-8"))
    monkeypatch.setattr(_config, "load_settings_for_repo", lambda _p: settings)

    _reconcile._spawn_post_merge_worker(42, "/tmp/canon")
    assert _model_of(captured["cmd"]) == "claude-opus-4-8"


def test_spawn_worker_config_failure_falls_open(monkeypatch):
    """A config-load failure falls open to the sonnet default and never crashes
    the (strictly non-fatal) dispatch."""
    import fno.config as _config
    from fno.graph import _reconcile

    captured: dict = {}
    _stub_spawn_run(monkeypatch, captured)

    def _boom(_p):
        raise RuntimeError("corrupt config")

    monkeypatch.setattr(_config, "load_settings_for_repo", _boom)

    sid = _reconcile._spawn_post_merge_worker(42, "/tmp/canon")
    assert sid == "abc123"
    assert _model_of(captured["cmd"]) == "claude-sonnet-5"


# --- task 2.3: location - dispatch marker lands under canonical ----------


def test_node_cwd_resolves_target_canonical(tmp_path, monkeypatch):
    """P1 regression: a full-graph reconcile closing a foreign-repo node must
    resolve THAT repo's canonical from node_cwd, not the caller's cwd."""
    import fno.paths as paths

    target_canonical = tmp_path / "repoB"
    target_canonical.mkdir()
    monkeypatch.setattr(paths, "resolve_canonical_worktree", lambda cwd=None: target_canonical)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))

    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaG", auto_run=True, node_cwd=str(tmp_path / "repoB-worktree"), spawn=spawn
    )
    assert res.outcome == "dispatched"
    # marker + spawn cwd both land under the RESOLVED target canonical, not cwd
    assert (target_canonical / ".fno" / "post-merge-dispatched" / "shaG").exists()
    assert spawn.calls[0][1] == str(target_canonical)


def test_marker_under_provided_canonical_not_cwd(tmp_path):
    """The dispatch always targets the canonical root it is given, never the
    caller's cwd (a worktree run must still mark the canonical)."""
    canonical = tmp_path / "canonical"
    canonical.mkdir()
    spawn = _Spawn()
    dispatch_post_merge_ritual(
        7, dedup_key="shaF", auto_run=True, canonical_root=canonical, spawn=spawn
    )
    assert (canonical / ".fno" / "post-merge-dispatched" / "shaF").exists()
    # the spawn cwd is the canonical, so the worker's ritual resolves canonical too
    assert spawn.calls[0][1] == str(canonical)


# --- task 2.3: merge SHA threading through reconcile ---------------------


def test_query_parses_merge_sha():
    class _Res:
        returncode = 0
        stdout = (
            '{"number": 7, "state": "MERGED", "url": "u", '
            '"mergedAt": "t", "mergeCommit": {"oid": "cafef00d"}}'
        )
        stderr = ""

    def runner(cmd, **kw):
        assert "mergeCommit" in cmd[cmd.index("--json") + 1]
        return _Res()

    state = query_pr_merge_state(7, repo="o/r", runner=runner)
    assert state.merge_sha == "cafef00d"


def test_scan_threads_merge_sha_onto_record():
    entries = [
        {"id": "x-0001", "pr_number": 7, "pr_url": "https://github.com/o/r/pull/7"}
    ]

    def query(number, repo=None, cwd=None):
        return PrMergeState(
            number=number, state="MERGED", url="https://github.com/o/r/pull/7",
            merged_at="t", merge_sha="beefcafe",
        )

    records = scan_merge_drift(entries, query=query, list_merged=lambda **kw: [])
    closeable = [r for r in records if r.closeable]
    assert len(closeable) == 1
    assert closeable[0].merge_sha == "beefcafe"


# --- daemon adapter: the real _default_dispatch_ritual cold-spawn chain ---


class _Cand:
    def __init__(self, pr_number, repo_dir, source_session_id=None):
        self.pr_number = pr_number
        self.repo_dir = repo_dir
        self.source_session_id = source_session_id


class _Obs:
    def __init__(self, merge_sha=None):
        self.merge_sha = merge_sha


class _FireResult:
    def __init__(self, ok, rc=0):
        self.ok = ok
        self.rc = rc


def _arm_auto_run(repo_dir):
    """Arm the post_merge.auto_run opt-in for a repo dir. _default_dispatch_ritual
    now honors it (x-7930): a bare tmp_path defaults auto_run off -> 'disabled'."""
    cfg = repo_dir / ".fno" / "config.toml"
    cfg.parent.mkdir(parents=True, exist_ok=True)
    cfg.write_text("[post_merge]\nauto_run = true\n")


def test_default_dispatch_ritual_cold_fire_ok_marks(tmp_path, monkeypatch):
    """The daemon's real adapter: a successful headless fire is a completed
    hand-off (dispatched, marker set)."""
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    _arm_auto_run(tmp_path)
    fired: list = []

    def fire(verb, pr, repo_dir, *, model=None):
        fired.append((verb, pr, str(repo_dir)))
        return _FireResult(ok=True)

    res = _default_dispatch_ritual(
        _Cand(7, tmp_path, source_session_id=None), _Obs(merge_sha="shaD1"), fire
    )
    assert res.outcome == "dispatched"
    assert fired == [("merged", 7, str(tmp_path))]
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaD1").exists()


def test_default_dispatch_ritual_cold_fire_notok_no_marker(tmp_path):
    """A not-ok headless fire raises inside _cold_spawn -> spawn-failed, and NO
    marker is written so the next tick retries (the load-bearing invariant the
    generic dispatcher tests only assert at the seam)."""
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    _arm_auto_run(tmp_path)

    def fire(verb, pr, repo_dir, *, model=None):
        return _FireResult(ok=False, rc=1)

    res = _default_dispatch_ritual(
        _Cand(7, tmp_path, source_session_id=None), _Obs(merge_sha="shaD2"), fire
    )
    assert res.outcome == "spawn-failed"
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaD2").exists()


def test_default_dispatch_ritual_respects_auto_run_off(tmp_path):
    """x-7930 / codex P1: pr-watch must honor the post_merge.auto_run opt-in that
    reconcile honors. A `ready` repo that never armed auto_run must NOT auto-run
    /fno:pr merged + the canonical sync when pr-watch merely enables. No config
    -> auto_run off -> disabled, nothing fired, no marker."""
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    fired: list = []

    def fire(verb, pr, repo_dir, *, model=None):
        fired.append(pr)
        return _FireResult(ok=True)

    res = _default_dispatch_ritual(
        _Cand(7, tmp_path, source_session_id=None), _Obs(merge_sha="shaOFF"), fire
    )
    assert res.outcome == "disabled"
    assert fired == []
    assert not (tmp_path / ".fno" / "post-merge-dispatched" / "shaOFF").exists()


def test_cross_detector_one_handoff_per_sha(tmp_path):
    """US3: reconcile (via canonical_root) and the daemon adapter (via node_cwd)
    converge on the SAME per-SHA marker under one canonical, so a merge both
    detectors observe is handed off exactly once."""
    from fno.pr_watch._dispatch import _default_dispatch_ritual

    canonical = tmp_path / "canon"
    canonical.mkdir()
    _arm_auto_run(canonical)

    # Detector A: reconcile-style call (its own canonical_root + merge_sha).
    spawn_a = _Spawn(short_id="A")
    first = dispatch_post_merge_ritual(
        7, dedup_key="shaXD", auto_run=True, canonical_root=canonical,
        spawn=spawn_a, source_session_id=None,
    )
    assert first.outcome == "dispatched"

    # Detector B: the daemon adapter, resolving the SAME canonical from node_cwd.
    fired_b: list = []

    def fire(verb, pr, repo_dir, *, model=None):
        fired_b.append(pr)
        return _FireResult(ok=True)

    second = _default_dispatch_ritual(
        _Cand(7, canonical, source_session_id=None), _Obs(merge_sha="shaXD"), fire
    )
    assert second.outcome == "already-dispatched"
    assert fired_b == []  # the second detector fired nothing
    assert len(spawn_a.calls) == 1  # exactly one hand-off total


# --- warm-session routing: inject XOR cold, one marker -------------------


class _WarmInject:
    def __init__(self, delivered=True, reason="delivered"):
        self.delivered = delivered
        self.reason = reason
        self.calls: list[tuple[str, int]] = []

    def __call__(self, session_id: str, pr_number: int, source_harness=None):
        self.calls.append((session_id, pr_number, source_harness))
        return (self.delivered, self.reason)


def _patch_resolver(monkeypatch, result):
    import fno.post_merge_route as pmr

    monkeypatch.setattr(
        pmr, "resolve_warm_session", lambda sid, harness=None: result
    )


def test_warm_delivery_skips_cold_and_marks(tmp_path, monkeypatch):
    """AC1-HP: live originating session -> exactly one inject, no cold
    dispatch, shared marker set."""
    _patch_resolver(monkeypatch, "sess-live-1")
    warm = _WarmInject(delivered=True)
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW1", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-live-1", warm_inject=warm,
    )
    assert res.outcome == "routed-warm"
    assert warm.calls == [("sess-live-1", 7, None)]
    assert spawn.calls == []
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaW1").exists()


def test_warm_inject_failure_falls_back_cold(tmp_path, monkeypatch):
    """AC1-ERR: probe passes but the send fails -> cold dispatch, reason kept."""
    _patch_resolver(monkeypatch, "sess-live-2")
    warm = _WarmInject(delivered=False, reason="not-live")
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW2", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-live-2", warm_inject=warm,
    )
    assert res.outcome == "dispatched"
    assert res.detail == "cold: not-live"
    assert len(spawn.calls) == 1
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaW2").exists()


def test_warm_queue_timeout_falls_back_cold(tmp_path, monkeypatch):
    """US4: a busy session queues the inject; past the confirm budget the
    route cold-dispatches."""
    _patch_resolver(monkeypatch, "sess-busy")
    warm = _WarmInject(delivered=False, reason="queue-timeout")
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW3", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-busy", warm_inject=warm,
    )
    assert res.outcome == "dispatched"
    assert res.detail == "cold: queue-timeout"
    assert len(spawn.calls) == 1


def test_no_source_session_takes_cold_path(tmp_path):
    """AC1-EDGE: a node with no source_session_id cold-dispatches, no inject."""
    warm = _WarmInject()
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW4", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id=None, warm_inject=warm,
    )
    assert res.outcome == "dispatched"
    assert warm.calls == []
    assert len(spawn.calls) == 1


def test_unresolved_session_takes_cold_path(tmp_path, monkeypatch):
    """Dead / unregistered / identity-mismatch resolves to None -> cold."""
    _patch_resolver(monkeypatch, None)
    warm = _WarmInject()
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW5", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-dead", warm_inject=warm,
    )
    assert res.outcome == "dispatched"
    assert res.detail == "cold: no-live-source-session"
    assert warm.calls == []
    assert len(spawn.calls) == 1


def test_existing_marker_blocks_warm_inject_too(tmp_path, monkeypatch):
    """US3: a merge already handled by the other detector never re-routes --
    neither inject nor cold fires once the shared marker exists."""
    _patch_resolver(monkeypatch, "sess-live-3")
    spawn = _Spawn()
    first = dispatch_post_merge_ritual(
        7, dedup_key="shaW6", auto_run=True, canonical_root=tmp_path, spawn=spawn
    )
    assert first.outcome == "dispatched"
    warm = _WarmInject()
    second = dispatch_post_merge_ritual(
        7, dedup_key="shaW6", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-live-3", warm_inject=warm,
    )
    assert second.outcome == "already-dispatched"
    assert warm.calls == []
    assert len(spawn.calls) == 1


def test_warm_resolver_error_degrades_to_cold(tmp_path, monkeypatch):
    """A resolver crash must never break the dispatch (fallback floor)."""
    import fno.post_merge_route as pmr

    def _boom(sid, harness=None):
        raise RuntimeError("resolver exploded")

    monkeypatch.setattr(pmr, "resolve_warm_session", _boom)
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaW7", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-x", warm_inject=_WarmInject(),
    )
    assert res.outcome == "dispatched"
    assert res.detail is not None and res.detail.startswith("cold: warm-error")
    assert len(spawn.calls) == 1


def test_source_harness_threads_to_resolver_and_inject(tmp_path, monkeypatch):
    """x-c4dd: the harness selects the live vehicle. dispatch passes
    source_harness to BOTH the resolver and the inject seam, so a codex-shipped
    node warm-routes to its own panel instead of cold-spawning a claude worker."""
    import fno.post_merge_route as pmr

    seen = {}

    def _resolver(sid, harness=None):
        seen["resolver"] = (sid, harness)
        return sid  # live

    monkeypatch.setattr(pmr, "resolve_warm_session", _resolver)
    warm = _WarmInject(delivered=True)
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        9, dedup_key="shaWH", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="codex-sess", source_harness="codex",
        warm_inject=warm,
    )
    assert res.outcome == "routed-warm"
    assert seen["resolver"] == ("codex-sess", "codex")
    assert warm.calls == [("codex-sess", 9, "codex")]
    assert spawn.calls == []


# --- Wave 3 (x-7930): notify the origin session on a cold-dispatched merge ---


class _Notify:
    def __init__(self, fail=False):
        self.fail = fail
        self.calls: list[tuple] = []

    def __call__(self, session_id, pr_number, source_harness, cold_reason):
        self.calls.append((session_id, pr_number, source_harness, cold_reason))
        if self.fail:
            raise RuntimeError("mail boom")


def test_cold_dispatch_notifies_origin_once(tmp_path, monkeypatch):
    """AC2-HP: a cold-dispatched merge sends exactly one origin mail; a re-tick
    (marker exists) sends none."""
    _patch_resolver(monkeypatch, None)  # origin not live -> cold path
    notify = _Notify()
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaN1", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-origin", source_harness="claude",
        warm_inject=_WarmInject(), notify_origin=notify,
    )
    assert res.outcome == "dispatched"
    assert notify.calls == [("sess-origin", 7, "claude", "no-live-source-session")]

    # Re-tick: marker exists -> already-dispatched -> no second mail.
    second = dispatch_post_merge_ritual(
        7, dedup_key="shaN1", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-origin", source_harness="claude",
        warm_inject=_WarmInject(), notify_origin=notify,
    )
    assert second.outcome == "already-dispatched"
    assert len(notify.calls) == 1


def test_routed_warm_sends_no_origin_mail(tmp_path, monkeypatch):
    """Locked #5: the warm route ran the ritual in the live origin already, so
    no advisory mail is sent."""
    _patch_resolver(monkeypatch, "sess-live")
    notify = _Notify()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaN2", auto_run=True, canonical_root=tmp_path,
        spawn=_Spawn(), source_session_id="sess-live",
        warm_inject=_WarmInject(delivered=True), notify_origin=notify,
    )
    assert res.outcome == "routed-warm"
    assert notify.calls == []


def test_no_source_session_no_origin_mail(tmp_path):
    """AC2-EDGE: no source_session_id -> no mail attempted, no error."""
    notify = _Notify()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaN3", auto_run=True, canonical_root=tmp_path,
        spawn=_Spawn(), source_session_id=None, notify_origin=notify,
    )
    assert res.outcome == "dispatched"
    assert notify.calls == []


def test_default_notifier_addresses_origin_session_durably(monkeypatch):
    """The default notifier addresses the origin by canonical handle and routes
    through the durable name-lane send (resolved=None: the cold path is already
    a warm-miss, so the durable bus is the floor)."""
    import fno.graph._reconcile as rec

    seen = {}

    def _fake_send(message, *, from_name, resolved, recipient, provider, **kw):
        seen.update(
            message=message, from_name=from_name, resolved=resolved,
            recipient=recipient, provider=provider,
        )

    monkeypatch.setattr("fno.mail.cli._name_lane_send", _fake_send)
    rec._notify_origin_merged("abcdef1234567890", 42, "claude", "no-live-source-session")
    assert seen["recipient"] == "claude-abcdef12"  # canonical_handle
    assert seen["resolved"] is None  # durable floor, no live resolution
    assert seen["provider"] == "claude"
    assert seen["from_name"] == "fno"
    assert "PR #42 merged" in seen["message"]


def test_origin_mail_failure_keeps_marker_and_dispatch(tmp_path, monkeypatch):
    """AC2-ERR: a mail failure never breaks the dispatch or withholds the marker."""
    _patch_resolver(monkeypatch, None)
    notify = _Notify(fail=True)
    spawn = _Spawn()
    res = dispatch_post_merge_ritual(
        7, dedup_key="shaN4", auto_run=True, canonical_root=tmp_path,
        spawn=spawn, source_session_id="sess-origin",
        warm_inject=_WarmInject(), notify_origin=notify,
    )
    assert res.outcome == "dispatched"
    assert len(notify.calls) == 1
    assert (tmp_path / ".fno" / "post-merge-dispatched" / "shaN4").exists()
