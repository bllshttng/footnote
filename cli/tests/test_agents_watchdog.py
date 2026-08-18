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
    assert rate_limit_window([(None, "API Error: 429 quota exceeded")], NOW_1840)[0] == "unknown"
    rows = [Row("dddd4444-0000", "k1", "blocked", None, "/tmp/k1")]
    [v] = _run(rows, {"dddd4444-0000": _facts("API Error: 429 quota exceeded, try later")})
    assert v.verdict == LEAVE


def test_newest_429_stamp_decides_the_window():
    """Records join newest-last, so a first-match stamp search reads the
    OLDEST 429's reset. Two 429s in the tail with the old window passed and
    the newest still closed must read live, or the wake lane spends a turn
    inside the open window."""
    old = "API Error: 429 rate limit, window resets at 02:10:00 SGT"
    new = RATE_LIMIT_TAIL
    tail = [(None, old), (None, new)]
    window, epoch, _stamp = rate_limit_window(tail, NOW_1840)
    assert window == "live"
    assert epoch == datetime(2026, 8, 16, 18, 48, 21, tzinfo=timezone.utc).timestamp()
    # Once the NEWEST window has passed too, the row is wakeable again.
    assert rate_limit_window(tail, NOW_1850)[0] == "passed"
    # A newest rate record with no stamp of its own is unknown, never an
    # older record's stamp.
    assert rate_limit_window(
        [(None, "API Error: 429 resets 02:10:00 SGT"),
         (None, "API Error: 429 quota exceeded")],
        NOW_1840,
    )[0] == "unknown"


def test_taxonomy_quota_phrasings_mark_the_window():
    """The wake window must mark every spelling the failover taxonomy marks.
    Three workers died on the usage-limit phrasing on 2026-08-17, and a
    rate-limit-only regex reads their tails as ordinary silence."""
    assert rate_limit_window(
        [(None, "API Error: Claude usage limit reached. Window resets at 02:48:21 SGT")],
        NOW_1840,
    )[0] == "live"
    assert rate_limit_window(
        [(None, "API Error: quota exceeded, resets 02:48:21 SGT")], NOW_1840
    )[0] == "live"
    # A usage-limit tail with no parseable stamp is unknown: fail safe.
    assert rate_limit_window(
        [(None, "API Error Request rejected 429, Usage limit reached for 5 hour")],
        NOW_1840,
    )[0] == "unknown"
    # A real quota body with no error-shaped words still holds the window:
    # the classifier is the gate, never an extra word prefilter.
    assert rate_limit_window(
        [(None, "429 rate limit reached, retry after 8s. window resets at 02:48:21 SGT")],
        NOW_1840,
    )[0] == "live"
    # Prose quoting the full vocabulary errs CLOSED (withholds a wake) - the
    # accepted cost, since erring open burns a turn inside a live window.
    assert rate_limit_window(
        [(None, "we are rate limited on the search API. sync at 02:48:21 SGT")],
        NOW_1840,
    )[0] == "live"
    # A bare 429 mention without the taxonomy's phrases is not a rate event.
    assert rate_limit_window(
        [(None, "reviewing PR 429 now")], NOW_1840
    )[0] == "none"
    row = Row("ab12cd34-0000", "u1", "blocked", None, "/tmp/u1")
    [v] = _run(
        [row],
        {"ab12cd34-0000": _facts(
            "API Error Request rejected 429, Usage limit reached for 5 hour",
            age_min=125,
        )},
    )
    assert v.verdict == LEAVE


def test_chatty_attach_cannot_push_a_live_window_out_of_scope():
    """A 5-hour usage limit outlives any 15-record tail: the window reads the
    60-record classification window, so restore noise between the 429 and
    the sweep cannot reopen a closed window."""
    stamp = "API Error: usage limit reached. resets at 02:48:21 SGT"
    noise = [(NOW_1840 - 60 * i, f"restore noise {i}") for i in range(30, 0, -1)]
    records = [(NOW_1840 - 31 * 60, stamp), *noise]
    facts = TailFacts(records, records[-1][0], " ".join(t for _, t in records))
    window, epoch, _stamp = rate_limit_window(facts.records, NOW_1840)
    assert window == "live"
    assert epoch == datetime(2026, 8, 16, 18, 48, 21, tzinfo=timezone.utc).timestamp()


