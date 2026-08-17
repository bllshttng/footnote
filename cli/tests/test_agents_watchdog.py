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

import typer

from fno.agents import watchdog
from fno.agents.watchdog import (
    GHOST,
    LEAVE,
    REAP,
    REROUTE,
    Row,
    STALE,
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


def _facts(
    text: str, age_min: float = 5, role: str = "assistant", kind: str = "text"
) -> TailFacts:
    epoch = NOW_1840 - age_min * 60
    return TailFacts([(epoch, text)], epoch, text, role, text, kind)


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
    rows = [Row("bbbb2222-0000", "w1", "working", None, "/tmp/w1")]
    [v] = _run(rows, {})
    assert v.verdict == GHOST
    assert v.basis == "no transcript for bbbb2222-0000"
    assert v.action == "report"
    outcome, detail = apply_verdict(v, lanes="all")
    assert outcome == "reported"
    assert "outside" in detail


def test_healthy_injectable_row_is_leave():
    rows = [Row("aaaa1111-0000", "w1", "working", None, "/tmp/w1")]
    [v] = _run(rows, {"aaaa1111-0000": _facts("still on it")})
    assert v.verdict == LEAVE
    assert "reachable" in v.basis


def test_sgt_stamp_is_reroute_at_1840_and_wake_at_1850():
    row = Row("cccc3333-0000", "r1", "blocked", None, "/tmp/r1")
    # 125m silent: past classify_tail's 2h window, so the tail POSITIVELY
    # asserts the session owes its next move (stalled) - the wake gate's
    # resumability marker, not a missing-429 absence.
    [before] = _run([row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL, age_min=125)})
    assert before.verdict == REROUTE
    assert "18:48:21Z" in before.basis and "8m out" in before.basis
    [after] = _run(
        [row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL, age_min=125)}, now_s=NOW_1850
    )
    assert after.verdict == WAKE
    assert "window passed" in after.basis


def test_unparseable_reset_stamp_is_leave_never_wake():
    assert rate_limit_window("429 quota exceeded", NOW_1840)[0] == "unknown"
    rows = [Row("dddd4444-0000", "k1", "blocked", None, "/tmp/k1")]
    [v] = _run(rows, {"dddd4444-0000": _facts("429 quota exceeded, try later")})
    assert v.verdict == LEAVE
    assert "unknown" in v.basis


def test_identity_joins_on_claim_holder_not_name():
    # Two rows whose NAMES both carry node x-9d11, but the recorded manifest
    # node differs (x-2222 vs none). Only the claim/manifest join may decide.
    rows = [
        Row("eeee1111-0000", "target-x-9d11-alpha", "working", "x-2222", "/tmp/a"),
        Row("ffff2222-0000", "target-x-9d11-beta", "working", None, "/tmp/b"),
    ]
    transcripts = {r.row_id: _facts("ok", age_min=30) for r in rows}
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
        Row("aaaa1111-0000", "w1", "working", "x-done", "/tmp/w1"),
        Row("bbbb2222-0000", "w2", "working", "x-mine", "/tmp/w2"),
    ]
    transcripts = {r.row_id: _facts("ok", age_min=30) for r in rows}
    vs = _run(
        rows,
        transcripts,
        claims={"x-mine": {"state": "live", "holder": "target-session:bbbb2222-0000"}},
        nodes={"x-done": {"status": "done"}},
    )
    assert vs[0].verdict == REAP and "node x-done done" in vs[0].basis
    # Holding your own claim is ownership, not a reap reason.
    assert vs[1].verdict == LEAVE


def test_done_node_but_executing_leaves_not_reaps():
    # The c696fddd case (king ruling 2026-08-17): re-tasked after its PR
    # merged, basis transcript, last event a tool call 0 minutes ago. A done
    # node proves the old task ended and proves nothing about re-tasking.
    rows = [Row("c696fddd-0000", "t-xb56a", "working", "x-b56a", "/tmp/w")]
    [v] = _run(
        rows,
        {"c696fddd-0000": _facts("tool_use Bash", age_min=0, kind="tool")},
        nodes={"x-b56a": {"status": "done"}},
    )
    assert v.verdict == LEAVE
    assert "executing" in v.basis


