"""x-55c3: the external fleet watchdog's verdicts, apply lanes, and the
``--all`` enumeration they read.

The classifier is pure, so every verdict test injects transcripts, claims,
and node state - no live fleet. The apply-lane tests inject runners and tail
reads. The one real-filesystem test (worktree refusal) builds a throwaway git
repo. The two traps are pinned here on purpose: identity joins on the claim
holder, never a name; a wake is confirmed by transcript content, never a state
field.
"""
from __future__ import annotations

import json
import subprocess
from datetime import datetime, timezone

from fno.agents import watchdog
from fno.agents.watchdog import (
    GHOST,
    LEAVE,
    REAP,
    REROUTE,
    Row,
    TailFacts,
    Verdict,
    WAKE,
    apply_verdict,
    rate_limit_window,
    verdicts,
)

# 2026-08-16T18:40:00Z. The plan's measured case: a 02:48:21 SGT reset stamp
# (= 18:48:21Z) is 8m out at 18:40Z and 2m past at 18:50Z.
NOW_1840 = datetime(2026, 8, 16, 18, 40, 0, tzinfo=timezone.utc).timestamp()
NOW_1850 = datetime(2026, 8, 16, 18, 50, 0, tzinfo=timezone.utc).timestamp()
RATE_LIMIT_TAIL = "API Error: 429 rate limit, window resets at 02:48:21 SGT"


def _facts(text: str, age_min: float = 5) -> TailFacts:
    epoch = NOW_1840 - age_min * 60
    return TailFacts([(epoch, text)], epoch, text)


def _run(rows, transcripts, *, claims=None, nodes=None, now_s=NOW_1840):
    return verdicts(
        rows,
        transcript_for=lambda sid: transcripts.get(sid),
        claim_for=lambda node: (claims or {}).get(node, {}),
        node_state_for=lambda node: (nodes or {}).get(node),
        now_s=now_s,
    )


# ---------------------------------------------------------------------------
# Verdicts
# ---------------------------------------------------------------------------

def test_no_transcript_working_row_is_ghost_and_apply_all_still_refuses():
    rows = [Row("bbbb2222-0000", "w1", "working", None, "/tmp/w1", None)]
    [v] = _run(rows, {})
    assert v.verdict == GHOST
    assert v.basis == "no transcript for bbbb2222-0000"
    assert v.action == "report"
    outcome, detail = apply_verdict(v, lanes="all")
    assert outcome == "reported"
    assert "outside" in detail


def test_healthy_injectable_row_is_leave():
    rows = [Row("aaaa1111-0000", "w1", "working", None, "/tmp/w1", None)]
    [v] = _run(rows, {"aaaa1111-0000": _facts("still on it")})
    assert v.verdict == LEAVE
    assert "reachable" in v.basis


def test_sgt_stamp_is_reroute_at_1840_and_wake_at_1850():
    row = Row("cccc3333-0000", "r1", "blocked", None, "/tmp/r1", None)
    [before] = _run([row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL)})
    assert before.verdict == REROUTE
    assert "18:48:21Z" in before.basis and "8m out" in before.basis
    [after] = _run(
        [row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL)}, now_s=NOW_1850
    )
    assert after.verdict == WAKE
    assert "window passed" in after.basis


def test_unparseable_reset_stamp_is_leave_never_wake():
    assert rate_limit_window("429 quota exceeded", NOW_1840)[0] == "unknown"
    rows = [Row("dddd4444-0000", "k1", "blocked", None, "/tmp/k1", None)]
    [v] = _run(rows, {"dddd4444-0000": _facts("429 quota exceeded, try later")})
    assert v.verdict == LEAVE
    assert "unknown" in v.basis


def test_identity_joins_on_claim_holder_not_name():
    # Two rows whose NAMES both carry node x-9d11, but the recorded manifest
    # node differs (x-2222 vs none). Only the claim/manifest join may decide.
    rows = [
        Row("eeee1111-0000", "target-x-9d11-alpha", "working", "x-2222", "/tmp/a", None),
        Row("ffff2222-0000", "target-x-9d11-beta", "working", None, "/tmp/b", None),
    ]
    transcripts = {r.row_id: _facts("ok") for r in rows}
    vs = _run(
        rows,
        transcripts,
        claims={"x-2222": {"state": "live", "holder": "target-session:zzzz9999-0000"}},
    )
    by_name = {v.name: v for v in vs}
    assert by_name["target-x-9d11-alpha"].verdict == REAP
    assert "claim held by zzzz9999-0000" in by_name["target-x-9d11-alpha"].basis
    # The same-name row with no recorded node and no transcript for the OTHER
    # holder is untouched by the claim it never joined.
    assert by_name["target-x-9d11-beta"].verdict == LEAVE