def test_old_passed_plus_new_live_429_is_reroute_not_wake():
    row = Row("eeee5555-0000", "r2", "blocked", None, "/tmp/r2")
    old = "API Error: 429 rate limit, window resets at 02:10:00 SGT"
    facts = TailFacts(
        [(NOW_1840 - 130 * 60, old), (NOW_1840 - 125 * 60, RATE_LIMIT_TAIL)],
        NOW_1840 - 125 * 60,
        f"{old} {RATE_LIMIT_TAIL}",
        "assistant",
        RATE_LIMIT_TAIL,
        "text",
    )
    [v] = _run([row], {"eeee5555-0000": facts})
    assert v.verdict == REROUTE
    assert "18:48:21Z" in v.basis


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
        # graph_node_id is the documented join key; title is the free-text
        # task input and must join nothing - not even a bare node-shaped one.
        {"title": "fix the login bug", "graph_node_id": "x-d214",
         "sessions": ["e65d5fff-8ba4-46d4-b2df-f19b2eb832f1"]},
        {"title": "x-d214", "sessions": ["aaaa1111-0000"]},
    ]}))
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)
    nodes = watchdog._ledger_nodes()
    assert nodes["e65d5fff-8ba4-46d4-b2df-f19b2eb832f1"] == "x-d214"
    assert "aaaa1111-0000" not in nodes


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
        watchdog, "tail_facts", lambda *a, **k: _facts("stopped mid turn")
    )
    outcome, detail = apply_verdict(
        v, lanes="wake", cwd="/tmp/k1", runner=lambda *a, **k: _Proc(0)
    )
    assert outcome == "refused"
    assert "not in the transcript" in detail


def test_wake_applies_when_message_lands(monkeypatch):
    v = Verdict("dddd4444-0000", "k1", "stopped", WAKE, "stopped 30m", "resume")
    reads = {"n": 0}

    def fake_tail(*a, **k):
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


def test_lifecycle_subprocesses_run_in_the_rows_worktree(monkeypatch):
    """A registry-less row from another project must resolve in its own
    project: the delegated resume carries the row's cwd, never the sweep
    caller's ambient one."""
    seen = {}
    reads = {"n": 0}

    def fake_tail(*a, **k):
        reads["n"] += 1
        if reads["n"] == 1:
            return _facts("stopped mid turn")
        return _facts("continue", age_min=0)

    def runner(argv, **kw):
        seen["cwd"] = kw.get("cwd")
        return _Proc(0)

    monkeypatch.setattr(watchdog, "tail_facts", fake_tail)
    v = Verdict("dddd4444-0000", "k1", "stopped", WAKE, "stopped 30m", "resume")
    outcome, detail = apply_verdict(
        v, lanes="wake", cwd="/tmp/other-project", runner=runner
    )
    assert outcome == "applied", detail
    assert seen["cwd"] == "/tmp/other-project"


def test_untimestamped_continue_record_does_not_confirm_wake(monkeypatch):
    """A torn/summary record with no timestamp of its own is presence, not a
    landing: it must not confirm a wake whose message never arrived."""
    before_epoch = NOW_1840 - 5 * 60
    facts = TailFacts(
        [(None, "we continue with the plan"), (before_epoch, "old turn")],
        before_epoch,
        "we continue with the plan old turn",
    )
    monkeypatch.setattr(watchdog, "tail_facts", lambda *a, **k: facts)
    assert not watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", before_epoch
    )


def test_wake_lane_only_wakes_even_with_lanes_all_available():
    reroute = Verdict("cccc3333-0000", "r1", "blocked", REROUTE, "429", "redispatch")
    outcome, _ = apply_verdict(reroute, lanes="wake", cwd="/tmp/r1")
    assert outcome == "reported"


def test_wake_confirmation_requires_the_exact_message(monkeypatch):
    """A substring match read 'Let me continue with the tests' as a landed
    wake. The record's whole text must equal the message."""
    before = NOW_1840 - 5 * 60
    facts = TailFacts(
        [(before, "stopped mid turn"), (NOW_1840, "Let me continue with the tests")],
        NOW_1840,
        "stopped mid turn Let me continue with the tests",
    )
    monkeypatch.setattr(watchdog, "tail_facts", lambda *a, **k: facts)
    assert not watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", before
    )


def test_wake_confirmation_scans_past_the_classification_tail(monkeypatch):
    """A chatty attach pushes the message past the 15-record classification
    tail. Confirmation reads deeper, so a landed wake never reads refused."""
    marker = NOW_1840 - 400 * 60
    records = [(marker, "continue")] + [
        (marker + i * 10, f"restore noise {i}") for i in range(1, 101)
    ]
    facts = TailFacts(records, records[-1][0], " ".join(t for _, t in records))
    # The message sits beyond the 60-record classification window and inside
    # the deeper confirmation window.
    assert all(
        t != "continue" for _e, t in facts.records[-watchdog._TAIL_RECORDS:]
    )
    assert any(
        t == "continue" for _e, t in facts.records[-watchdog._CONFIRM_RECORDS:]
    )
    monkeypatch.setattr(watchdog, "tail_facts", lambda *a, **k: facts)
    assert watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", marker - 60
    )


