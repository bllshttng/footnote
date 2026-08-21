"""Tests for `fno do pr ritual` (x-bbde) and its four absorbed bugs.

The legs shell existing fno verbs; a fake runner stands in for fno/gh/git so
every leg is exercised without a real backlog/graph/gh. Pure helpers
(``_canonical_root``, ``_parking_lot_path``) are tested directly, including the
real-git worktree path for x-fb99.
"""
from __future__ import annotations

import subprocess
from types import SimpleNamespace


from fno.config import PostMergeBlock
from fno.pr import _ritual
from fno.pr._proc import Result


# --- fake runner ---------------------------------------------------------

class FakeRunner:
    """Records argv; returns canned Results keyed on the subcommand."""

    def __init__(self, *, diff_files=0, additions=0, deletions=0,
                 deferred=None, reconcile_closed=None, claim_rc=0,
                 spawn_rc=0, agent_rows=None, branch="feat/x", state="MERGED",
                 reconcile_held=None, reconcile_candidates=None,
                 reconcile_contained=None, reconcile_errors=(),
                 reconcile_sync_outcome=None, reconcile_closure_refused=None):
        self.calls: list[list[str]] = []
        self._diff = (diff_files, additions, deletions)
        self._deferred = deferred or []
        self._closed = reconcile_closed or []
        self._held = reconcile_held or []
        self._candidates = reconcile_candidates
        self._contained = reconcile_contained or ()
        self._errors = reconcile_errors
        self._sync_outcome = reconcile_sync_outcome
        self._closure_refused = reconcile_closure_refused
        self._claim_rc = claim_rc
        self._spawn_rc = spawn_rc
        self._rows = agent_rows or []
        self._branch = branch
        # The ritual's premise is a MERGED PR, so that is the default here.
        # The removal legs now READ this rather than assuming it; see
        # test_pr_ritual_merge_guard.py for the refusal cases.
        self._state = state

    def __call__(self, argv, *, cwd=None, timeout=None):
        self.calls.append(list(argv))
        head = argv[0]
        if head == "gh":
            if "list" in argv:
                return Result(0, '[{"number":7,"mergedAt":"2026-07-23T00:00:00Z"}]', "")
            if "view" in argv:
                return Result(0, '{"state":"%s","headRefName":"%s","changedFiles":%d,"additions":%d,"deletions":%d}'
                              % (self._state, self._branch, self._diff[0], self._diff[1], self._diff[2]), "")
            return Result(0, "{}", "")
        # fno-py <sub> ...
        sub = argv[1] if len(argv) > 1 else ""
        if sub == "claim" and "acquire" in argv:
            return Result(self._claim_rc, "acquired" if self._claim_rc == 0 else "held", "")
        if sub == "backlog" and "reconcile" in argv:
            import json
            payload = {"closed": [{"node_id": n} for n in self._closed]}
            if self._candidates is not None:
                payload["candidates"] = [{"node_id": n} for n in self._candidates]
            payload["promise_unmet"] = [{"node_id": n} for n in self._held]
            if self._contained:
                payload["contained_closed"] = list(self._contained)
            if self._errors:
                payload["contained_errors"] = list(self._errors)
            if self._sync_outcome:
                payload["sync_catchup"] = {"outcome": self._sync_outcome}
            if self._closure_refused:
                payload["closure_refused"] = self._closure_refused
            return Result(0, json.dumps(payload), "")
        if sub == "backlog" and "find" in argv:
            import json
            return Result(0, json.dumps(self._deferred), "")
        if sub == "agents" and "list" in argv:
            import json
            return Result(0, json.dumps({"agents": self._rows}), "")
        if sub == "agents" and "spawn" in argv:
            return Result(self._spawn_rc, "spawned", "")
        if sub == "agents" and ("stop" in argv or "rm" in argv):
            return Result(0, "", "")
        return Result(0, "", "")


def _bare(tmp_path, runner, *, autonomous=False, pr=7, parking_lot=None,
          node_ids=None, self_reap=False, model=PostMergeBlock().model):
    """A Ritual built without __init__'s git/config resolution (hermetic).

    ``model`` defaults to the REAL block's value rather than a literal: this
    fake pm stands in for PostMergeBlock, and a hand-written default here would
    let the two drift silently (the fake is what a leg actually reads).
    """
    r = object.__new__(_ritual.Ritual)
    pm = SimpleNamespace(sync_command=None, self_reap=self_reap,
                         parking_lot_path=parking_lot, model=model)
    r.ctx = _ritual._Ctx(
        pr=pr, autonomous=autonomous, canon=tmp_path, settings=None, pm=pm,
        project="", lane_project="", parking_lot=(tmp_path / parking_lot) if parking_lot else None,
        holder="postmerge:pr-holder:test",
        node_ids=list(node_ids or []),
    )
    r.runner = runner
    r.cwd = tmp_path
    return r


def _argv_sub(calls, sub):
    """First fno call argv whose fno subcommand == sub."""
    for c in calls:
        if len(c) > 1 and c[0] != "gh" and c[1] == sub:
            return c
    return None


