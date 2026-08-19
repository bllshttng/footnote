"""x-cdca: a pane spawn must not report "a pane exists" as "a worker is running".

The defect: ``dispatch_spawn_pane`` created a codex pane, failed to bind a
session within a 3.0s window, and returned an exit-0 receipt carrying
``status: spawning`` and an empty ``short_id``. A pane that would bind in four
seconds and a pane that had already died produced BYTE-IDENTICAL receipts, so
a caller had no way to tell a slow starter from a corpse -- and re-prompted the
corpse.

Coverage:
  - the three outcomes of ``_await_pane_binding`` and the exit code each earns
  - ambiguity resolves to still-booting, never to died (the asymmetry rule)
  - death evidence is captured BEFORE the pane is gone, and never fails a spawn
  - the receipt invariant ``bound == bool(short_id)`` on claude/codex
  - a regression pin: the production codex binding path never keys on rollout
    mtime (the misattribution the node reported, whose code is already gone)
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from fno.agents import mux_spawn
from fno.agents.mux_spawn import (
    MuxSpawnResult,
    _submit_spawn_seed,
    _await_pane_binding,
    _backfill_codex_session_id,
    _write_pane_death_log,
)

MUX = {"session": "main", "pane_id": 81}
SID = "019cc081-de0d-7283-97cc-751c46742a07"
AGY_HARNESS = "agy"

#: `fno mux pane wait --timeout 0` exit codes: 12 = the child exited
#: (EXIT_WAIT_EXITED, a POSITIVE death marker), 0/11 = still up, anything else
#: = the mux could not answer.
WAIT_DEAD = 12
WAIT_ALIVE = 0
WAIT_UNKNOWN = 3


def _proc(returncode: int = 0, stdout: str = "") -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr="")


def _runner(*, wait_rc: int = WAIT_ALIVE, tail: str = "", read_rc: int = 0, raises=None):
    """A fake `fno mux` answering every verb the binding loop calls.

    The `ls` arm is EXPLICIT and lists our pane on purpose. An earlier version
    fell through to `_proc(0)` with empty stdout, so `json.loads("")` raised and
    the absence probe returned None - which made the ambiguity tests pass for a
    reason the real CLI never produces (it answers exit 0 with `[]`). A fake that
    supplies the answer the code is waiting for is how this PR shipped a broken
    death detector twice; the fake must model the real verb, not a convenient one.
    """
    calls: list[list[str]] = []

    def run(argv, **_kw):
        calls.append(argv)
        # argv is [fno, "mux", "pane", <verb>, ...]
        verb = argv[3] if len(argv) > 3 else ""
        if verb == "wait":
            return _proc(wait_rc)
        if verb == "read":
            if raises is not None:
                raise raises
            return _proc(read_rc, tail)
        if verb == "ls":
            return _proc(0, json.dumps([{"pane_id": MUX["pane_id"]}, {"pane_id": 99}]))
        return _proc(0)

    run.calls = calls  # type: ignore[attr-defined]
    return run


def _no_sleep(_seconds: float) -> None:
    """Collapse the poll delay so tests do not pay it."""


# ---------------------------------------------------------------------------
# The three outcomes
# ---------------------------------------------------------------------------


def test_binding_returns_immediately_without_sleeping() -> None:
    """A healthy spawn pays none of the window: the loop exits on the first tick."""
    slept: list[float] = []
    out = _await_pane_binding(
        MUX, lambda: SID, runner=_runner(), sleep=slept.append, window_s=999.0
    )
    assert out.session_id == SID
    assert out.pane_alive is True
    assert out.reason == ""
    assert slept == [], "a bound worker must not sleep out the window"


def test_confirmed_dead_pane_is_the_died_outcome() -> None:
    out = _await_pane_binding(
        MUX,
        lambda: None,
        runner=_runner(wait_rc=WAIT_DEAD, tail="error: unexpected argument\n"),
        sleep=_no_sleep,
        window_s=999.0,
    )
    assert out.session_id is None
    assert out.pane_alive is False
    assert out.reason == "pane-died-before-binding"
    assert "unexpected argument" in out.tail


def test_window_expiry_with_a_live_pane_is_still_booting() -> None:
    out = _await_pane_binding(
        MUX, lambda: None, runner=_runner(wait_rc=WAIT_ALIVE), sleep=_no_sleep, window_s=0.0
    )
    assert out.session_id is None
    assert out.pane_alive is True
    assert out.reason == "binding-window-expired"


# ---------------------------------------------------------------------------
# The asymmetry rule: ambiguity resolves to still-booting, never to died.
# A false "died" kills a working worker; a false "still booting" costs a retry.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("wait_rc", [WAIT_UNKNOWN, 1, 127])
def test_an_unanswerable_wait_never_reads_as_death(wait_rc: int) -> None:
    """`wait` cannot answer, but the listing names our pane, so it is alive.

    The invariant is that an ambiguous exit code never becomes death - not that
    it becomes None. Falling back to a listing that PROVES the pane is present
    is strictly better information than a shrug.
    """
    out = _await_pane_binding(
        MUX, lambda: None, runner=_runner(wait_rc=wait_rc), sleep=_no_sleep, window_s=0.0
    )
    assert out.reason == "binding-window-expired"
    assert out.pane_alive is not False, "an unanswerable probe must never condemn"
    assert out.pane_alive is True


@pytest.mark.parametrize("wait_rc", [WAIT_UNKNOWN, 1, 127])
def test_neither_signal_answering_resolves_to_unknown(wait_rc: int) -> None:
    """When the listing cannot answer either, the verdict is None, never death."""
    out = _await_pane_binding(
        MUX,
        lambda: None,
        runner=_ls_runner(wait_rc=wait_rc, panes=[], ls_out="[]"),
        sleep=_no_sleep,
        window_s=0.0,
    )
    assert out.reason == "binding-window-expired"
    assert out.pane_alive is None


def test_a_zero_window_still_buys_exactly_one_probe() -> None:
    """A clamped window must not skip the loop: one look, then a verdict."""
    probes: list[int] = []

    def probe():
        probes.append(1)
        return None

    _await_pane_binding(
        MUX, probe, runner=_runner(wait_rc=WAIT_ALIVE), sleep=_no_sleep, window_s=-5.0
    )
    assert len(probes) == 1


def test_a_pane_that_dies_after_one_poll_is_caught() -> None:
    """The realistic shape: alive on tick 1, gone on tick 2."""
    seq = iter([WAIT_ALIVE, WAIT_DEAD])

    def run(argv, **_kw):
        # argv is [fno, "mux", "pane", <verb>, ...]
        verb = argv[3] if len(argv) > 3 else ""
        if verb == "wait":
            return _proc(next(seq))
        if verb == "read":
            return _proc(0, "booting\n")
        return _proc(0)

    out = _await_pane_binding(
        MUX, lambda: None, runner=run, sleep=_no_sleep, window_s=999.0
    )
    assert out.pane_alive is False
    assert out.reason == "pane-died-before-binding"


# ---------------------------------------------------------------------------
# Evidence: captured before the pane is gone, and never fatal
# ---------------------------------------------------------------------------


def test_a_raising_pane_read_never_fails_the_binding_wait() -> None:
    """Evidence is best-effort; binding is not."""
    out = _await_pane_binding(
        MUX,
        lambda: None,
        runner=_runner(wait_rc=WAIT_DEAD, raises=OSError("mux gone")),
        sleep=_no_sleep,
        window_s=0.0,
    )
    assert out.reason == "pane-died-before-binding"
    assert out.tail == ""


def test_an_empty_later_read_does_not_erase_earlier_evidence() -> None:
    """A TUI that stops painting must not blank the output that explains it."""
    reads = iter(["boom: bad flag\n", "", ""])
    waits = iter([WAIT_ALIVE, WAIT_DEAD])

    def run(argv, **_kw):
        # argv is [fno, "mux", "pane", <verb>, ...]
        verb = argv[3] if len(argv) > 3 else ""
        if verb == "wait":
            return _proc(next(waits))
        if verb == "read":
            return _proc(0, next(reads))
        return _proc(0)

    out = _await_pane_binding(MUX, lambda: None, runner=run, sleep=_no_sleep, window_s=999.0)
    assert "boom: bad flag" in out.tail


def test_death_log_is_written_and_pointed_at(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    path = _write_pane_death_log("worker-x", "error: unexpected argument '--effort'\n")
    assert path
    assert Path(path).read_text().startswith("error: unexpected argument")


def test_an_unwritable_log_dir_degrades_instead_of_raising(tmp_path: Path, monkeypatch) -> None:
    """The spawn still reports the death; it just cannot point at a file."""

    def boom():
        raise OSError("read-only filesystem")

    monkeypatch.setattr("fno.paths.state_dir", boom)
    assert _write_pane_death_log("worker-x", "some output\n") == ""


def test_an_empty_tail_writes_no_log() -> None:
    assert _write_pane_death_log("worker-x", "   \n") == ""


# ---------------------------------------------------------------------------
# The receipt invariant, on BOTH production callers of dispatch_spawn_pane
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("provider", ["claude", "codex"])
def test_bound_equals_short_id_truthiness(provider: str) -> None:
    """For claude and codex, short_id IS the handle, so it cannot disagree with
    `bound`. This is what makes an empty short_id a signal rather than a
    formatting detail."""
    for sid, short in ((SID, "abcd1234"), (None, "")):
        result = MuxSpawnResult(
            name="w",
            provider=provider,
            session="main",
            pane_id=81,
            child_pid=4242,
            session_uuid=sid,
            short_id=short,
            status="live" if sid else "spawning",
            bound=sid is not None,
        )
        assert result.bound == bool(result.short_id)


#: A harness whose transport key is NOT short_id (US8). Named rather than
#: inlined because `MuxSpawnResult.provider` is an older spelling of the harness
#: axis, and a harness literal bound to a `provider=` keyword is the four-axis
#: confusion `scripts/ci/check-axis-vocabulary.sh` exists to catch.
HARNESS_WITHOUT_SHORT_ID = "opencode"


def test_bound_is_keyed_on_the_session_not_on_short_id() -> None:
    """This harness's transport key is not short_id (US8), so an empty short_id
    there is NOT an unbound worker. `bound` reads the session uuid for that
    reason."""
    result = MuxSpawnResult(
        name="w",
        provider=HARNESS_WITHOUT_SHORT_ID,
        session="main",
        pane_id=81,
        child_pid=4242,
        session_uuid=SID,
        short_id="",
        status="live",
        bound=True,
    )
    assert result.bound and not result.short_id


@pytest.mark.parametrize(
    # `harness`, not `provider`: these are binaries, not model vendors.
    "harness,named,expected",
    [
        # The codex branches name their reason precisely and it survives.
        ("codex", "pane-died-before-binding", "pane-died-before-binding"),
        (
            "codex",
            "binding-window-expired",
            "binding-window-expired: pane is live; fno agents reconcile will backfill its session id",
        ),
        ("codex", "no-child-pid-to-correlate", "no-child-pid-to-correlate"),
        # Every OTHER unbound route - an opencode backfill miss, a happy-claude
        # row - names none, and must still not emit a null.
        # A branch that names no reason gets a GENERIC one: naming the harness
        # would read as "this harness binds no session", which is what
        # `bound is None` means and the opposite of a miss.
        ("opencode", None, "unbound-reason-unrecorded"),
        ("claude", None, "unbound-reason-unrecorded"),
    ],
)
def test_every_unbound_receipt_names_a_reason(
    harness: str, named: str | None, expected: str
) -> None:
    """An unbound receipt whose reason is null is the same empty signal as an
    empty short_id. Pinning the IMPLICATION, not a list of branches, so a new
    unbound branch cannot reintroduce a null by forgetting."""
    assert mux_spawn._resolve_unbound_reason(False, named, harness) == expected


def test_a_bound_receipt_never_carries_a_reason() -> None:
    """Including a stale one left over from an earlier probe in the same spawn."""
    assert mux_spawn._resolve_unbound_reason(True, "binding-window-expired", "codex") is None


# ---------------------------------------------------------------------------
# Name reclamation is scoped to PROVED death, not to the terminal vocabulary
# ---------------------------------------------------------------------------


def test_orphaned_is_not_reclaimable_because_it_means_live() -> None:
    """`orphaned` is stamped when a message fails to route - "agent is live but
    not currently routable" - and reconcile keeps the pid because the process is
    still running. Reclaiming one would delete a LIVE worker's row, hiding it
    from `agents list`, reconcile, and lane cleanup while a second pane starts
    on the same node. A control-socket hiccup is not a death."""
    from fno.agents.registry import TERMINAL_STATUSES

    assert "orphaned" in TERMINAL_STATUSES
    assert "orphaned" not in mux_spawn._RECLAIMABLE_STATUSES
    assert mux_spawn._RECLAIMABLE_STATUSES <= TERMINAL_STATUSES
    assert {"exited", "failed", "permanent_dead"} == set(mux_spawn._RECLAIMABLE_STATUSES)


def test_the_timeout_message_names_the_timeout_that_applied() -> None:
    """The binding probes pass 2s; naming the 30s default would be a diagnostics
    lie in the subsystem this change exists to make truthful."""
    from fno.agents.dispatch import DispatchAskError

    def run(_argv, **_kw):
        raise subprocess.TimeoutExpired(cmd="fno", timeout=2.0)

    with pytest.raises(DispatchAskError) as exc:
        mux_spawn._run_mux(["mux", "pane", "ls"], run, timeout=2.0)
    assert "within 2.0s" in str(exc.value)
    assert "30" not in str(exc.value)


# ---------------------------------------------------------------------------
# `bound` is TRI-state: a harness that binds no session asserts nothing
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("harness", ["claude", "codex", "opencode"])
def test_a_session_binding_harness_reports_a_real_boolean(harness: str) -> None:
    assert mux_spawn._resolve_bound(SID, harness) is True
    assert mux_spawn._resolve_bound(None, harness) is False


@pytest.mark.parametrize("harness", ["gemini", "agy"])
def test_a_harness_with_no_spawn_time_session_asserts_nothing(harness: str) -> None:
    """gemini and agy are pane-hostable but expose no session id at all.

    Reporting False would call a healthy worker a failure; reporting True would
    be the same unverified "it's live" this change exists to remove. None says
    the spawn made no claim, and carries no reason because there is no failure
    to explain.
    """
    assert mux_spawn._resolve_bound(None, harness) is None
    assert mux_spawn._resolve_unbound_reason(None, None, harness) is None


def test_the_doubt_field_defaults_to_no_claim() -> None:
    """A field whose job is to carry doubt must not default to certainty: every
    construction that omits it would otherwise assert a bound worker."""
    from dataclasses import fields

    default = next(f for f in fields(MuxSpawnResult) if f.name == "bound").default
    assert default is None


def test_seed_field_defaults_to_no_seed_and_carries_submission() -> None:
    """A missing seed and an unconfirmed seed must never share a receipt."""
    from dataclasses import fields

    default = next(f for f in fields(MuxSpawnResult) if f.name == "seed").default
    assert default is None
    result = MuxSpawnResult(
        name="w",
        provider=AGY_HARNESS,
        session="main",
        pane_id=81,
        child_pid=4242,
        session_uuid=None,
        seed="submitted",
    )
    assert result.seed == "submitted"


@pytest.mark.parametrize("harness", ["claude", "codex", "agy"])
def test_shared_readiness_submits_a_preloaded_seed(harness: str) -> None:
    """Every pane harness reaches the same text-then-submit gate."""
    calls: list[list[str]] = []

    def run(argv, **_kw):
        calls.append(argv)
        verb = argv[3]
        if verb == "wait":
            return _proc(WAIT_ALIVE)
        if verb == "read":
            return _proc(0, "seed text")
        if verb == "send":
            return _proc(0)
        raise AssertionError(argv)

    state, detail, source = _submit_spawn_seed(harness, "main", 81, "seed text", run)
    assert state == "submitted"
    assert detail == ""
    assert source == "preloaded"
    send = next(call for call in calls if call[3] == "send")
    assert "--submit" in send
    assert send[send.index("--text") + 1] == ""


def test_shared_readiness_never_certifies_an_unconfirmed_submit() -> None:
    def run(argv, **_kw):
        verb = argv[3]
        if verb == "wait":
            return _proc(WAIT_ALIVE)
        if verb == "read":
            return _proc(0, "idle composer")
        if verb == "send":
            return _proc(17, stdout="")
        raise AssertionError(argv)

    state, detail, source = _submit_spawn_seed("agy", "main", 81, "seed text", run)
    assert state == "unconfirmed"
    assert detail == "text delivered, submission unconfirmed"
    assert source == "delivered"


def test_shared_readiness_clears_agy_trust_before_seeding() -> None:
    reads = iter(
        [
            "Do you trust the authors of files in this folder?",
            "Antigravity CLI\n>\n",
        ]
    )
    calls: list[list[str]] = []

    def run(argv, **_kw):
        calls.append(argv)
        if argv[3] == "wait":
            return _proc(WAIT_ALIVE)
        if argv[3] == "read":
            return _proc(0, next(reads))
        if argv[3] == "send":
            return _proc(0)
        raise AssertionError(argv)

    state, detail, source = _submit_spawn_seed("agy", "main", 81, "seed text", run)
    assert state == "submitted"
    assert detail == ""
    assert source == "trust-cleared"
    sends = [call for call in calls if call[3] == "send"]
    assert len(sends) == 2
    assert sends[0][sends[0].index("--text") + 1] == ""
    assert sends[1][sends[1].index("--text") + 1] == "seed text"
    assert all("--submit" in call for call in sends)


def test_shared_readiness_waits_for_a_slow_first_paint() -> None:
    reads = iter(["", "Antigravity CLI\n>\n"])

    def run(argv, **_kw):
        if argv[3] == "wait":
            return _proc(WAIT_ALIVE)
        if argv[3] == "read":
            return _proc(0, next(reads))
        if argv[3] == "send":
            return _proc(0)
        raise AssertionError(argv)

    state, detail, source = _submit_spawn_seed("agy", "main", 81, "seed text", run)
    assert (state, detail, source) == (
        "unconfirmed",
        "text delivered, submission unconfirmed",
        "",
    )


def test_shared_readiness_fails_when_agy_trust_does_not_clear() -> None:
    def run(argv, **_kw):
        if argv[3] == "wait":
            return _proc(WAIT_ALIVE)
        if argv[3] == "read":
            return _proc(0, "Do you trust the contents of this project?")
        if argv[3] == "send":
            return _proc(0)
        raise AssertionError(argv)

    state, detail, source = _submit_spawn_seed("agy", "main", 81, "seed text", run)
    assert state == "unconfirmed"
    assert "trust gate did not clear" in detail
    assert source == "trust-cleared"


# ---------------------------------------------------------------------------
# A probe never fails a spawn (the pane already exists by the time it runs)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("boom", [PermissionError("denied"), OSError("ENOMEM"), RuntimeError("x")])
def test_a_raising_liveness_probe_answers_unknown_instead_of_escaping(boom) -> None:
    """An escape here orphans a live pane with no registry row, which is worse
    than every reason the probe might have failed. Held to the same standard as
    the tail read it sits beside."""

    def run(_argv, **_kw):
        raise boom

    assert mux_spawn._mux_pane_alive(MUX, run) is None


# ---------------------------------------------------------------------------
# The reaped-pane case, which is the NORMAL death and the one exit 12 misses
# ---------------------------------------------------------------------------


def _ls_runner(*, wait_rc: int, panes: list, ls_rc: int = 0, ls_out=None):
    """A mux whose `pane wait` answers `wait_rc` and whose `pane ls` lists `panes`."""

    def run(argv, **_kw):
        verb = argv[3] if len(argv) > 3 else ""
        if verb == "wait":
            return _proc(wait_rc)
        if verb == "ls":
            body = ls_out if ls_out is not None else json.dumps(
                [{"pane_id": p} for p in panes]
            )
            return _proc(ls_rc, body)
        if verb == "read":
            return _proc(0, "")
        return _proc(0)

    return run


def test_a_reaped_pane_is_death_even_though_wait_cannot_say_so() -> None:
    """When a pane's child exits the mux drops the pane, so a later `pane wait`
    finds nothing to watch and returns the GENERIC error code 1 - which also
    covers io failure, version skew, and server error. Exit 12 only fires when
    the child dies while a watcher is already subscribed, which a --timeout 0
    probe almost never is. Without the listing fallback the death branch would
    be near-unreachable and a corpse would land as `spawning`."""
    assert mux_spawn._mux_pane_alive(MUX, _ls_runner(wait_rc=1, panes=[99])) is False


def test_a_pane_still_in_the_listing_is_alive() -> None:
    assert mux_spawn._mux_pane_alive(MUX, _ls_runner(wait_rc=1, panes=[81, 99])) is True


@pytest.mark.parametrize(
    "kwargs",
    [
        {"ls_rc": 1},                    # the enumeration itself failed
        {"ls_out": "not json"},          # unparseable
        {"ls_out": '{"panes": []}'},     # not a list
        # THE ONE THAT MATTERS: `fno mux pane ls --json` prints exactly this and
        # exits 0 when the session socket is refused or absent, so an empty list
        # is "I could not reach the session" and "the session is empty" wearing
        # the same bytes. Reading it as death condemns a live worker whenever the
        # mux is briefly unreachable - and this helper's False is the condemn
        # signal for reconcile and reachability.pane_falsifier, not just spawn.
        {"ls_out": "[]"},
    ],
)
def test_an_unsuccessful_enumeration_is_never_read_as_death(kwargs) -> None:
    """The positive control is a NON-EMPTY listing. Without one, "not in the
    list" is an absence with two explanations."""
    assert mux_spawn._mux_pane_alive(MUX, _ls_runner(wait_rc=1, panes=[], **kwargs)) is None


