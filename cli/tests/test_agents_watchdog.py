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

import pytest
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
    UNCLAIMED,
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

# What a FINISHED session's tail actually looks like. The literal word
# "done" is not a finished session and never was: reap takes the promise
# marker classify_tail keys on, so a fixture that skips it is testing a
# session that merely stopped writing.
FINISHED_TAIL = "<promise>PR is green and reviewed</promise>"

# Well outside REAP_RECENT_MESSAGE_S measured from NOW_1840. A row that is meant
# to reach a REAP verdict has to carry a stale last_message_at, because x-944f
# makes an ABSENT stamp UNKNOWN: absence has two causes (nothing stamped it, the
# session never spoke) and the predicate cannot tell them apart. Fixtures that
# assert a refusal from an EARLIER read leave the stamp unset on purpose - that
# is what proves the earlier guard still answers first.
STALE_MESSAGE_STAMP = "2026-08-15T00:00:00Z"


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
    assert outcome == watchdog.SKIPPED
    assert "outside" in detail


def test_healthy_injectable_row_is_leave():
    rows = [Row("aaaa1111-0000", "w1", "working", None, "/tmp/w1")]
    [v] = _run(rows, {"aaaa1111-0000": _facts("still on it")})
    assert v.verdict == LEAVE
    # A healthy working row is a wake CANDIDATE and leaves at the stalled
    # gate, so its basis names the tail reading that decided it rather than
    # the generic no-lane line. The basis states what was measured either
    # way, never "reachable" - nothing here probes reachability.
    assert "does not owe a move" in v.basis


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
        Row("eeee1111-0000", "target-x-9d11-alpha", "working", "x-2222", "/tmp/a",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP, probe="dead"),
        Row("ffff2222-0000", "target-x-9d11-beta", "working", None, "/tmp/b"),
    ]
    transcripts = {r.row_id: _facts(FINISHED_TAIL, age_min=200) for r in rows}
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
        Row("aaaa1111-0000", "w1", "working", "x-done", "/tmp/w1",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP, probe="dead"),
        Row("bbbb2222-0000", "w2", "working", "x-mine", "/tmp/w2"),
    ]
    transcripts = {r.row_id: _facts(FINISHED_TAIL, age_min=200) for r in rows}
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
    # The tool-call guard answers first now; both readings say the same
    # thing, that this session is mid-task.
    assert "tool call" in v.basis


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
    assert "wake ceiling" in v.basis
    assert v.action == "report"
    outcome, _ = apply_verdict(v, lanes="all")
    assert outcome == watchdog.SKIPPED


def test_deliverable_reaps_regardless_of_age():
    # King ruling 2026-08-17: the deliverable read outranks the age ceiling.
    # A row whose node is done with its PR merged reaps at any age; age
    # decides what to do with an UNKNOWN row, never a finished one.
    rows = [Row("e65d5fff-0000", "t-xd214-mail-raw-onto-rpc", "blocked",
                "x-d214", "/canonical", origin="spawn",
                last_message_at=STALE_MESSAGE_STAMP, probe="dead")]
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
    monkeypatch.setattr(watchdog, "_CONFIRM_ATTEMPTS", 3)
    monkeypatch.setattr(watchdog, "_CONFIRM_INTERVAL_S", 0.0)
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
    monkeypatch.setattr(watchdog, "_CONFIRM_ATTEMPTS", 3)
    monkeypatch.setattr(watchdog, "_CONFIRM_INTERVAL_S", 0.0)
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
    monkeypatch.setattr(watchdog, "_CONFIRM_ATTEMPTS", 3)
    monkeypatch.setattr(watchdog, "_CONFIRM_INTERVAL_S", 0.0)
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
    monkeypatch.setattr(watchdog, "_CONFIRM_ATTEMPTS", 3)
    monkeypatch.setattr(watchdog, "_CONFIRM_INTERVAL_S", 0.0)
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
    assert outcome == watchdog.SKIPPED


def test_wake_confirmation_requires_the_exact_message(monkeypatch):
    monkeypatch.setattr(watchdog, "_CONFIRM_ATTEMPTS", 3)
    monkeypatch.setattr(watchdog, "_CONFIRM_INTERVAL_S", 0.0)
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
    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda **k: (raw, []))
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
    outcome, detail = apply_verdict(
        v, lanes="all", cwd=str(repo), reap_enabled=True
    )
    assert outcome == "refused" and "1 unpushed commit(s)" in detail


def test_reap_applies_on_clean_worktree(tmp_path, monkeypatch):
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
    # The delete-target guard has its own test below; this one exercises
    # the mechanism past it. The receipt gate has its own tests too.
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(watchdog, "_persist_reap_receipt", lambda rid: (True, "staged"))
    outcome, _ = apply_verdict(
        v, lanes="all", cwd=str(repo), runner=runner, reap_enabled=True
    )
    assert outcome == "applied"
    assert any("stop" in " ".join(a) for a in stopped)
    assert any("rm" in " ".join(a) for a in stopped)
    # Both lifecycle calls resolve in the row's worktree, not the caller's.
    assert cwds == [str(repo), str(repo)]
    # rm is never forced: claude rm's own refusal on a dirty worktree is a
    # safety feature the lane leans on.
    assert not any("--force" in " ".join(a) for a in stopped)


def test_reroute_delegates_to_the_full_failover(monkeypatch):
    # These exercise the failover mechanism PAST the shared-manifest
    # guard, which has its own test.
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
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
    # These exercise the failover mechanism PAST the shared-manifest
    # guard, which has its own test.
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
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
    # These exercise the failover mechanism PAST the shared-manifest
    # guard, which has its own test.
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
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
    # "partial" not "reported": the provider HAS rotated, and the lane-skip
    # word is dropped by every caller.
    assert outcome == "partial"
    assert "human notified" in detail

    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/tmp/r1", failover_fn=returning("no-swap")
    )
    assert outcome == "refused" and "left as-is" in detail


# ---------------------------------------------------------------------------
# Enumeration (change 3): a stopped row survives into the returned map
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Retire: stop a finished worker, destroy nothing
# ---------------------------------------------------------------------------
#
# A worker that finishes its deliverable and never exits holds a live slot
# against config.agents.max_live forever. terminal_stop.rs already stops that
# population, but its marker is written by finalize, which a /fno:blueprint or
# /fno:think worker never reaches. Nothing tells those workers to stop.

RETIRE_GRACE = 900


def _retire_run(rows, transcripts, *, now_s=NOW_1840, grace=RETIRE_GRACE):
    """The classifier with an EXPLICIT grace: reading the machine's real config
    inside a unit test makes the verdict depend on the developer's settings."""
    return verdicts(
        rows,
        transcript_for=lambda sid: transcripts.get(sid),
        claim_for=lambda node: {},
        node_state_for=lambda node: None,
        now_s=now_s,
        retire_grace_s_value=grace,
    )


def _spawned(row_id="dddd4444-0000", name="bp-worker", state="idle"):
    """A row footnote spawned, carrying the literal every spawn birth site
    writes. Retire keys on that positive value, so a fixture that invents its
    own spelling would test a lane no live row can enter."""
    return Row(row_id, name, state, None, "/tmp/bp", "spawn")


def test_a_finished_spawned_worker_past_the_grace_retires():
    row = _spawned()
    [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=20)})
    assert v.verdict == watchdog.RETIRE
    assert v.action == "stop"
    assert "declared itself done" in v.basis
    assert "worktree and row survive" in v.basis


def test_retire_stops_and_removes_nothing():
    """The whole reason this lane can be armed where reap cannot: one stop, no
    rm, and the receipt names the undo."""
    calls = []

    def runner(argv, **kw):
        calls.append(list(argv))
        return _Proc(0)

    v = Verdict("dddd4444-0000", "bp-worker", "idle", watchdog.RETIRE,
                "worker declared itself done", "stop")
    outcome, detail = apply_verdict(v, lanes="all", cwd="/tmp/bp", runner=runner)

    assert outcome == "applied"
    assert len(calls) == 1, "retire is exactly one call"
    assert "stop" in " ".join(calls[0])
    assert not any("rm" in a for a in calls[0]), "retire never removes anything"
    assert "resume" in detail, "the receipt must name the undo"


def test_a_pending_tool_call_never_retires():
    """A transcript's last record can be the assistant turn that ISSUED a tool
    call, which means the tool has not returned. A worker twenty minutes into a
    build, a `gh pr checks --watch` or a full test run is silent to every other
    read here, and if that same turn mentioned a promise the tail classifies
    `done` and this lane stops it mid-call. reap refuses on the same reading."""
    row = Row("dddd4444-0000", "bp-worker", "working", None, "/tmp/bp", "spawn")
    facts = _facts("Running the suite. " + FINISHED_TAIL, age_min=20, kind="tool")
    [v] = _retire_run([row], {row.row_id: facts})
    assert v.verdict != watchdog.RETIRE


def test_a_question_after_a_prose_mention_still_holds_the_row():
    """The tag appears in prose in this repo routinely, and a worker can mention
    it and THEN ask its question. A span-deleting strip matched from that FIRST
    mention to the only closing tag, took the question with it, and retired the
    row with the question stranded."""
    text = (
        "The loop keys on <promise> here.\n"
        "Should I widen it to <watching> too?\n"
        "<promise>MISSION COMPLETE</promise>"
    )
    assert watchdog._question_pending(_facts(text, age_min=20)) is True
    row = Row("dddd4444-0000", "bp-worker", "idle", None, "/tmp/bp", "spawn")
    [v] = _retire_run([row], {row.row_id: _facts(text, age_min=20)})
    assert v.verdict != watchdog.RETIRE, "the question outlives the promise"


def test_a_bare_prose_mention_alone_is_not_a_question():
    """The other direction of the same read: over-detection costs a slot that
    never gets reclaimed, so the cut may not answer yes on a mention alone."""
    text = "The loop keys on <promise> here.\n" + FINISHED_TAIL
    assert watchdog._question_pending(_facts(text, age_min=20)) is False


def test_retire_is_outside_the_wake_lane():
    """--apply is documented as the one action that cannot destroy work, and
    retire stops a session, so it needs --apply-all."""
    v = Verdict("dddd4444-0000", "bp-worker", "idle", watchdog.RETIRE, "b", "stop")
    outcome, detail = apply_verdict(v, lanes="wake")
    assert outcome == watchdog.SKIPPED
    assert "outside" in detail


def test_a_promise_that_also_asks_a_question_never_retires():
    """classify_tail returns on its FIRST match - watching, then promise, then
    question - so a turn that both promises and asks classifies `done` and never
    `your-move`. That is the modal shape here: a blueprint worker mails its plan
    and asks the operator one thing in the same turn. Retiring it strands the
    question, so the predicate asks again rather than trusting the classifier's
    single answer."""
    row = _spawned()
    tail = f"{FINISHED_TAIL}\nOne thing before I stop: should the grace stay 900?"
    [v] = _retire_run([row], {row.row_id: _facts(tail, age_min=20)})
    assert v.verdict != watchdog.RETIRE
    assert "question the operator owes" in v.basis


def test_a_question_asked_before_the_promise_never_retires():
    """The REAL shape, and the one the ordering above does not cover. The worker
    is instructed to emit its promise last (skills/target/references/
    pre-promise.md), so a turn that asks and then finishes ends on `>`, not on
    `?`. Reading `endswith("?")` against the raw text answers False on exactly
    the population `_question_pending` exists for, and the row retires with the
    question stranded."""
    row = _spawned()
    tail = "Plan mailed. Should I open the PR too?\n<promise>MISSION COMPLETE</promise>"
    [v] = _retire_run([row], {row.row_id: _facts(tail, age_min=20)})
    assert v.verdict != watchdog.RETIRE
    assert "question the operator owes" in v.basis


def test_tag_stripping_never_hides_a_pending_question():
    """The over-strip risk the tag regex introduces, pinned in both directions.

    Stripping terminal tags to find the real end of a turn can eat the question
    it was meant to expose. An UNCLOSED `<promise>` was the live case: stripping
    only the tag left its body behind as the apparent end of the turn, so a turn
    that genuinely ended on a question answered "nothing pending" and the worker
    retired with the operator still owing an answer.

    The false direction matters as much: over-stripping into a permanent "a
    question is pending" would stop the lane firing at all.
    """
    ask = "Should I open the PR?"
    for tail, pending in (
        (f"{ask}\n<promise>DONE</promise>", True),
        (f"{ask}\n<promise>DONE", True),               # unclosed: body is promise content
        (f"{ask}\n<promise/>", True),
        (f'{ask}\n<watching reason="ci" pr="1">', True),
        ("<promise>DONE</promise>\nOne more thing?", True),
        ("All green.\n<promise>DONE</promise>", False),
        ("All done, nothing pending.", False),
        ("<watching>x</watching>\nStill ok?\n<promise>D</promise>", True),
        # A bare prose mention consumes to end of text when stripped, so this
        # shape is caught by the RAW read instead. Agents working on this repo
        # write the tag in prose routinely; without the raw read the row retires
        # with the operator's question unanswered.
        ("the loop keys on <promise> here. Should I widen it?", True),
        ("I mentioned <watching> above. Ready to merge?", True),
        # And the raw read must not invent a question where there is none.
        ("the loop keys on <promise> here. Widening it now.", False),
    ):
        facts = _facts(tail, age_min=20)
        assert watchdog._question_pending(facts) is pending, tail


def test_a_clean_promise_with_no_question_still_retires():
    """The counterweight: stripping the terminal tags must not invent a question
    where there is none, or the lane never fires at all."""
    row = _spawned()
    for tail in (
        FINISHED_TAIL,
        "All done, PR is green.\n<promise>MISSION COMPLETE</promise>",
    ):
        [v] = _retire_run([row], {row.row_id: _facts(tail, age_min=20)})
        assert v.verdict == watchdog.RETIRE, tail


def test_a_promise_carrying_a_help_tag_never_retires():
    row = _spawned()
    tail = f"{FINISHED_TAIL}\n<help reason='blocked'>needs a ruling</help>"
    [v] = _retire_run([row], {row.row_id: _facts(tail, age_min=20)})
    assert v.verdict != watchdog.RETIRE
    assert "question the operator owes" in v.basis


def test_a_working_or_watching_tail_never_retires_at_any_age():
    """Silence is a reading about the last write, never a verdict about whether
    the work is over. Only a session that SAID it finished is retirable."""
    for tail in ("still on it", "<watching>pr 42</watching>"):
        row = _spawned()
        [v] = _retire_run([row], {row.row_id: _facts(tail, age_min=24 * 60)})
        assert v.verdict != watchdog.RETIRE, tail


def test_grace_zero_disarms_the_lane():
    row = _spawned()
    [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=20)}, grace=0)
    assert v.verdict != watchdog.RETIRE


def test_inside_the_grace_never_retires():
    """The follow-up window is the point: a worker that just delivered can still
    be asked one more thing."""
    row = _spawned()
    [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=5)})
    assert v.verdict != watchdog.RETIRE


def test_an_unjoined_or_operator_row_never_retires():
    """Only the positive `spawn` stamp retires. `operator` is a session a human
    started by hand, `adopted` is one the harness-store healer found already
    running (routinely an operator's own terminal), and None is a row the
    registry join did not answer for. An unanswered read is never a verdict."""
    for origin in ("operator", "adopted", None):
        row = Row("dddd4444-0000", "operator-session", "idle", None, "/tmp/bp", origin)
        [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=24 * 60)})
        assert v.verdict != watchdog.RETIRE, origin


def test_an_unreadable_transcript_never_retires():
    row = _spawned()
    [v] = _retire_run([row], {})
    assert v.verdict != watchdog.RETIRE


def test_a_row_whose_process_is_gone_never_retires():
    """Retire exists to reclaim a LIVE slot. A stopped process holds none, so
    stopping it again frees nothing and the receipt's promised undo is false."""
    for state in ("stopped", "exited", "killed"):
        row = Row("dddd4444-0000", "bp-worker", state, None, "/tmp/bp", "spawn")
        [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=20)})
        assert v.verdict != watchdog.RETIRE, state


def test_the_retirable_set_accounts_for_every_live_state_claude_has():
    """`_RETIRABLE_STATES` is hand-kept, so something has to make it track the
    harness. Nothing else does: a new claude status that `_LIVE_STATUS_INPUT`
    maps folds to a canonical word `_row_state` returns with an EMPTY drift
    warning, and a word absent from the set simply stops being classified. The
    lane would go quiet on that population and the slot leak would come back
    with nothing said.

    So every canonical word claude's live vocabulary folds to must be either
    retirable or listed here as a deliberate exclusion. Adding a status without
    deciding which fails this test."""
    from fno.agents.harnesses.claude import _LIVE_STATUS_INPUT

    # Deliberately NOT retirable, each for a reason `_RETIRABLE_STATES` records.
    excluded = {"blocked"}

    folded = {
        watchdog._CANONICAL_STATE.get(mapped, mapped.lower())
        for mapped in _LIVE_STATUS_INPUT.values()
    }
    unaccounted = folded - watchdog._RETIRABLE_STATES - excluded
    assert not unaccounted, (
        "claude gained live state(s) "
        + ", ".join(sorted(unaccounted))
        + "; decide whether retire acts on them and update _RETIRABLE_STATES "
        "or this test's exclusion list"
    )
    # The exclusion list is itself a claim about the harness: a word that is no
    # longer live must not sit here looking like a considered decision.
    assert excluded <= folded, "an exclusion naming a state claude no longer emits"


def test_a_live_pane_painting_done_still_retires():
    """The counterweight to the test above, and the reason retire keys on
    the stopped words rather than `_TERMINAL_STATES`. `Done` is a member of
    claude's own KNOWN_LIVE_STATUSES, so a pane wearing it is ALIVE - a worker
    that finished and parked, which is this lane's entire target population.
    Excluding it reads as caution and silently empties the lane."""
    for state in sorted(watchdog._RETIRABLE_STATES):
        row = Row("dddd4444-0000", "bp-worker", state, None, "/tmp/bp", "spawn")
        [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=20)})
        assert v.verdict == watchdog.RETIRE, state


