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
from types import SimpleNamespace

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


def test_single_sgt_429_is_report_only_until_provider_quorum():
    row = Row("cccc3333-0000", "r1", "blocked", None, "/tmp/r1")
    # 125m silent: past classify_tail's 2h window, so the tail POSITIVELY
    # asserts the session owes its next move (stalled) - the wake gate's
    # resumability marker, not a missing-429 absence.
    [before] = _run([row], {"cccc3333-0000": _facts(RATE_LIMIT_TAIL, age_min=125)})
    assert before.verdict == LEAVE
    assert "provider quorum" in before.basis
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


def test_old_passed_plus_new_live_429_waits_for_provider_quorum():
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
    assert v.verdict == LEAVE
    assert "provider quorum" in v.basis


def test_identity_joins_on_claim_holder_not_name():
    # Two rows whose NAMES both carry node x-9d11, but the recorded manifest
    # node differs (x-2222 vs none). Only the claim/manifest join may decide.
    rows = [
        Row("eeee1111-0000", "target-x-9d11-alpha", "working", "x-2222", "/tmp/a"),
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
        Row("aaaa1111-0000", "w1", "working", "x-done", "/tmp/w1"),
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


def test_ac12_obs_sweep_payload_and_receipt_include_measured_provider_outages(
    monkeypatch, tmp_path
):
    path = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)
    measured = {
        "instrument": "measured",
        "breakers": [{"provider": "zai", "account": "acct-a"}],
        "counts": {"open": 1},
        "refusals": [],
    }
    rows = [Row("aaaa1111-0000", "w1", "working", None, "/tmp")]
    payload, _ = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: _facts("ok"),
        claim_fn=lambda node: {},
        graph_fn=lambda: {},
        provider_outage_fn=lambda: measured,
    )
    assert payload["provider_outages"] == measured
    watchdog.write_sweep_file(
        "tick", payload["counts"], NOW_1840,
        provider_outages=payload["provider_outages"],
    )
    assert json.loads(path.read_text())["provider_outages"] == measured


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
    assert payload["provider_outages"] == {
        "instrument": "unknown",
        "breakers": [],
        "counts": {"provider_outage_collector_missing": 1},
        "refusals": [{"reason": "provider_outage_collector_missing", "count": 1}],
    }
    # Rows ride along index-aligned so apply lanes can reach each cwd.
    assert out_rows == rows


def test_ac12_obs_sweep_file_never_invents_measured_provider_evidence(
    monkeypatch, tmp_path
):
    path = tmp_path / "watchdog-sweep.json"
    monkeypatch.setattr(watchdog, "sweep_path", lambda: path)

    watchdog.write_sweep_file("tick", {LEAVE: 1}, NOW_1840)

    stored = json.loads(path.read_text())
    assert stored["provider_outages"] == {
        "instrument": "unknown",
        "breakers": [],
        "counts": {"provider_outage_report_missing": 1},
        "refusals": [{"reason": "provider_outage_report_missing", "count": 1}],
    }


@pytest.mark.parametrize("mode", ["off", "report", "wake"])
def test_ac12_obs_only_handoff_mode_arms_provider_migration(mode):
    settings = SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(enabled=True, watchdog=mode),
    )
    assert watchdog.handoff_armed(settings) is False


def test_ac12_obs_handoff_mode_obeys_master_vetoes():
    armed = SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(enabled=True, watchdog="handoff"),
    )
    assert watchdog.lane_armed(armed) is True
    assert watchdog.handoff_armed(armed) is True
    assert watchdog.handoff_armed(SimpleNamespace()) is False
    assert watchdog.handoff_armed(SimpleNamespace(
        autonomy=SimpleNamespace(enabled=False),
        recovery=SimpleNamespace(enabled=True, watchdog="handoff"),
    )) is False