def test_done_node_quiet_on_a_tool_call_still_leaves():
    # Quiet past the idle threshold but the LAST event is a tool call: the
    # session may be re-tasked and waiting on something long. Never reap on
    # tool activity.
    rows = [Row("c696fddd-0000", "t-xb56a", "working", "x-b56a", "/tmp/w")]
    [v] = _run(
        rows,
        {"c696fddd-0000": _facts("tool_use Bash", age_min=60, kind="tool")},
        nodes={"x-b56a": {"status": "done"}},
    )
    assert v.verdict == LEAVE
    assert "tool call" in v.basis


def test_stopped_row_is_wakeable():
    rows = [Row("dddd4444-0000", "k1", "stopped", None, "/tmp/k1")]
    [v] = _run(rows, {"dddd4444-0000": _facts("stopped mid turn", age_min=130)})
    assert v.verdict == WAKE


def test_wake_age_ceiling_buckets_old_rows_as_stale_not_wake():
    # The king's measured case: a session stopped 87852 minutes (61 days) is
    # not recovery - dead node, stale branch, stale context. Past the 1d
    # ceiling the row is a needs-human bucket and never reaches an action
    # lane, not even under --apply=all.
    rows = [Row("dddd4444-0000", "k1", "stopped", None, "/tmp/k1")]
    [v] = _run(rows, {"dddd4444-0000": _facts("stopped mid turn", age_min=87852)})
    assert v.verdict == STALE
    assert "past the 1d wake ceiling" in v.basis
    assert v.action == "report"
    outcome, _ = apply_verdict(v, lanes="all")
    assert outcome == "reported"


def test_deliverable_reaps_regardless_of_age():
    # King ruling 2026-08-17: the deliverable read outranks the age ceiling.
    # A row whose node is done with its PR merged reaps at any age; age
    # decides what to do with an UNKNOWN row, never a finished one.
    rows = [Row("e65d5fff-0000", "t-xd214-mail-raw-onto-rpc", "blocked",
                "x-d214", "/canonical")]
    [v] = _run(
        rows,
        {"e65d5fff-0000": _facts("blocked mid turn", age_min=87852)},
        nodes={"x-d214": {"status": "done", "pr_url": "https://github.com/pull/891"}},
    )
    assert v.verdict == REAP
    assert "node x-d214 done" in v.basis


def test_no_deliverable_ages_into_stale():
    # x-35cc reads status idea: no deliverable, so the ceiling is the test.
    rows = [Row("ab12cd34-0000", "t-x35cc", "stopped", "x-35cc", "/tmp")]
    [v] = _run(rows, {"ab12cd34-0000": _facts("stopped mid turn", age_min=61 * 1440)},
               nodes={"x-35cc": {"status": "idea"}})
    assert v.verdict == STALE


def test_ledger_join_finds_nodes_for_manifest_less_rows(monkeypatch, tmp_path):
    # A worker that ran in the canonical checkout has no manifest of its own;
    # the execution ledger's machine-written (sessions -> node) row is its
    # recorded identity. The ``t-`` shorthand name stays untrusted: the node's
    # dash is stripped in it, so the slug boundary is unknowable.
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"title": "x-d214", "sessions": ["e65d5fff-8ba4-46d4-b2df-f19b2eb832f1"]},
    ]}))
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)
    nodes = watchdog._ledger_nodes()
    assert nodes["e65d5fff-8ba4-46d4-b2df-f19b2eb832f1"] == "x-d214"


def test_no_parseable_evidence_never_wakes():
    # A tail with no parseable timestamp is the "basis no-evidence" row the
    # king caught marked wake: absence of evidence must never reach an action.
    facts = TailFacts([(None, "stopped mid turn")], None, "stopped mid turn",
                      "assistant", "stopped mid turn")
    rows = [Row("eeee6666-0000", "n1", "stopped", None, "/tmp/n1")]
    [v] = _run(rows, {"eeee6666-0000": facts})
    assert v.verdict == LEAVE
    assert "no parseable transcript evidence" in v.basis