def test_an_unmeasurable_state_never_retires():
    """The state guard is POSITIVE membership, and this is why. `_row_state`
    returns "" for a row carrying no state under either alias, and returns an
    unmapped new spelling verbatim. Neither is a stopped word, so a negative
    test admits both - and claude has already renamed that field once, when
    every row read "". Under a negative test the next rename turns one
    --apply-all into a fleet-wide stop of every row whose tail carries a
    promise, live workers included."""
    for state in ("", "Some New Spelling", "compacting", "completed"):
        row = Row("dddd4444-0000", "bp-worker", state, None, "/tmp/bp", "spawn")
        [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=20)})
        assert v.verdict != watchdog.RETIRE, state


def test_a_row_needing_input_never_retires():
    """`blocked` is claude's `Needs input`, and a worker that has finished does
    not need input - so the row is one a human owes something to, the opposite
    of what this lane reclaims. `_question_pending` cannot cover it: that reads
    the assistant's own text and a permission prompt is not assistant text.
    Stopping such a session takes the prompt away from the operator."""
    row = Row("dddd4444-0000", "bp-worker", "blocked", None, "/tmp/bp", "spawn")
    [v] = _retire_run([row], {row.row_id: _facts(FINISHED_TAIL, age_min=20)})
    assert v.verdict != watchdog.RETIRE


def test_an_unmarked_row_never_retires():
    """The spawn marker must be MEASURED, not derived from a missing field.
    `store_fallback` adopts any session it finds in the claude store - exactly
    the shape of an operator's own session the SessionStart hook never
    registered. Reading its origin as "worker" because the field is absent, or
    because it merely is not `operator`, would let --apply-all stop the
    operator's own terminal.

    The positive control rides along on purpose: without it every assertion
    here would still pass if the fixture stopped reaching the lane at all."""
    # `adopted`, and never the row's `status`. Keying on status was a hole:
    # status is a LIVENESS stamp that `fno agents reconcile` flips back to
    # "live" as soon as the session answers a probe, so the guard survived
    # exactly one pass and the adopted operator terminal then read as a worker.
    # `origin` is written once at registration and nothing flips it.
    facts = {"dddd4444-0000": _facts(FINISHED_TAIL, age_min=20)}
    for origin in (None, "adopted", "something-new"):
        row = Row("dddd4444-0000", "adopted-session", "idle", None, "/tmp/bp", origin)
        [v] = _retire_run([row], facts)
        assert v.verdict != watchdog.RETIRE, origin

    control = Row("dddd4444-0000", "bp-worker", "idle", None, "/tmp/bp", "spawn")
    [v] = _retire_run([control], facts)
    assert v.verdict == watchdog.RETIRE


def test_every_registry_row_is_born_with_an_origin_and_a_substrate():
    """The producer half, and it has to cover EVERY birth site in BOTH languages.

    The consumer only acts on a positive marker, so a birth path that forgets
    one does not open a hole - it makes the lane silently unsatisfiable for the
    workers that path creates, which is the same defect pointing the other way.

    Scanning only Python was that defect in the test itself. Rust mints rows
    too, through `RegistryEntry { .. }` literals in the daemon, the three
    `*_ask` create paths and both adopt paths, and a seventh added later would
    have shipped unmarked with nothing failing. So this walks the Rust struct
    literals as well, and accepts the attribute call form (`registry.AgentEntry`)
    that a plain `func.id` check lets through.

    The walk carries TWO markers. `origin` says what created the row;
    `substrate` says which lane it was spawned on. An explicit `None` is the
    point for substrate: it records that the writer considered the question
    and could not answer it, where an omission cannot be told apart from
    never-recorded - and a silent default of "pane" is the exact guess the
    field exists to replace.
    """
    import ast
    import pathlib
    import re

    missing = []

    pkg = pathlib.Path(watchdog.__file__).parent
    for path in sorted(pkg.glob("*.py")):
        for node in ast.walk(ast.parse(path.read_text())):
            if not isinstance(node, ast.Call):
                continue
            func = node.func
            name = getattr(func, "id", None) or getattr(func, "attr", None)
            if name != "AgentEntry":
                continue
            # `AgentEntry(**row)` rehydrates a row that was already born.
            if any(kw.arg is None for kw in node.keywords):
                continue
            for marker in ("origin", "substrate"):
                if not any(kw.arg == marker for kw in node.keywords):
                    missing.append(f"{path.name}:{node.lineno} ({marker})")

    # Rust has no AST here, so read the literal's field list: from the opening
    # brace to the matching close at the same indent. Struct-update syntax
    # (`..other`) inherits the field and is not a birth statement.
    rust = next(
        (d / "crates" / "fno-agents" / "src")
        for d in pkg.parents
        if (d / "crates" / "fno-agents" / "src").is_dir()
    )
    # Match the literal ANYWHERE on the line. Anchoring it to the start of the
    # line after indent was the same mistake this test exists to catch: it saw 8
    # of the 20 literals, because most are written `let e = RegistryEntry {` or
    # `entries.push(state::RegistryEntry {`, and it passed by not looking.
    opener = re.compile(r"(?<![A-Za-z0-9_])(?:[A-Za-z_][A-Za-z0-9_]*::)*RegistryEntry\s*\{\s*$")
    seen = 0
    for path in sorted(rust.rglob("*.rs")):
        lines = path.read_text().split("\n")
        for i, line in enumerate(lines):
            # Type positions, not literals: `struct X {`, `impl X {`, and any
            # signature whose return type is X. Widening the match to find the
            # literals swept these in, and a type position has no fields to
            # carry a marker.
            if re.search(r"\b(struct|impl|trait|enum)\b", line) or "->" in line:
                continue
            if opener.search(line) is None:
                continue
            seen += 1
            body = []
            for later in lines[i + 1:]:
                if later.strip() in ("}", "};", "})", "}),"):
                    break
                body.append(later)
            joined = "\n".join(body)
            for marker in ("origin", "substrate"):
                if ".." in joined and not re.search(
                    rf"^\s*{marker}:", joined, re.MULTILINE
                ):
                    continue  # struct-update inherits this marker
                if not re.search(rf"^\s*{marker}:", joined, re.MULTILINE):
                    missing.append(f"{path.name}:{i + 1} ({marker})")

    # A scan that matches nothing passes for the same reason a correct one does,
    # so state the floor. This is a positive control on the instrument, and it
    # caught the anchored regex above reading 8 sites instead of 20.
    assert seen >= 15, f"the Rust scan reached only {seen} literals; it is not looking"
    assert not missing, (
        "every registry row must state what created it and which lane it ran; "
        "unmarked at " + ", ".join(missing)
    )


def test_arming_the_lane_never_demotes_the_stale_escalation():
    """A retire near-miss is a LEAVE, and a LEAVE returned above the wake
    ceiling deleted the escalation for the rows that most need a human. Measured
    on review: a spawned row owing the operator an answer and quiet 13h read
    `stale / needs a human` with the lane off, and `leave / none` with it armed.
    The verdict must not depend on whether the lane is armed."""
    row = Row("dddd4444-0000", "bp-worker", "blocked", None, "/tmp/bp", "spawn")
    tail = f"{FINISHED_TAIL}\nShould the grace stay 900?"
    facts = {row.row_id: _facts(tail, age_min=13 * 60)}

    [off] = _retire_run([row], facts, grace=0)
    [armed] = _retire_run([row], facts, grace=RETIRE_GRACE)

    assert off.verdict == STALE
    assert armed.verdict == STALE, "arming the lane must not hide a needs-human row"
    assert "needs a human" in armed.basis


def test_arming_the_lane_replaces_the_stale_escalation_with_an_action():
    """The other half, and the one the test above cannot see because it pins
    `blocked`. `working` is the only state in both the wake lane and
    `_RETIRABLE_STATES`, so arming retire DOES change that row's verdict.

    That is allowed, and the invariant is narrower than "the verdict must not
    depend on whether the lane is armed". A stale escalation says a human should
    look. When the tail closes a promise and owes no question, there is nothing
    to look at, so arming must replace the escalation with an ACTION - never
    with `leave`, which is the silence the near-miss ordering bug produced."""
    row = Row("dddd4444-0000", "bp-worker", "working", None, "/tmp/bp", "spawn")
    facts = {row.row_id: _facts(FINISHED_TAIL, age_min=13 * 60)}

    [off] = _retire_run([row], facts, grace=0)
    [armed] = _retire_run([row], facts, grace=RETIRE_GRACE)

    assert off.verdict == STALE
    assert armed.verdict == watchdog.RETIRE
    assert armed.action == "stop", "an escalation may become an action, never silence"


def test_a_decorated_question_still_holds_the_row():
    """A closing question is rarely bare. Agents write it bold, quoted or
    parenthesised, and `endswith("?")` answered no for every one of those - so
    the row retired with the operator's question stranded, which is the single
    outcome this predicate exists to prevent."""
    for closing in (
        "**Do you want me to cover the migration path too?**",
        '"Should the grace stay 900?"',
        "(shall I widen it?)",
        "Should the grace stay 900?",
    ):
        text = f"Plan delivered.\n{closing}\n{FINISHED_TAIL}"
        assert watchdog._question_pending(_facts(text, age_min=20)) is True, closing
        row = Row("dddd4444-0000", "bp-worker", "working", None, "/tmp/bp", "spawn")
        [v] = _retire_run([row], {row.row_id: _facts(text, age_min=20)})
        assert v.verdict != watchdog.RETIRE, closing


def test_a_quoted_promise_never_retires_and_a_real_one_still_does():
    """Thirty-four files in this repo carry a literal closing promise tag, this
    module among them. A worker summarising a diff to one of them quotes the
    closed block, and every read below answers as if it had declared itself
    done. The lane ships armed, so that stops a live session by default.

    Both directions are pinned here. The lane must still act on a real promise
    that happens to sit near quoted material, or the fix would close the leak by
    emptying the lane."""
    promise = "<promise>PR is green and reviewed</promise>"
    cases = {
        "plain": (promise, True),
        "fenced": (f"Diff:\n```\n{promise}\n```\nStill working.", False),
        "tilde fenced": (f"Diff:\n~~~\n{promise}\n~~~\nStill working.", False),
        "inline span": (f"The tag is `{promise}` here.", False),
        # A turn that opens a fence and stops is a worker cut off mid-quote, so
        # everything after the opener is quoted. Requiring the closing fence read
        # this as a declaration.
        "unterminated fence": (f"Diff:\n```\n{promise}\n", False),
        "quote then real": (f"```\n{promise}\n```\n{promise}", True),
        "real then quote": (f"{promise}\n```\n{promise}\n```", True),
    }
    for name, (text, should_retire) in cases.items():
        row = Row("dddd4444-0000", "bp-worker", "working", None, "/tmp/bp", "spawn")
        [v] = _retire_run([row], {row.row_id: _facts(text, age_min=20)})
        retired = v.verdict == watchdog.RETIRE
        assert retired is should_retire, f"{name}: retired={retired}"


def test_a_prose_mention_of_the_tag_never_retires():
    """`classify_tail` answers `done` on any `<promise` in the last turn, prose
    mention included, and agents working on this repo write the tag in prose
    routinely. Reading that single answer stopped a live worker whose turn only
    said it was widening something. The done half asks for the CLOSED block, the
    same way the question half already refuses to trust a bare mention."""
    row = Row("dddd4444-0000", "bp-worker", "working", None, "/tmp/bp", "spawn")
    text = "the loop keys on <promise> here. Widening it now."
    [v] = _retire_run([row], {row.row_id: _facts(text, age_min=20)})
    assert v.verdict != watchdog.RETIRE


def test_a_turn_cut_off_mid_promise_never_retires():
    """An unclosed tag means the turn was cut off while writing its promise,
    which is not a worker calmly declaring itself finished. Refusing costs a
    slot that stays held; acting stops a session that never said it was done."""
    row = Row("dddd4444-0000", "bp-worker", "working", None, "/tmp/bp", "spawn")
    [v] = _retire_run([row], {row.row_id: _facts("<promise>MISSION COMPL", age_min=20)})
    assert v.verdict != watchdog.RETIRE


def test_an_open_429_window_never_retires():
    """A session waiting out a rate limit is silent and is NOT finished. Retire
    sits above the reroute lane, so without this it stops exactly the rows
    reroute exists to move onto a fresh account, and stops them without
    rotating anything. `reap_decision` refuses on the same reading."""
    row = Row("dddd4444-0000", "bp-worker", "blocked", None, "/tmp/bp", "spawn")
    tail = f"{FINISHED_TAIL}\n{RATE_LIMIT_TAIL}"
    [v] = _retire_run([row], {row.row_id: _facts(tail, age_min=20)})
    assert v.verdict != watchdog.RETIRE
    assert v.verdict == REROUTE, "the row belongs to reroute, not retire"


def test_the_predicate_is_the_only_route_to_a_retire_verdict():
    """The sibling of the reap version of this test, and for the same reason: a
    guard on one of N paths is decorative. Every condition retire refuses on -
    terminal state, open 429, pending question, grace - lives in
    `retire_decision`, so a second construction site is a bypass of all four."""
    import inspect

    source = inspect.getsource(watchdog._verdict_one)
    assert source.count("RETIRE,") == 1, (
        "a second RETIRE verdict site bypasses retire_decision"
    )
    assert "retire_decision(" in source


def test_reap_outranks_retire_on_the_same_row():
    """Precedence: ghost > reap > retire. A row that satisfies both takes the
    more specific verdict, so the reap lane's config freeze still governs it."""
    row = Row("eeee5555-0000", "w1", "idle", "x-1", "/tmp/w1", "spawn",
              STALE_MESSAGE_STAMP, "dead")
    monkey_nodes = {"x-1": {"status": "done"}}
    [v] = verdicts(
        [row],
        transcript_for=lambda sid: _facts(FINISHED_TAIL, age_min=20),
        claim_for=lambda node: {},
        node_state_for=lambda node: monkey_nodes.get(node),
        now_s=NOW_1840,
        retire_grace_s_value=RETIRE_GRACE,
    )
    assert v.verdict == REAP


def test_one_rotation_per_sweep_across_reroute_rows(monkeypatch):
    """_default_failover mutates the GLOBAL active provider and its storm cap
    is per row, so eight blocked rows would walk the whole account queue in
    one --apply-all. The sweep rotates once, like the recovery sweep."""
    # These exercise the failover mechanism PAST the shared-manifest
    # guard, which has its own test.
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(
        watchdog, "tail_facts", lambda *a, **k: _facts(RATE_LIMIT_TAIL, age_min=125)
    )
    calls = []

    def failover(candidate, err):
        calls.append(candidate.name)
        return "swapped"

    rotation = watchdog.RotationBudget()
    rows = [
        Verdict(f"cccc333{i}-0000", f"r{i}", "blocked", REROUTE, "429", "redispatch")
        for i in range(4)
    ]
    outcomes = [
        apply_verdict(
            v, lanes="all", cwd=f"/tmp/r{i}",
            failover_fn=failover, rotation=rotation,
        )
        for i, v in enumerate(rows)
    ]
    assert len(calls) == 1, calls
    assert outcomes[0][0] == "applied"
    for outcome, detail in outcomes[1:]:
        # "held", not the silent lane-skip word: an action was withheld and
        # the operator has to see that rows 2 and 3 did nothing.
        assert outcome == "held"
        assert "already rotated this sweep" in detail
    # With no budget passed the caller keeps the old per-row behavior.
    calls.clear()
    apply_verdict(rows[0], lanes="all", cwd="/tmp/r0", failover_fn=failover)
    assert len(calls) == 1


def test_wake_confirmation_polls_for_the_flushed_turn(monkeypatch):
    """resume returns when the live STATE reads working, before the injected
    turn is flushed, so a one-shot read reports a landed wake as refused."""
    reads = {"n": 0}

    def fake_tail(*a, **k):
        reads["n"] += 1
        if reads["n"] < 4:
            return _facts("stopped mid turn")
        return _facts("continue", age_min=0)

    monkeypatch.setattr(watchdog, "tail_facts", fake_tail)
    slept = []
    assert watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", NOW_1840 - 600,
        attempts=6, interval_s=0.01, sleep=slept.append,
    )
    assert slept, "a polling confirm must have waited at least once"

    # A wake that never lands still reports refused after the attempts run out.
    monkeypatch.setattr(
        watchdog, "tail_facts", lambda *a, **k: _facts("stopped mid turn")
    )
    assert not watchdog.confirm_wake_landed(
        "dddd4444-0000", "/tmp/k1", "continue", NOW_1840 - 600,
        attempts=3, interval_s=0.0, sleep=lambda _s: None,
    )


def test_shared_checkout_rows_never_inherit_one_manifest_node(
    monkeypatch, tmp_path
):
    """Under worktree.policy = never every session runs in the canonical
    checkout and reads the SAME graph_node_id. A done node must not make every
    quiet sibling reapable, so the manifest decides only for a LINKED worktree
    and the session-keyed ledger decides otherwise."""
    from fno.agents import registry as registry_mod
    from fno.agents.harnesses import claude as claude_mod
    import fno.paths as paths_mod

    shared = tmp_path / "canonical"
    (shared / ".fno").mkdir(parents=True)
    (shared / ".git").mkdir()  # a canonical checkout: .git is a DIRECTORY
    (shared / ".fno" / "target-state.md").write_text("graph_node_id: x-done\n")

    linked = tmp_path / "wt"
    (linked / ".fno").mkdir(parents=True)
    (linked / ".git").write_text("gitdir: /elsewhere\n")  # linked: a FILE
    (linked / ".fno" / "target-state.md").write_text("graph_node_id: x-mine\n")

    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"graph_node_id": "x-done", "sessions": ["aaaa1111-0000"]},
    ]}))
    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)
    monkeypatch.setattr(
        registry_mod, "load_registry", lambda: (_ for _ in ()).throw(OSError())
    )
    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda **k: ([
        {"sessionId": "aaaa1111-0000", "state": "working", "cwd": str(shared)},
        {"sessionId": "bbbb2222-0000", "state": "working", "cwd": str(shared)},
        {"sessionId": "cccc3333-0000", "state": "working", "cwd": str(linked)},
    ], []))

    rows, _warnings = watchdog.fleet_rows()
    by_id = {r.row_id: r for r in rows}
    # The ledger names the one session that really ran the node.
    assert by_id["aaaa1111-0000"].node == "x-done"
    # Its sibling on the same checkout inherits nothing.
    assert by_id["bbbb2222-0000"].node is None
    # A linked worktree still reads its own manifest.
    assert by_id["cccc3333-0000"].node == "x-mine"