# --- x-fb99: canonical root from a worktree ------------------------------

def _git(cwd, *args):
    env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "a@b.c",
           "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "a@b.c"}
    full = {"PATH": "/usr/bin:/bin", **env}
    subprocess.run(["git", *args], cwd=str(cwd), check=True, env=full,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def test_canonical_root_resolves_from_worktree(tmp_path):
    # x-fb99: from a worktree cwd, the canonical root is the MAIN worktree,
    # not the worktree itself. A bare --show-toplevel would return the worktree.
    main = tmp_path / "main"
    main.mkdir()
    _git(main, "init", "-q", "-b", "main")
    (main / "f").write_text("x")
    _git(main, "add", "-A")
    _git(main, "commit", "-qm", "init")
    wt = tmp_path / "wt"
    _git(main, "worktree", "add", "-q", str(wt), "-b", "feature")
    assert _ritual._canonical_root(wt).resolve() == main.resolve()


def test_parking_lot_path_joins_canonical_and_rejects_escape(tmp_path):
    pm = SimpleNamespace(parking_lot_path="internal/etl/backlog/parking-lot.md")
    assert _ritual._parking_lot_path(tmp_path, pm) == tmp_path / "internal/etl/backlog/parking-lot.md"
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path=None)) is None
    # x-fb99 backstop: a stale installed fno that accepted an absolute / '..'
    # path must not let the join escape the canonical root.
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path="/etc/passwd")) is None
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path="../sibling")) is None
    assert _ritual._parking_lot_path(tmp_path, SimpleNamespace(parking_lot_path="a/../../b")) is None


# --- x-c4ff: legs call the real verbs (no dangling references) -----------

def test_leg_skill_diff_calls_real_verb(tmp_path, capsys):
    # x-c4ff: the skill-diff leg calls the existing `skill-diff reconcile`,
    # not a nonexistent `fno skill-diff`. Dangling reference = this fails.
    runner = FakeRunner()
    r = _bare(tmp_path, runner)
    r.leg_skill_diff()
    sub = _argv_sub(runner.calls, "skill-diff")
    assert sub is not None and "reconcile" in sub
    rec = [line for line in capsys.readouterr().out.splitlines() if line.startswith("step=skill-diff")]
    assert rec and "status=ok" in rec[0]


def test_leg_sync_canonical_calls_real_verb(tmp_path, capsys):
    # x-c4ff: the canonical-sync leg calls the existing canonical spelling.
    runner = FakeRunner()
    pm = SimpleNamespace(sync_command="git pull", self_reap=False, parking_lot_path=None)
    r = _bare(tmp_path, runner)
    r.ctx.pm = pm
    r.leg_sync_canonical()
    assert any(c[1:4] == ["do", "pr", "sync-canonical"] for c in runner.calls)


def test_sync_canonical_skipped_when_unconfigured(tmp_path, capsys):
    runner = FakeRunner()
    r = _bare(tmp_path, runner)  # pm.sync_command = None
    r.leg_sync_canonical()
    rec = [line for line in capsys.readouterr().out.splitlines() if line.startswith("step=sync-canonical")]
    assert rec and "status=skipped" in rec[0] and "not configured" in rec[0]


# --- x-0d66: advance leg bounded + progress lines ------------------------

def test_advance_stream_is_bounded(tmp_path, capsys, monkeypatch):
    # x-0d66: a hung advance must be killed at the bound, not wedge the ritual.
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["bash", "-lc"])
    r = _bare(tmp_path, FakeRunner(), node_ids=["fno-abc1"])
    r._stream("advance", ["sleep 30"], 1.0)
    out = capsys.readouterr().out
    assert "step=advance status=failed" in out
    assert "timeout" in out


def test_advance_stream_emits_progress(tmp_path, capsys, monkeypatch):
    # x-0d66: progress lines surface partial-dispatch state instead of silence.
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["bash", "-lc"])
    r = _bare(tmp_path, FakeRunner())
    r._stream("advance", ["echo dispatched-x; echo dispatched-y"], 5.0)
    out = capsys.readouterr().out
    assert "  advance: dispatched-x" in out
    assert "step=advance status=ok" in out


# --- AC3: a failing leg is loud -----------------------------------------

def test_failing_leg_records_failure_and_exit(tmp_path, capsys):
    # AC3-ERR: a non-zero exit surfaces as status=failed and run() exits 1.
    class _FailSync(FakeRunner):
        def __call__(self, argv, *, cwd=None, timeout=None):
            super().__call__(argv, cwd=cwd, timeout=timeout)
            if argv[1:4] == ["do", "pr", "sync-canonical"]:
                return Result(3, "sync failed: boom", "")
            return super().__call__(argv, cwd=cwd, timeout=timeout) if False else self._last

    runner = FakeRunner()
    r = _bare(tmp_path, runner)

    # Simulate the sync leg failing by calling _leg directly with a runner
    # variant that returns non-zero for sync-canonical.
    class _R:
        def __init__(self, inner):
            self._inner = inner

        def __call__(self, argv, *, cwd=None, timeout=None):
            self._inner.calls.append(list(argv))
            if argv[1:4] == ["do", "pr", "sync-canonical"]:
                return Result(3, "sync failed: boom", "")
            return FakeRunner.__call__(self._inner, argv, cwd=cwd, timeout=timeout)

    r.runner = _R(runner)
    pm = SimpleNamespace(sync_command="git pull", self_reap=False, parking_lot_path=None)
    r.ctx.pm = pm
    r.leg_sync_canonical()
    out = capsys.readouterr().out
    assert "step=sync-canonical status=failed" in out
    assert "exit=3" in out


