"""Which lane answers a row whose REAP read did not answer.

`_verdict_one` used to return STALE on REAP_UNKNOWN above the retire block, so
a row with no pid and no heartbeat - the only reading `_probe_liveness` can
give such a row, and 55 of 55 rows on the machine this was measured on - was
reported instead of stopped, even when its node was done AND merged. Retire is
strictly weaker than reap (a `stop` that `fno agents resume` undoes, never an
`rm`) and carries its own probe and state guards, so being asked first inherits
none of reap's protection.

Its own module because `test_agents_watchdog.py` is over the file budget and
may only shrink. These tests answer one question, so they read better here than
appended to a 5,700-line file.
"""
from __future__ import annotations

from datetime import datetime, timezone

from fno.agents import watchdog
from fno.agents.watchdog import Row, TailFacts, verdicts

NOW_1840 = datetime(2026, 8, 16, 18, 40, 0, tzinfo=timezone.utc).timestamp()
FINISHED_TAIL = "<promise>PR is green and reviewed</promise>"
STALE_MESSAGE_STAMP = "2026-08-15T00:00:00Z"
RETIRE_GRACE = 900


def _facts(text: str, age_min: float = 5, role: str = "assistant",
           kind: str = "text") -> TailFacts:
    epoch = NOW_1840 - age_min * 60
    return TailFacts([(epoch, text)], epoch, text, role, text, kind)


def _run(rows, transcripts, *, nodes=None, grace=RETIRE_GRACE, node_state=None):
    return verdicts(
        rows,
        transcript_for=lambda sid: transcripts.get(sid),
        claim_for=lambda node: {},
        node_state_for=node_state or (lambda node: (nodes or {}).get(node)),
        now_s=NOW_1840,
        retire_grace_s_value=grace,
    )


def test_a_reap_that_did_not_answer_no_longer_preempts_retire():
    """The measured defect: 4 of 4 shipped-basis rows answered retire=True from
    `retire_decision` and `stale` from `_verdict_one`."""
    row = Row("eeee5555-0000", "t-shipped", "idle", "x-1234", "/tmp/bp",
              "spawn", STALE_MESSAGE_STAMP)
    [v] = _run(
        [row],
        {row.row_id: _facts(FINISHED_TAIL, age_min=20)},
        node_state=lambda node: {"status": "done", "merge_status": "merged"},
    )
    assert v.verdict == watchdog.RETIRE
    assert v.action == "stop"


def test_a_reap_that_did_not_answer_still_reports_stale_when_retire_declines():
    """Retire answering first must not downgrade the non-answer to `leave`.
    A read that did not answer is a different fact from a row read and found
    healthy, and it stays a human's to resolve."""
    def raises(node):
        raise RuntimeError("graph unreadable (OSError('boom'))")

    row = Row("eeee5555-0001", "t-working", "working", "x-1234", "/tmp/bp",
              "spawn", STALE_MESSAGE_STAMP)
    [v] = _run(
        [row],
        {row.row_id: _facts("still working", age_min=20)},
        node_state=raises,
    )
    assert v.verdict == watchdog.STALE
    assert v.action == "report"
    assert "unreadable" in v.basis


def test_an_unreadable_claims_root_reaches_the_verdict_as_a_failed_read(
    monkeypatch,
):
    """The production seam, end to end. `_claim_view` used to swallow the
    exception and answer `{}`, which reads as "no claim holds this node" - so
    `reap_decision`'s documented UNKNOWN-on-a-raise contract held only for
    seams a test injected."""
    from fno.claims import core as claims_core

    def boom(*args, **kwargs):
        raise OSError("claims root gone")

    monkeypatch.setattr(claims_core, "claim_status", boom)
    view = watchdog._claim_view("x-1234")
    assert isinstance(view, watchdog._Unreadable)

    rows = [Row("eeee5555-0002", "t-claimless", "working", "x-1234", "/tmp/bp",
                "spawn", STALE_MESSAGE_STAMP)]
    payload, _ = watchdog.run_sweep(
        now_s=NOW_1840,
        rows_provider=lambda: (rows, []),
        transcript_fn=lambda sid: _facts("still working", age_min=20),
        claim_fn=lambda node: view,
        graph_fn=lambda: {},
        provider_outage_fn=lambda: {},
    )
    [verdict] = payload["verdicts"]
    assert verdict["verdict"] == watchdog.STALE
    assert "unreadable" in verdict["basis"]


def test_a_shared_worktree_row_probed_alive_or_unknown_is_refused_and_names_it():
    """The probe gate, named (x-ad13): on the same shared tree, a row the
    probe reports ALIVE refuses as NO, and a row the probe cannot answer
    refuses as UNKNOWN. Neither is ever a YES; the basis names which.

    Asked of `reap_decision` itself, because the folded verdict stopped
    measuring this gate for the unknown case once retire moved above reap's
    non-answer. That row now RETIREs - a stop the row and the shared worktree
    both survive - and the destructive lane still refused it, which is what
    this test is about. The folded assertion below pins the harmless half:
    neither probe reading ever reaches `stop+rm`.
    """
    facts = _facts(FINISHED_TAIL, age_min=30)
    nodes = {"x-done": {"status": "done"}}
    for probe, answer_expected, verdict_expected in (
        ("alive", watchdog.REAP_NO, "leave"),
        ("unknown", watchdog.REAP_UNKNOWN, "retire"),
    ):
        row = Row("aaaa1111-0000", "quiet", "working", "x-done", "/wt/x-bcb5",
                  origin="spawn", last_message_at=STALE_MESSAGE_STAMP,
                  probe=probe)
        answer, basis = watchdog.reap_decision(
            row,
            facts=facts,
            node_state_for=lambda node: nodes.get(node),
            claim_for=lambda node: {},
            now_s=NOW_1840,
            quiet_after_s=watchdog.REAP_QUIET_AFTER_S,
        )
        assert answer == answer_expected, (probe, answer, basis)
        assert "liveness probe" in basis, (probe, basis)
        assert probe in basis
        [v] = _run([row], {row.row_id: facts}, nodes=nodes)
        assert v.verdict == verdict_expected, (probe, v.verdict, v.basis)
        assert v.action != "stop+rm", (probe, v.action)