def test_a_manual_sweep_never_certifies_the_cadence(monkeypatch, tmp_path):
    """A hand-run sweep refreshes the file but proves nothing about the
    launchd cadence, and the cadence is what the staleness read measures."""
    path = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)
    import os

    watchdog.write_sweep_file("manual", {LEAVE: 3}, NOW_1840)
    os.utime(path, (NOW_1840 - 30, NOW_1840 - 30))
    fresh_manual = watchdog.sweep_staleness(now_s=NOW_1840)
    assert fresh_manual["stale"] is True
    assert fresh_manual["source"] == "manual"

    watchdog.write_sweep_file("tick", {LEAVE: 3}, NOW_1840)
    os.utime(path, (NOW_1840 - 30, NOW_1840 - 30))
    assert watchdog.sweep_staleness(now_s=NOW_1840)["stale"] is False


def test_sweep_file_persists_terminal_harness_residue(monkeypatch, tmp_path):
    path = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)
    watchdog.write_sweep_file(
        "tick", {LEAVE: 3}, NOW_1840, terminal_harness_rows=3
    )
    assert json.loads(path.read_text())["terminal_harness_rows"] == 3


def test_sweep_file_stamps_recoverable_count_and_preserves_it_on_manual_sweep(
    monkeypatch, tmp_path
):
    path = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)

    watchdog.write_sweep_file("tick", {LEAVE: 3}, NOW_1840, recoverable_count=4)
    first = json.loads(path.read_text())
    watchdog.write_sweep_file("manual", {LEAVE: 3}, NOW_1840 + 60)
    second = json.loads(path.read_text())

    assert first["recoverable_count"] == 4
    assert second["recoverable_count"] == 4
    assert second["last_tick_epoch"] == first["last_tick_epoch"]


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
    rows = [
        Row("aaaa1111-0000", "w1", "working", None, "/tmp"),
        Row("bbbb2222-0000", "w2", "stopped", None, "/tmp"),
        Row("cccc3333-0000", "w3", "done", None, "/tmp"),
        Row("dddd4444-0000", "w4", "exited", None, "/tmp"),
    ]
    payload, out_rows = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: _facts("ok"),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
    )
    assert payload["generated_at"] == "2026-08-16T18:40:00Z"
    assert payload["verdicts"][0]["verdict"] == LEAVE
    assert payload["terminal_harness_rows"] == 3
    # Rows ride along index-aligned so apply lanes can reach each cwd.
    assert out_rows == rows


def test_recoverable_verdict_has_a_bounded_since_parser():
    assert watchdog.RECOVERABLE in watchdog.VERDICTS
    assert watchdog.parse_recovery_since("24h") == 24 * 3600
    with pytest.raises(ValueError, match="since"):
        watchdog.parse_recovery_since("yesterday")


def test_recoverable_sweep_zero_is_positive_evidence(monkeypatch, tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    scan = CodexRecoveryScan((), True, 0, 0, 0, ())
    monkeypatch.setattr(
        watchdog,
        "scan_recoverable_codex_rollouts",
        lambda *args, **kwargs: scan,
    )

    payload, rows, result = watchdog.run_recoverable_sweep(
        cwd=tmp_path, recency_seconds=24 * 3600, now_s=NOW_1840
    )

    assert result is scan
    assert rows == []
    assert payload["complete"] is True
    assert payload["counts"] == {watchdog.RECOVERABLE: 0}


def _recovery_candidate(tmp_path, index, *, usable=True):
    from fno.agents.discover import RecoverableCodexRollout

    session_id = f"01a039bb-0000-7000-8000-{index:012x}"
    rollout = tmp_path / f"rollout-{session_id}.jsonl"
    records = [
        {
            "type": "session_meta",
            "payload": {"id": session_id, "cwd": str(tmp_path)},
        }
    ]
    if usable:
        records.append(
            {
                "timestamp": "2026-08-16T18:40:00Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "resume marker"}],
                },
            }
        )
    rollout.write_text(
        "".join(json.dumps(record) + "\n" for record in records),
        encoding="utf-8",
    )
    mtime = rollout.stat().st_mtime
    last_event_at = "2026-08-16T18:40:00Z"
    return RecoverableCodexRollout(
        session_id=session_id,
        cwd=str(tmp_path),
        rollout_path=rollout,
        mtime=mtime,
        transcript_usable=usable,
        last_event_at=last_event_at if usable else None,
        last_turn_marker="resume marker" if usable else None,
        unusable_reason=None if usable else "no_readable_transcript_turn",
    )


def test_recoverable_sweep_reports_discovered_usable_and_unusable_counts(tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    usable = _recovery_candidate(tmp_path, 1)
    unusable = _recovery_candidate(tmp_path, 2, usable=False)
    scan = CodexRecoveryScan((usable, unusable), True, 2, 0, 0, ())

    payload, rows, result = watchdog.run_recoverable_sweep(
        cwd=tmp_path,
        recency_seconds=24 * 3600,
        now_s=NOW_1840,
        scan_fn=lambda *args, **kwargs: scan,
    )

    assert result is scan
    assert len(rows) == 2
    assert payload["recoverable_count"] == 2
    assert payload["usable_recoverable_count"] == 1
    assert payload["unusable_recoverable_count"] == 1
    by_id = {row["row_id"]: row for row in payload["verdicts"]}
    assert by_id[usable.session_id]["action"] == "adopt"
    assert by_id[unusable.session_id]["action"] == "refuse"
    assert "no_readable_transcript_turn" in by_id[unusable.session_id]["basis"]


def test_recoverable_sweep_filters_one_explicit_full_session_id(tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    first = _recovery_candidate(tmp_path, 20)
    selected = _recovery_candidate(tmp_path, 21)
    scan = CodexRecoveryScan((first, selected), True, 2, 0, 0, ())

    payload, rows, filtered = watchdog.run_recoverable_sweep(
        cwd=tmp_path,
        recency_seconds=24 * 3600,
        now_s=NOW_1840,
        scan_fn=lambda *args, **kwargs: scan,
        session_id=selected.session_id,
    )

    assert [row.session_id for row in filtered.recoverable] == [selected.session_id]
    assert [row.row_id for row in rows] == [selected.session_id]
    assert payload["recoverable_count"] == 1
    assert payload["usable_recoverable_count"] == 1
    assert payload["selected_session_id"] == selected.session_id


def test_recoverable_apply_refuses_unusable_candidate_without_writing(tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 3, usable=False)
    adopted = []

    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
        adopt_fn=lambda *args, **kwargs: adopted.append(args),
    )

    assert adopted == []
    assert results == [
        {
            "session_id": candidate.session_id,
            "outcome": "refused",
            "reason": "transcript_unusable",
            "transcript_usable": False,
            "detail": "transcript_unusable: no_readable_transcript_turn",
        }
    ]


def test_recoverable_apply_refuses_transcript_loss_after_adoption(tmp_path):
    from types import SimpleNamespace
    from fno import paths
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 4)
    registry_rows = []

    def adopt(_hit, **_kwargs):
        registry_rows.append(
            SimpleNamespace(
                harness="codex",
                harness_session_id=candidate.session_id,
                cwd=candidate.cwd,
                origin="adopted",
                name="01a039bb",
                log_path=str(
                    paths.state_dir() / "agents" / candidate.session_id / "output.jsonl"
                ),
            )
        )
        candidate.rollout_path.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": candidate.session_id,
                        "cwd": candidate.cwd,
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )

    def update_registry(updater, **_kwargs):
        registry_rows[:] = updater(list(registry_rows))
        return list(registry_rows)

    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
        adopt_fn=adopt,
        confine_fn=lambda token, hits, **kwargs: hits,
        load_registry_fn=lambda path: list(registry_rows),
        update_registry_fn=update_registry,
    )

    assert registry_rows == []
    assert results[0]["outcome"] == "refused"
    assert results[0]["reason"] == "transcript_changed"
    assert results[0]["transcript_usable"] is False
    assert results[0]["registry_rollback"] == "removed"
    assert "applied" not in results[0]["detail"]


def test_recoverable_apply_requires_complete_coverage_before_adoption(monkeypatch, tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    scan = CodexRecoveryScan(
        (), False, 1, 1, 0, ("rollout JSON failed: broken.jsonl",)
    )
    adopted = []

    results = watchdog.apply_recoverable(
        scan,
        scope_cwd=tmp_path,
        adopt_fn=lambda *args, **kwargs: adopted.append(args),
    )

    assert adopted == []
    assert results == [{
        "session_id": None,
        "outcome": "refused",
        "detail": "recovery coverage incomplete: rollout JSON failed: broken.jsonl",
    }]


def test_cli_recoverable_rejects_invalid_since_before_scanning(monkeypatch, tmp_path, capsys):
    from fno.agents import cli as agents_cli

    with pytest.raises(typer.Exit) as exc:
        agents_cli.cmd_watchdog(
            json_out=False,
            apply=False,
            apply_all=False,
            only=watchdog.RECOVERABLE,
            mail_to="",
            since="yesterday",
            cwd=str(tmp_path),
        )

    assert exc.value.exit_code == 2
    assert "invalid since" in capsys.readouterr().err


def test_cli_recoverable_dry_run_prints_completed_zero(monkeypatch, tmp_path, capsys):
    from fno.agents import cli as agents_cli
    from fno.agents.discover import CodexRecoveryScan

    scan = CodexRecoveryScan((), True, 0, 0, 0, ())
    real_run = watchdog.run_recoverable_sweep
    monkeypatch.setattr(
        watchdog,
        "run_recoverable_sweep",
        lambda **kwargs: real_run(
            **kwargs, scan_fn=lambda *args, **kw: scan
        ),
    )
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *args, **kwargs: None)
    monkeypatch.setattr(watchdog, "emit_event", lambda *args, **kwargs: None)
    monkeypatch.setattr(watchdog, "_last_events_signature", lambda: "")

    agents_cli.cmd_watchdog(
        json_out=False,
        apply=False,
        apply_all=False,
        only=watchdog.RECOVERABLE,
        mail_to="",
        since="24h",
        cwd=str(tmp_path),
    )

    assert (
        "recoverable=0 usable=0 unusable=0 complete=true"
        in capsys.readouterr().out
    )


def test_cli_recoverable_json_apply_names_result_counts(monkeypatch, tmp_path, capsys):
    from fno.agents import cli as agents_cli
    from fno.agents.discover import CodexRecoveryScan

    scan = CodexRecoveryScan((), True, 0, 0, 0, ())
    payload = {
        "generated_at": "2026-08-16T18:40:00Z",
        "verdicts": [],
        "counts": {watchdog.RECOVERABLE: 0},
        "warnings": [],
        "complete": True,
        "scanned_count": 0,
        "malformed_count": 0,
        "unreadable_count": 0,
        "cwd": str(tmp_path),
        "recoverable_count": 0,
        "usable_recoverable_count": 0,
        "unusable_recoverable_count": 0,
    }
    results = [
        {"session_id": "one", "outcome": "applied", "detail": "ok"},
        {"session_id": "two", "outcome": "refused", "detail": "no"},
        {"session_id": "three", "outcome": "deferred", "detail": "later"},
    ]
    monkeypatch.setattr(
        watchdog,
        "run_recoverable_sweep",
        lambda **kwargs: (payload, [], scan),
    )
    monkeypatch.setattr(watchdog, "apply_recoverable", lambda *args, **kwargs: results)
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *args, **kwargs: None)
    monkeypatch.setattr(watchdog, "emit_event", lambda *args, **kwargs: None)
    monkeypatch.setattr(watchdog, "_last_events_signature", lambda: "")

    agents_cli.cmd_watchdog(
        json_out=True,
        apply=True,
        apply_all=False,
        only=watchdog.RECOVERABLE,
        mail_to="",
        since="24h",
        cwd=str(tmp_path),
    )

    rendered = json.loads(capsys.readouterr().out)
    assert rendered["result_counts"] == {
        "applied": 1,
        "refused": 1,
        "deferred": 1,
    }


def test_recoverable_apply_checks_the_exact_adopted_registry_row(tmp_path):
    from types import SimpleNamespace
    from fno import paths
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 5)
    session_id = candidate.session_id
    scan = CodexRecoveryScan((candidate,), True, 1, 0, 0, ())
    adopted = []
    adopt_kwargs = {}

    def adopt(hit, **kwargs):
        adopted.append(hit)
        adopt_kwargs.update(kwargs)

    results = watchdog.apply_recoverable(
        scan,
        scope_cwd=tmp_path,
        adopt_fn=adopt,
        confine_fn=lambda token, hits, **kwargs: hits,
        load_registry_fn=lambda path: [SimpleNamespace(
            harness="codex",
            harness_session_id=session_id,
            cwd=str(tmp_path),
            origin="adopted",
            name="019f48e1",
            log_path=str(
                paths.state_dir() / "agents" / session_id / "output.jsonl"
            ),
        )],
    )

    # The follow-up path rides the adoption write, not a later patch.
    assert adopt_kwargs["log_path"] == str(
        paths.state_dir() / "agents" / session_id / "output.jsonl"
    )

    assert len(adopted) == 1
    assert results == [
        {
            "session_id": session_id,
            "outcome": "applied",
            "transcript_usable": True,
            "last_event_at": candidate.last_event_at,
            "last_turn_marker": candidate.last_turn_marker,
            "registry_row_count": 1,
            "detail": f"adopted {session_id} handle=019f48e1",
        }
    ]


def test_recoverable_apply_refuses_a_row_without_the_follow_up_path(tmp_path):
    from types import SimpleNamespace
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 7)
    session_id = candidate.session_id

    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
        adopt_fn=lambda hit, **kwargs: None,
        confine_fn=lambda token, hits, **kwargs: hits,
        load_registry_fn=lambda path: [SimpleNamespace(
            harness="codex",
            harness_session_id=session_id,
            cwd=candidate.cwd,
            origin="adopted",
            name="019f48e1",
            log_path="",
        )],
    )

    assert results[0]["outcome"] == "refused"
    assert results[0]["reason"] == "adoption_failed"
    assert "follow-up path" in results[0]["detail"]


def test_recoverable_apply_names_a_vanished_candidate(tmp_path):
    from fno.agents.discover import CodexRecoveryScan

    candidate = _recovery_candidate(tmp_path, 6)
    candidate.rollout_path.unlink()
    results = watchdog.apply_recoverable(
        CodexRecoveryScan((candidate,), True, 1, 0, 0, ()),
        scope_cwd=tmp_path,
    )

    assert results[0]["outcome"] == "refused"
    assert "vanished" in results[0]["detail"]


def test_recoverable_apply_defers_remaining_candidates_when_budget_is_spent(tmp_path):
    from fno.agents.discover import CodexRecoveryScan, RecoverableCodexRollout

    candidates = tuple(
        RecoverableCodexRollout(
            f"019f48e1-deferred-{index}", str(tmp_path), tmp_path / f"{index}.jsonl", NOW_1840
        )
        for index in range(2)
    )
    results = watchdog.apply_recoverable(
        CodexRecoveryScan(candidates, True, 2, 0, 0, ()),
        scope_cwd=tmp_path,
        should_apply=lambda: False,
    )

    assert [result["outcome"] for result in results] == ["deferred", "deferred"]
    assert all("next tick" in result["detail"] for result in results)


def test_cli_prints_the_terminal_harness_row_count(monkeypatch, capsys):
    from fno.agents import cli as agents_cli

    row = Row("aaaa1111-0000", "w1", "stopped", None, "/tmp")
    verdict = Verdict(
        "aaaa1111-0000", "w1", "stopped", LEAVE, "terminal", "none"
    )
    payload = {
        "generated_at": "x",
        "verdicts": [verdict._asdict()],
        "counts": {LEAVE: 1},
        "warnings": [],
        "terminal_harness_rows": 3,
    }
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (payload, [row]))
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: None)
    monkeypatch.setattr(watchdog, "mail_gate", lambda *a, **k: (True, "", ""))

    # The terminal-row count rides the DIAGNOSTIC table (--only); the default
    # surface is the unfinished-work report.
    agents_cli.cmd_watchdog(
        json_out=False, apply=False, apply_all=False, only=LEAVE, mail_to=""
    )

    assert "terminal harness rows: 3" in capsys.readouterr().out