def test_run_exits_nonzero_when_a_leg_fails(tmp_path, capsys, monkeypatch):
    # AC3 end-to-end: reconcile failure -> exit 1, every later leg still runs.
    class _FailReconcile(FakeRunner):
        def __call__(self, argv, *, cwd=None, timeout=None):
            self.calls.append(list(argv))
            sub = argv[1] if len(argv) > 1 and argv[0] != "gh" else ""
            if sub == "backlog" and "reconcile" in argv and "session" not in argv:
                return Result(1, "corrupt graph", "")
            return FakeRunner.__call__(self, argv, cwd=cwd, timeout=timeout)

    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["true"])  # no real fno-py needed
    runner = _FailReconcile()
    r = _bare(tmp_path, runner)
    # stub the claim runner path: acquire_mutex uses self.runner too
    r.runner = runner
    rc = r.run()
    out = capsys.readouterr().out
    assert rc == 1
    assert "step=reconcile status=failed" in out
    # later legs still ran and printed receipts
    assert "step=judgment" in out
    assert "step=reap-rows" in out


# --- x-40be: the reconcile receipt says held-open, never ok ----------------

def test_reconcile_held_open_reads_deferred_never_ok(tmp_path, capsys):
    """The #819 ritual held seven nodes open and printed status=ok: a partial
    ritual read as a clean one. Held-open work is still owed - the existing
    _DEFERRED status - and the ids are named so the operator can act."""
    r = _bare(tmp_path, FakeRunner(reconcile_held=["x-ffc9", "x-6c67"]))
    r.leg_stamp()
    out = capsys.readouterr().out
    assert "step=reconcile status=deferred" in out
    assert "held_open=2: x-ffc9, x-6c67" in out
    assert "step=reconcile status=ok" not in out


def test_reconcile_closure_refused_reads_deferred_never_ok(tmp_path, capsys):
    """Round-7 review fix: leg_stamp ignored `closure_refused` entirely, so a
    flaky gh query during trailer binding (no other drift found) read as a
    clean "no-drift" run - masking a real closure failure that a --json call
    site already had the answer for in its own response body."""
    r = _bare(tmp_path, FakeRunner(reconcile_closure_refused="could not query PR #7: timeout"))
    r.leg_stamp()
    out = capsys.readouterr().out
    assert "step=reconcile status=deferred" in out
    assert "closure binding refused: could not query PR #7: timeout" in out
    assert "step=reconcile status=ok" not in out


def test_reconcile_closed_still_reads_ok(tmp_path, capsys):
    r = _bare(tmp_path, FakeRunner(reconcile_closed=["x-1"], reconcile_candidates=["x-1"]))
    r.leg_stamp()
    out = capsys.readouterr().out
    assert "step=reconcile status=ok detail=closed=1" in out


def test_leg_stamp_scopes_reconcile_to_pr_number(tmp_path, capsys):
    """x-59a6 AC4-HP: leg_stamp routes through plural (--pr-number) reconcile
    rather than the old bare full sweep, so a multi-node PR's trailer claims
    get bound here, not just whichever node happens to already carry a ref."""
    r = _bare(tmp_path, FakeRunner(reconcile_closed=["x-a1", "x-a2"]), pr=42)
    r.leg_stamp()
    sub = _argv_sub(r.runner.calls, "backlog")
    assert sub is not None and "reconcile" in sub
    assert "--pr-number" in sub and "42" in sub


def test_leg_advance_never_keys_off_a_single_closed_id(tmp_path):
    """x-59a6: --closed keys advance's own dependents check off ONE id, and
    leg_stamp's reconcile already dispatched dependents for every node it
    closed - passing node_ids[0] here would silently drop the 2nd+ closed
    node's dependents from this leg's (redundant) coverage."""
    r = _bare(tmp_path, FakeRunner(), node_ids=["x-b1", "x-b2"])
    captured: dict = {}
    r._stream = lambda step, argv, timeout: captured.update(step=step, argv=argv)
    r.leg_advance()
    assert captured["step"] == "advance"
    assert "advance" in captured["argv"]
    assert "--closed" not in captured["argv"]


def test_reconcile_no_drift_is_distinguishable_from_a_held_close(tmp_path, capsys):
    """"closed=0" alone covered three outcomes; with candidates empty the
    receipt now says no-drift (found no node), distinct from held-open."""
    r = _bare(tmp_path, FakeRunner(reconcile_candidates=[]))
    r.leg_stamp()
    out = capsys.readouterr().out
    assert "step=reconcile status=ok detail=no-drift" in out


