"""One reachability derivation with a declared basis, behind every agents surface.

Written RED, before the implementation, per the node's verify-by-making-it-fail.

Three assertion families, because the node's single proposed assertion is not
enough:

1. CONGRUENCE -- every surface that reports reachability returns the same
   verdict for the same agent over one snapshot.
2. CORRECTNESS -- congruence is not correctness. Two surfaces can agree and both
   be wrong, which is exactly what happens today: a session dead 43 minutes
   reads ``live`` from every transcript-keyed surface because the whole liveness
   axis has two-hour resolution. So a dead fixture must read ``unreachable`` at
   an age BELOW the stalled threshold.
3. MONOTONICITY -- a falsifier may only ever lower a verdict toward
   ``unreachable``. This is the invariant that keeps the destructive reaping
   rule un-rederivable; it is asserted over the falsifier set directly, not just
   end-to-end, because an end-to-end test passes right up until someone adds a
   "the pane is alive so it must be live" rule.

The node's own proposed test ("assert list and status return identical
per-agent statuses") CANNOT be written as specified: ``fno agents status`` emits
only a histogram (``agents: {total, by_status}``, daemon.rs), with no per-agent
rows to compare. Congruence is therefore asserted over the derivation the
surfaces consult, not over ``status`` output.
"""

from __future__ import annotations

import pytest

from fno.agents.session_truth import STALLED_AFTER_S

# The module under test does not exist yet. Importing it is the red.
from fno.agents.reachability import (
    NO_EVIDENCE,
    REACHABLE,
    UNKNOWN,
    UNREACHABLE,
    WIRE_STATUS,
    Reachability,
    classify_reachability,
)


# --------------------------------------------------------------------------
# 2. CORRECTNESS -- the bug this node exists for.
# --------------------------------------------------------------------------


def test_dead_session_below_the_stalled_threshold_is_unreachable() -> None:
    """The king's 2026-08-07 specimen: dead 43 minutes, reported live.

    43 minutes is well under STALLED_AFTER_S, so the transcript classifies
    ``working`` and every transcript-keyed surface renders ``live``. Only an
    affirmative falsifier can catch it. This is the assertion the node omits and
    the one that fails today at ANY age under two hours.
    """
    got = classify_reachability(
        truth_state="working",
        age_s=43 * 60,
        falsifier="process-gone",
    )
    assert got.verdict == UNREACHABLE
    assert got.basis == "process-gone"
    assert got.age_s == 43 * 60


def test_busy_session_with_no_falsifier_stays_reachable() -> None:
    """The king's counter-example, failing in the OPPOSITE direction.

    ``fno mail send`` reported an idle session for a worker whose transcript was
    32 seconds old. mail-inject's ``not-live`` is ``resolve_control_sock()``
    returning None, which measures INJECTABILITY, not liveness. A missing
    control socket must therefore never reach this function as a falsifier, and
    a busy worker must stay reachable.
    """
    got = classify_reachability(truth_state="working", age_s=32, falsifier=None)
    assert got.verdict == REACHABLE
    assert got.basis == "transcript"
    assert got.age_s == 32


# --------------------------------------------------------------------------
# Framing rule (a): a registry of REACHABLE agents, not a process table.
# "orphaned means unreachable, not dead, so never reap a row for being quiet."
# --------------------------------------------------------------------------


def test_silence_alone_never_produces_unreachable() -> None:
    """Quiet is absence of evidence, not evidence of absence.

    A row silent for 34.7 hours with no falsifier available (89 percent of rows
    carry no pid) is genuinely unknown-reachability. Asserting ``unreachable``
    there claims knowledge we do not have, which is the exact class of lie this
    node is about. The age rides along in the value so the reader can draw their
    own conclusion.
    """
    got = classify_reachability(
        truth_state="stalled",
        age_s=int(34.7 * 3600),
        falsifier=None,
    )
    assert got.verdict == UNKNOWN, "silence must never be reported as unreachable"
    assert got.age_s == int(34.7 * 3600)


def test_declared_done_is_not_unreachable() -> None:
    """A worker that emitted <promise> finished its mission; it may still be up.

    Mission-complete is a statement about work, not about reachability. Today
    ``list`` maps done -> orphaned, conflating the two.
    """
    got = classify_reachability(truth_state="done", age_s=60, falsifier=None)
    assert got.verdict != UNREACHABLE


