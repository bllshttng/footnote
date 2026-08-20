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
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP),
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
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP),
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
                last_message_at=STALE_MESSAGE_STAMP)]
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
    # the mechanism past it.
    monkeypatch.setattr(watchdog, "_is_linked_worktree", lambda cwd: True)
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
    row = Row("dddd4444-0000", "bp-worker", "working", None, "/tmp/bp", True)
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
    row = Row("dddd4444-0000", "bp-worker", "idle", None, "/tmp/bp", True)
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


def test_every_registry_row_is_born_with_an_origin():
    """The producer half, and it has to cover EVERY birth site.

    The consumer above only acts on a positive marker, so a spawn path that
    forgets one does not open a hole - it makes the lane silently unsatisfiable
    for the workers that path creates, which is the same defect pointing the
    other way. A row can only be born through an `AgentEntry(...)` literal, so
    the check is over all of them rather than over the three that exist today."""
    import ast
    import pathlib

    pkg = pathlib.Path(watchdog.__file__).parent
    missing = []
    for path in sorted(pkg.glob("*.py")):
        tree = ast.parse(path.read_text())
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            if getattr(node.func, "id", None) != "AgentEntry":
                continue
            # `AgentEntry(**row)` rehydrates a row that was already born.
            if any(kw.arg is None for kw in node.keywords):
                continue
            if not any(kw.arg == "origin" for kw in node.keywords):
                missing.append(f"{path.name}:{node.lineno}")
    assert not missing, (
        "every registry row must state what created it; unmarked at " + ", ".join(missing)
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
              STALE_MESSAGE_STAMP)
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

    agents_cli.cmd_watchdog(
        json_out=False, apply=False, apply_all=False, only=None, mail_to=""
    )

    assert "terminal harness rows: 3" in capsys.readouterr().out


def test_cli_escalates_stale_rows_before_only_filter(monkeypatch):
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
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (payload, rows))
    monkeypatch.setattr(watchdog, "write_sweep_file", lambda *a, **k: None)
    monkeypatch.setattr(watchdog, "mail_gate", lambda *a, **k: (True, "", ""))
    monkeypatch.setattr(watchdog, "_last_events_signature", lambda: "")
    monkeypatch.setattr(watchdog, "emit_event", lambda *a, **k: None)
    captured = []
    monkeypatch.setattr(
        stale_escalate,
        "escalate_stale",
        lambda rows, **kwargs: captured.extend(rows) or ("recorded", "q-stale"),
    )

    agents_cli.cmd_watchdog(
        json_out=False, apply=False, apply_all=False, only=WAKE, mail_to=""
    )

    assert [(row.row_id, row.node, row.basis) for row in captured] == [
        ("stale-row", None, "blocked 14h")
    ]