def test_reconcile_healed_only_is_not_no_drift(tmp_path, capsys):
    """A heal-only sweep (contained nodes closed, no new drift) mutates the
    graph; the receipt must not claim nothing was found."""
    r = _bare(
        tmp_path,
        FakeRunner(reconcile_candidates=[], reconcile_contained=["x-h1", "x-h2"]),
    )
    r.leg_stamp()
    out = capsys.readouterr().out
    assert "step=reconcile status=ok detail=healed-only" in out
    assert "no-drift" not in out


def test_reconcile_contained_errors_and_failed_sync_are_not_no_drift(tmp_path, capsys):
    """Cascade-close failures and a failed canonical sync leave reconcile
    exiting 0 with empty close sets; the receipt must not then claim
    no-drift, or the failure is observable nowhere but an unread JSON body."""
    for kwargs, needle in (
        ({"reconcile_errors": ["x-e1"]}, "closed=0 contained-errors=1 sync=not-run"),
        ({"reconcile_sync_outcome": "failed"}, "closed=0 contained-errors=0 sync=failed"),
    ):
        r = _bare(tmp_path, FakeRunner(reconcile_candidates=[], **kwargs))
        r.leg_stamp()
        out = capsys.readouterr().out
        assert f"step=reconcile status=ok detail={needle}" in out
        assert "no-drift" not in out


def test_reconcile_synced_catchup_reads_healed_only(tmp_path, capsys):
    """A canonical catch-up sync mutated repo state; that is not no-drift."""
    r = _bare(
        tmp_path,
        FakeRunner(reconcile_candidates=[], reconcile_sync_outcome="synced"),
    )
    r.leg_stamp()
    out = capsys.readouterr().out
    assert "step=reconcile status=ok detail=healed-only" in out
    assert "no-drift" not in out


# --- AC2: empty inputs spawn nothing; non-empty spawns headless ----------

def test_judgment_autonomous_empty_skips(tmp_path, capsys):
    runner = FakeRunner(diff_files=0)  # below bar, no deferrals
    r = _bare(tmp_path, runner, autonomous=True)
    r.leg_judgment()
    out = capsys.readouterr().out
    assert "step=judgment status=skipped" in out
    assert "reason=no-inputs" in out or "diff-below-bar" in out
    # no spawn
    assert not any(c[1] == "agents" and "spawn" in c for c in runner.calls if len(c) > 1)


def test_judgment_autonomous_nonempty_spawns_headless(tmp_path, capsys, monkeypatch):
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["fno-py"])
    runner = FakeRunner(diff_files=14, additions=300, deletions=20)
    r = _bare(tmp_path, runner, autonomous=True, parking_lot="internal/x/parking-lot.md")
    r.leg_judgment()
    out = capsys.readouterr().out
    assert "step=judgment status=ok" in out
    assert "spawned headless" in out
    spawns = [c for c in runner.calls if len(c) > 1 and c[1] == "agents" and "spawn" in c]
    assert len(spawns) == 1
    argv = spawns[0]
    assert "--substrate" in argv and "headless" in argv[argv.index("--substrate") + 1]
    assert "bg" not in argv
    # The prompt is the SOLE positional (the MESSAGE); the name rides --name.
    # Before the axis redesign the grammar was `[name] [message]` and this passed
    # `judgment-pr-<n> <prompt>` as two positionals - which now fails closed. A
    # FakeRunner only records argv, so a structural assertion alone let the real
    # break hide; the normalizer guard below is what actually exercises the
    # grammar this leg depends on.
    assert argv[-1].startswith("Post-merge judgment")   # the prompt = message
    i = argv.index("--name")
    assert argv[i + 1] == "judgment-pr-7" and len(argv[i + 1]) <= 64
    # codex P2: the worker gets its own --timeout, not a 60s outer kill.
    assert "--timeout" in argv
    # Guard against silent grammar drift: the constructed argv must SURVIVE the
    # real spawn normalizer (the layer that refuses two positionals), not merely
    # look right. `agents spawn` is argv[1:] with the verb token dropped.
    from fno.agents.spawn_defaults import normalize_spawn_args

    spawn_argv = argv[argv.index("spawn"):]  # ["spawn", ...]
    normalized = normalize_spawn_args(spawn_argv)  # raises SystemExit(2) if invalid
    assert normalized[normalized.index("--name") + 1] == "judgment-pr-7"