def test_fresh_tail_does_not_owe_a_move():
    # Positive marker, not absence: a tail still inside classify_tail's 2h
    # window reads working, and working rows never wake - the watchdog does
    # not race a session that may still be moving.
    rows = [Row("ffff7777-0000", "f1", "stopped", None, "/tmp/f1")]
    [v] = _run(rows, {"ffff7777-0000": _facts("stopped mid turn", age_min=30)})
    assert v.verdict == LEAVE
    assert "does not owe a move" in v.basis


def test_stopped_row_under_live_429_window_waits_not_wakes():
    # Reroute only catches blocked rows; a stopped row whose window is still
    # closed must not wake into the bounce that costs a real turn.
    rows = [Row("eeee5555-0000", "s1", "stopped", None, "/tmp/s1")]
    [v] = _run(rows, {"eeee5555-0000": _facts(RATE_LIMIT_TAIL)})
    assert v.verdict == LEAVE
    assert "window not open" in v.basis and "18:48:21Z" in v.basis


def test_busy_row_without_transcript_is_ghost():
    # "busy" is claude's working spelling (_LIVE_STATUS_INPUT); missing it
    # here read a ghost as a healthy leave.
    rows = [Row("ffff6666-0000", "b1", "busy", None, "/tmp/b1")]
    [v] = _run(rows, {})
    assert v.verdict == GHOST


def test_generated_no_session_holder_never_reaps():
    # A generated holder ({stamp}-{pid junk}-{hex}, target_cli's fallback for
    # a context with no session env) is an operator/daemon context, not
    # another live session; stop+rm on its say-so destroys the wrong row.
    rows = [Row("aaaa1111-0000", "w1", "working", "x-op", "/tmp/w1")]
    transcripts = {r.row_id: _facts("ok") for r in rows}
    [v] = _run(
        rows,
        transcripts,
        claims={"x-op": {"state": "live",
                         "holder": "target-session:20260816T184000Z-cl123-1a2b3c"}},
    )
    assert v.verdict == LEAVE


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


def test_untimestamped_continue_record_does_not_confirm_wake(monkeypatch):
    """A torn/summary record with no timestamp of its own is presence, not a
    landing: it must not confirm a wake whose message never arrived."""
    before_epoch = NOW_1840 - 5 * 60
    facts = TailFacts(
        [(None, "we continue with the plan"), (before_epoch, "old turn")],
        before_epoch,
        "we continue with the plan old turn",
    )
    monkeypatch.setattr(watchdog, "tail_facts", lambda sid, cwd: facts)
    assert not watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", before_epoch
    )


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


def test_reroute_delegates_to_the_full_failover(monkeypatch):
    seen = {}

    def fake_failover(candidate, err):
        seen["cwd"] = candidate.cwd
        seen["name"] = candidate.name
        seen["swap"] = getattr(err, "triggers_swap", None)
        return "swapped"

    # The tail must classify swap-class or the lane refuses before delegating.
    monkeypatch.setattr(
        watchdog, "tail_facts", lambda sid, cwd: _facts(RATE_LIMIT_TAIL, age_min=125)
    )
    v = Verdict("cccc3333-0000", "r1", "blocked", REROUTE, "429", "redispatch")
    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/tmp/r1", failover_fn=fake_failover
    )
    assert outcome == "applied", detail
    assert seen == {"cwd": "/tmp/r1", "name": "r1", "swap": True}


def test_reroute_refuses_when_no_alternate_is_armed(monkeypatch):
    monkeypatch.setattr(
        watchdog, "tail_facts", lambda sid, cwd: _facts(RATE_LIMIT_TAIL, age_min=125)
    )

    def exhausted(candidate, err):
        return "queue-exhausted"

    v = Verdict("cccc3333-0000", "r1", "blocked", REROUTE, "429", "redispatch")
    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/tmp/r1", failover_fn=exhausted
    )
    # Refusing beats a stop/respawn loop onto the same capped account.
    assert outcome == "refused" and "queue-exhausted" in detail


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
    rows = [Row("aaaa1111-0000", "w1", "working", None, "/tmp")]
    payload, out_rows = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: _facts("ok"),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    assert payload["generated_at"] == "2026-08-16T18:40:00Z"
    assert payload["verdicts"][0]["verdict"] == LEAVE
    assert payload["counts"] == {LEAVE: 1}
    # Rows ride along index-aligned so apply lanes can reach each cwd.
    assert out_rows == rows


