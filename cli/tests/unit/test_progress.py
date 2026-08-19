"""``classify_progress`` -- the orthogonal axis beside reachability.

Written RED, before the implementation. See the plan's vocabulary table:
reachability answers "can I reach this process"; progress answers "is it
taking its own next turn, waiting on the operator, parked, or refused".

Precedence, per the plan: a falsified/unreachable row is tested first, then
the refusal predicate (Locked Decision 3), then the truth-state arms. A
refused worker emits exactly one assistant message and stops, so
``classify_tail`` reads it as ``working`` for two hours -- the refusal test
has to run before the truth-state arms or the axis reports ``advancing`` for
the exact row it exists to catch.
"""

from __future__ import annotations

# The module under test does not exist yet at import time (RED).
from fno.agents.reachability import (
    ADVANCING,
    AWAITING_OPERATOR,
    MODEL_REFUSED,
    NO_EVIDENCE,
    OPERATOR_TURN,
    PARKED,
    PROGRESS_UNKNOWN,
    PROMISE,
    REACHABLE,
    REFUSED,
    SILENT,
    TRANSCRIPT_TURN,
    UNREACHABLE,
    Progress,
    classify_reachability,
    classify_progress,
)
from fno.agents.session_truth import STALE_ATTENTION_S

_REFUSED_MODEL = {"kind": "observed", "model": "glm-5.2[1m]"}


def _refused_kwargs(**overrides) -> dict:
    kwargs = dict(
        truth_state="working",
        reachability=REACHABLE,
        observed_model=_REFUSED_MODEL,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=1,
    )
    kwargs.update(overrides)
    return kwargs


def test_ac1_done_and_reachable_is_parked() -> None:
    got = classify_progress(
        truth_state="done",
        reachability=REACHABLE,
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=1,
    )
    assert got == Progress(PARKED, PROMISE)


def test_ac2_working_and_reachable_is_advancing() -> None:
    got = classify_progress(
        truth_state="working",
        reachability=REACHABLE,
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=1,
    )
    assert got == Progress(ADVANCING, TRANSCRIPT_TURN)


def test_ac2b_your_move_is_awaiting_operator() -> None:
    got = classify_progress(
        truth_state="your-move",
        reachability=REACHABLE,
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=1,
    )
    assert got == Progress(AWAITING_OPERATOR, OPERATOR_TURN)


def test_ac3_refusal_outranks_the_active_truth_state() -> None:
    got = classify_progress(**_refused_kwargs())
    assert got == Progress(REFUSED, MODEL_REFUSED)


def test_ac4_a_routed_worker_is_never_condemned() -> None:
    got = classify_progress(
        **_refused_kwargs(route_settings_path="/x/route-settings/ab12.json")
    )
    assert got != Progress(REFUSED, MODEL_REFUSED)
    assert got == Progress(ADVANCING, TRANSCRIPT_TURN)


def test_ac5_unmeasured_observed_model_kinds_never_refuse() -> None:
    for kind in ("no-transcript", "not-file-backed", "no-model-yet", "unreadable"):
        got = classify_progress(**_refused_kwargs(observed_model={"kind": kind}))
        assert got.verdict != REFUSED, f"kind={kind} must never refuse"


def test_ac6_stalled_is_unknown_silent_never_parked() -> None:
    got = classify_progress(
        truth_state="stalled",
        reachability=REACHABLE,
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=STALE_ATTENTION_S + 1,
    )
    assert got == Progress(PROGRESS_UNKNOWN, SILENT)
    assert got.verdict != PARKED


def test_deliberately_wedged_open_turn_is_reachable_but_not_advancing() -> None:
    """A live process with an open turn and no output must not look healthy.

    ``working`` is the transcript-tail shape for an open turn until the
    two-hour reap-safety threshold expires. The ten-minute activity age is the
    positive-progress boundary: beyond it, the worker is still reachable but
    there is no evidence that it is advancing.
    """
    age_s = STALE_ATTENTION_S + 1
    reach = classify_reachability(
        truth_state="working",
        age_s=age_s,
        falsifier=None,
    )
    got = classify_progress(
        truth_state="working",
        reachability=reach.verdict,
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=age_s,
    )
    assert reach.verdict == REACHABLE
    assert got == Progress(PROGRESS_UNKNOWN, SILENT)


def test_unreadable_activity_age_is_unknown_never_advancing() -> None:
    got = classify_progress(
        truth_state="working",
        reachability=REACHABLE,
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=None,
    )
    assert got == Progress(PROGRESS_UNKNOWN, NO_EVIDENCE)


def test_ac7_unreachable_is_unknown_no_evidence_regardless_of_truth_state() -> None:
    for state in ("working", "done", "your-move", "stalled", None, "unknown"):
        got = classify_progress(
            truth_state=state,
            reachability=UNREACHABLE,
            observed_model=None,
            harness="claude",
            route_settings_path=None,
            last_activity_age_s=1,
        )
        assert got == Progress(PROGRESS_UNKNOWN, NO_EVIDENCE), state


def test_ac8_the_verdict_never_reads_the_refusal_prose() -> None:
    """No parameter carries the transcript's last-message text at all.

    Proven structurally: the same call, issued twice, is deterministic, and
    the function signature has no place for prose to enter.
    """
    import inspect

    params = inspect.signature(classify_progress).parameters
    assert "last_message" not in params
    assert "message" not in params

    kwargs = _refused_kwargs()
    assert classify_progress(**kwargs) == classify_progress(**kwargs)


def test_ac9_no_transcript_evidence_is_unknown_no_evidence() -> None:
    got = classify_progress(
        truth_state=None,
        reachability="unknown",
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=None,
    )
    assert got == Progress(PROGRESS_UNKNOWN, NO_EVIDENCE)

    got_unknown_state = classify_progress(
        truth_state="unknown",
        reachability="unknown",
        observed_model=None,
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=None,
    )
    assert got_unknown_state == Progress(PROGRESS_UNKNOWN, NO_EVIDENCE)


def test_ac18_a_missing_transcript_never_yields_refused_or_parked() -> None:
    """Task 4: absence is not a verdict, restated for the progress axis.

    A handle with no transcript resolves ``truth_state=None`` upstream
    (:mod:`fno.agents.session_truth`), which must land here as
    ``unknown``/``no-evidence`` -- never a refusal and never ``parked``.
    """
    got = classify_progress(
        truth_state=None,
        reachability="unknown",
        observed_model={"kind": "no-transcript"},
        harness="claude",
        route_settings_path=None,
        last_activity_age_s=None,
    )
    assert got == Progress(PROGRESS_UNKNOWN, NO_EVIDENCE)


def test_non_claude_harness_is_never_refused() -> None:
    """The predicate is claude-only by construction (registry.py:365-366)."""
    got = classify_progress(**_refused_kwargs(harness="codex"))
    assert got.verdict != REFUSED


def test_anthropic_model_id_is_never_refused() -> None:
    got = classify_progress(
        **_refused_kwargs(
            observed_model={"kind": "observed", "model": "claude-opus-4-1"}
        )
    )
    assert got.verdict != REFUSED