def test_judgment_spawn_forwards_configured_model(tmp_path, capsys, monkeypatch):
    """config.post_merge.model must reach the worker, not just the schema.

    The field carried a tier default for the reasoning the judgment needs, but
    the spawn passed no --model, so the worker silently ran on the harness /
    account default and the whole setting was inert.
    """
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["fno-py"])
    runner = FakeRunner(diff_files=14, additions=300, deletions=20)
    r = _bare(tmp_path, runner, autonomous=True, parking_lot="internal/x/parking-lot.md")
    r.leg_judgment()
    argv = [c for c in runner.calls if len(c) > 1 and c[1] == "agents" and "spawn" in c][0]
    assert argv[argv.index("--model") + 1] == PostMergeBlock().model
    # The model is a claude id, and an explicit --model bypasses the
    # provider-scoping that would drop it: unpinned, a codex-ambient session
    # hands the claude id to codex and 400s after the round-trip.
    assert argv[argv.index("--harness") + 1] == "claude"
    # NOT --role: `post-merge` is in DEFAULT_ROUTED_ROLES and would auto-route
    # this leg to the weaker secondary provider - the opposite of the intent.
    assert "--role" not in argv
    # The model must survive the real normalizer, not just sit in a list.
    from fno.agents.spawn_defaults import normalize_spawn_args

    normalized = normalize_spawn_args(argv[argv.index("spawn"):])
    assert normalized[normalized.index("--model") + 1] == PostMergeBlock().model


def test_judgment_spawn_omits_model_when_unset(tmp_path, capsys, monkeypatch):
    """A blank/absent model adds no flag - an empty --model value refuses at spawn."""
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["fno-py"])
    runner = FakeRunner(diff_files=14, additions=300, deletions=20)
    r = _bare(tmp_path, runner, autonomous=True,
              parking_lot="internal/x/parking-lot.md", model="")
    r.leg_judgment()
    argv = [c for c in runner.calls if len(c) > 1 and c[1] == "agents" and "spawn" in c][0]
    assert "--model" not in argv


def test_judgment_attended_defers_to_skill(tmp_path, capsys):
    # An attended run never spawns; the skill body does judgment inline.
    runner = FakeRunner(diff_files=50, additions=900, deletions=100)
    r = _bare(tmp_path, runner, autonomous=False, parking_lot="internal/x/parking-lot.md")
    r.leg_judgment()
    out = capsys.readouterr().out
    assert "deferred-to-skill" in out
    assert not any(len(c) > 1 and c[1] == "agents" and "spawn" in c for c in runner.calls)


def test_judgment_prompt_default_ships_no_personal_marker(tmp_path):
    # OSS hygiene: the autonomous prompt must never carry an operator's initials.
    # Default (no parking lot, no marker) routes maintainer items to the
    # dedicated repo-local file, untagged, and keeps narrative OUT of it
    # (x-codex-review P2: the dedicated file is maintainer-only).
    r = _bare(tmp_path, FakeRunner(), autonomous=True, pr=7)
    prompt = r._judgment_prompt(deferred=0, files=14, lines=320)
    assert "#jc" not in prompt
    assert ".fno/tasks/user.md" in prompt
    assert "no tag" in prompt
    assert "do NOT write narrative here" in prompt


def test_judgment_prompt_tags_only_when_marker_configured_on_shared_lot(tmp_path):
    # The marker is a discriminator earned by a SHARED destination: applied only
    # when both a parking lot and a marker are configured, else never mentioned.
    r = _bare(tmp_path, FakeRunner(), autonomous=True, pr=7,
              parking_lot="internal/x/parking-lot.md")
    # No marker on the (shared) lot -> untagged, and still no operator initials.
    # A shared lot DOES take narrative alongside maintainer items.
    prompt = r._judgment_prompt(deferred=0, files=14, lines=320)
    assert "#jc" not in prompt
    assert "tagged" not in prompt
    assert "narrative" in prompt
    # Opt in via the configured marker -> it appears, but only the configured one.
    r.ctx.pm.maintainer_marker = "#maintainer"
    prompt = r._judgment_prompt(deferred=0, files=14, lines=320)
    assert "#maintainer" in prompt
    assert "#jc" not in prompt


def test_judgment_autonomous_spawns_without_parking_lot_above_bar(
    tmp_path, capsys, monkeypatch
):
    # The dedicated maintainer destination (.fno/tasks/user.md) must be reachable
    # on the normal no-parking-lot path: an above-bar diff spawns the headless
    # worker even with no parking lot, so maintainer items are captured rather
    # than dropped (x-codex-review P2: the bar is decoupled from parking-lot
    # availability).
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["fno-py"])
    runner = FakeRunner(diff_files=14, additions=300, deletions=20)
    r = _bare(tmp_path, runner, autonomous=True, pr=7)  # NO parking_lot
    r.leg_judgment()
    out = capsys.readouterr().out
    assert "step=judgment status=ok" in out
    assert "spawned headless" in out
    assert "parking_lot=unset" in out and "bar=above" in out


# --- codex review fixes: enabled gate + node recovery --------------------

def test_run_skips_when_post_merge_disabled(tmp_path, capsys, monkeypatch):
    # codex P1: config.post_merge.enabled=false must not acquire the mutex or
    # run any leg. The replaced bash exited 0 without mutations; the verb must too.
    monkeypatch.setattr(_ritual, "fno_py_cmd", lambda: ["true"])
    runner = FakeRunner()
    r = _bare(tmp_path, runner)
    r.ctx.pm = SimpleNamespace(enabled=False, sync_command=None,
                               self_reap=False, parking_lot_path=None)
    rc = r.run()
    assert rc == 0
    out = capsys.readouterr().out
    assert "post_merge.enabled is false" in out
    assert "step=reconcile" not in out  # no leg ran
    assert not any(len(c) > 1 and c[1] == "claim" and "acquire" in c
                   for c in runner.calls)  # mutex never acquired