def test_cli_refused_sweep_never_escalates(monkeypatch):
    from fno.agents import cli as agents_cli
    from fno.agents import stale_escalate

    refused = _refused_payload()
    monkeypatch.setattr(watchdog, "run_sweep", lambda **kw: (refused, []))
    monkeypatch.setattr(
        stale_escalate,
        "escalate_stale",
        lambda *a, **k: (_ for _ in ()).throw(AssertionError("must not escalate")),
    )

    with pytest.raises(typer.Exit) as exc:
        agents_cli.cmd_watchdog(
            json_out=False, apply=False, apply_all=False, only=None, mail_to=""
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


def test_a_shared_worktree_is_never_reaped_even_with_a_done_node():
    """Reap ends in ``rm``, which deletes the worktree out from under a peer.

    Measured on the live fleet: two sessions working in one linked worktree,
    both resolving one node. When that node goes done the quiet one earns a
    reap and destroys the checkout the busy one is mid-task in.
    ``_is_linked_worktree`` cannot see this - a linked worktree's .git is a
    file no matter how many sessions are standing in it.
    """
    rows = [
        Row("aaaa1111-0000", "quiet", "working", "x-done", "/wt/x-bcb5"),
        Row("bbbb2222-0000", "busy", "working", "x-done", "/wt/x-bcb5"),
        Row("cccc3333-0000", "alone", "working", "x-done", "/wt/solo",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP),
    ]
    vs = _run(
        rows,
        {
            "aaaa1111-0000": _facts(FINISHED_TAIL, age_min=30),
            "bbbb2222-0000": _facts("tool_use Bash", age_min=0, kind="tool"),
            "cccc3333-0000": _facts(FINISHED_TAIL, age_min=30),
        },
        nodes={"x-done": {"status": "done"}},
    )
    assert vs[0].verdict == STALE, "a co-tenanted worktree must never reap"
    assert "share /wt/x-bcb5" in vs[0].basis and vs[0].action == "report"
    # The sole occupant is untouched: this narrows reap, it does not kill it.
    assert vs[2].verdict == REAP


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
    rows = [
        Row("aaaa1111-0000", "quiet", "working", "x-done", "/wt/one",
            origin="spawn", last_message_at=STALE_MESSAGE_STAMP),
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
    assert vs[0].verdict == STALE, vs[0].basis
    assert "share /wt/one" in vs[0].basis

    # An UNREADABLE sibling transcript also holds the tree: absence is not
    # evidence that the sibling left.
    vs = _run(
        rows,
        {"aaaa1111-0000": _facts(FINISHED_TAIL, age_min=120)},
        nodes={"x-done": {"status": "done"}},
    )
    assert vs[0].verdict == STALE, vs[0].basis

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
                                only=None, mail_to=None)
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

def _decide(row, *, facts=None, nodes=None, claims=None, cotenants=0,
            node_state_for=None, claim_for=None, now_s=NOW_1840):
    return watchdog.reap_decision(
        row,
        facts=facts,
        node_state_for=node_state_for or (lambda n: (nodes or {}).get(n)),
        claim_for=claim_for or (lambda n: (claims or {}).get(n, {})),
        now_s=now_s,
        quiet_after_s=watchdog.REAP_QUIET_AFTER_S,
        cotenants=cotenants,
    )


def test_every_failed_read_is_unknown_and_unknown_never_reaps():
    """The structural rule, stated once as a test.

    Eight findings over three rounds were one defect: a reading about one
    thing used as a verdict about another, usually an ABSENCE read as a
    positive answer. Fixing them site by site converged on nothing. This
    pins the shape instead: whatever fails, the answer is UNKNOWN, and the
    only way to YES is through three positive markers.
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

    # A shared worktree is not "mine to delete".
    answer, _ = _decide(row, facts=quiet, nodes={"x-done": {"status": "done"}},
                        cotenants=1)
    assert answer == watchdog.REAP_UNKNOWN

    # All three markers present, and only then.
    answer, basis = _decide(row, facts=quiet, nodes={"x-done": {"status": "done"}})
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
              origin="spawn", last_message_at=STALE_MESSAGE_STAMP)
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
    """
    expected = {
        "operator": watchdog.REAP_NO,
        None: watchdog.REAP_UNKNOWN,
        "spawn": watchdog.REAP_YES,
    }
    for origin, want in expected.items():
        row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo",
                  origin=origin, last_message_at=STALE_MESSAGE_STAMP)
        answer, basis = _decide(
            row,
            facts=_facts(FINISHED_TAIL, age_min=200),
            nodes={"x-done": {"status": "done"}},
        )
        assert answer == want, f"origin={origin!r} gave {answer}: {basis}"


def test_an_adopted_row_survives_its_recency_stamp_going_stale():
    """The reachable case the first cut of this predicate would have deleted.

    `mint_adopted_entry` writes `origin: None` beside a FRESH
    `last_message_at`, and adopt takes in both a session a human started by
    hand and a footnote orphan. Inside the window the stamp protects the row.
    This asserts what happens AFTER it goes stale, which is where the two
    protectors used to fall silent together and hand over a worktree nobody
    could prove was a worker's.
    """
    adopted = Row("aaaa1111-0000", "adopted-external", "working", "x-done",
                  "/wt/solo", origin=None, last_message_at=STALE_MESSAGE_STAMP)

    answer, basis = _decide(
        adopted,
        facts=_facts(FINISHED_TAIL, age_min=200),
        nodes={"x-done": {"status": "done"}},
    )

    assert answer == watchdog.REAP_UNKNOWN
    assert "origin was never recorded" in basis
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

    _, basis = _decide(row, facts=quiet, nodes=nodes, cotenants=1)
    assert "share /wt/shared" in basis

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
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/shared")

    # Shared worktree still names the worktree.
    _, basis = _decide(row, facts=quiet, nodes=nodes, cotenants=1)
    assert "share /wt/shared" in basis

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