def test_ac1_det_production_collector_joins_explicit_registry_route_identity(
    monkeypatch, tmp_path
):
    fup = (
        "API Error: Request rejected (429): Fair Usage Policy; "
        "submit a request to restore access"
    )
    paths = {}
    entries = []
    rows = []
    for index in (1, 2):
        sid = f"session-{index}"
        path = tmp_path / f"{sid}.jsonl"
        path.write_text(json.dumps({
            "type": "assistant",
            "timestamp": "2026-08-16T18:39:30Z",
            "isApiErrorMessage": True,
            "apiErrorStatus": 429,
            "message": {"role": "assistant", "content": fup},
        }) + "\n", encoding="utf-8")
        paths[sid] = path
        entries.append(SimpleNamespace(
            harness_session_id=sid,
            harness="claude",
            route_provider_id="anthropic",
            account_record_id="acct-a",
            cwd=str(tmp_path),
        ))
        rows.append(Row(sid, f"worker-{index}", "working", "x-abcd", str(tmp_path)))

    report = watchdog.measure_provider_outages(
        rows,
        now_s=NOW_1840,
        entries_provider=lambda: entries,
        transcript_path_for=lambda identity: paths[identity.session_id],
        journal=tmp_path / "provider-outages.json",
    )

    assert report["instrument"] == "measured"
    assert report["breakers"][0]["provider"] == "anthropic"
    assert report["breakers"][0]["account"] == "acct-a"


def test_ac4_err_production_collector_missing_axes_is_count_bearing_unknown(tmp_path):
    rows = [Row("session-1", "worker", "working", "x-abcd", str(tmp_path))]
    report = watchdog.measure_provider_outages(
        rows,
        now_s=NOW_1840,
        entries_provider=lambda: [SimpleNamespace(
            harness_session_id="session-1", harness="claude", cwd=str(tmp_path),
        )],
        journal=tmp_path / "provider-outages.json",
    )
    assert report["instrument"] == "unknown"
    assert report["breakers"] == []
    assert report["counts"] == {"unknown_route_identity": 1}


def test_ac1_det_production_collector_persists_exact_pane_fallback_before_vote(
    tmp_path,
):
    fup = (
        "API Error: Request rejected (429): Fair Usage Policy; "
        "submit a request to restore access"
    )
    rows = []
    entries = []
    reads = []
    for index in (1, 2):
        sid = f"agy-session-{index}"
        rows.append(Row(sid, f"agy-{index}", "working", "x-abcd", str(tmp_path)))
        entries.append(SimpleNamespace(
            harness_session_id=sid,
            harness="agy",
            route_provider_id="zai",
            account_record_id="acct-a",
            cwd=str(tmp_path),
            mux={"session": "stable-session", "pane_id": 40 + index},
        ))

    def pane_read(session, pane_id):
        reads.append((session, pane_id))
        return fup

    journal = tmp_path / "provider-outages.json"
    snapshots = tmp_path / "pane-snapshots"
    report = watchdog.measure_provider_outages(
        rows,
        now_s=NOW_1840,
        entries_provider=lambda: entries,
        transcript_path_for=lambda _identity: None,
        pane_read_fn=pane_read,
        pane_snapshot_dir=snapshots,
        journal=journal,
    )

    assert report["instrument"] == "measured"
    assert report["breakers"][0]["row_ids"] == ["agy-session-1", "agy-session-2"]
    assert reads == [("stable-session", 41), ("stable-session", 42)]
    stored = [json.loads(path.read_text()) for path in snapshots.glob("*.json")]
    assert len(stored) == 2
    assert all(item["observed_at"] == NOW_1840 for item in stored)
    assert {(item["mux_session"], item["pane_id"]) for item in stored} == {
        ("stable-session", "41"), ("stable-session", "42"),
    }
    assert all(item["provider"] == "zai" and item["account"] == "acct-a" for item in stored)