def _patch_sidecar_scan(monkeypatch, rows):
    """Route the PR-recovery scan's sidecar projection at synthetic rows.

    The reader migrated from read_graph to the sidecar seam; the same synthetic
    node data now arrives as per-id sidecars (pr_number/pr_url are the fields
    the recovery scan consumes).
    """
    from fno.tracker import sidecar as sidecar_store
    from fno.tracker.sidecar import Sidecar

    monkeypatch.setattr(
        sidecar_store,
        "load_all",
        lambda: {
            e["id"]: Sidecar(id=e["id"], pr_number=e.get("pr_number"),
                            pr_url=e.get("pr_url"))
            for e in rows
        },
    )

def test_recover_node_for_pr_scopes_by_repo(tmp_path, monkeypatch):
    # codex P2: when reconcile closed nothing (dominant ship-gate path),
    # recover the PR's node from the graph, scoped by origin slug + pr_url so a
    # foreign repo sharing a pr_number is never reaped.
    r = _bare(tmp_path, FakeRunner(), pr=7)
    monkeypatch.setattr(r, "_resolve_origin_slug", lambda: "owner/repo")
    _patch_sidecar_scan(monkeypatch, [
        {"id": "fno-abc1", "pr_number": 7, "pr_url": "https://github.com/owner/repo/pull/7"},
        {"id": "fno-forei", "pr_number": 7, "pr_url": "https://github.com/other/repo/pull/7"},
        {"id": "fno-other", "pr_number": 99, "pr_url": "https://github.com/owner/repo/pull/99"},
    ])
    assert r._recover_node_for_pr() == ["fno-abc1"]


# --- scan ACs (ported from tests/post-merge/test_reap_build_worker.sh) ----

def test_origin_slug_resolves_every_remote_form():
    # AC9b: every GitHub remote form (scp, https, ssh, git://, port, creds,
    # case, trailing /) resolves to owner/repo.
    forms = ["git@github.com:o/r.git", "https://github.com/o/r",
             "ssh://git@github.com/o/r.git", "git://github.com/o/r",
             "https://GitHub.com/O/R.git", "ssh://git@github.com:22/o/r.git",
             "https://user:tok@github.com/o/r.git", "https://github.com/o/r.git/"]
    assert all(_ritual._parse_origin_slug(u) == "o/r" for u in forms)


def test_origin_slug_rejects_lookalike_hosts():
    # AC9c: a lookalike domain or a github.com path segment yields no slug (a
    # substring match would admit a foreign repo's node).
    look = ["https://notgithub.com/o/r.git",
            "https://gitlab.com/mirrors/github.com/o/r.git",
            "/tmp/github.com/o/r.git", "https://github.com.evil.test/o/r.git"]
    assert all(_ritual._parse_origin_slug(u) is None for u in look)
    assert _ritual._parse_origin_slug("git@gitlab.com:mirror/x.git") is None


def test_scan_nodes_acs():
    # AC1: pr_number match -> unioned. AC2c: two same-repo matches -> both.
    entries = [
        {"id": "x-1234", "pr_number": 292, "pr_url": "https://github.com/o/r/pull/292"},
        {"id": "x-5678", "pr_number": 292, "pr_url": "https://github.com/o/r/pull/292"}]
    assert set(_ritual._scan_nodes(entries, 292, "o/r")) == {"x-1234", "x-5678"}
    # AC4: a same-numbered PR in a FOREIGN repo is excluded.
    entries = [{"id": "x-mine", "pr_number": 292, "pr_url": "https://github.com/o/r/pull/292"},
               {"id": "x-theirs", "pr_number": 292, "pr_url": "https://github.com/other/repo/pull/292"}]
    assert _ritual._scan_nodes(entries, 292, "o/r") == ["x-mine"]
    # AC5: a superstring slug is excluded; a case-differing slug still matches.
    entries = [{"id": "x-super", "pr_number": 292, "pr_url": "https://github.com/o/r-extra/pull/292"},
               {"id": "x-upper", "pr_number": 292, "pr_url": "https://github.com/O/R/pull/292"}]
    assert _ritual._scan_nodes(entries, 292, "o/r") == ["x-upper"]
    # AC6: a url-less node is never matched. AC7: a corrupt non-string pr_url is
    # skipped, not fatal to the scan.
    entries = [{"id": "x-here", "pr_number": 292},
               {"id": "x-corrupt", "pr_number": 292, "pr_url": {"not": "a string"}},
               {"id": "x-good", "pr_number": 292, "pr_url": "https://github.com/o/r/pull/292"}]
    assert _ritual._scan_nodes(entries, 292, "o/r") == ["x-good"]
    # AC3: no matching pr_number -> empty.
    assert _ritual._scan_nodes(entries, 999, "o/r") == []
    # No slug -> empty (AC8: the union is skipped wholesale).
    assert _ritual._scan_nodes(entries, 292, None) == []