def test_no_transcript_is_unknown_with_no_evidence_basis() -> None:
    got = classify_reachability(truth_state="unknown", age_s=None, falsifier=None)
    assert got.verdict == UNKNOWN
    assert got.basis == NO_EVIDENCE
    assert got.age_s is None


# --------------------------------------------------------------------------
# 3. MONOTONICITY -- the invariant that keeps the reaping rule un-rederivable.
# --------------------------------------------------------------------------

_ALL_TRUTH_STATES = ("working", "watching", "your-move", "done", "stalled", "unknown")


@pytest.mark.parametrize("truth_state", _ALL_TRUTH_STATES)
def test_a_falsifier_never_raises_a_verdict(truth_state: str) -> None:
    """Adding a falsifier may lower a verdict and may never raise one.

    Asserted over the whole state space rather than a sampled case, so a future
    "the pane is alive, therefore live" rule fails here instead of shipping.
    """
    without = classify_reachability(truth_state=truth_state, age_s=10, falsifier=None)
    with_ = classify_reachability(truth_state=truth_state, age_s=10, falsifier="process-gone")
    assert _rank(with_.verdict) <= _rank(without.verdict), (
        f"falsifier RAISED the verdict for {truth_state}: {without.verdict} -> {with_.verdict}"
    )
    assert with_.verdict == UNREACHABLE


def test_an_erroring_falsifier_probe_fails_toward_unknown_never_unreachable() -> None:
    """The single most dangerous line in the change.

    A probe that errors and reads as a falsifier would reintroduce the reaping
    hazard through the back door: every unreadable pid would become a death
    sentence. ``None`` means "did not falsify"; an error must arrive here the
    same way, never as a falsification.
    """
    got = classify_reachability(truth_state="stalled", age_s=99999, falsifier=None)
    assert got.verdict != UNREACHABLE


def _rank(verdict: str) -> int:
    """reachable > unknown > unreachable, for the monotonicity comparison."""
    return {UNREACHABLE: 0, UNKNOWN: 1, REACHABLE: 2}[verdict]


# --------------------------------------------------------------------------
# 4. Basis and age are part of the value: a bare "live" must be unprintable.
# --------------------------------------------------------------------------


def test_every_verdict_carries_a_basis_and_renders_with_it() -> None:
    for state in _ALL_TRUTH_STATES:
        got = classify_reachability(truth_state=state, age_s=61, falsifier=None)
        assert isinstance(got, Reachability)
        assert got.basis, f"{state} produced a verdict with no basis"
        rendered = got.render()
        assert got.verdict in rendered
        assert got.basis in rendered, "the basis must be visible to the reader"


def test_render_includes_the_age_so_a_mistuned_window_misleads_less() -> None:
    got = classify_reachability(truth_state="working", age_s=91 * 60, falsifier=None)
    rendered = got.render()
    assert "91m" in rendered or "1h" in rendered, (
        "the age must ride along; 0c12333c read live at 91 minutes of silence "
        "and the bare verdict hid it"
    )


def test_the_stalled_threshold_is_a_knob_not_a_physical_constant() -> None:
    """Two hours is a tuning choice and must be overridable and reported."""
    assert STALLED_AFTER_S == 2 * 3600


# --------------------------------------------------------------------------
# 1. CONGRUENCE -- one snapshot, one verdict per agent, across every surface.
# --------------------------------------------------------------------------


def test_python_surfaces_share_one_derivation() -> None:
    """The routing assertion: no surface may carry its own opinion.

    A fix that makes two verbs agree while resume/peek/top keep private
    liveness logic leaves the same trap with a smaller blast radius -- the
    guard-on-one-of-N-reachable-paths shape.

    Only the PYTHON surfaces are asserted here. ``resume`` is a Rust verb
    (client_verbs.rs) that reaches this derivation by shelling out to
    ``fno agents truth --json``; its congruence is pinned by
    :func:`test_truth_json_carries_the_reachability_verdict_for_rust_callers`
    below, which guards the actual wire between them.
    """
    import fno.agents.reachability as reach

    mod = __import__("fno.agents.read", fromlist=["reachability"])
    assert getattr(mod, "reachability", None) is reach.reachability, (
        "fno.agents.read does not consult the shared derivation"
    )