def test_the_positive_control_is_another_pane_not_merely_exit_zero() -> None:
    """A listing naming a DIFFERENT pane proves the server answered with real
    content, which is what makes our pane's absence evidence rather than silence."""
    assert mux_spawn._mux_pane_alive(MUX, _ls_runner(wait_rc=1, panes=[99])) is False


def test_exit_12_still_short_circuits_without_a_listing_call() -> None:
    """The definitive signal must not pay for a second round trip."""
    calls = []

    def run(argv, **_kw):
        calls.append(argv[3] if len(argv) > 3 else "")
        return _proc(WAIT_DEAD)

    assert mux_spawn._mux_pane_alive(MUX, run) is False
    assert "ls" not in calls


def test_the_binding_window_stays_under_the_dispatch_subprocess_budget() -> None:
    """`run_dispatch_one` kills the whole `fno dispatch one` subprocess after
    20s, and that budget also covers process start, selection, and pane
    creation. A window at or near 20s gets the subprocess killed BEFORE the
    registry append, leaving a live pane with no row."""
    assert mux_spawn._BINDING_WINDOW_S <= 10.0


def test_repeat_deaths_do_not_overwrite_each_others_evidence(tmp_path, monkeypatch) -> None:
    """The deaths worth reading are usually repeats, so a name-stable path would
    destroy the run you wanted to compare against."""
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    first = _write_pane_death_log("w", "first corpse\n", pane_id=81)
    second = _write_pane_death_log("w", "second corpse\n", pane_id=84)
    assert first != second
    assert Path(first).read_text() == "first corpse\n"


# ---------------------------------------------------------------------------
# Regression pin for the node's claim 4 (already fixed; this keeps it fixed)
# ---------------------------------------------------------------------------


def test_production_codex_binding_never_keys_on_rollout_mtime(monkeypatch) -> None:
    """The July misattribution came from selecting a codex session by newest
    rollout mtime, which happily adopted an unrelated `codex exec` probe.

    That code is gone: with a child pid, the id is read from a rollout held open
    by the pane's OWN process tree. This pin makes the cwd+time store query
    explode if the production path ever falls back to it again.
    """

    def forbidden(*_a, **_kw):
        raise AssertionError(
            "production codex binding fell back to mtime/cwd store discovery; "
            "it must correlate through the pane's own pid tree"
        )

    monkeypatch.setattr("fno.agents.discover.codex_session_ids_started_in", forbidden)
    monkeypatch.setattr(mux_spawn, "_codex_session_id_for_pid", lambda *_a, **_kw: None)

    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), 0, child_pid=4242, sleep=_no_sleep
        )
        is None
    )