def test_recover_skips_when_no_origin_slug(tmp_path, monkeypatch):
    # AC8: an unresolvable origin yields no graph recovery (reconcile-closed ids
    # are unaffected).
    r = _bare(tmp_path, FakeRunner(), pr=7)
    monkeypatch.setattr(r, "_resolve_origin_slug", lambda: None)
    _patch_sidecar_scan(monkeypatch, [
        {"id": "x-1234", "pr_number": 7, "pr_url": "https://github.com/o/r/pull/7"}])
    assert r._recover_node_for_pr() == []


def test_recover_falls_through_to_gh(tmp_path, monkeypatch):
    # AC9: a non-GitHub git origin (a mirror) falls through to the gh fallback.

    class _GhFallback(FakeRunner):
        def __call__(self, argv, *, cwd=None, timeout=None):
            self.calls.append(list(argv))
            if argv[:1] == ["git"] and "get-url" in argv:
                return Result(0, "git@gitlab.com:mirror/x.git\n", "")
            if argv[:1] == ["gh"] and "repo" in argv:
                return Result(0, "o/r\n", "")
            return FakeRunner.__call__(self, argv, cwd=cwd, timeout=timeout)

    runner = _GhFallback()
    r = _bare(tmp_path, runner, pr=7)
    _patch_sidecar_scan(monkeypatch, [
        {"id": "x-1234", "pr_number": 7, "pr_url": "https://github.com/o/r/pull/7"}])
    assert r._recover_node_for_pr() == ["x-1234"]


# --- reap ACs (ported US1/US2/US3 from the shell harness) ----------------

def test_reap_stop_precedes_rm_when_self_reap_on(tmp_path, capsys):
    # US1: self_reap on, finished row -> stop THEN rm, naming the row.

    class _Rec(FakeRunner):
        def __init__(self):
            super().__init__(agent_rows=[{"name": "target-x-1234-slug", "status": "orphaned"}])
            self.order = []

        def __call__(self, argv, *, cwd=None, timeout=None):
            self.calls.append(list(argv))
            if len(argv) > 1 and argv[1] == "agents" and "stop" in argv:
                self.order.append(("stop", argv[-1]))
                return Result(0, "", "")
            if len(argv) > 1 and argv[1] == "agents" and "rm" in argv:
                self.order.append(("rm", argv[-1]))
                return Result(0, "", "")
            return FakeRunner.__call__(self, argv, cwd=cwd, timeout=timeout)

    runner = _Rec()
    r = _bare(tmp_path, runner, node_ids=["x-1234"])
    r.ctx.pm = SimpleNamespace(sync_command=None, self_reap=True, parking_lot_path=None)
    r.leg_reap_rows()
    assert runner.order == [("stop", "target-x-1234-slug"), ("rm", "target-x-1234-slug")]


def test_reap_self_reap_off_removes_nothing_prints_manual_cmd(tmp_path, capsys):
    # US2: self_reap off -> no stop/rm calls; the receipt carries the manual cmd.
    runner = FakeRunner(agent_rows=[{"name": "target-x-1234-slug", "status": "orphaned"}])
    r = _bare(tmp_path, runner, node_ids=["x-1234"])
    r.ctx.pm = SimpleNamespace(sync_command=None, self_reap=False, parking_lot_path=None)
    r.leg_reap_rows()
    out = capsys.readouterr().out
    assert not any(len(c) > 1 and c[1] == "agents" and ("stop" in c or "rm" in c)
                   for c in runner.calls)
    assert "fno agents stop target-x-1234-slug && fno agents rm target-x-1234-slug" in out


def test_reap_live_row_untouched(tmp_path, capsys):
    # US3c: a status=live row is never reaped (the guard that prevents data loss).
    runner = FakeRunner(agent_rows=[{"name": "target-x-1234-live", "status": "live"}])
    r = _bare(tmp_path, runner, node_ids=["x-1234"])
    r.ctx.pm = SimpleNamespace(sync_command=None, self_reap=True, parking_lot_path=None)
    r.leg_reap_rows()
    assert not any(len(c) > 1 and c[1] == "agents" and ("stop" in c or "rm" in c)
                   for c in runner.calls)


def test_reap_rows_recovers_node_when_reconcile_closed_nothing(tmp_path, monkeypatch, capsys):
    # codex P2 end-to-end: empty node_ids + graph has the PR's node -> recovery
    # fills node_ids -> reap proceeds instead of skipping.
    r = _bare(tmp_path, FakeRunner(agent_rows=[]), pr=7)
    monkeypatch.setattr(r, "_resolve_origin_slug", lambda: "owner/repo")
    _patch_sidecar_scan(monkeypatch, [
        {"id": "fno-abc1", "pr_number": 7, "pr_url": "https://github.com/owner/repo/pull/7"}])
    r.leg_reap_rows()
    out = capsys.readouterr().out
    assert "step=reap-rows" in out
    assert "no closed node ids" not in out  # recovered, not skipped