def test_tail_window_bounds_the_role_triple_too(monkeypatch, tmp_path):
    """last_role/last_text/last_kind must come from inside the same
    max_records window as the records they are classified against. A triple
    read from an arbitrarily older record pairs a stale text with a fresh
    age, and reap's tool gate can fire on an ancient tool call."""
    import fno.provenance.observed as obs

    lines = [{
        "timestamp": "2026-08-16T15:00:00Z",
        "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "ancient turn"}]},
    }]
    for i in range(80):
        lines.append({"timestamp": f"2026-08-16T1{i % 2}:{i:02d}:00Z",
                      "text": f"progress {i}"})
    path = tmp_path / "t.jsonl"
    path.write_text("\n".join(json.dumps(x) for x in lines))
    monkeypatch.setattr(obs, "resolve_transcript_path", lambda *a, **k: path)

    facts = watchdog.tail_facts("sid", "/tmp")
    assert len(facts.records) == watchdog._TAIL_RECORDS
    assert facts.records[-1][1] == "progress 79"
    # The ancient role-bearing record fell outside the window, so the triple
    # reads empty rather than stale.
    assert facts.last_role is None and facts.last_text == ""
    assert facts.last_kind is None


def test_rows_without_a_session_id_are_skipped_loudly(monkeypatch, tmp_path):
    """A short id never resolves a transcript, so a live row keyed on it
    reads ghost. Such rows are skipped with a warning, never classified."""
    from fno.agents import registry as registry_mod
    from fno.agents.harnesses import claude as claude_mod
    import fno.paths as paths_mod

    raw = [
        {"id": "51f36553", "state": "working", "cwd": "/tmp/w2", "name": "tgt-live"},
        {"sessionId": "baf9409a-2af5-444a-846c-059e8fa2f758", "state": "stopped",
         "cwd": "/tmp/w", "name": "tgt-stopped"},
    ]
    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda: (raw, []))
    monkeypatch.setattr(
        registry_mod, "load_registry", lambda: (_ for _ in ()).throw(OSError())
    )
    monkeypatch.setattr(paths_mod, "ledger_json", lambda: tmp_path / "no.json")

    rows, warnings = watchdog.fleet_rows()
    assert [r.row_id for r in rows] == ["baf9409a-2af5-444a-846c-059e8fa2f758"]
    assert any("no session id" in w for w in warnings)


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
    cwds = []
    def runner(argv, **kw):
        stopped.append(argv)
        cwds.append(kw.get("cwd"))
        return _Proc(0)
    v = Verdict("eeee1111-0000", "w1", "working", REAP, "node x done", "stop+rm")
    outcome, _ = apply_verdict(v, lanes="all", cwd=str(repo), runner=runner)
    assert outcome == "applied"
    assert any("stop" in " ".join(a) for a in stopped)
    assert any("rm" in " ".join(a) for a in stopped)
    # Both lifecycle calls resolve in the row's worktree, not the caller's.
    assert cwds == [str(repo), str(repo)]
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
    # The lifecycle address is the SESSION ID, not the display name: a
    # registry-less row's name is claude's friendly label with no registry
    # row behind it, so a stop by name cannot use the session-store fallback.
    assert seen == {"cwd": "/tmp/r1", "name": "cccc3333-0000", "swap": True}


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


def test_reroute_receipts_tell_the_truth(monkeypatch):
    """The receipt must not lie about an action already taken: on
    rotated-no-worker the stop and the node-claim force-release may already
    have run, and notified means a human ping, not a delivery."""
    monkeypatch.setattr(
        watchdog, "tail_facts", lambda sid, cwd: _facts(RATE_LIMIT_TAIL, age_min=125)
    )
    v = Verdict("cccc3333-0000", "r1", "blocked", REROUTE, "429", "redispatch")

    def returning(outcome_value):
        def fn(candidate, err):
            return outcome_value
        return fn

    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/tmp/r1", failover_fn=returning("rotated-no-worker")
    )
    assert outcome == "refused"
    assert "no replacement spawned" in detail and "Re-check" in detail
    assert "left as-is" not in detail

    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/tmp/r1", failover_fn=returning("notified")
    )
    assert outcome == "reported"
    assert "human notified" in detail

    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/tmp/r1", failover_fn=returning("no-swap")
    )
    assert outcome == "refused" and "left as-is" in detail


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