# ---------------------------------------------------------------------------
# The mail lane (push, not pull) and its change gate
# ---------------------------------------------------------------------------

def _payload_two_rows():
    return {
        "verdicts": [
            {"row_id": "aaaa1111-0000", "name": "w1", "state": "working",
             "verdict": LEAVE, "basis": "reachable", "action": "none"},
            {"row_id": "dddd4444-0000", "name": "k1", "state": "blocked",
             "verdict": WAKE, "basis": "blocked 30m", "action": "resume"},
        ],
        "counts": {LEAVE: 1, WAKE: 1},
    }


def test_digest_is_house_style_and_names_the_basis():
    text = watchdog.digest_text(_payload_two_rows())
    assert "- wake k1: blocked 30m" in text
    # The mail lane is style-gated at send time; a semicolon or the modal
    # 'could' makes the whole send exit non-zero without delivering, and a
    # bare verdict line under the header reads as an illegal mid-paragraph
    # wrap (rule 6) - the first tick's digest was refused by exactly that
    # and never delivered. Every body line after the header must be a list
    # item, which is the legal block shape.
    assert ";" not in text
    assert "could" not in text.lower()
    body = text.splitlines()
    assert body[1] == ""
    for line in body[2:]:
        assert line.startswith("- "), f"digest body line not a list item: {line!r}"


def test_mail_digest_sends_on_change_and_skips_when_unchanged(monkeypatch, tmp_path):
    import json as _json

    (tmp_path / "watchdog-sweep.json").write_text(_json.dumps(
        {"source": "tick", "at": "x", "counts": {}, "signature": "old-sig"}
    ))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json")
    sent = []

    def runner(argv, **kw):
        sent.append(argv)
        return _Proc(0, stdout="msg-1 delivered (hosted)\n")

    payload = _payload_two_rows()
    ok, receipt = watchdog.mail_digest(payload, "king", runner=runner)
    assert ok and "delivered" in receipt
    assert sent and "mail" in " ".join(sent[0]) and "king" in sent[0]

    # Same signature as the stored sweep: no second send, no spam.
    watchdog.write_sweep_file(
        "tick", payload["counts"], NOW_1840, watchdog.verdict_signature(payload)
    )
    sent.clear()
    ok2, detail2 = watchdog.mail_digest(payload, "king", runner=runner)
    assert ok2 and "unchanged" in detail2
    assert sent == []


def test_mail_digest_project_recipient(monkeypatch, tmp_path):
    import json as _json

    (tmp_path / "watchdog-sweep.json").write_text(_json.dumps(
        {"source": "tick", "at": "x", "counts": {}, "signature": ""}
    ))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json")
    seen = {}

    def runner(argv, **kw):
        seen["argv"] = argv
        return _Proc(0, stdout="msg-2 queued (durable)\n")

    ok, _ = watchdog.mail_digest(_payload_two_rows(), "project:fno", runner=runner)
    assert ok
    assert "--to-project" in seen["argv"] and "fno" in seen["argv"]


def test_staleness_reads_loud_and_never_clean(monkeypatch, tmp_path):
    import json as _json
    import os

    path = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)

    # No sweep ever ran: the loudest case, never a clean one.
    assert watchdog.sweep_staleness(now_s=NOW_1840)["stale"] is True

    path.write_text(_json.dumps({"source": "tick", "at": "x", "counts": {}}))
    fresh_age = 300
    os.utime(path, (NOW_1840 - fresh_age, NOW_1840 - fresh_age))
    assert watchdog.sweep_staleness(now_s=NOW_1840) == {
        "age_s": fresh_age, "stale": False, "source": "tick", "at": "x",
    }

    dead_age = 26 * 3600  # the measured case: a cadence dead for 26 hours
    os.utime(path, (NOW_1840 - dead_age, NOW_1840 - dead_age))
    s = watchdog.sweep_staleness(now_s=NOW_1840)
    assert s["stale"] is True and s["age_s"] == dead_age