def test_ac1_det_readable_transcript_is_not_duplicated_as_a_pane_vote(tmp_path):
    transcript = tmp_path / "session.jsonl"
    transcript.write_text(json.dumps({
        "type": "assistant",
        "timestamp": "2026-08-16T18:39:30Z",
        "isApiErrorMessage": True,
        "apiErrorStatus": 429,
        "message": {"role": "assistant", "content": "API Error 429: Fair Usage Policy"},
    }) + "\n", encoding="utf-8")
    entry = SimpleNamespace(
        harness_session_id="session-1", harness="agy",
        route_provider_id="zai", account_record_id="acct-a", cwd=str(tmp_path),
        mux={"session": "stable-session", "pane_id": 9},
    )
    reads = []

    report = watchdog.measure_provider_outages(
        [Row("session-1", "agy", "working", "x-abcd", str(tmp_path))],
        now_s=NOW_1840,
        entries_provider=lambda: [entry],
        transcript_path_for=lambda _identity: transcript,
        pane_read_fn=lambda *args: reads.append(args) or "API Error 429: Fair Usage Policy",
        pane_snapshot_dir=tmp_path / "snapshots",
        journal=tmp_path / "provider-outages.json",
    )

    assert report["counts"]["accepted"] == 1
    assert reads == []


def test_production_measure_applies_validated_outage_policy_settings(tmp_path):
    from fno.config import RecoveryBlock

    fup = "API Error: Request rejected (429): Fair Usage Policy"
    entries = []
    rows = []
    paths = {}
    for index in (1, 2):
        sid = f"session-{index}"
        transcript = tmp_path / f"{sid}.jsonl"
        transcript.write_text(json.dumps({
            "type": "assistant", "timestamp": "2026-08-16T18:39:30Z",
            "isApiErrorMessage": True, "apiErrorStatus": 429,
            "message": {"role": "assistant", "content": fup},
        }) + "\n")
        paths[sid] = transcript
        entries.append(SimpleNamespace(
            harness_session_id=sid, harness="claude", route_provider_id="zai",
            account_record_id="acct-a", cwd=str(tmp_path), mux=None,
        ))
        rows.append(Row(sid, sid, "working", "x-abcd", str(tmp_path)))

    report = watchdog.measure_provider_outages(
        rows,
        now_s=NOW_1840,
        settings=SimpleNamespace(recovery=RecoveryBlock(provider_outage_quorum=3)),
        entries_provider=lambda: entries,
        transcript_path_for=lambda identity: paths[identity.session_id],
        journal=tmp_path / "provider-outages.json",
    )
    assert report["breakers"] == []


def test_ac5_hlth_production_candidate_uses_policy_order_and_observed_health(tmp_path):
    from fno.agents.provider_outage import CanaryProof

    entries = [
        SimpleNamespace(
            account_record_id="full", harness="codex", route_provider_id="openai",
            model_name="gpt-full", route_settings_path=None,
        ),
        SimpleNamespace(
            account_record_id="good", harness="agy", route_provider_id="google",
            model_name="gemini-good", route_settings_path=None,
        ),
    ]
    canaries = []

    def canary(candidate, _row, now_s):
        canaries.append(candidate.record_id)
        return CanaryProof(
            source="pane", content="FNO_PROVIDER_HEALTH_OK", observed_at=now_s,
            persisted=True, assistant_role=False, pane_id="88", stopped=True,
        )

    selected = watchdog.production_handoff_candidate(
        {"provider": "zai", "account": "broken"},
        Row("source", "worker", "working", "x-abcd", str(tmp_path)),
        NOW_1840,
        entries_provider=lambda: entries,
        route_policy_provider=lambda _row: (["full", "good"], {}),
        account_env_for=lambda account, _root: {"ACCOUNT": account},
        route_env_for=lambda _entry: {},
        runtime_exhausted_fn=lambda account, _root: account == "full",
        harness_installed_fn=lambda harness: harness in {"codex", "agy"},
        pane_occupancy_fn=lambda _harness: 3,
        canary_fn=canary,
        open_breakers_provider=lambda: [],
    )

    assert selected is not None
    assert (selected.record_id, selected.harness, selected.provider, selected.model) == (
        "good", "agy", "google", "gemini-good",
    )
    assert selected.pane_count == 3
    assert selected.runtime_exhausted is False
    assert canaries == ["good"]