def test_cli_default_report_escalates_findings_not_session_rows(monkeypatch):
    """The default surface escalates the unfinished-work findings and never
    builds a durable question out of session verdicts."""
    from fno.agents import cli as agents_cli
    from fno.agents import stale_escalate

    rows = [
        Row("stale-row", "stale-worker", "blocked", None, "/tmp/stale"),
        Row("wake-row", "wake-worker", "blocked", "x-1234", "/tmp/wake"),
    ]
    verdicts = [
        Verdict("stale-row", "stale-worker", "blocked", STALE, "blocked 14h", "human"),
        Verdict("wake-row", "wake-worker", "blocked", WAKE, "blocked 30m", "resume"),
    ]
    payload = {
        "generated_at": "x",
        "verdicts": [verdict._asdict() for verdict in verdicts],
        "counts": {STALE: 1, WAKE: 1},
        "warnings": [],
    }
    # The default path never reads the session sweep; pin that by refusing
    # if it does.
    monkeypatch.setattr(
        watchdog,
        "run_sweep",
        lambda **kw: (_ for _ in ()).throw(AssertionError("report must not sweep")),
    )
    finding = uw.Finding(
        kind=uw.KIND_STARTED,
        subject="x-7d02",
        basis="in_progress, claim free, idle 116h",
        clear_command="/fno:target x-7d02",
        node_id="x-7d02",
    )

    class _Snap:
        generated_at = "x"
        findings = (finding,)
        complete = True
        warnings = ()
        dimensions = {
            dim: uw.DimensionState(uw.MEASURED, 1 if dim == uw.KIND_STARTED else 0, None)
            for dim in uw.DIMENSIONS
        }

    monkeypatch.setattr(uw, "build_report", lambda roots, **kw: _Snap())
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: None)
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    captured = []
    monkeypatch.setattr(
        stale_escalate,
        "escalate_unfinished",
        lambda findings, **kwargs: captured.extend(findings) or ("recorded", "q-uw"),
    )

    agents_cli.cmd_watchdog(
        json_out=False, apply=False, apply_all=False, only=None, mail_to=""
    )

    assert [f.subject for f in captured] == ["x-7d02"]


def test_cli_refused_sweep_never_escalates(monkeypatch):
    from fno.agents import cli as agents_cli

    refused = _refused_payload()
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (refused, []))
    monkeypatch.setattr(
        uw,
        "build_report",
        lambda roots, **kw: (_ for _ in ()).throw(AssertionError("must not report")),
    )

    with pytest.raises(typer.Exit) as exc:
        agents_cli.cmd_watchdog(
            json_out=False, apply=False, apply_all=False, only=LEAVE, mail_to=""
        )

    assert exc.value.exit_code == 3


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
    assert "terminal harness rows: 0" in text
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


def test_machine_report_queues_from_unregistered_cwd(monkeypatch, tmp_path):
    import json as _json
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "agents"))

    (tmp_path / "watchdog-sweep.json").write_text(
        _json.dumps({"source": "tick", "at": "x", "counts": {}, "signature": ""})
    )
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "watchdog-sweep.json")
    unregistered = tmp_path / "runner"
    unregistered.mkdir()
    monkeypatch.chdir(unregistered)
    payload = {
        "verdicts": [
            {
                "row_id": f"row-{i}",
                "name": f"worker-{i}",
                "state": "blocked",
                "verdict": WAKE,
                "basis": "a measured watchdog basis with enough detail to exceed the authored word cap",
                "action": "resume",
            }
            for i in range(7)
        ],
        "counts": {WAKE: 7},
    }

    ok, receipt = watchdog.mail_digest(payload, "project:fno")

    assert ok
    assert "queued (durable)" in receipt
    from fno.bus.log import iter_messages

    [message] = [row for row in iter_messages() if row.to == "fno"]
    assert message.to_kind == "project"
    assert len(message.body.split()) > 80


def test_terminal_harness_residue_mails_even_when_every_verdict_is_leave(
    monkeypatch, tmp_path
):
    path = tmp_path / "watchdog-sweep.json"
    path.write_text(json.dumps({"signature": ""}))
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)
    sent = []

    def runner(argv, **kw):
        sent.append(argv)
        return _Proc(0, stdout="msg-3 delivered (hosted)\n")

    payload = {
        "verdicts": [
            {"row_id": "aaaa1111", "name": "w1", "state": "stopped",
             "verdict": LEAVE, "basis": "terminal", "action": "none"}
        ],
        "counts": {LEAVE: 1},
        "terminal_harness_rows": 1,
    }
    ok, receipt = watchdog.mail_digest(payload, "king", runner=runner)
    assert ok and "delivered" in receipt
    assert sent and "terminal harness rows: 1" in sent[0][-1]


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
        recovery=SimpleNamespace(watchdog="report", enabled=True),
        autonomy=SimpleNamespace(enabled=True),
    )
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)

    report = _install.liveness_report_live()
    assert report["watchdog"]["stale"] is True
    assert report["watchdog"]["source"] == "tick"

    # And the reader shares the TICK's condition, all three parts of it. The
    # master panic switch stops the sweep, so a freshness alarm about that
    # silence is an alarm about a deliberate decision - it used to fire
    # forever.
    settings.autonomy.enabled = False
    assert "watchdog" not in _install.liveness_report_live()
    settings.autonomy.enabled = True
    settings.recovery.enabled = False
    assert "watchdog" not in _install.liveness_report_live()

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
                                only=LEAVE, mail_to=None)
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
        watchdog, "emit_event",
        lambda kind, data, **kw: events.append(kind),
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


# ---------------------------------------------------------------------------
# Review round: the enumeration budget, shared worktrees, and honest receipts
# ---------------------------------------------------------------------------

def test_the_sweep_buys_the_whole_fleet_not_the_interactive_budget(monkeypatch):
    """The measured P1: the roster probe outran its own timeout every tick.

    ``claude agents --json --all`` is a fleet-wide live-status probe (3.4s /
    1.1s / 3.4s on a 43-row fleet), and inheriting the 3.0s interactive
    default returned zero rows, tripped ROSTER_REFUSAL, and left the lane
    reporting itself stale forever. Asserting the constant alone would pass
    while the call site kept the default, so this captures what is SPENT.
    """
    from fno.agents.harnesses import claude as claude_mod

    spent = {}

    def fake_rows(timeout=3.0):
        spent["timeout"] = timeout
        return [], []

    monkeypatch.setattr(claude_mod, "claude_agents_rows", fake_rows)
    watchdog.fleet_rows()
    assert spent["timeout"] == watchdog.ROSTER_TIMEOUT_S
    # The interactive default is the value that broke it; any budget at or
    # under it re-breaks the sweep on the same fleet that measured 3.4s.
    assert spent["timeout"] > claude_mod._AGENTS_JSON_TIMEOUT_DEFAULT


def test_a_shared_worktree_row_probed_dead_is_reapable_and_the_tree_is_held(
    monkeypatch,
):
    """The guard split (x-ad13): the ROW is metadata, the TREE is a checkout.

    Measured on the live fleet: two sessions working one linked worktree,
    both resolving one node, and the co-tenancy guard refusing every row on
    every tree - on this project every session shares one checkout. The
    co-tenancy fact now guards the WORKTREE branch at apply (the tally rides
    the verdict's ``cotenants`` field down to ``_apply_reap``), never the
    row verdict: a row on a shared tree, positively probed dead, earns REAP,
    and the apply holds the destructive step instead.
    """
    rows = [
        Row("aaaa1111-0000", "quiet", "working", "x-done", "/wt/x-bcb5",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP, probe="dead"),
        Row("bbbb2222-0000", "busy", "working", "x-done", "/wt/x-bcb5",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP),
    ]
    vs = _run(
        rows,
        {
            "aaaa1111-0000": _facts(FINISHED_TAIL, age_min=30),
            "bbbb2222-0000": _facts("tool_use Bash", age_min=0, kind="tool"),
        },
        nodes={"x-done": {"status": "done"}},
    )
    # Probed dead with a done node: reapable even though a peer shares the
    # tree. The verdict carries the tally for the apply lane.
    assert vs[0].verdict == REAP, vs[0].basis
    assert vs[0].cotenants == 1
    # The busy co-tenant is not reapable - its own tail says it is mid-task,
    # which is a fact about the ROW, never about the tree.
    assert vs[1].verdict != REAP

    # The worktree branch: the apply holds the rm and the tree is not
    # touched - no stop, no rm reaches the runner while a peer stands in it.
    monkey_flags = {"ran": False}

    def runner(argv, **kwargs):
        monkey_flags["ran"] = True
        return subprocess.CompletedProcess(argv, 0, "", "")

    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: None)
    v = Verdict("aaaa1111-0000", "quiet", "working", REAP,
                "node x-done done", "stop+rm", cotenants=1)
    outcome, detail = watchdog._apply_reap(v, cwd="/wt/x-bcb5", runner=runner)
    assert outcome == "refused" and "share /wt/x-bcb5" in detail
    assert "not touched" in detail
    assert monkey_flags["ran"] is False, "the destructive step ran on a shared tree"


def test_a_shared_worktree_row_probed_alive_or_unknown_is_refused_and_names_it():
    """The probe gate, named (x-ad13): on the same shared tree, a row the
    probe reports ALIVE refuses as NO, and a row the probe cannot answer
    refuses as UNKNOWN. Neither is ever a YES; the basis names which."""
    shared = {
        "aaaa1111-0000": _facts(FINISHED_TAIL, age_min=30),
    }
    nodes = {"x-done": {"status": "done"}}
    for probe, expect in (("alive", "leave"), ("unknown", "stale")):
        row = Row("aaaa1111-0000", "quiet", "working", "x-done", "/wt/x-bcb5",
                  origin="spawn", last_message_at=STALE_MESSAGE_STAMP,
                  probe=probe)
        [v] = _run([row], shared, nodes=nodes)
        assert v.verdict == expect, (probe, v.verdict, v.basis)
        assert "liveness probe" in v.basis, (probe, v.basis)
        assert probe in v.basis


def test_a_king_shaped_row_is_never_reaped():
    """The measured hazard the guard split was blocked on (x-ad13).

    `king-footnote-g4`, 2026-08-31: a live process, a stale ``exited_at``,
    a quiet heartbeat, the shared canonical checkout, a done node - every
    read this predicate had had a positive answer for except the one that
    asks whether the worker is still running. Relax the old co-tenancy
    guard without the probe gate and the first sweep reaps two live kings.
    The probe gate is what makes the split landable, so this exact shape
    must refuse forever.
    """
    row = Row("aaaa1111-0000", "king-footnote-g4", "working", "x-4c87",
              "/shared/canonical", origin="spawn",
              last_message_at=STALE_MESSAGE_STAMP, probe="alive")
    [v] = _run(
        [row],
        {"aaaa1111-0000": _facts(FINISHED_TAIL, age_min=120)},
        nodes={"x-4c87": {"status": "done"}},
    )
    assert v.verdict != REAP, v.basis
    assert "probe positively reports the worker alive" in v.basis, v.basis
    # The stop lane reads the same probe: retire must not stop a live king
    # either, however done its tail reads.
    assert v.verdict != watchdog.RETIRE, v.basis


def test_a_crowned_row_is_never_reaped_on_a_node_done_basis():
    """A king spans many nodes; one node's ``done`` says nothing about the
    crown. Measured 2026-08-31: all 14 ledger hits resolved to done nodes,
    including two live kings - the node-done basis selects crowned rows
    first. With the probe death proof STAGED (the stricter setup, so the
    crown rule is isolated from the probe gate), a crowned row still refuses
    where its uncrowned twin reaps."""
    common = dict(
        state="working", node="x-4c87", cwd="/shared/canonical",
        origin="spawn", last_message_at=STALE_MESSAGE_STAMP, probe="dead",
    )
    king = Row("aaaa1111-0000", "king-footnote-g4", crowned=True, **common)
    worker = Row("bbbb2222-0000", "plain-worker", crowned=False, **common)
    quiet = _facts(FINISHED_TAIL, age_min=120)
    nodes = {"x-4c87": {"status": "done"}}

    answer, basis = _decide(king, facts=quiet, nodes=nodes)
    assert answer == watchdog.REAP_NO, basis
    # The refusal is the deliverable's, not any later guard's: the node-done
    # basis was never minted for a crowned row.
    assert basis == "", basis

    answer, basis = _decide(worker, facts=quiet, nodes=nodes)
    assert answer == watchdog.REAP_YES and "node x-4c87 done" in basis


def test_a_crown_does_not_block_the_claim_basis():
    """The crown bars only the NODE-DONE basis. Another live session
    holding the node's claim is a fact about THIS row's deliverable and
    still registers as the basis; the row then answers on the rest of its
    reads, here a full yes since every one of them passes."""
    row = Row("aaaa1111-0000", "king-footnote-g4", "working", "x-4c87",
              "/shared/canonical", origin="spawn",
              last_message_at=STALE_MESSAGE_STAMP, probe="dead", crowned=True)
    quiet = _facts(FINISHED_TAIL, age_min=120)
    answer, basis = _decide(
        row, facts=quiet,
        claims={"x-4c87": {"state": "live", "holder": "target-session:zzzz9999-0000"}},
    )
    assert answer == watchdog.REAP_YES and "claim held by" in basis


def test_probe_liveness_positive_markers_only(monkeypatch):
    """The group 1 probe's vocabulary, pinned: positive markers answer
    alive/dead, silence answers unknown - never a guess in either
    direction. The pid leg is delegated to ``spawn_gate._pid_alive`` (the
    one reader that knows the incarnation-token units), so it is staged at
    that seam; this test pins the probe's own read of the answers."""
    class Entry:
        def __init__(self, pid=None, start=None, exited_at=None, beat=None,
                     ttl_ms=None):
            self.pid = pid
            self.pid_start_time = start
            self.exited_at = exited_at
            self.inside_leg = (
                None if beat is None
                else {**beat, **({"ttl_ms": ttl_ms} if ttl_ms is not None else {})}
            )

    import fno.agents.spawn_gate as spawn_gate

    # pid leg: a matching incarnation is a positive LIFE marker...
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, start: True)
    assert watchdog._probe_liveness(Entry(pid=4242, start=100000)) == "alive"
    # ...a gone-or-recycled process is a POSITIVE death proof...
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, start: False)
    assert watchdog._probe_liveness(Entry(pid=4242, start=999999)) == "dead"
    # ...and an unreadable probe is silence, never death: the heartbeat rung
    # still gets its say.
    monkeypatch.setattr(spawn_gate, "_pid_alive", lambda pid, start: None)
    beat = {"received_at": "2026-08-31T08:00:00Z"}
    assert watchdog._probe_liveness(Entry(
        pid=4242, start=1, exited_at="2026-08-31T00:42:40Z", beat=beat,
    )) == "alive"
    assert watchdog._probe_liveness(Entry(pid=4242, start=1)) == "unknown"
    monkeypatch.undo()

    # heartbeat leg: received STRICTLY LATER than exited_at proves the row
    # advanced past its own exit stamp...
    assert watchdog._probe_liveness(Entry(
        exited_at="2026-08-31T00:42:40Z", beat=beat)) == "alive"
    # ...a beat that predates the exit stamp proves nothing...
    assert watchdog._probe_liveness(Entry(
        exited_at="2026-08-31T09:00:00Z", beat=beat)) == "unknown"
    # ...and a TTL'd beat past its window is not a marker.
    assert watchdog._probe_liveness(Entry(
        exited_at="2026-08-31T00:42:40Z", beat=beat, ttl_ms=1000)) == "unknown"

    # Silence on every rung is unknown - a row with no pid, no heartbeat
    # and no exit stamp is UNPROVEN, never dead.
    assert watchdog._probe_liveness(Entry()) == "unknown"
    assert watchdog._probe_liveness(None) == "unknown"


def test_ledger_join_reads_the_singular_session_key_without_spreading_it(
    monkeypatch, tmp_path
):
    """Older ledger entries record ``session_id`` (a bare string). Iterating
    it spreads one node across every CHARACTER of the id; dropping it loses
    the row entirely. ledger_join.py guards both shapes and so must this."""
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"graph_node_id": "x-old", "session_id": "aaaa1111-0000"},
    ]}))
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)
    nodes = watchdog._ledger_nodes()
    assert nodes == {"aaaa1111-0000": "x-old"}


@pytest.mark.parametrize("raw", ["null", "NULL", "none", "None", "nil", "NIL", "", "'", '"'])
def test_ledger_join_drops_unresolved_node_sentinels(monkeypatch, tmp_path, raw):
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [
        {"graph_node_id": raw, "sessions": ["sentinel-session"]},
        {"graph_node_id": "x-real", "sessions": ["real-session"]},
    ]}))
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)
    nodes = watchdog._ledger_nodes()
    assert nodes == {"real-session": "x-real"}


def test_row_state_survives_an_alias_rename_and_odd_casing():
    """A raw ``r["state"]`` read reads "" under a rename, and "" is in no lane
    set - the whole fleet would classify leave behind a fresh sweep file."""
    assert watchdog._row_state({"status": "Working"}) == ("working", "")
    # `state` wins when both are present, matching _STATUS_KEYS' order.
    assert watchdog._row_state({"state": "blocked", "status": "idle"}) == ("blocked", "")
    # Both spellings of blocked fold onto one word. Missing this is what let
    # a `needs input` row fall through every lane to leave.
    assert watchdog._row_state({"state": "needs input"}) == ("blocked", "")
    assert watchdog._row_state({"state": "busy"}) == ("working", "")
    # An unknown spelling is loud, never a silent no-lane row.
    state, warning = watchdog._row_state({"state": "quiescing"})
    assert state == "quiescing" and "unmapped" in warning
    assert watchdog._row_state({})[1]


def test_a_failed_rm_after_a_successful_stop_reports_rather_than_refuses(
    monkeypatch,
):
    """"refused" means "declined to act". The stop already landed, so that
    receipt lies about a session which is now dead, and a reader who retries
    it sends a stop to a stopped session."""
    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: None)
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(watchdog, "_persist_reap_receipt", lambda rid: (True, "staged"))
    seen = []

    def runner(argv, **kwargs):
        seen.append(argv[-2])
        failed = argv[-2] == "rm"
        return subprocess.CompletedProcess(
            argv, 1 if failed else 0, "", "registry locked" if failed else ""
        )

    v = Verdict("aaaa1111-0000", "w1", "working", REAP, "node x-done done", "stop+rm")
    outcome, detail = watchdog._apply_reap(v, cwd="/wt/x", runner=runner)
    assert seen == ["stop", "rm"]
    assert outcome == "partial"
    assert "already stopped" in detail and "never re-run this as a stop" in detail