def test_event_lane_suppresses_unchanged_rows(monkeypatch, tmp_path):
    """The mail lane speaks on change, and so must the event lane: stuck
    rows on a 600s tick would append thousands of events a day saying one
    thing. The sweep file carries the event lane's own last signature so
    the two change gates never share state."""
    (tmp_path / "watchdog-sweep.json").write_text(json.dumps(
        {"source": "tick", "at": "x", "counts": {}, "signature": "",
         "events_signature": "dddd4444-0000:wake"}
    ))
    monkeypatch.setattr(
        watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json"
    )
    assert watchdog._last_events_signature() == "dddd4444-0000:wake"
    payload = _payload_two_rows()  # wake row dddd4444, leave row aaaa1111
    assert watchdog.fresh_non_leave(
        payload, watchdog._last_events_signature()
    ) == set()
    # A verdict that changed for the same row is fresh again.
    payload["verdicts"][1]["verdict"] = GHOST
    fresh = watchdog.fresh_non_leave(payload, "dddd4444-0000:wake")
    assert fresh == {"dddd4444-0000"}


def test_json_liveness_carries_watchdog_freshness(monkeypatch, tmp_path):
    """`pr-watch status --json` and the doctor read liveness_report_live, and
    that dict is the ONLY surface they see: a sweep starved while the pr_watch
    tick stayed healthy must read loud there, not only in the human status
    lines. The threshold is two CONFIGURED intervals, not the fixed default."""
    import os
    import time as _time
    from types import SimpleNamespace
    from fno.pr_watch import _install

    sweep = tmp_path / "watchdog-sweep.json"
    sweep.write_text(json.dumps({"source": "tick", "at": "x", "counts": {}}))
    # 3x a 600s interval: stale under any plausible clock skew.
    os.utime(sweep, (_time.time() - 1800,) * 2)
    monkeypatch.setattr(watchdog, "sweep_path", lambda: sweep)
    monkeypatch.setattr(_install, "_LAUNCH_AGENTS_DIR", tmp_path)
    monkeypatch.setattr(_install, "_launchctl_is_loaded", lambda: False)
    settings = SimpleNamespace(
        pr_watch=SimpleNamespace(enabled=True, interval_seconds=600),
        recovery=SimpleNamespace(watchdog="report"),
    )
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)

    report = _install.liveness_report_live()
    assert report["watchdog"]["stale"] is True
    assert report["watchdog"]["source"] == "tick"

    # Lane off: no freshness verdict manufactured for a lane nobody armed.
    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: SimpleNamespace(
            pr_watch=settings.pr_watch,
            recovery=SimpleNamespace(watchdog="off"),
        ),
    )
    assert "watchdog" not in _install.liveness_report_live()


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


def test_manual_apply_survives_one_crashing_row_and_emits_verdicts(
    monkeypatch, tmp_path
):
    """One broken row never aborts the rest of an --apply run, and the
    classification events ride apply modes exactly like the tick's."""
    import contextlib
    import io as _io
    from fno.agents import cli as agents_cli

    verdicts = [
        Verdict("aaaa1111-0000", "w1", "stopped", WAKE, "blocked 30m", "resume"),
        Verdict("bbbb2222-0000", "w2", "stopped", WAKE, "blocked 30m", "resume"),
    ]
    rows = [Row("aaaa1111-0000", "w1", "stopped", None, "/tmp/w1"),
            Row("bbbb2222-0000", "w2", "stopped", None, "/tmp/w2")]
    payload = {"generated_at": "x", "verdicts": [v._asdict() for v in verdicts],
               "counts": {WAKE: 2}, "warnings": []}
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (payload, rows))
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "mail_gate", lambda *a, **k: (True, "no recipient", "")
    )

    def crashy(v, **kw):
        if v.row_id.startswith("aaaa"):
            raise TypeError("boom")
        return "applied", "woke w2"

    monkeypatch.setattr(watchdog, "apply_verdict", crashy)
    events = []
    monkeypatch.setattr(
        watchdog, "emit_event", lambda kind, data: events.append(kind)
    )

    out = _io.StringIO()
    with contextlib.redirect_stdout(out):
        agents_cli.cmd_watchdog(json_out=True, apply=True, apply_all=False,
                                only=None, mail_to="")
    data = json.loads(out.getvalue().strip().splitlines()[-1])
    by_row = {r["row_id"]: r for r in data["results"]}
    assert by_row["aaaa1111-0000"]["outcome"] == "refused"
    assert "crashed" in by_row["aaaa1111-0000"]["detail"]
    assert by_row["bbbb2222-0000"]["outcome"] == "applied"
    # Verdict events fire in apply mode too, matching the tick's contract.
    assert events.count("watchdog_verdict") == 2