def test_failed_send_keeps_the_gate_open(monkeypatch, tmp_path):
    """A digest that failed to deliver must not advance the change gate: the
    stamp stays the PREVIOUS signature so the next sweep retries instead of
    swallowing the verdict behind a signature it never sent."""
    import json as _json

    (tmp_path / "watchdog-sweep.json").write_text(_json.dumps(
        {"source": "tick", "at": "x", "counts": {}, "signature": "old-sig"}
    ))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json")

    ok, receipt, stamp = watchdog.mail_gate(
        _payload_two_rows(), "king", runner=lambda *a, **k: _Proc(1, stderr="boom")
    )
    assert not ok
    assert stamp == "old-sig"

    # Delivered: the current signature advances the gate.
    ok2, _, stamp2 = watchdog.mail_gate(
        _payload_two_rows(), "king", runner=lambda *a, **k: _Proc(0)
    )
    assert ok2 and stamp2 == watchdog.verdict_signature(_payload_two_rows())

    # No recipient configured: nothing to warn about, gate unchanged.
    ok3, _, stamp3 = watchdog.mail_gate(_payload_two_rows(), "")
    assert ok3 and stamp3 == "old-sig"


# ---------------------------------------------------------------------------
# x-4c87: a zero-row roster is an unreadable instrument, never an empty fleet
# ---------------------------------------------------------------------------

def _refused_payload():
    payload, rows = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: ([], ["claude agents --json failed: exit 1"]),
    )
    assert rows == []
    return payload


def test_zero_row_roster_refuses_instead_of_sweeping_clean():
    """King report 2026-08-17: after a binary update the roster read 0 rows
    against an intact registry. A sweep over that writes counts={} and a
    fresh mtime, indistinguishable from a healthy quiet fleet. Zero rows must
    refuse: the payload says why and classifies nothing."""
    payload = _refused_payload()
    assert payload["refused"] and "0 rows" in payload["refused"]
    assert payload["verdicts"] == [] and payload["counts"] == {}
    # The instrument's own warning rides along, not swallowed.
    assert any("failed" in w for w in payload["warnings"])


def test_refused_sweep_writes_no_file_and_advances_no_gate(monkeypatch, tmp_path):
    import json as _json

    sweep = tmp_path / "watchdog-sweep.json"
    sweep.write_text(_json.dumps(
        {"source": "tick", "at": "x", "counts": {"wake": 1}, "signature": "prev"}
    ))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: sweep)
    payload = _refused_payload()

    fired = []

    def runner(argv, **kw):
        fired.append(argv)
        return _Proc(0)

    ok, receipt = watchdog.mail_digest(payload, "king", runner=runner)
    assert not ok and payload["refused"] in receipt
    ok2, receipt2, stamp = watchdog.mail_gate(payload, "king", runner=runner)
    assert not ok2 and stamp == "prev"
    assert fired == []  # nothing sent: zero rows read is not zero rows found

    # The stored sweep file is untouched - mtime and signature both survive,
    # so staleness (not a clean write) is what the next status read shows.
    stored = _json.loads(sweep.read_text())
    assert stored["signature"] == "prev" and stored["counts"] == {"wake": 1}


def test_cli_refused_sweep_exits_loud_without_writing(monkeypatch, tmp_path):
    from fno.agents import cli as agents_cli

    refused = _refused_payload()  # built BEFORE the patch it would recurse into
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (refused, []))
    wrote = []
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: wrote.append(a))
    monkeypatch.setattr(watchdog, "mail_gate", lambda *a, **k: (_ for _ in ()).throw(
        AssertionError("mail must not run on a refused sweep")
    ))

    try:
        agents_cli.cmd_watchdog(json_out=False, apply=False, apply_all=False,
                                only=None, mail_to=None)
    except typer.Exit as e:
        assert e.exit_code == 3
    else:
        raise AssertionError("a refused sweep must exit non-zero")
    assert wrote == []