def test_store_only_row_reap_names_the_scope_mismatch_not_just_an_exit_code(
    monkeypatch,
):
    """24 of 43 enumerated rows are store-only, so reap's registry-scoped
    resolution returns exit 2 permanently. A bare exit code reads as
    transient and invites a retry loop."""
    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: None)
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)

    def stop_not_found(argv, **kwargs):
        return subprocess.CompletedProcess(argv, 2, "", "")

    v = Verdict("aaaa1111-0000", "w1", "working", REAP, "node x-done done", "stop+rm")
    outcome, detail = watchdog._apply_reap(v, cwd="/wt/x", runner=stop_not_found)
    assert outcome == "refused"
    assert "registry" in detail, detail


def test_the_roster_warns_before_it_outgrows_its_budget(monkeypatch):
    """A fixed budget against a growing fleet fails silently on the day the
    fleet crosses it, and the resulting refusal reads as a broken fleet
    rather than a budget to raise. The approach to the line must speak."""
    from fno.agents.harnesses import claude as claude_mod

    slow = watchdog.ROSTER_TIMEOUT_S * watchdog.ROSTER_HEADROOM + 1
    clock = iter([1000.0, 1000.0 + slow])
    monkeypatch.setattr(watchdog.time, "time", lambda: next(clock))
    monkeypatch.setattr(
        claude_mod, "claude_agents_rows",
        lambda **k: ([{"sessionId": "aaaa1111-0000", "state": "working"}], []),
    )
    _rows, warnings = watchdog.fleet_rows()
    assert any("of its" in w and "budget" in w for w in warnings), warnings
    assert any("ROSTER_TIMEOUT_S" in w for w in warnings)


def test_a_fast_roster_probe_stays_quiet(monkeypatch):
    """The warning is a headroom signal, not a per-sweep banner."""
    from fno.agents.harnesses import claude as claude_mod

    clock = iter([1000.0, 1000.5])
    monkeypatch.setattr(watchdog.time, "time", lambda: next(clock))
    monkeypatch.setattr(claude_mod, "claude_agents_rows", lambda **k: ([], []))
    _rows, warnings = watchdog.fleet_rows()
    assert warnings == []


def test_a_bounded_caller_spends_its_own_budget_not_the_lanes(monkeypatch):
    """The tick's wall-clock deadline is the shorter budget AND the fatal one.

    A roster probe that outlives it raises the tick's deadline signal, which
    exits 75 and kills every later leg. Measured: arming the lane on the tick
    with the standalone 30s budget timed the whole tick out. So a bounded
    caller passes what it has left and the enumeration must honour it.
    """
    from fno.agents.harnesses import claude as claude_mod

    spent = {}

    def fake_rows(timeout=3.0):
        spent["timeout"] = timeout
        return [], []

    monkeypatch.setattr(claude_mod, "claude_agents_rows", fake_rows)
    watchdog.fleet_rows(timeout=4.0)
    assert spent["timeout"] == 4.0
    # The lane's own budget is still the CEILING, never a floor a caller
    # can raise past.
    watchdog.fleet_rows(timeout=999.0)
    assert spent["timeout"] == watchdog.ROSTER_TIMEOUT_S
    # An exhausted tick still probes rather than passing a zero timeout.
    watchdog.fleet_rows(timeout=0.0)
    assert spent["timeout"] == 1.0


def test_the_second_spelling_of_blocked_reaches_every_lane():
    """claude spells one state two ways and the lane knew only one.

    `busy` was folded onto working by hand; its sibling `needs input` was
    missed, so a row wearing it could never ghost, reroute or wake - it fell
    to leave while looking healthy. The fold now comes from the harness map,
    so this pins the behaviour rather than the table.
    """
    from fno.agents.harnesses.claude import _LIVE_STATUS_INPUT

    # Every input spelling claude maps onto "Needs input" must reach the
    # reroute lane. Enumerating the map is the point: a new spelling added
    # there fails this test instead of silently falling to leave.
    blocked_spellings = [
        raw for raw, out in _LIVE_STATUS_INPUT.items() if out == "Needs input"
    ]
    assert "needs input" in blocked_spellings
    for raw in blocked_spellings:
        state, warning = watchdog._row_state({"state": raw})
        assert (state, warning) == ("blocked", ""), raw
        row = Row("cccc3333-0000", "r1", state, None, "/tmp/r1")
        [v] = _run(row and [row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL)})
        assert v.verdict == REROUTE, f"{raw} never reached reroute"


def test_reap_never_fires_on_an_unreadable_transcript():
    """An absence has two explanations and this lane cannot tell them apart.

    tail_facts returns None both for a session that never wrote a transcript
    and for one it could not READ (resolver miss, OSError, moved path).
    Reaping on that deletes a clean worktree every time the read fails.
    """
    rows = [Row("aaaa1111-0000", "w1", "idle", "x-done", "/wt/solo")]
    [v] = _run(rows, {}, nodes={"x-done": {"status": "done"}})
    # STALE, not LEAVE: leave says the row was read and is healthy. An
    # unanswered read is a different fact and it belongs to a human.
    assert v.verdict == STALE
    assert "not evidence" in v.basis


def test_occupancy_is_read_from_the_transcript_not_the_roster():
    """The roster is the store this module exists because it lies.

    The measured 2026-08-15 inversion had claude report a WORKING session as
    done. Keying occupancy on that field let a live row count as zero, so its
    quiet sibling reaped the tree the live one was mid-task in. Occupancy now
    asks the transcript, and anything it cannot read counts as occupied.
    """
    # A live sibling by TRANSCRIPT, while its roster state says stopped.
    # Since x-ad13 the tally guards the WORKTREE branch, so it rides the
    # REAP verdict's ``cotenants`` field instead of refusing the verdict:
    # the transcript, not the roster, still decides who occupies the tree.
    rows = [
        Row("aaaa1111-0000", "quiet", "working", "x-done", "/wt/one",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP, probe="dead"),
        Row("bbbb2222-0000", "live-but-reads-stopped", "stopped", "x-done", "/wt/one",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP),
    ]
    vs = _run(
        rows,
        {
            "aaaa1111-0000": _facts(FINISHED_TAIL, age_min=120),
            "bbbb2222-0000": _facts("still working", age_min=0),
        },
        nodes={"x-done": {"status": "done"}},
    )
    assert vs[0].verdict == REAP, vs[0].basis
    assert vs[0].cotenants == 1, vs[0].cotenants

    # An UNREADABLE sibling transcript also holds the tree: absence is not
    # evidence that the sibling left.
    vs = _run(
        rows,
        {"aaaa1111-0000": _facts(FINISHED_TAIL, age_min=120)},
        nodes={"x-done": {"status": "done"}},
    )
    assert vs[0].verdict == REAP, vs[0].basis
    # The unreadable sibling still counts as occupied (absence is not
    # evidence it left), so the tally that rides the verdict is 1.
    assert vs[0].cotenants == 1, vs[0].cotenants

    # Two genuinely quiet rows have nobody to protect, so the lane still works.
    vs = _run(
        rows,
        {
            "aaaa1111-0000": _facts(FINISHED_TAIL, age_min=120),
            "bbbb2222-0000": _facts(FINISHED_TAIL, age_min=120),
        },
        nodes={"x-done": {"status": "done"}},
    )
    assert vs[0].verdict == REAP, vs[0].basis
    assert vs[0].cotenants == 0, vs[0].cotenants


def test_the_roster_refusal_carries_its_cause(monkeypatch, tmp_path, capsys):
    """The refusal says the roster was unreadable; the warnings say WHY.
    Dropping them leaves the one actionable line on the floor."""
    import contextlib
    import io as _io

    import pytest

    from fno.agents import cli as agents_cli

    payload = {"refused": "0 rows", "warnings": ["claude agents --json timed out after 30.0s"],
               "verdicts": [], "counts": {}}
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (payload, []))
    err = _io.StringIO()
    with contextlib.redirect_stderr(err), pytest.raises(typer.Exit):
        agents_cli.cmd_watchdog(json_out=False, apply=False, apply_all=False,
                                only=LEAVE, mail_to=None)
    assert "timed out after 30.0s" in err.getvalue()


def test_a_watchdog_event_uses_a_source_the_schema_accepts(tmp_path, monkeypatch):
    """A source outside the envelope enum raised inside _build, and the
    swallow turned that into the whole manual lane writing NOTHING while
    every receipt still read normally. The event has to actually land."""
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    watchdog.emit_event(
        "watchdog_verdict",
        {"row_id": "aaaa1111-0000", "name": "w1", "verdict": WAKE, "basis": "b"},
    )
    written = (tmp_path / "events.jsonl")
    assert written.exists(), "the event was swallowed, not written"
    record = json.loads(written.read_text().splitlines()[-1])
    assert record["type"] == "watchdog_verdict"
    assert record["source"] == "daemon"


# ---------------------------------------------------------------------------
# The reap predicate: one gate, and every failed read is UNKNOWN
# ---------------------------------------------------------------------------

def _decide(row, *, facts=None, nodes=None, claims=None,
            node_state_for=None, claim_for=None, now_s=NOW_1840):
    return watchdog.reap_decision(
        row,
        facts=facts,
        node_state_for=node_state_for or (lambda n: (nodes or {}).get(n)),
        claim_for=claim_for or (lambda n: (claims or {}).get(n, {})),
        now_s=now_s,
        quiet_after_s=watchdog.REAP_QUIET_AFTER_S,
    )


def test_every_failed_read_is_unknown_and_unknown_never_reaps():
    """The structural rule, stated once as a test.

    Eight findings over three rounds were one defect: a reading about one
    thing used as a verdict about another, usually an ABSENCE read as a
    positive answer. Fixing them site by site converged on nothing. This
    pins the shape instead: whatever fails, the answer is UNKNOWN, and the
    only way to YES is through the positive markers, ending in a POSITIVE
    death proof from the probe (x-ad13).
    """
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo",
              origin="spawn", last_message_at=STALE_MESSAGE_STAMP)
    quiet = _facts(FINISHED_TAIL, age_min=120)

    def boom(_n):
        raise OSError("store unreadable")

    # A raising node store is not "not done".
    answer, basis = _decide(row, facts=quiet, node_state_for=boom)
    assert answer == watchdog.REAP_UNKNOWN and "unreadable" in basis

    # A raising claim store is not "unclaimed".
    answer, basis = _decide(row, facts=quiet, nodes={"x-done": {"status": "x"}},
                            claim_for=boom)
    assert answer == watchdog.REAP_UNKNOWN and "unreadable" in basis

    # A missing transcript is not "finished".
    answer, _ = _decide(row, facts=None, nodes={"x-done": {"status": "done"}})
    assert answer == watchdog.REAP_UNKNOWN

    # An unparseable last event is not "quiet".
    blind = TailFacts([(None, "x")], None, "x", "assistant", "x", "text")
    answer, _ = _decide(row, facts=blind, nodes={"x-done": {"status": "done"}})
    assert answer == watchdog.REAP_UNKNOWN

    # A silent probe is not death (x-ad13).
    answer, basis = _decide(row, facts=quiet, nodes={"x-done": {"status": "done"}})
    assert answer == watchdog.REAP_UNKNOWN
    assert "probe is silent" in basis

    # All three markers present, plus a POSITIVE death proof, and only then.
    answer, basis = _decide(row._replace(probe="dead"), facts=quiet,
                            nodes={"x-done": {"status": "done"}})
    assert answer == watchdog.REAP_YES
    assert "quiet" in basis


def test_the_predicate_is_the_only_route_to_a_reap_verdict():
    """A guard on one of N paths is decorative. This asserts there is one
    path: no reap verdict exists that the predicate did not authorize."""
    import inspect

    source = inspect.getsource(watchdog._verdict_one)
    # The single REAP construction in the classifier is the one guarded by
    # the predicate's YES. Any second one is a bypass.
    assert source.count("REAP,") == 1, (
        "a second REAP verdict site bypasses reap_decision"
    )
    assert "reap_decision(" in source


def test_reap_ships_frozen_and_the_freeze_is_at_the_funnel(monkeypatch):
    """Wake and reroute are recoverable; a reap deletes a worktree and a
    wrong one is not undoable. So reap classifies but does not execute until
    an operator arms it, and the freeze lives at the one funnel rather than
    in whichever CLI flag happens to expose the lane today."""
    v = Verdict("aaaa1111-0000", "w1", "working", REAP, "node x-done done", "stop+rm")

    def never(*a, **k):
        raise AssertionError("a frozen reap must not run a lifecycle command")

    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/wt/solo", runner=never, reap_enabled=False
    )
    assert outcome == "frozen"
    assert "watchdog_reap" in detail

    # Armed, it reaches the mechanism (and stops there on the clean-tree gate).
    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: "1 unpushed commit(s)")
    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/wt/solo", runner=never, reap_enabled=True
    )
    assert outcome == "refused" and "unpushed" in detail


def test_unreadable_config_is_never_permission_to_delete(monkeypatch):
    """The one switch whose wrong answer deletes a worktree fails closed."""
    import fno.config as config_mod

    def boom():
        raise OSError("settings.yaml is a directory")

    monkeypatch.setattr(config_mod, "load_settings", boom)
    assert watchdog._reap_execution_enabled() is False


def test_reap_refuses_a_cwd_it_cannot_prove_is_a_worktree(monkeypatch):
    """`claude rm` removes "session record + worktree", and the ledger join
    means cwd is routinely a repo ROOT: a worktree.policy = "never" project,
    or a bg session in the canonical checkout. Whether that arm scopes its
    delete is an external binary's undocumented behaviour, and being wrong
    costs the main checkout."""
    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: None)
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: False)

    def never(*a, **k):
        raise AssertionError("rm must not run against a canonical checkout")

    v = Verdict("aaaa1111-0000", "w1", "working", REAP, "node x-done done", "stop+rm")
    outcome, detail = watchdog._apply_reap(v, cwd="/repos/main", runner=never)
    assert outcome == "refused"
    assert "not a linked worktree" in detail


def test_a_session_still_in_play_is_never_reaped():
    """Silence is a reading about the last write, never a verdict that the
    work is over. A worker parked on <watching>, one holding a question, and
    one waiting out a rate limit are all silent and all still in play."""
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo",
              origin="spawn", last_message_at=STALE_MESSAGE_STAMP, probe="dead")
    nodes = {"x-done": {"status": "done"}}

    parked = _facts("<watching>ci</watching>", age_min=200)
    [v] = _run([row], {"aaaa1111-0000": parked}, nodes=nodes)
    assert v.verdict != REAP and "watching" in v.basis

    asking = _facts("Which branch should I target?", age_min=200)
    [v] = _run([row], {"aaaa1111-0000": asking}, nodes=nodes)
    assert v.verdict != REAP and "your-move" in v.basis

    # Silence inside a live 429 window is the rate limit, not a finished job.
    [v] = _run([row], {"aaaa1111-0000": _facts(RATE_LIMIT_TAIL, age_min=200)},
               nodes=nodes)
    assert v.verdict != REAP

    # A session that died mid-turn still reaps: the deliverable ruling says
    # a done node reaps at any age, and a stalled tail owes a move nobody is
    # coming to make.
    [v] = _run([row], {"aaaa1111-0000": _facts("half a sentence", age_min=200)},
               nodes=nodes)
    assert v.verdict == REAP and "stalled" in v.basis


def test_a_hand_run_sweep_does_not_report_the_cadence_stale(tmp_path, monkeypatch):
    """One file serves both cadences, so a manual write used to erase the
    only evidence the tick had run. Status then blamed the daemon for the
    operator having looked."""
    monkeypatch.setattr(watchdog, "sweep_path", lambda: tmp_path / "sweep.json")

    watchdog.write_sweep_file("tick", {LEAVE: 3}, NOW_1840)
    watchdog.write_sweep_file("manual", {LEAVE: 3}, NOW_1840 + 60)

    state = watchdog.sweep_staleness(NOW_1840 + 120, stale_after_s=3600)
    assert state["stale"] is False, state
    # And a cadence that really died still reads stale, hand-run or not.
    dead = watchdog.sweep_staleness(NOW_1840 + 7200, stale_after_s=3600)
    assert dead["stale"] is True, dead


def test_a_filtered_hand_run_keeps_the_rows_it_filtered_out():
    """Stamping only the subset silently retracts every filtered-out row, so
    the next tick re-emits all of them."""
    merged = watchdog.union_signature("a:wake;b:ghost", "b:ghost;c:reap")
    assert merged == "a:wake;b:ghost;c:reap"
    assert watchdog.union_signature("", "a:wake") == "a:wake"


def test_reroute_refuses_a_shared_manifest_it_would_respawn_from(monkeypatch):
    """`_redispatch` re-derives the node from the cwd's target-state manifest,
    which is the read fleet_rows refuses to trust for identity: on a canonical
    checkout every session reads the same node. Acting on it force-releases a
    claim another live session may hold, then spawns a duplicate onto it."""
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: False)

    def never(*a, **k):
        raise AssertionError("failover must not run against a shared manifest")

    v = Verdict("cccc3333-0000", "r1", "blocked", REROUTE, "429 resets", "redispatch")
    outcome, detail = apply_verdict(
        v, lanes="all", cwd="/repos/main", failover_fn=never
    )
    assert outcome == "refused"
    assert "not a linked worktree" in detail


def test_a_row_claiming_working_still_wakes_when_its_tail_stalled():
    """The measured case, 2026-08-18: a row read live and `working` while its
    last transcript message was an API error 56 minutes old. The lane never
    touched it; woken by hand it opened a PR fifteen minutes later.

    A state word is what a session CLAIMS, and this module exists because
    that claim lies. Candidacy is the state; the wake itself still needs the
    tail to say the session owes a move.
    """
    row = Row("aaaa1111-0000", "t-live-but-dead", "working", None, "/wt/w1")
    dead = _facts("API Error 500: internal server error", age_min=180)
    [v] = _run([row], {"aaaa1111-0000": dead})
    assert v.verdict == WAKE, v.basis
    assert "180m silent" in v.basis

    # But candidacy is not the whole story, and this is the half the ruling's
    # own evidence lands on. `classify_tail` only calls a tail stalled past
    # STALLED_AFTER_S, which is two hours, so the measured row at 56 minutes
    # is still `working` to the classifier and this lane leaves it. Adding
    # `working` here makes that row reachable; it does not make it reachable
    # at 56 minutes. Anyone expecting the sweep to catch it sooner has to
    # move that threshold, not this set.
    [early] = _run(
        [row],
        {"aaaa1111-0000": _facts("API Error 500: internal server error", age_min=56)},
    )
    assert early.verdict == LEAVE
    assert "does not owe a move" in early.basis

    # And the claim cuts both ways: a working row whose tail is still moving
    # is left alone, so the state word never wakes anything on its own.
    [healthy] = _run([row], {"aaaa1111-0000": _facts("mid refactor", age_min=1)})
    assert healthy.verdict == LEAVE

    # A working row with NO transcript stays a ghost: that lane outranks wake
    # and a row with nothing to read must never reach an action.
    [ghost] = _run([row], {})
    assert ghost.verdict == GHOST