def test_ac5_hlth_production_candidate_canaries_configured_route_without_registry_row(
    tmp_path, monkeypatch,
):
    from fno import paths
    from fno.agents import provider_outage

    config = tmp_path / ".fno" / "config.toml"
    config.parent.mkdir()
    config.write_text("""
[accounts]
active_combo = "outage"

[[accounts.records]]
id = "broken"
name = "Broken"
harness = "claude"
auth = "api_key"
route_provider_id = "zai"
model_name = "glm-5.3"
env = { ANTHROPIC_API_KEY = "test" }

[[accounts.records]]
id = "codex-backup"
name = "Codex backup"
harness = "codex"
auth = "api_key"
route_provider_id = "openai"
model_name = "gpt-5.6-sol"
env = { OPENAI_API_KEY = "test" }

[accounts.combos.outage]
providers = ["broken", "codex-backup"]
strategy = "fallback"
""", encoding="utf-8")
    canaries = []
    state = tmp_path / "state"
    monkeypatch.setattr(paths, "state_dir", lambda: state)
    monkeypatch.setattr(
        watchdog.subprocess,
        "run",
        lambda *_args, **_kwargs: SimpleNamespace(
            returncode=0, stdout="FNO_PROVIDER_HEALTH_OK\n", stderr=""
        ),
    )

    def run_canary(candidate, **kwargs):
        canaries.append(candidate)
        return kwargs["collect_proof"](SimpleNamespace(
            session="stable-session", pane_id=88, name="health-canary",
        ))

    monkeypatch.setattr(provider_outage, "run_health_canary", run_canary)

    selected = watchdog.production_handoff_candidate(
        {"provider": "zai", "account": "broken"},
        Row("source", "worker", "working", "x-missing", str(tmp_path)),
        NOW_1840,
        entries_provider=lambda: [],
        runtime_exhausted_fn=lambda _account, _root: False,
        harness_installed_fn=lambda harness: harness == "codex",
        pane_occupancy_fn=lambda _harness: 0,
        open_breakers_provider=lambda: [],
    )

    assert selected is not None
    assert (
        selected.record_id,
        selected.harness,
        selected.provider,
        selected.model,
    ) == ("codex-backup", "codex", "openai", "gpt-5.6-sol")
    assert selected.account_env == {"OPENAI_API_KEY": "test"}
    assert [candidate.record_id for candidate in canaries] == ["codex-backup"]
    [proof] = list((state / "recovery" / "provider-canaries").glob("*.json"))
    persisted = json.loads(proof.read_text())
    assert persisted["provider"] == "openai"
    assert persisted["account"] == "codex-backup"


def test_ac5_hlth_node_pin_to_broken_route_refuses_automatic_movement(tmp_path):
    canaries = []
    selected = watchdog.production_handoff_candidate(
        {"provider": "zai", "account": "broken"},
        Row("source", "worker", "working", "x-abcd", str(tmp_path)),
        NOW_1840,
        entries_provider=lambda: [],
        route_policy_provider=lambda _row: (["other"], {"provider": "zai"}),
        canary_fn=lambda *_args: canaries.append(True),
    )
    assert selected is None
    assert canaries == []