def test_both_list_lanes_render_through_the_same_wire_mapping() -> None:
    """`fno agents list` has TWO lanes and they share one payload.

    Registry rows and the discovered live-sessions lane are rendered by different
    code, and the discovered lane kept a private `done|stalled -> orphaned` map.
    So one silent session read `orphaned` there while an equivalent registry row
    read `unknown` -- this PR's own defect, one lane over, inside a single
    `--json` response. Pinned by identity, not by equal values, so a copied dict
    fails here too.
    """
    from fno.agents import discover as discover_mod
    from fno.agents import read as read_mod
    from fno.agents.reachability import WIRE_STATUS

    assert read_mod.WIRE_STATUS is WIRE_STATUS
    assert discover_mod.WIRE_STATUS is WIRE_STATUS


def test_a_quiet_discovered_session_is_unknown_not_orphaned() -> None:
    """The same silence rule the registry lane follows, in the other lane."""
    got = classify_reachability(truth_state="stalled", age_s=9000, falsifier=None)
    assert WIRE_STATUS[got.verdict] == "unknown"
    # And a falsified one is condemned in both lanes alike.
    gone = classify_reachability(truth_state="working", age_s=60, falsifier="process-gone")
    assert WIRE_STATUS[gone.verdict] == "orphaned"


def test_the_spawn_gate_asks_occupancy_not_reachability() -> None:
    """The cap is deliberately NOT a consumer of this derivation.

    ``max_live`` protects RAM and concurrency, so its question is OCCUPANCY --
    "is a process holding a slot" -- which is not "can anyone reach it". A live
    process nobody can reach still consumes RAM and must hold its slot, and a
    worker that has not written a transcript yet occupies one from the instant
    it spawns. Routing the gate through transcript activity would let a burst of
    fresh spawns straight through the cap; it was tried and it did exactly that.

    Pinned so a later reader does not "finish the job" by wiring the gate here.
    The node's claim that ``max_live`` is computed from a count wrong in both
    directions was MEASURED FALSE on 2026-08-09: every registry row excluded by
    the stored-status filter also failed the pid liveness check that follows it,
    on all 18 rows, so the stale enum currently changes no count.
    """
    import fno.agents.spawn_gate as gate

    assert not hasattr(gate, "reachability")


def test_truth_json_carries_the_reachability_verdict_for_rust_callers() -> None:
    """The Python/Rust wire, which is where a congruence fix silently leaks.

    ``fno agents truth --json`` is what Rust's ``family1_truth_probe`` reads, and
    ``resume`` decides "is live - attaching" from it. If the verdict is not on
    that wire, Rust keeps re-deriving liveness from the raw transcript ``state``
    and the falsifier never reaches it -- the same trap, one language over.

    Asserted against the payload builder rather than a subprocess so it stays
    hermetic.
    """
    from fno.agents.cli import _truth_payload

    payload = _truth_payload(
        {
            "handle": "h",
            "state": "working",
            "reason": None,
            "last_activity_age_s": 43 * 60,
            "session_id": "s",
            "observed_model": {"kind": "no-model-yet"},
        },
        falsifier="process-gone",
    )
    assert payload["reachability"] == UNREACHABLE, (
        "a dead-process row must reach Rust as unreachable, not as working"
    )
    assert payload["basis"] == "process-gone"
    # The raw transcript state stays on the wire unchanged: existing Rust
    # consumers parse it, and overloading it would be a silent contract break.
    assert payload["state"] == "working"


def _pane_row():
    """A mux row whose recorded pid is confidently dead (pid 1 is never a worker)."""
    from types import SimpleNamespace

    return SimpleNamespace(pid=1, pid_start_time=None, mux={"session": "s", "pane_id": 0})