def test_only_the_lane_skip_is_silent(monkeypatch, tmp_path):
    """Callers used to list which outcomes were worth printing, and three
    receipts were swallowed by that list in turn. The default is surface now,
    so an outcome cannot go silent by not being listed."""
    import contextlib
    import io as _io

    from fno.agents import cli as agents_cli

    verdicts = [
        Verdict("aaaa1111-0000", "held-row", "blocked", REROUTE, "429", "redispatch"),
        Verdict("bbbb2222-0000", "frozen-row", "working", REAP, "node done", "stop+rm"),
    ]
    rows = [Row(v.row_id, v.name, v.state, None, "/tmp/w") for v in verdicts]
    payload = {"generated_at": "x", "verdicts": [v._asdict() for v in verdicts],
               "counts": {REROUTE: 1, REAP: 1}, "warnings": []}
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (payload, rows))
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: None)
    monkeypatch.setattr(watchdog, "_last_events_signature", lambda: "")
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    monkeypatch.setattr(
        watchdog, "apply_verdict",
        lambda v, **kw: ("held", "reroute held: already rotated this sweep")
        if v.verdict == REROUTE else ("frozen", "reap classified but not executed"),
    )

    err = _io.StringIO()
    with contextlib.redirect_stderr(err):
        agents_cli.cmd_watchdog(json_out=False, apply=False, apply_all=True,
                                only=None, mail_to=None)
    text = err.getvalue()
    assert "held" in text and "already rotated" in text, text
    assert "frozen" in text, text


# ---------------------------------------------------------------------------
# the unclaimed advisory (x-cd1e)
# ---------------------------------------------------------------------------


def test_a_live_row_on_an_unclaimed_node_is_flagged():
    """Nothing today notices a live worker on a node no claim covers. Seven of
    nine nodes named by a live worker read free on 2026-08-19."""
    rows = [Row("cccc3333-0000", "t-x76d1-worker", "working", "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0000": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "free"}},
    )
    assert v.verdict == UNCLAIMED
    assert "node x-76d1 carries NO claim while this row is live" in v.basis


def test_the_advisory_never_becomes_an_action():
    """The worker is fine; the record is wrong. A wake, a reroute or a reap on
    this row would all be wrong, so the action lanes must refuse it."""
    rows = [Row("cccc3333-0001", "t-worker", "working", "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0001": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "free"}},
    )
    assert v.action == "none"
    outcome, _detail = apply_verdict(v, lanes="all")
    assert outcome != "applied"


def test_a_claimed_node_reads_leave_exactly_as_before():
    rows = [Row("cccc3333-0002", "t-worker", "working", "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0002": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "live", "holder": "target-session:cccc3333-0002"}},
    )
    assert v.verdict == LEAVE


def test_an_unresolved_node_is_never_flagged():
    """The stated blind spot, pinned. Row.node comes from the worktree manifest
    then the ledger, both written downstream of `fno do target init`, so a worker
    that never ran init carries node=None and is invisible here. Claiming
    otherwise would make this the decorative guard it exists to remove."""
    rows = [Row("cccc3333-0003", "t-worker", "working", None, "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0003": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "free"}},
    )
    assert v.verdict == LEAVE


def test_an_unreadable_claim_is_not_a_finding():
    """An advisory that fires on an unreadable store trains its reader to
    ignore it."""
    def _boom(_node):
        raise RuntimeError("claims root gone")

    rows = [Row("cccc3333-0004", "t-worker", "working", "x-76d1", "/tmp/w")]
    [v] = verdicts(
        rows,
        transcript_for=lambda sid: _facts("still going", age_min=2),
        claim_for=_boom,
        node_state_for=lambda node: None,
        now_s=NOW_1840,
    )
    # STALE, not LEAVE: the existing reap predicate already refuses to render
    # an unanswered read as a healthy row. What matters here is that it is not
    # reported as an unclaimed node.
    assert v.verdict == STALE
    assert v.verdict != UNCLAIMED


@pytest.mark.parametrize("state", ["done", "completed", "exited", "killed", "stopped"])
def test_every_terminal_row_state_is_skipped_not_just_done(state):
    """A finished row released its claim correctly, so flagging it reports the
    system working. `claude agents --json --all` keeps terminal rows forever,
    so a set narrower than the module's own terminal states put `completed`,
    `exited` and `killed` rows in the digest permanently."""
    rows = [Row("cccc3333-0007", "t-worker", state, "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0007": _facts("done here", age_min=2)},
        claims={"x-76d1": {"state": "free"}},
    )
    assert v.verdict != UNCLAIMED


def test_a_finished_node_is_not_flagged_as_unclaimed():
    """A done node has no claim because the work is over and the claim was
    released. The row can still read `working` while the graph says done, so it
    reaches the LEAVE and was upgraded, putting correctly-completed work in the
    events stream and the mail digest every tick."""
    rows = [Row("cccc3333-0009", "t-worker", "working", "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0009": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "free"}},
        nodes={"x-76d1": {"status": "done"}},
    )
    assert v.verdict != UNCLAIMED


def test_an_unreadable_node_state_flags_nothing():
    """A report built on a failed read trains its reader to ignore the report."""
    def _boom(_node):
        raise RuntimeError("graph unreadable")

    rows = [Row("cccc3333-0010", "t-worker", "working", "x-76d1", "/tmp/w")]
    [v] = verdicts(
        rows,
        transcript_for=lambda sid: _facts("still going", age_min=2),
        claim_for=lambda node: {"state": "free"},
        node_state_for=_boom,
        now_s=NOW_1840,
    )
    assert v.verdict != UNCLAIMED


def test_a_lapsed_claim_is_not_flagged():
    """`stale` is the NORMAL reading for a healthy worker parked in a CI wait:
    the heartbeat runs on tool calls, so a session waiting on purpose stops
    renewing. Flagging it would put a working fleet in the digest every tick."""
    rows = [Row("cccc3333-0008", "t-worker", "working", "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0008": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "stale", "holder": "target-session:old"}},
    )
    assert v.verdict != UNCLAIMED


def test_a_suspect_claim_is_not_flagged_as_unclaimed():
    """Suspect is a claim, and a protected slot. Only a plain free reading is
    the gap this reports."""
    rows = [Row("cccc3333-0005", "t-worker", "working", "x-76d1", "/tmp/w")]
    [v] = _run(
        rows,
        {"cccc3333-0005": _facts("still going", age_min=2)},
        claims={"x-76d1": {"state": "suspect", "holder": "target-session:dead"}},
    )
    assert v.verdict == LEAVE


def test_the_advisory_reaches_the_digest():
    """LEAVE rows are filtered out of every report, so a verdict that reported
    nothing would be a guard nobody ever reads."""
    from fno.agents.watchdog import digest_text

    v = Verdict("cccc3333-0006", "t-worker", "working", UNCLAIMED,
                "node x-76d1 carries NO claim while this row is live", "none")
    payload = {
        "verdicts": [v._asdict()],
        "counts": {UNCLAIMED: 1},
        "terminal_harness_rows": 0,
    }
    assert "t-worker" in digest_text(payload)


# ---------------------------------------------------------------------------
# x-944f: the two reap protectors
# ---------------------------------------------------------------------------

# 30 minutes before NOW_1840, so it is INSIDE REAP_RECENT_MESSAGE_S (2h).
RECENT_MESSAGE_STAMP = "2026-08-16T18:10:00Z"


def test_an_operator_row_is_refused_and_the_basis_names_origin():
    """Rule 3: a session a human started by hand is never reaped.

    Every other precondition is satisfied on purpose - node done, sole
    occupant, transcript present, tail finished, quiet for hours, stamp
    stale. The ONLY thing standing between this row and a deleted worktree
    is `origin`, which is what makes the assertion about `origin` and not
    about the fixture.
    """
    row = Row("aaaa1111-0000", "hand-started", "working", "x-done", "/wt/solo",
              origin="operator", last_message_at=STALE_MESSAGE_STAMP)

    answer, basis = _decide(
        row,
        facts=_facts(FINISHED_TAIL, age_min=200),
        nodes={"x-done": {"status": "done"}},
    )

    assert answer == watchdog.REAP_NO
    assert "origin=operator" in basis
    # By IDENTITY against the module constant, never a copy of its wording: a
    # duplicated literal here would stay green while the emitted rule drifted.
    assert watchdog.REAP_PROTECTION_RULES["origin"] in basis


def test_the_operator_refusal_outranks_every_later_read():
    """The origin protector answers FIRST, so it holds even when the reads
    below it cannot run at all. An operator row with no transcript is still
    refused for being an operator's, not for the missing transcript."""
    row = Row("aaaa1111-0000", "hand-started", "working", "x-done", "/wt/solo",
              origin="operator")

    answer, basis = _decide(row, facts=None, nodes={"x-done": {"status": "done"}})

    assert answer == watchdog.REAP_NO
    assert "origin=operator" in basis


def test_the_three_origin_values_reach_three_different_verdicts():
    """`None`, "spawn" and "operator" are THREE facts, not two.

    Collapsing never-recorded into not-an-operator is the defect this node was
    filed against, and the first cut of this predicate committed it: only
    "operator" refused, so `None` fell through to the delete exactly like a
    known worker. Each value now reaches a verdict the others cannot.

    "adopted" is the fourth, and it lands with `None` on purpose: the healers
    that mint those rows observed a session already running and never observed
    who started it. Naming the non-answer must not turn it into an answer.
    """
    expected = {
        "operator": watchdog.REAP_NO,
        None: watchdog.REAP_UNKNOWN,
        "adopted": watchdog.REAP_UNKNOWN,
        "spawn": watchdog.REAP_YES,
    }
    for origin, want in expected.items():
        row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo",
                  origin=origin, last_message_at=STALE_MESSAGE_STAMP,
                  probe="dead")
        answer, basis = _decide(
            row,
            facts=_facts(FINISHED_TAIL, age_min=200),
            nodes={"x-done": {"status": "done"}},
        )
        assert answer == want, f"origin={origin!r} gave {answer}: {basis}"


def test_an_adopted_row_survives_its_recency_stamp_going_stale():
    """The reachable case the first cut of this predicate would have deleted.

    `mint_adopted_entry` writes its marker beside a FRESH `last_message_at`,
    and adopt takes in both a session a human started by hand and a footnote
    orphan. Inside the window the stamp protects the row. This asserts what
    happens AFTER it goes stale, which is where the two protectors used to fall
    silent together and hand over a worktree nobody could prove was a worker's.

    Both spellings, because the fleet carries both: a row minted before the
    healers stamped anything reads None, and one minted since reads "adopted".
    They are the same non-answer and must reach the same refusal.
    """
    for origin, said in ((None, "was never recorded"), ("adopted", "reads adopted")):
        adopted = Row("aaaa1111-0000", "adopted-external", "working", "x-done",
                      "/wt/solo", origin=origin,
                      last_message_at=STALE_MESSAGE_STAMP)

        answer, basis = _decide(
            adopted,
            facts=_facts(FINISHED_TAIL, age_min=200),
            nodes={"x-done": {"status": "done"}},
        )

        assert answer == watchdog.REAP_UNKNOWN, origin
        assert said in basis, basis
        assert watchdog.REAP_PROTECTION_RULES["origin"] in basis


def test_the_unrecorded_origin_read_never_silences_a_more_specific_refusal():
    """The same late placement the recency read needed, for the same reason.

    Put up beside the early operator read, this answers first on every refusal
    and the specific guards below it stop speaking. Each row here carries an
    unrecorded origin, so an early read would have swallowed all three.
    """
    quiet = _facts(FINISHED_TAIL, age_min=200)
    nodes = {"x-done": {"status": "done"}}
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/shared",
              origin=None, last_message_at=STALE_MESSAGE_STAMP)

    # A silent probe still does not outrank the origin read (x-ad13): the
    # probe is the LAST gate, so the ownership silence speaks first.
    _, basis = _decide(row, facts=quiet, nodes=nodes)
    assert "origin" in basis and "probe" not in basis

    _, basis = _decide(row, facts=_facts("<watching>ci</watching>", age_min=200),
                       nodes=nodes)
    assert "watching" in basis

    _, basis = _decide(row, facts=None, nodes=nodes)
    assert "no transcript to read" in basis


def test_a_recent_message_protects_and_the_basis_names_recency_and_the_age():
    """Rule 2: liveness is last-message recency, not a pid.

    The transcript reads FINISHED and hours quiet, which is exactly what a
    dead process looks like to this lane. A message landed 30m ago anyway.
    The operator drives sessions by hand in ways no probe observes, so the
    stamp refuses the reap whatever any pid says.
    """
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo",
              origin="spawn", last_message_at=RECENT_MESSAGE_STAMP)

    answer, basis = _decide(
        row,
        facts=_facts(FINISHED_TAIL, age_min=200),
        nodes={"x-done": {"status": "done"}},
    )

    assert answer == watchdog.REAP_NO
    assert watchdog.REAP_PROTECTION_RULES["recency"] in basis
    # The MEASURED age, so a human auditing the refusal can check the window
    # rather than take the verdict's word for it.
    assert "30m ago" in basis


def test_an_unreadable_stamp_is_unknown_and_never_reaps():
    """Absent has two causes - nothing stamped it, the session never spoke -
    and this lane cannot tell them apart, so it answers UNKNOWN like every
    other unreadable read here. Asserted as the UNKNOWN verdict, which is a
    positive marker, never as the absence of a reap."""
    for stamp in (None, "", "not-a-timestamp"):
        row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo",
                  origin="spawn", last_message_at=stamp)
        answer, basis = _decide(
            row,
            facts=_facts(FINISHED_TAIL, age_min=200),
            nodes={"x-done": {"status": "done"}},
        )
        assert answer == watchdog.REAP_UNKNOWN, f"stamp={stamp!r}: {basis}"
        assert watchdog.REAP_PROTECTION_RULES["recency"] in basis


def test_the_recency_read_never_silences_a_more_specific_refusal():
    """The protector runs LAST, and this is what that position buys.

    Placed earlier it answered first on every refusal, so a row refused for a
    shared worktree reported "recency unproven" instead - the specific guard
    still ran and could never speak. Each row here has an unreadable stamp,
    so an early read would have swallowed all three.
    """
    quiet = _facts(FINISHED_TAIL, age_min=200)
    nodes = {"x-done": {"status": "done"}}
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/shared",
              origin="spawn")

    # A silent probe still does not outrank the recency read (x-ad13): the
    # probe is the LAST gate, so the unreadable stamp speaks first.
    _, basis = _decide(row, facts=quiet, nodes=nodes)
    assert "recency is unproven" in basis and "probe" not in basis

    # A tail still in play still names the tail.
    _, basis = _decide(row, facts=_facts("<watching>ci</watching>", age_min=200),
                       nodes=nodes)
    assert "watching" in basis

    # An unreadable transcript still names the transcript.
    _, basis = _decide(row, facts=None, nodes=nodes)
    assert "no transcript to read" in basis


def test_fleet_rows_carries_both_protectors_off_the_registry_row():
    """The two fields were always one line away in `fleet_rows`, and
    discarding them is why every reap decision was made without them. A
    protector the predicate cannot see is not a protector."""
    assert "origin" in watchdog.Row._fields
    assert "last_message_at" in watchdog.Row._fields
    # Defaulted, so an older construction site keeps working and the default
    # is the honest never-recorded rather than a fabricated "spawn".
    blank = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo")
    assert blank.origin is None
    assert blank.last_message_at is None


def test_origin_is_on_the_shared_list_row_contract():
    """A field the reap lane REFUSES on has to be readable by the human
    auditing that refusal. It was rendered by nobody, so the refusal was
    unauditable. The contract file is what pins both serializers to it."""
    import json as _json
    import pathlib as _pathlib

    contract = _json.loads(
        (_pathlib.Path(watchdog.__file__).resolve().parents[4]
         / "schemas" / "agents-list-row.json").read_text()
    )
    assert "origin" in contract["required"]


# ---------------------------------------------------------------------------
# The unfinished-work report: the operator surface, distinct from the verdict
# classifier above. The classifier stays the internal recovery engine; the
# report answers the outcome question (was work started and never finished?)
# over four dimensions, each row naming the one verb that clears it.
# ---------------------------------------------------------------------------

from pathlib import Path  # noqa: E402
from types import SimpleNamespace  # noqa: E402

from fno.agents import unfinished_work as uw  # noqa: E402


def _probe(
    handle: str = "sess-0001",
    *,
    pid_alive=None,
    transcript_age_s=None,
    claim_state=None,
    stored_exited=False,
):
    return uw.OwnerProbe(
        handle=handle,
        pid_alive=pid_alive,
        transcript_age_s=transcript_age_s,
        claim_state=claim_state,
        stored_exited=stored_exited,
    )


def _node_obs(
    node_id: str,
    *,
    status: str = "in_progress",
    claim_state: str = "free",
    touched_epoch=None,
    worktree=None,
    ahead=None,
    probes=(),
    cwd=None,
):
    return uw.NodeObs(
        node_id=node_id,
        status=status,
        touched_at_epoch=touched_epoch,
        cwd=cwd,
        worktree_path=worktree,
        ahead_count=ahead,
        claim={"state": claim_state} if claim_state is not None else None,
        owner_probes=tuple(probes),
    )