@pytest.mark.parametrize("mode", ["report", "wake"])
def test_ac12_obs_report_and_wake_never_call_provider_handoff(mode, tmp_path):
    settings = SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(enabled=True, watchdog=mode),
    )
    called = []
    results = watchdog.supervise_provider_handoffs(
        {"instrument": "measured", "breakers": [{
            "provider": "zai", "account": "acct-a", "outage_epoch": 1,
            "row_ids": ["source"], "fingerprints": ["f1", "f2"],
        }]},
        [Row("source", "worker", "working", "x-abcd", str(tmp_path))],
        settings=settings,
        now_s=NOW_1840,
        candidate_for=lambda *_args: called.append("candidate"),
        handoff_fn=lambda *_args, **_kwargs: called.append("handoff"),
    )
    assert results == []
    assert called == []


def test_ac12_obs_handoff_mode_requires_fresh_canary_then_runs_one_transaction(
    tmp_path
):
    from fno.agents.outage_handoff import HandoffResult
    from fno.agents.provider_outage import CanaryProof, RouteCandidate

    settings = SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(enabled=True, watchdog="handoff"),
    )
    candidate = RouteCandidate(
        record_id="codex-work", harness="codex", provider="openai",
        account="acct-b", account_env={}, model="gpt-5.6-sol", route_env={},
        canary=CanaryProof(
            source="pane", content="FNO_PROVIDER_HEALTH_OK",
            observed_at=NOW_1840 - 1, persisted=True, assistant_role=False,
            pane_id="pane-7", stopped=True,
        ),
    )
    calls = []
    decisions = []

    def handoff(request, **_kwargs):
        calls.append(request)
        return HandoffResult(
            node=request.node, outage_epoch=request.outage_epoch,
            attempt="attempt-1", phase="committed", replayed=False,
        )

    results = watchdog.supervise_provider_handoffs(
        {"instrument": "measured", "breakers": [{
            "provider": "zai", "account": "acct-a", "outage_epoch": 1,
            "row_ids": ["source"], "fingerprints": ["f1", "f2"],
        }]},
        [Row("source", "worker", "working", "x-abcd", str(tmp_path))],
        settings=settings,
        now_s=NOW_1840,
        candidate_for=lambda *_args: candidate,
        handoff_fn=handoff,
        deps_factory=lambda: object(),
        decision_fn=lambda **kwargs: decisions.append(kwargs),
        journal_root=tmp_path / "transactions",
    )

    assert results[0]["phase"] == "committed"
    assert len(calls) == 1
    assert calls[0].destination_provider == "openai"
    assert calls[0].destination_model == "gpt-5.6-sol"
    assert calls[0].source_provider == "zai"
    assert calls[0].source_account == "acct-a"
    assert calls[0].evidence_fingerprints == ("f1", "f2")
    assert decisions[0]["authority_source"] == "daemon-automation"
    assert decisions[0]["source"] == "daemon"


def test_production_supervisor_applies_configured_canary_ttl(tmp_path):
    from fno.agents.outage_handoff import HandoffResult
    from fno.agents.provider_outage import CanaryProof, RouteCandidate
    from fno.config import RecoveryBlock

    settings = SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=RecoveryBlock(
            enabled=True, watchdog="handoff", provider_health_marker_ttl_seconds=121,
        ),
    )
    candidate = RouteCandidate(
        record_id="codex-work", harness="codex", provider="openai",
        account="acct-b", account_env={}, model="gpt-5.6-sol", route_env={},
        canary=CanaryProof(
            source="pane", content="FNO_PROVIDER_HEALTH_OK",
            observed_at=NOW_1840 - 121, persisted=True, assistant_role=False,
            pane_id="pane-7", stopped=True,
        ),
    )
    calls = []
    results = watchdog.supervise_provider_handoffs(
        {"instrument": "measured", "breakers": [{
            "provider": "zai", "account": "acct-a", "outage_epoch": 1,
            "row_ids": ["source"], "fingerprints": ["f1", "f2"],
        }]},
        [Row("source", "worker", "working", "x-abcd", str(tmp_path))],
        settings=settings,
        now_s=NOW_1840,
        candidate_for=lambda *_args: candidate,
        handoff_fn=lambda request, **_kwargs: calls.append(request) or HandoffResult(
            node=request.node, outage_epoch=request.outage_epoch,
            attempt="attempt-1", phase="committed", replayed=False,
        ),
        deps_factory=lambda: object(),
        decision_fn=lambda **_kwargs: None,
        journal_root=tmp_path / "transactions",
    )
    assert results[0]["phase"] == "committed"
    assert len(calls) == 1