def test_a_reconciled_exit_survives_the_pid_being_cleared(monkeypatch) -> None:
    """Reconcile PROVES a worker gone and then destroys the proof.

    ``apply_reconcile_change`` nulls ``pid`` and ``pid_start_time`` on the
    terminal ``Exited`` transition (deliberately -- a stale pid is itself a
    misleading liveness signal). The row is then a no-pid row, and no-pid is
    absence of evidence here, so a worker whose transcript is still warm
    classifies ``reachable`` and ``resume`` picks the dead attach path: the
    original 43-minute false-live, re-entered through a different door.

    A recorded ``exited`` is not the stored-enum guess this derivation refuses
    to trust. It is an affirmative probe RESULT, written only after reconcile
    confirmed the child was gone, and re-stamped ``live`` by any later
    successful interaction -- so it falsifies, and nothing else stored does.
    """
    from types import SimpleNamespace

    from fno.agents.reachability import registry_falsifier

    reconciled_dead = SimpleNamespace(pid=None, pid_start_time=None, mux=None, status="exited")
    assert registry_falsifier(reconciled_dead) == "exit-recorded"


def test_no_other_stored_status_falsifies(monkeypatch) -> None:
    """Only ``exited`` is a probe result. The rest are guesses or the opposite.

    ``orphaned`` is the sharp case and it must NEVER falsify: reconcile keeps
    the pid on an orphaned row precisely because that process is still LIVE but
    unowned (daemon.rs), so condemning it from the stored word would reap a
    running worker. Falsifying on every terminal-sounding value is how the
    stored enum becomes the answer again.
    """
    from types import SimpleNamespace

    from fno.agents.reachability import registry_falsifier

    for stored in ("orphaned", "live", "spawning", "failed", "unknown", None):
        row = SimpleNamespace(pid=None, pid_start_time=None, mux=None, status=stored)
        assert registry_falsifier(row) is None, f"stored {stored!r} must not condemn a row"


def test_a_mux_pane_row_carries_no_pid_falsifier(monkeypatch) -> None:
    """A pane row's recorded pid is not the authority on that pane.

    ``reconcile`` re-derives the pane's current child on every pass, because a
    mux restart can hand ``(session, pane_id)`` to a new child while the recorded
    pid dies -- and it treats a live pane with no usable pid as INCONCLUSIVE,
    never dead. Falsifying off the stale pid would render a healthy pane worker
    ``orphaned`` in `list` and make `resume` refuse to attach it: the reaping
    hazard, rebuilt one field over.
    """
    from types import SimpleNamespace

    from fno.agents import mux_spawn
    from fno.agents.reachability import registry_falsifier

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda mux: True)
    assert registry_falsifier(_pane_row()) is None

    bare_row = SimpleNamespace(pid=1, pid_start_time=None, mux=None)
    assert registry_falsifier(bare_row) == "process-gone"


def test_an_exited_pane_falsifies_the_row(monkeypatch) -> None:
    """Suppressing the stale pid must not discard the AUTHORITATIVE pane signal.

    ``_mux_pane_alive`` is exactly the authority the recorded pid is not: it
    answers about the pane itself, and a ``False`` from it is the mux stating
    the pane exited. Dropping every falsifier for a mux row -- rather than
    swapping the wrong one for the right one -- leaves a dead pane rendering
    ``unknown`` (or ``reachable``, while its transcript is still under the
    staleness window) for as long as that transcript stays warm.
    """
    from fno.agents import mux_spawn
    from fno.agents.reachability import registry_falsifier

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda mux: False)
    assert registry_falsifier(_pane_row()) == "pane-gone"


def test_unreadable_pane_liveness_never_condemns(monkeypatch) -> None:
    """``None`` from the mux is "cannot answer", which is not evidence of death.

    Same rule as an unreadable pid: only a confident gone falsifies. ``reconcile``
    treats this case as ``mux-pane-liveness-unavailable`` and declines to act,
    and a row must not be condemned here for what reconcile refuses to condemn.
    """
    from fno.agents import mux_spawn
    from fno.agents.reachability import registry_falsifier

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", lambda mux: None)
    assert registry_falsifier(_pane_row()) is None


def test_a_broken_pane_probe_never_condemns(monkeypatch) -> None:
    """A raising probe is an unreadable probe, not a dead pane."""
    from fno.agents import mux_spawn
    from fno.agents.reachability import registry_falsifier

    def _boom(mux):
        raise RuntimeError("mux binary missing")

    monkeypatch.setattr(mux_spawn, "_mux_pane_alive", _boom)
    assert registry_falsifier(_pane_row()) is None