def _wt_obs(
    path: str,
    *,
    dirty=0,
    ahead=None,
    node_id=None,
    probes=(),
    repo="/repo",
):
    return uw.WorktreeObs(
        path=path,
        repo_root=repo,
        branch=None,
        dirty_count=dirty,
        ahead_count=ahead,
        node_id=node_id,
        owner_probes=tuple(probes),
    )


def _pr_obs(
    number: int,
    *,
    state="OPEN",
    opened_epoch=None,
    node="x-prnode",
    probes=(),
    url=None,
):
    return uw.PrObs(
        pr_number=number,
        pr_url=url or f"https://github.com/o/r/pull/{number}",
        node_id=node,
        state=state,
        opened_at_epoch=opened_epoch,
        owner_probes=tuple(probes),
    )


def _uw_obs(
    nodes=(),
    worktrees=(),
    prs=(),
    *,
    graph_ok=True,
    claims_ok=True,
    registry_ok=True,
    github_ok=True,
    unscanned_roots=(),
    warnings=(),
    now_s=NOW_1840,
):
    return uw.Observations(
        now_epoch=now_s,
        graph_ok=graph_ok,
        claims_ok=claims_ok,
        registry_ok=registry_ok,
        github_ok=github_ok,
        nodes=tuple(nodes),
        worktrees=tuple(worktrees),
        prs=tuple(prs),
        unscanned_roots=tuple(unscanned_roots),
        warnings=tuple(warnings),
    )


# --- AC1: the report answers the outcome question ------------------------


def test_ac1_clean_scan_renders_four_measured_zero_dimensions():
    snap = uw.classify(_uw_obs())
    payload = uw.snapshot_payload(snap)
    text = uw.snapshot_digest(snap)

    assert snap.complete is True
    assert payload["findings"] == []
    assert payload["counts"] == {dim: 0 for dim in uw.DIMENSIONS}
    assert text.splitlines()[0] == "unfinished work: 0 finding(s)"
    for dim in uw.DIMENSIONS:
        assert f"{dim}=0 measured" in text


def test_ac1_session_bookkeeping_never_enters_the_operator_surfaces():
    """100 registry-absent rollouts and six past-ceiling stale rows produced
    zero actionable outcomes in the measured sweep; the report must say that
    with positive measured markers, never by repeating the session rows."""
    snap = uw.classify(_uw_obs())
    blob = json.dumps(uw.snapshot_payload(snap)) + uw.snapshot_digest(snap) \
        + uw.snapshot_signature(snap)
    assert "recoverable" not in blob
    assert "stale" not in blob
    assert "orphaned" not in blob


# --- AC2: liveness is read from pid/transcript, never a stored word -------


def test_ac2_live_pid_defeats_any_stored_terminal_word():
    probe = _probe(pid_alive=True, transcript_age_s=99999.0)
    assert uw.owner_verdict(probe) == "live"

    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/fix-scoreboard-clock", dirty=82, probes=[probe])])
    )
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_DIRTY].state == uw.MEASURED


def test_ac2_fresh_transcript_defeats_stored_state_too():
    assert uw.owner_verdict(_probe(transcript_age_s=30.0)) == "live"
    assert uw.owner_verdict(_probe(transcript_age_s=30.0, claim_state="stale")) == "live"


def test_ac2_unreadable_liveness_is_unknown_not_ownerless():
    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/x", dirty=5, probes=[_probe()])])
    )
    assert snap.findings == ()
    dim = snap.dimensions[uw.KIND_DIRTY]
    assert dim.state == uw.UNKNOWN_DIM
    assert snap.complete is False


def test_ac2_positive_control_genuinely_orphaned_still_ownerless():
    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/x", dirty=5, probes=[_probe(pid_alive=False)])])
    )
    assert [f.subject for f in snap.findings] == ["/w/x"]


def test_ac2_no_candidates_is_gone_only_when_the_store_read():
    """A worktree nobody registered on reads ownerless only when the registry
    read succeeded; a failed read must not manufacture an ownerless verdict."""
    snap = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=5)]))
    assert [f.kind for f in snap.findings] == [uw.KIND_DIRTY]

    snap2 = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=5)], registry_ok=False))
    assert snap2.findings == ()
    assert snap2.dimensions[uw.KIND_DIRTY].state == uw.UNKNOWN_DIM


# --- AC3: started nodes with free claims ---------------------------------


def test_ac3_started_free_claim_nodes_carry_target_verbs_and_idle_order():
    older = _node_obs(
        "x-7d02", touched_epoch=NOW_1840 - 116 * 3600, worktree="/w/x-7d02", ahead=8
    )
    newer = _node_obs(
        "x-3b05", touched_epoch=NOW_1840 - 10 * 3600, worktree="/w/x-3b05", ahead=6
    )
    snap = uw.classify(_uw_obs(nodes=[newer, older]))

    assert [f.subject for f in snap.findings] == ["x-7d02", "x-3b05"]
    assert all(f.kind == uw.KIND_STARTED for f in snap.findings)
    assert all(f.clear_command == f"/fno:target {f.subject}" for f in snap.findings)
    assert snap.findings[0].age_s == pytest.approx(116 * 3600)
    assert snap.findings[0].ahead_count == 8
    assert "origin/main" in snap.findings[0].basis
    assert snap.findings[1].basis.count("origin/main") == 1


def test_ac3_non_free_claim_or_live_owner_excludes_the_node():
    held = _node_obs("x-held", claim_state="live")
    suspect = _node_obs("x-susp", claim_state="suspect")
    stale_lease = _node_obs("x-lease", claim_state="stale")
    corrupted = _node_obs("x-corr", claim_state="corrupted")
    owned = _node_obs("x-owned", probes=[_probe(pid_alive=True)])

    snap = uw.classify(
        _uw_obs(nodes=[held, suspect, stale_lease, corrupted, owned])
    )
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_STARTED].state == uw.MEASURED


# --- AC4: done nodes whose branch never reached main ----------------------


def test_ac4_done_ahead_of_main_names_count_and_squash_reading():
    snap = uw.classify(
        _uw_obs(
            nodes=[
                _node_obs("x-aaae", status="done", worktree="/w/x-aaae"),
            ],
            worktrees=[_wt_obs("/w/x-aaae", dirty=0, ahead=66, node_id="x-aaae")],
        )
    )
    [finding] = snap.findings
    assert finding.kind == uw.KIND_DONE_AHEAD
    assert finding.subject == "x-aaae"
    assert finding.ahead_count == 66
    assert "66" in finding.basis
    assert "squash" in finding.basis
    assert "stranded" in finding.clear_command
    assert "/w/x-aaae" in finding.basis


def test_ac4_unreadable_ahead_count_marks_dimension_unknown():
    snap = uw.classify(
        _uw_obs(
            nodes=[_node_obs("x-aaae", status="done", worktree="/w/x-aaae")],
            worktrees=[_wt_obs("/w/x-aaae", dirty=0, ahead=None, node_id="x-aaae")],
        )
    )
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_DONE_AHEAD].state == uw.UNKNOWN_DIM


def test_ac4_metric_reads_fresh_origin_main_not_the_tracking_ref(tmp_path):
    """The measured defect: a stale remote-tracking ref inflated the ahead
    count to 936 against a true 8. The fixture builds a real bare origin,
    rebases a feature branch onto an origin advance, then rewinds the local
    remote-tracking ref to the pre-advance commit: the tracking-ref count
    reads 11, the true origin/main..HEAD count is 8, and only a fresh fetch
    separates them."""
    import subprocess as _sp

    def _git(cwd, *argv, check=True):
        return _sp.run(
            ["git", "-C", str(cwd), *argv],
            capture_output=True, text=True, check=check,
        )

    origin = tmp_path / "origin.git"
    _sp.run(["git", "init", "-q", "--bare", str(origin)], check=True)
    clone = tmp_path / "clone"
    _sp.run(["git", "clone", "-q", str(origin), str(clone)], check=True)
    _git(clone, "config", "user.email", "t@t.t")
    _git(clone, "config", "user.name", "t")
    _git(clone, "checkout", "-q", "-b", "main")
    (clone / "a.txt").write_text("base\n", encoding="utf-8")
    _git(clone, "add", "a.txt")
    _git(clone, "commit", "-q", "-m", "base")
    _git(clone, "push", "-q", "-u", "origin", "main")
    base_sha = _git(clone, "rev-parse", "HEAD").stdout.strip()

    _git(clone, "checkout", "-q", "-b", "feature")
    for i in range(8):
        (clone / "a.txt").write_text(f"base\n{i}\n", encoding="utf-8")
        _git(clone, "add", "a.txt")
        _git(clone, "commit", "-q", "-m", f"f{i}")
    _git(clone, "branch", "--set-upstream-to=origin/main", "feature")

    # Advance origin/main from a second clone, rebase onto it, then rewind
    # the local remote-tracking ref: exactly the stale-ref condition.
    other = tmp_path / "other"
    _sp.run(["git", "clone", "-q", str(origin), str(other)], check=True)
    _git(other, "config", "user.email", "t@t.t")
    _git(other, "config", "user.name", "t")
    for i in range(3):
        (other / "b.txt").write_text(f"{i}\n", encoding="utf-8")
        _git(other, "add", "b.txt")
        _git(other, "commit", "-q", "-m", f"m{i}")
    _git(other, "push", "-q", "origin", "main")

    _git(clone, "fetch", "-q", "origin")
    _git(clone, "rebase", "-q", "origin/main")
    _git(clone, "update-ref", "refs/remotes/origin/main", base_sha)

    # The control: against the stale tracking ref the two-dot count lies
    # (3 main commits + 8 feature commits). This is the 936-equivalent
    # inflation the metric exists to refuse.
    inflated = _git(
        clone, "rev-list", "--count", "feature@{upstream}..feature"
    ).stdout.strip()
    assert int(inflated) == 11

    assert uw.fetch_origin_main(clone) is True
    assert uw.ahead_of_main(clone) == 8


def test_ac4_fetch_failure_and_garbage_count_are_unknown_never_a_count():
    class _FailRun:
        def __call__(self, argv, **kw):
            class _P:
                returncode = 1
                stdout = ""
                stderr = "no remote"

            return _P()

    class _GarbageRun:
        def __call__(self, argv, **kw):
            class _P:
                returncode = 0
                stdout = "not a count\n"
                stderr = ""

            return _P()

    assert uw.fetch_origin_main(Path("/nope"), runner=_FailRun()) is False
    assert uw.ahead_of_main(Path("/nope"), runner=_FailRun()) is None
    assert uw.ahead_of_main(Path("/nope"), runner=_GarbageRun()) is None


def test_vanished_cwd_degrades_to_unknown_never_a_crash(tmp_path):
    """A git call whose cwd vanished (a worktree row deleted mid-scan) is an
    unreadable read: the metric answers unknown, the report does not die."""
    gone = tmp_path / "vanished"
    assert uw.fetch_origin_main(gone) is False
    assert uw.ahead_of_main(gone) is None
    assert uw.dirty_path_count(gone) is None


# --- AC5: dirty worktrees with no owner -----------------------------------


def test_ac5_dirty_ownerless_worktree_is_keyed_by_absolute_path():
    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/fix-scoreboard-clock", dirty=82)])
    )
    [finding] = snap.findings
    assert finding.kind == uw.KIND_DIRTY
    assert finding.subject == "/w/fix-scoreboard-clock"
    assert finding.dirty_count == 82
    assert "/w/fix-scoreboard-clock" in finding.clear_command


def test_ac5_dirty_worktree_with_a_node_names_the_target_verb():
    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/x-46f9", dirty=3, node_id="x-46f9")])
    )
    [finding] = snap.findings
    assert finding.clear_command == "/fno:target x-46f9"


def test_ac5_failed_git_status_marks_dimension_unknown():
    snap = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=None)]))
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_DIRTY].state == uw.UNKNOWN_DIM


def test_ac5_clean_worktrees_do_not_emit():
    snap = uw.classify(
        _uw_obs(worktrees=[_wt_obs("/w/clean", dirty=0, ahead=0, node_id="x-ok")])
    )
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_DIRTY].state == uw.MEASURED


# --- AC6: ownerless open PRs older than 24h -------------------------------


def test_ac6_pr_age_boundary_and_verb():
    old = _pr_obs(101, opened_epoch=NOW_1840 - 24 * 3600 - 1)
    exact = _pr_obs(102, opened_epoch=NOW_1840 - 24 * 3600)
    young = _pr_obs(103, opened_epoch=NOW_1840 - 3600)
    snap = uw.classify(_uw_obs(prs=[old, exact, young]))

    [finding] = snap.findings
    assert finding.kind == uw.KIND_PR
    assert finding.pr_number == 101
    assert finding.clear_command == "/fno:pr check 101"
    assert "pull/101" in finding.basis
    assert finding.age_s == pytest.approx(24 * 3600 + 1)


def test_ac6_merged_live_owned_and_unreadable_prs_do_not_emit():
    merged = _pr_obs(104, state="MERGED", opened_epoch=NOW_1840 - 48 * 3600)
    live_owned = _pr_obs(
        105, opened_epoch=NOW_1840 - 48 * 3600, probes=[_probe(pid_alive=True)]
    )
    snap = uw.classify(_uw_obs(prs=[merged, live_owned]))
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_PR].state == uw.MEASURED

    unreadable = _uw_obs(prs=[_pr_obs(106, state=None, opened_epoch=NOW_1840 - 48 * 3600)])
    snap2 = uw.classify(unreadable)
    assert snap2.findings == ()
    assert snap2.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM

    snap3 = uw.classify(
        _uw_obs(
            prs=[_pr_obs(107, opened_epoch=NOW_1840 - 48 * 3600, probes=[_probe()])],
        )
    )
    assert snap3.findings == ()
    assert snap3.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM

    snap4 = uw.classify(_uw_obs(prs=[_pr_obs(108, opened_epoch=NOW_1840 - 48 * 3600)], github_ok=False))
    assert snap4.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM


# --- AC7: every finding names its clearing verb ---------------------------


def test_ac7_every_emitted_finding_carries_subject_basis_and_verb():
    snap = uw.classify(
        _uw_obs(
            nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)],
            worktrees=[_wt_obs("/w/d", dirty=4)],
            prs=[_pr_obs(9, opened_epoch=NOW_1840 - 30 * 3600)],
        )
    )
    assert len(snap.findings) == 3
    for finding in snap.findings:
        assert finding.subject
        assert finding.basis
        assert finding.clear_command
        assert ":" in uw.finding_identity(finding)


def test_ac7_a_finding_without_a_verb_is_not_emitted(monkeypatch):
    monkeypatch.setattr(uw, "_dirty_clear_command", lambda w: "")
    snap = uw.classify(_uw_obs(worktrees=[_wt_obs("/w/x", dirty=7)]))
    assert snap.findings == ()
    assert any("clearing verb" in w for w in snap.warnings)


# --- AC8: a clean dimension is positive evidence --------------------------


def test_ac8_any_failed_read_blocks_complete():
    for kwargs in (
        {"graph_ok": False},
        {"claims_ok": False},
        {"registry_ok": False},
        {"github_ok": False},
        {"unscanned_roots": ("/repo/other",)},
    ):
        snap = uw.classify(_uw_obs(**kwargs))
        assert snap.complete is False, kwargs
        assert any(
            d.state == uw.UNKNOWN_DIM for d in snap.dimensions.values()
        ), kwargs


def test_ac8_unknown_dimensions_carry_their_warning():
    snap = uw.classify(_uw_obs(graph_ok=False))
    dim = snap.dimensions[uw.KIND_STARTED]
    assert dim.state == uw.UNKNOWN_DIM
    assert dim.warning


def test_ac8_ordering_is_deterministic_severity_then_age_then_subject():
    a = _node_obs("x-b", touched_epoch=NOW_1840 - 5 * 3600)
    b = _node_obs("x-a", touched_epoch=NOW_1840 - 5 * 3600)
    c = _node_obs("x-c", touched_epoch=NOW_1840 - 50 * 3600)
    snap = uw.classify(_uw_obs(nodes=[a, b, c], worktrees=[_wt_obs("/w/z", dirty=1)]))
    assert [f.subject for f in snap.findings] == ["x-c", "x-a", "x-b", "/w/z"]


# --- the digest and the signature -----------------------------------------


def test_digest_is_house_style_blocks_and_names_verbs():
    snap = uw.classify(
        _uw_obs(nodes=[_node_obs("x-7d02", touched_epoch=NOW_1840 - 116 * 3600)])
    )
    text = uw.snapshot_digest(snap)
    lines = text.splitlines()
    assert lines[0] == "unfinished work: 1 finding(s)"
    assert any(ln.startswith("- ") for ln in lines)
    assert "/fno:target x-7d02" in text
    # One physical line per paragraph: exactly one blank separator before
    # the dimension block, every content line whole.
    assert lines.count("") == 1
    assert all(ln.strip() for ln in lines if ln != "")


def test_signature_keys_on_outcome_identity_not_rows():
    s1 = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 99 * 3600)]))
    s2 = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 1 * 3600)]))
    assert uw.snapshot_signature(s1) == uw.snapshot_signature(s2)

    s3 = uw.classify(_uw_obs(nodes=[_node_obs("x-2", touched_epoch=NOW_1840 - 99 * 3600)]))
    assert uw.snapshot_signature(s1) != uw.snapshot_signature(s3)


# --- AC9: one producer, two callers; budget refusal ------------------------

_REPO_ROOT_UW = Path(watchdog.__file__).resolve().parents[4]


def _source(rel: str) -> str:
    return (_REPO_ROOT_UW / rel).read_text(encoding="utf-8")


def test_ac9_census_both_callers_route_through_one_producer():
    """Manual verb and scheduled tick consume the same build_report +
    publish_report pair; neither rebuilds the four predicates itself."""
    manual = _source("cli/src/fno/agents/cli.py")
    tick = _source("cli/src/fno/pr_watch/cli.py")
    assert "uw.build_report" in manual and "uw.publish_report" in manual
    assert "_uw.build_report" in tick and "_uw.publish_report" in tick