# --- AC1/AC5: archive leg (found / inside-worktree / missing script) ----

def test_archive_defers_when_run_inside_worktree(tmp_path, capsys, monkeypatch):
    # AC5-EDGE: never self-remove; defer to the standing sweep with a named receipt.
    runner = FakeRunner(branch="feature/x")
    r = _bare(tmp_path, runner)
    monkeypatch.setattr(r, "_find_worktree", lambda branch: str(r.cwd))
    r.leg_archive()
    out = capsys.readouterr().out
    # `deferred`, not `skipped`. The old emit attached a command to a skipped
    # status; it read as "nothing to do" and nobody ever ran the command, which
    # is why the freshest merged worktrees were the ones left on disk.
    assert "step=archive status=deferred" in out
    assert "sweep-will-reap" in out


def test_archive_runs_script_when_worktree_found(tmp_path, capsys, monkeypatch):
    # AC1-HP: a found worktree for the merged branch is archived.
    runner = FakeRunner(branch="feature/x")
    r = _bare(tmp_path, runner)
    wt = tmp_path / "wt"
    wt.mkdir()
    (tmp_path / "scripts" / "setup").mkdir(parents=True)
    (tmp_path / "scripts" / "setup" / "archive-worktree.sh").write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(r, "_find_worktree", lambda branch: str(wt))
    r.leg_archive()
    out = capsys.readouterr().out
    assert "step=archive status=ok" in out
    assert "archived" in out
    # the archive script was invoked with --yes, never --force
    archive_calls = [c for c in runner.calls if c[:1] == ["bash"] and "archive-worktree.sh" in " ".join(c)]
    assert archive_calls and "--yes" in archive_calls[0]
    assert "--force" not in archive_calls[0]


def test_archive_skips_when_no_worktree(tmp_path, capsys, monkeypatch):
    runner = FakeRunner(branch="feature/x")
    r = _bare(tmp_path, runner)
    monkeypatch.setattr(r, "_find_worktree", lambda branch: None)
    r.leg_archive()
    out = capsys.readouterr().out
    assert "step=archive status=skipped" in out


# --- AC4: idempotency / mutex -------------------------------------------

def test_mutex_held_stops_clean(tmp_path, capsys):
    # AC4-FR / concurrency: if another runner owns the mutex, stop at status=skipped.
    runner = FakeRunner(claim_rc=1)
    r = _bare(tmp_path, runner)
    won = r.acquire_mutex()
    assert won is False
    out = capsys.readouterr().out
    assert "step=mutex status=skipped" in out
    assert "already-held" in out
    assert not r.ctx.owns_claim


def test_mutex_released_on_success(tmp_path):
    runner = FakeRunner(claim_rc=0)
    r = _bare(tmp_path, runner)
    r.acquire_mutex()
    assert r.ctx.owns_claim
    r.release_mutex()
    assert not r.ctx.owns_claim
    # a release call was made
    assert any(len(c) > 1 and c[1] == "claim" and "release" in c for c in runner.calls)


def test_config_leg_reads_post_merge_off_settings_model(tmp_path, monkeypatch):
    # Regression (x-bbde verb shipped non-functional): SettingsModel exposes
    # post_merge / project DIRECTLY - there is no `.config` wrapper attribute.
    # The old `getattr(settings, "config")` was always None, so ctx.pm was
    # always None and the config leg failed on leg 1 for EVERY repo. This
    # exercises the real __init__ config load (the _bare helper bypasses it).
    pm = SimpleNamespace(parking_lot_path=None, enabled=True)
    model = SimpleNamespace(post_merge=pm, project=SimpleNamespace(id="fno"))
    monkeypatch.setattr(_ritual, "load_settings_for_repo", lambda p: model)
    monkeypatch.setattr(_ritual, "_canonical_root", lambda cwd: tmp_path)
    r = _ritual.Ritual(pr=7, autonomous=False, cwd=tmp_path, runner=FakeRunner())
    assert r.ctx.pm is pm  # NOT None - the config leg would falsely fail otherwise
    assert r.ctx.project == "fno"


def test_lane_project_reads_worktree_local_override(tmp_path, monkeypatch):
    canonical = tmp_path / "canonical"
    lane = tmp_path / "lane"
    for root in (canonical, lane):
        (root / ".fno").mkdir(parents=True)
        (root / ".fno" / "config.toml").write_text(
            '[post_merge]\nenabled = true\n[project]\nid = "fno"\n',
            encoding="utf-8",
        )
    (lane / ".fno" / "config.local.toml").write_text(
        '[project]\nid = "fno-lane-node"\n', encoding="utf-8"
    )
    monkeypatch.setattr(_ritual, "_canonical_root", lambda cwd: canonical)
    monkeypatch.setattr(_ritual, "_worktree_root", lambda cwd: lane)
    monkeypatch.setattr(_ritual, "_session_holder", lambda: "postmerge:test")

    r = _ritual.Ritual(pr=7, autonomous=False, cwd=lane, runner=FakeRunner())

    assert r.ctx.project == "fno"
    assert r.ctx.lane_project == "fno-lane-node"