@pytest.mark.parametrize("phase,replayed,expected", [
    ("committed", True, 0),
    ("parked", False, 1),
    ("refused", False, 0),
])
def test_ac12_obs_terminal_decision_is_once_only(phase, replayed, expected, tmp_path):
    from fno.agents.outage_handoff import HandoffResult
    from fno.agents.provider_outage import CanaryProof, RouteCandidate

    settings = SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(enabled=True, watchdog="handoff"),
    )
    candidate = RouteCandidate(
        record_id="codex-work", harness="codex", provider="openai",
        account="acct-b", account_env={}, model="gpt-5.6-sol", route_env={},
        canary=CanaryProof(
            source="pane", content="FNO_PROVIDER_HEALTH_OK",
            observed_at=NOW_1840 - 1, persisted=True, assistant_role=False,
            pane_id="pane-7", stopped=True,
        ),
    )
    decisions = []
    watchdog.supervise_provider_handoffs(
        {"instrument": "measured", "breakers": [{
            "provider": "zai", "account": "acct-a", "outage_epoch": 1,
            "row_ids": ["source"], "fingerprints": ["f1", "f2"],
        }]},
        [Row("source", "worker", "working", "x-abcd", str(tmp_path))],
        settings=settings, now_s=NOW_1840,
        candidate_for=lambda *_args: candidate,
        handoff_fn=lambda request, **_kwargs: HandoffResult(
            node=request.node, outage_epoch=request.outage_epoch,
            attempt="attempt-1", phase=phase, replayed=replayed,
        ),
        deps_factory=lambda: object(),
        decision_fn=lambda **kwargs: decisions.append(kwargs),
        journal_root=tmp_path / "transactions",
    )
    assert len(decisions) == expected


def test_ac12_obs_enriched_handoff_transition_passes_event_schema():
    from fno.events import _build, validate

    event = _build("provider_handoff_transition", "daemon", {
        "node": "x-abcd",
        "outage_epoch": "1",
        "provider": "openai",
        "account": "codex-work",
        "source_provider": "zai",
        "source_account": "acct-a",
        "phase": "committed",
        "count": 8,
        "attempt": "attempt-1",
    })
    assert validate(event) is None
    assert watchdog.handoff_armed(SimpleNamespace(
        autonomy=SimpleNamespace(enabled=True),
        recovery=SimpleNamespace(enabled=False, watchdog="handoff"),
    )) is False


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
        Row("cccc3333-0000", "alone", "working", "x-done", "/wt/solo"),
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


def test_the_second_spelling_of_blocked_reaches_the_quorum_hold():
    """claude spells one state two ways and the lane knew only one.

    `busy` was folded onto working by hand; its sibling `needs input` was
    missed, so a row wearing it could bypass the explicit provider-quorum
    hold. The fold now comes from the harness map, so this pins the behaviour
    rather than the table.
    """
    from fno.agents.harnesses.claude import _LIVE_STATUS_INPUT

    # Every input spelling claude maps onto "Needs input" must reach the
    # quorum hold. Enumerating the map is the point: a new spelling added
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
        assert v.verdict == LEAVE, f"{raw} never reached the quorum hold"
        assert "provider quorum" in v.basis


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
        Row("aaaa1111-0000", "quiet", "working", "x-done", "/wt/one"),
        Row("bbbb2222-0000", "live-but-reads-stopped", "stopped", "x-done", "/wt/one"),
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
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo")
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
    row = Row("aaaa1111-0000", "w1", "working", "x-done", "/wt/solo")
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