def test_node_done_reaps_and_own_claim_does_not():
    rows = [
        Row("aaaa1111-0000", "w1", "working", "x-done", "/tmp/w1", None),
        Row("bbbb2222-0000", "w2", "working", "x-mine", "/tmp/w2", None),
    ]
    transcripts = {r.row_id: _facts("ok") for r in rows}
    vs = _run(
        rows,
        transcripts,
        claims={"x-mine": {"state": "live", "holder": "target-session:bbbb2222-0000"}},
        nodes={"x-done": {"status": "done"}},
    )
    assert vs[0].verdict == REAP and "node x-done done" in vs[0].basis
    # Holding your own claim is ownership, not a reap reason.
    assert vs[1].verdict == LEAVE


def test_bus_only_row_is_wakeable():
    rows = [Row("dddd4444-0000", "k1", "stopped", None, "/tmp/k1", "bus-only")]
    [v] = _run(rows, {"dddd4444-0000": _facts("stopped mid turn", age_min=30)})
    assert v.verdict == WAKE


# ---------------------------------------------------------------------------
# Apply lanes
# ---------------------------------------------------------------------------

class _Proc:
    def __init__(self, returncode=0, stderr="", stdout=""):
        self.returncode = returncode
        self.stderr = stderr
        self.stdout = stdout


def test_wake_reports_failure_when_message_missing_despite_working_state(
    monkeypatch,
):
    """The state field reading working is not evidence: wake.sh printed
    working -> working for a wake whose message never landed."""
    v = Verdict("dddd4444-0000", "k1", "stopped", WAKE, "stopped 30m", "resume")
    monkeypatch.setattr(
        watchdog, "tail_facts", lambda sid, cwd: _facts("stopped mid turn")
    )
    outcome, detail = apply_verdict(
        v, lanes="wake", cwd="/tmp/k1", runner=lambda *a, **k: _Proc(0)
    )
    assert outcome == "refused"
    assert "not in the transcript" in detail


def test_wake_applies_when_message_lands(monkeypatch):
    v = Verdict("dddd4444-0000", "k1", "stopped", WAKE, "stopped 30m", "resume")
    reads = {"n": 0}

    def fake_tail(sid, cwd):
        # First read (pre-wake marker) has no message; the post-wake read
        # carries it at a LATER epoch, the way a landed message really lands.
        reads["n"] += 1
        if reads["n"] == 1:
            return _facts("stopped mid turn")
        return _facts("continue", age_min=0)

    monkeypatch.setattr(watchdog, "tail_facts", fake_tail)
    outcome, detail = apply_verdict(
        v, lanes="wake", cwd="/tmp/k1", runner=lambda *a, **k: _Proc(0)
    )
    assert outcome == "applied"


def test_wake_lane_only_wakes_even_with_lanes_all_available():
    reroute = Verdict("cccc3333-0000", "r1", "blocked", REROUTE, "429", "redispatch")
    outcome, _ = apply_verdict(reroute, lanes="wake", cwd="/tmp/r1")
    assert outcome == "reported"


def test_reap_refuses_on_unpushed_commits_with_count_named(tmp_path):
    import os

    repo = tmp_path / "reap-me"
    repo.mkdir()
    def git(*args):
        subprocess.run(["git", "-C", str(repo), *args], check=True,
                       capture_output=True)
    git("init", "-q", "-b", "main")
    git("config", "user.email", "t@example.com")
    git("config", "user.name", "t")
    (repo / "f.txt").write_text("one\n")
    git("add", "f.txt")
    git("commit", "-q", "-m", "one")
    bare = tmp_path / "up.git"
    subprocess.run(["git", "init", "-q", "--bare", str(bare)], check=True)
    git("remote", "add", "origin", str(bare))
    git("push", "-q", "-u", "origin", "main")
    (repo / "g.txt").write_text("two\n")
    git("add", "g.txt")
    git("commit", "-q", "-m", "two")
    assert os.environ.get("GIT_DIR") is None  # sanity: repo-local git only

    refusal = watchdog.worktree_refusal(str(repo))
    assert refusal is not None and "1 unpushed commit(s)" in refusal

    v = Verdict("eeee1111-0000", "w1", "working", REAP, "node x done", "stop+rm")
    outcome, detail = apply_verdict(v, lanes="all", cwd=str(repo))
    assert outcome == "refused" and "1 unpushed commit(s)" in detail