def test_ac9_census_no_session_predicates_on_the_report_path():
    """The retired session-bookkeeping vocabulary cannot reach the report:
    the escalation module carries no stale marker and no reap instruction,
    and the report modules own none of the forbidden metric spellings."""
    escalate = _source("cli/src/fno/agents/stale_escalate.py")
    assert "watchdog-stale" not in escalate
    assert "--only stale" not in escalate
    assert "reap it or resume it" not in escalate


def _function_body(src: str, name: str) -> str:
    start = src.index(f"def {name}(")
    nxt = src.find("\ndef ", start + 1)
    return src[start : nxt if nxt != -1 else len(src)]


def test_census_the_report_metric_owns_only_the_fresh_ref_spelling():
    """Positive marker first: origin/main..HEAD appears in the metric's
    source. Then the absence: none of the three forbidden spellings (the
    stale-tracking-ref form, its short form, and the remotes-not form) may
    appear in the metric module or the report-path functions of the
    watchdog. The reap lane's own worktree check keeps its internal
    spelling; it is not on this path."""
    report_src = _source("cli/src/fno/agents/unfinished_work.py")
    assert "origin/main..HEAD" in report_src
    wd_src = _source("cli/src/fno/agents/watchdog.py")
    report_region = (
        _function_body(wd_src, "write_sweep_file")
        + _function_body(wd_src, "unfinished_mail_gate")
    )
    for forbidden in ("@{upstream}", "@{u}", "HEAD --not --remotes"):
        assert forbidden not in report_src
        assert forbidden not in report_region


def test_ac9_budget_spent_before_scanning_reads_unknown_never_clean(tmp_path):
    """The tick's fatal deadline: roots and PR states that did not fit stay
    unread, their dimensions say unknown, and the snapshot is incomplete, so
    nothing partial is stamped or mailed as complete."""
    import time as _time

    snap = uw.build_report(
        [tmp_path],
        now_s=NOW_1840,
        graph_entries=[],
        registry_rows=({}, True),
        claim_status_fn=lambda node: {"state": "free"},
        truth_resolver=lambda handle: None,
        pr_candidates=[SimpleNamespace(node_id="x-1", pr_number=9, pr_url=None)],
        deadline_monotonic=_time.monotonic() - 1.0,
    )
    assert snap.complete is False
    assert snap.dimensions[uw.KIND_DIRTY].state == uw.UNKNOWN_DIM
    assert snap.dimensions[uw.KIND_DONE_AHEAD].state == uw.UNKNOWN_DIM
    assert snap.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM


def test_ac9_manual_and_tick_stamps_agree_for_one_snapshot(tmp_path, monkeypatch):
    """Publishing the same snapshot from either cadence writes the same
    unfinished counts, completeness, and signature: only the cadence bookkeeping
    (source, tick epoch) differs."""
    import fno.paths as paths_mod

    sweep_file = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "s")
    )
    from fno.agents import stale_escalate as _se

    monkeypatch.setattr(
        _se, "escalate_unfinished", lambda findings, **kw: ("none", "")
    )

    snap = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)]))

    uw.publish_report(snap, source="manual", now_s=NOW_1840, mail_to="")
    manual = json.loads(sweep_file.read_text())
    uw.publish_report(snap, source="tick", now_s=NOW_1840 + 60, mail_to="")
    tick = json.loads(sweep_file.read_text())

    assert manual["unfinished_counts"] == tick["unfinished_counts"]
    assert manual["unfinished_work_complete"] == tick["unfinished_work_complete"] is True
    assert manual["unfinished_signature"] == tick["unfinished_signature"]
    assert manual["source"] == "manual" and tick["source"] == "tick"


def test_report_event_gate_advances_without_a_mail_recipient(tmp_path, monkeypatch):
    """The event gate is its own stamp: with watchdog_mail_to empty the mail
    stamp never moves, and a gate chained to it would re-emit every finding
    event on every cadence. The second publish of an unchanged finding set
    emits no finding events."""
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    events = []
    monkeypatch.setattr(watchdog, "emit_event", lambda kind, data: events.append(kind))
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    from fno.agents import stale_escalate as _se

    monkeypatch.setattr(_se, "escalate_unfinished", lambda f, **kw: ("none", ""))

    snap = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)]))
    uw.publish_report(snap, source="manual", now_s=NOW_1840, mail_to="")
    first = [e for e in events if e == "watchdog_unfinished_work_finding"]
    assert len(first) == 1

    events.clear()
    uw.publish_report(snap, source="manual", now_s=NOW_1840 + 600, mail_to="")
    assert not [e for e in events if e == "watchdog_unfinished_work_finding"]


def test_report_write_preserves_the_verdict_lanes_stamps(tmp_path, monkeypatch):
    """The report and the verdict lane share one sweep file. A report write
    must carry the verdict lane's stamps through untouched, or the next
    --apply run re-mails an unchanged verdict digest."""
    import fno.paths as paths_mod

    sweep_file = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    from fno.agents import stale_escalate as _se

    monkeypatch.setattr(_se, "escalate_unfinished", lambda f, **kw: ("none", ""))

    watchdog.write_sweep_file(
        "manual", {"wake": 1}, NOW_1840, "row-a:wake", events_signature="row-a:wake",
        terminal_harness_rows=3,
    )
    snap = uw.classify(_uw_obs(nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)]))
    uw.publish_report(snap, source="manual", now_s=NOW_1840, mail_to="")

    after = json.loads(sweep_file.read_text())
    assert after["counts"] == {"wake": 1}
    assert after["signature"] == "row-a:wake"
    assert after["events_signature"] == "row-a:wake"
    assert after["terminal_harness_rows"] == 3
    assert after["unfinished_work_complete"] is True


def test_graph_failure_marks_the_pr_dimension_unknown():
    """PR candidates are enumerated from the graph, so an unreadable graph
    is zero candidates for a reason: the unmeasurable case, never a clean
    zero."""
    snap = uw.classify(_uw_obs(graph_ok=False))
    assert snap.dimensions[uw.KIND_PR].state == uw.UNKNOWN_DIM


def test_ask_line_names_the_severity_order_not_the_alphabet():
    """The question's ask says 'clear the top finding first': top means the
    digest's severity order, and a dirty-worktree subject that sorts before
    a started node alphabetically must not win the slot."""
    from fno.agents import stale_escalate as se

    started = uw.Finding(
        kind=uw.KIND_STARTED,
        subject="x-1",
        basis="in_progress, claim free",
        clear_command="/fno:target x-1",
    )
    dirty = uw.Finding(
        kind=uw.KIND_DIRTY,
        subject="/w/aaa",
        basis="3 dirty path(s)",
        clear_command="git -C /w/aaa status",
    )
    key = se.dedupe_key([f"{f.kind}:{f.subject}" for f in (started, dirty)])
    text = se.question_text([dirty, started], key)
    # The listed order is severity order, and the ask line names the first
    # of that order.
    assert text.index("started_free_claim x-1") < text.index("dirty_ownerless_worktree /w/aaa")
    assert se._ask_line([dirty, started]) == "/fno:target x-1"


# --- two defects the first live sweep caught -------------------------------


def test_pid_probe_answers_with_a_live_process():
    """Positive control: the pid probe must actually answer. The first live
    run read every claim-holder pid as unreadable (a wrong arity on the
    spawn-gate probe raised inside its own try/except), which made every
    held worktree unknown-liveness."""
    import os

    assert uw._default_pid_alive(os.getpid()) is True
    assert uw._default_pid_alive(None) is None


def test_clearing_verbs_name_the_main_worktree_not_the_reporting_one(tmp_path):
    """A report run from inside a linked worktree must still scope its
    clearing verbs to the repository's MAIN worktree: `git worktree list`
    from any linked tree lists the main tree first, and that is the root a
    human or agent should stand in."""
    import subprocess as _sp

    main = tmp_path / "main"
    main.mkdir()
    for argv in (
        ["init", "-q"],
        ["config", "user.email", "t@t.t"],
        ["config", "user.name", "t"],
    ):
        _sp.run(["git", "-C", str(main), *argv], check=True)
    (main / "a.txt").write_text("base\n", encoding="utf-8")
    _sp.run(["git", "-C", str(main), "add", "a.txt"], check=True)
    _sp.run(["git", "-C", str(main), "commit", "-q", "-m", "base"], check=True)
    linked = tmp_path / "linked"
    _sp.run(
        ["git", "-C", str(main), "worktree", "add", "-q", str(linked), "-b", "feat"],
        check=True,
    )

    obs = uw.collect_observations(
        [linked],
        now_s=NOW_1840,
        graph_entries=[],
        registry_rows=({}, True),
        claim_status_fn=lambda node: {"state": "free"},
        truth_resolver=lambda handle: None,
        pr_candidates=[],
    )
    assert obs.worktrees, "linked worktree must be enumerated"
    for w in obs.worktrees:
        if Path(w.path).name == "linked":
            assert w.repo_root == str(main)


# --- review-round fixes: unknown reads and one shared fleet scope ----------


def test_claim_view_without_a_state_word_reads_unknown_not_excluded():
    snap = uw.classify(_uw_obs(nodes=[_node_obs("x-nostate", claim_state=None)]))
    assert snap.findings == ()
    assert snap.dimensions[uw.KIND_STARTED].state == uw.UNKNOWN_DIM


def test_publish_withholds_the_durable_question_on_an_incomplete_scan(
    tmp_path, monkeypatch
):
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "state_dir", lambda: tmp_path)
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    monkeypatch.setattr(
        watchdog, "unfinished_mail_gate", lambda *a, **k: (True, "no recipient", "")
    )
    from fno.agents import stale_escalate as _se

    called = []
    monkeypatch.setattr(
        _se, "escalate_unfinished", lambda f, **kw: called.append(1) or ("recorded", "q")
    )

    snap = uw.classify(
        _uw_obs(graph_ok=False, nodes=[_node_obs("x-1", touched_epoch=NOW_1840 - 3600)])
    )
    assert snap.complete is False
    notes: list[str] = []
    uw.publish_report(
        snap, source="manual", now_s=NOW_1840, mail_to="", log=notes.append
    )
    assert called == []
    assert any("incomplete scan" in note for note in notes)


def test_manual_report_and_tick_share_one_fleet_scope(monkeypatch):
    """The manual verb and the tick resolve their roots through one shared
    resolver, so a hand-run names the same fleet the tick's digest named."""
    from fno.agents import cli as agents_cli
    from fno.pr_watch import cli as prcli

    monkeypatch.setattr(uw, "report_roots", lambda: [Path("/fleet/scope")])
    assert prcli._watchdog_recovery_roots() == [Path("/fleet/scope")]

    class _Snap:
        generated_at = "x"
        findings = ()
        complete = True
        warnings = ()
        dimensions = {
            dim: uw.DimensionState(uw.MEASURED, 0, None) for dim in uw.DIMENSIONS
        }

    captured = {}

    def _fake_build(roots, **kw):
        captured["roots"] = [str(r) for r in roots]
        return _Snap()

    monkeypatch.setattr(uw, "build_report", _fake_build)
    monkeypatch.setattr(
        uw, "publish_report", lambda s, **kw: {"counts": {}, "findings": [], "warnings": []}
    )
    agents_cli.cmd_watchdog(
        json_out=False, apply=False, apply_all=False, only=None, mail_to=""
    )
    assert captured["roots"] == ["/fleet/scope"]


# -- x-b150: the reap receipt gate -------------------------------------------


def _receipt_row(**over):
    from types import SimpleNamespace as ns

    row = ns(
        name="king-mux",
        short_id="kingmux",
        harness="claude",
        harness_session_id="019cdddd-0000-7000-8000-000000000009",
        cwd="/wt/king",
        log_path="/tmp/king-mux.log",
        created_at="2026-08-30T10:00:00Z",
    )
    for k, v in over.items():
        setattr(row, k, v)
    return row


def test_reap_receipt_is_built_from_the_row_with_the_capability_table_resume(
    monkeypatch, tmp_path
):
    """The receipt's fields come from the registry row, and the resume
    command is rendered from the capability table - the same single source
    `fno whoami ledger` reads - never a hardcoded string."""
    import fno.agents.registry as registry_mod
    import fno.paths as paths_mod

    monkeypatch.setattr(
        registry_mod, "load_registry", lambda path=None: [_receipt_row()]
    )
    monkeypatch.setattr(paths_mod, "agents_registry_path", lambda: tmp_path / "reg.json")
    monkeypatch.setattr(paths_mod, "agents_home_dir", lambda: tmp_path)
    monkeypatch.setattr(paths_mod, "ledger_json", lambda: tmp_path / "no-ledger.json")

    ok, detail = watchdog._persist_reap_receipt("king-mux")

    assert ok, detail
    receipt = json.loads((tmp_path / "reap-receipts" / detail.split("/")[-1]).read_text())
    assert receipt["resume"] == (
        f"claude --resume {_receipt_row().harness_session_id}"
    )
    assert receipt["harness"] == "claude"
    assert receipt["harness_session_id"] == _receipt_row().harness_session_id
    assert receipt["cwd"] == "/wt/king"
    assert receipt["log_path"] == "/tmp/king-mux.log"
    assert receipt["row_name"] == "king-mux"
    assert "ledger" not in receipt, "no ledger entry exists; none may be invented"


def test_reap_receipt_names_the_harness_with_no_capability_row(monkeypatch, tmp_path):
    """grok ships a readiness manifest, not a capability row: no declared
    resume form, so the receipt cannot be built and the row never reaps."""
    import fno.agents.registry as registry_mod
    import fno.paths as paths_mod

    monkeypatch.setattr(
        registry_mod,
        "load_registry",
        lambda path=None: [_receipt_row(harness="grok")],
    )
    monkeypatch.setattr(paths_mod, "agents_registry_path", lambda: tmp_path / "reg.json")
    monkeypatch.setattr(paths_mod, "agents_home_dir", lambda: tmp_path)
    monkeypatch.setattr(paths_mod, "ledger_json", lambda: tmp_path / "no-ledger.json")

    ok, detail = watchdog._persist_reap_receipt("king-mux")

    assert not ok
    assert "grok" in detail


def test_apply_reap_refuses_when_the_receipt_cannot_be_staged(monkeypatch):
    """The gate sits before the stop: a row whose receipt cannot be written
    keeps its registry row and nothing runs against it."""
    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: None)
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(
        watchdog, "_persist_reap_receipt", lambda rid: (False, "no resumable identity")
    )
    ran = []

    def runner(argv, **kwargs):
        ran.append(argv)
        return subprocess.CompletedProcess(argv, 0, "", "")

    v = Verdict("aaaa1111-0000", "w1", "working", REAP, "node x done", "stop+rm")
    outcome, detail = watchdog._apply_reap(v, cwd="/wt/x", runner=runner)
    assert outcome == "refused" and "no resumable identity" in detail
    assert ran == [], "nothing may run against a row the gate held"


def test_reap_receipt_ignores_a_ledger_row_whose_sessions_is_a_string(
    monkeypatch, tmp_path
):
    """`sid in "string"` is a substring test; only a real list of session ids
    may match, or a malformed ledger row donates its node/pr to a receipt."""
    import fno.agents.registry as registry_mod
    import fno.paths as paths_mod

    ledger = tmp_path / "ledger.json"
    ledger.write_text(
        json.dumps(
            {"entries": [{"graph_node_id": "x-impostor", "sessions": "x-king-mux-sessions"}]}
        ),
        encoding="utf-8",
    )
    monkeypatch.setattr(registry_mod, "load_registry", lambda path=None: [_receipt_row()])
    monkeypatch.setattr(paths_mod, "agents_registry_path", lambda: tmp_path / "reg.json")
    monkeypatch.setattr(paths_mod, "agents_home_dir", lambda: tmp_path)
    monkeypatch.setattr(paths_mod, "ledger_json", lambda: ledger)

    ok, detail = watchdog._persist_reap_receipt("king-mux")

    assert ok, detail
    receipt = json.loads(
        (tmp_path / "reap-receipts" / detail.split("/")[-1]).read_text()
    )
    assert "ledger" not in receipt


def test_apply_reap_restages_the_receipt_between_stop_and_rm(monkeypatch):
    """rm removes the identity; the receipt written immediately before it must
    describe the row as it is at rm time, not as it was at classification."""
    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: None)
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
    order = []
    monkeypatch.setattr(
        watchdog,
        "_persist_reap_receipt",
        lambda rid: order.append("stage") or (True, "staged"),
    )

    def runner(argv, **kwargs):
        order.append(argv[-2])
        return subprocess.CompletedProcess(argv, 0, "", "")

    v = Verdict("aaaa1111-0000", "w1", "working", REAP, "node x done", "stop+rm")
    outcome, _ = watchdog._apply_reap(v, cwd="/wt/x", runner=runner)
    assert outcome == "applied"
    assert order == ["stage", "stop", "stage", "rm"]


def test_apply_reap_holds_the_rm_when_the_restage_fails(monkeypatch):
    """A receipt that cannot be re-staged after the stop is 'partial': the
    row stays, and the detail says the session is already stopped."""
    monkeypatch.setattr(watchdog, "worktree_refusal", lambda cwd: None)
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
    calls = {"n": 0}

    def gate(rid):
        calls["n"] += 1
        return (calls["n"] == 1, "staged" if calls["n"] == 1 else "identity changed")

    monkeypatch.setattr(watchdog, "_persist_reap_receipt", gate)
    ran = []

    def runner(argv, **kwargs):
        ran.append(argv[-2])
        return subprocess.CompletedProcess(argv, 0, "", "")

    v = Verdict("aaaa1111-0000", "w1", "working", REAP, "node x done", "stop+rm")
    outcome, detail = watchdog._apply_reap(v, cwd="/wt/x", runner=runner)
    assert outcome == "partial" and "already stopped" in detail
    assert ran == ["stop"], "a held rm must never run"
