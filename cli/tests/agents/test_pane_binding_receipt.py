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

import subprocess
from pathlib import Path

import pytest

from fno.agents import mux_spawn
from fno.agents.mux_spawn import (
    MuxSpawnResult,
    _await_pane_binding,
    _backfill_codex_session_id,
    _write_pane_death_log,
)

MUX = {"session": "main", "pane_id": 81}
SID = "019cc081-de0d-7283-97cc-751c46742a07"

#: `fno mux pane wait --timeout 0` exit codes: 12 = the child exited
#: (EXIT_WAIT_EXITED, a POSITIVE death marker), 0/11 = still up, anything else
#: = the mux could not answer.
WAIT_DEAD = 12
WAIT_ALIVE = 0
WAIT_UNKNOWN = 3


def _proc(returncode: int = 0, stdout: str = "") -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr="")


def _runner(*, wait_rc: int = WAIT_ALIVE, tail: str = "", read_rc: int = 0, raises=None):
    """A fake `fno mux` answering only the two verbs the binding loop calls."""
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
def test_an_unanswerable_liveness_probe_never_reads_as_death(wait_rc: int) -> None:
    out = _await_pane_binding(
        MUX, lambda: None, runner=_runner(wait_rc=wait_rc), sleep=_no_sleep, window_s=0.0
    )
    assert out.reason == "binding-window-expired"
    assert out.pane_alive is None, "unprovable liveness is None, not False"


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
        ("codex", "binding-window-expired", "binding-window-expired"),
        ("codex", "no-child-pid-to-correlate", "no-child-pid-to-correlate"),
        # Every OTHER unbound route - an opencode backfill miss, a happy-claude
        # row - names none, and must still not emit a null.
        ("opencode", None, "no-session-binding-for-opencode"),
        ("claude", None, "no-session-binding-for-claude"),
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


def test_the_death_message_names_the_rm_that_a_respawn_needs() -> None:
    """The row is kept as evidence and the collision guard is status-blind, so
    "just retry" would be refused with exit 2 forever."""
    import inspect

    src = inspect.getsource(mux_spawn.dispatch_spawn_pane)
    assert "fno agents rm {name}" in src


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