def test_reap_applies_on_clean_worktree(tmp_path):
    repo = tmp_path / "clean"
    repo.mkdir()
    def git(*args):
        subprocess.run(["git", "-C", str(repo), *args], check=True,
                       capture_output=True)
    git("init", "-q", "-b", "main")
    git("config", "user.email", "t@example.com")
    git("config", "user.name", "t")
    (repo / "f.txt").write_text("one\n")
    git("add", "f.txt")
    git("commit", "-q", "-m", "one")
    bare = tmp_path / "up2.git"
    subprocess.run(["git", "init", "-q", "--bare", str(bare)], check=True)
    git("remote", "add", "origin", str(bare))
    git("push", "-q", "-u", "origin", "main")

    stopped = []
    def runner(argv, **kw):
        stopped.append(argv)
        return _Proc(0)
    v = Verdict("eeee1111-0000", "w1", "working", REAP, "node x done", "stop+rm")
    outcome, _ = apply_verdict(v, lanes="all", cwd=str(repo), runner=runner)
    assert outcome == "applied"
    assert any("stop" in " ".join(a) for a in stopped)
    assert any("rm" in " ".join(a) for a in stopped)
    # rm is never forced: claude rm's own refusal on a dirty worktree is a
    # safety feature the lane leans on.
    assert not any("--force" in " ".join(a) for a in stopped)


def test_reroute_delegates_to_redispatch():
    seen = {}

    def fake_redispatch(candidate):
        seen["cwd"] = candidate.cwd
        seen["name"] = candidate.name
        return True

    v = Verdict("cccc3333-0000", "r1", "blocked", REROUTE, "429", "redispatch")
    outcome, _ = apply_verdict(
        v, lanes="all", cwd="/tmp/r1", redispatch_fn=fake_redispatch
    )
    assert outcome == "applied"
    assert seen == {"cwd": "/tmp/r1", "name": "r1"}


# ---------------------------------------------------------------------------
# Enumeration (change 3): a stopped row survives into the returned map
# ---------------------------------------------------------------------------

def test_stopped_row_survives_claude_agents_json(monkeypatch):
    from fno.agents.harnesses import claude as claude_mod

    payload = json.dumps([
        {"kind": "interactive", "name": "operator", "state": "idle"},
        {"kind": "background", "id": "baf9409a", "sessionId":
         "baf9409a-2af5-444a-846c-059e8fa2f758", "state": "stopped",
         "cwd": "/tmp/w", "name": "tgt-stopped"},
        {"kind": "background", "id": "51f36553", "state": "working",
         "cwd": "/tmp/w2", "name": "tgt-live"},
    ])
    argv_seen = {}

    def fake_run(argv, **kw):
        argv_seen["v"] = argv
        return subprocess.CompletedProcess(argv, 0, stdout=payload, stderr="")

    monkeypatch.setattr(claude_mod, "_subprocess_run", fake_run)
    rows, warnings = claude_mod.claude_agents_rows()
    live_map, _ = claude_mod.claude_agents_json()

    assert "--all" in argv_seen["v"]
    ids = {r.get("id") for r in rows}
    assert "baf9409a" in ids and "51f36553" in ids  # stopped row survives
    assert all(r.get("kind") != "interactive" for r in rows)
    assert live_map["baf9409a"]["live_status"] == "stopped"
    assert live_map["51f36553"]["live_status"] == "Working"
    assert warnings == []


def test_sweep_payload_shape():
    rows = [Row("aaaa1111-0000", "w1", "working", None, "/tmp", None)]
    payload = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: _facts("ok"),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    assert payload["generated_at"] == "2026-08-16T18:40:00Z"
    assert payload["verdicts"][0]["verdict"] == LEAVE
    assert payload["counts"] == {LEAVE: 1}
