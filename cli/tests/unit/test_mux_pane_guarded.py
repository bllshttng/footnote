"""Turn-taken confirmation for the mux-pane delivery lane (US4).

``_mux_pane_send`` guards the paste against the server-side turn-taken interlock:
a mid-turn recipient refuses with EXIT_TARGET_NOT_IDLE (15), so the lane demotes
to the caller's durable floor instead of swallowing the bytes and letting the
sender report ``hosted`` (Locked Decision 4: hosted-on-bytes-written is banned).
"""

from types import SimpleNamespace

import fno.agents.dispatch as dispatch


def _entry():
    return SimpleNamespace(mux={"session": "main", "pane_id": 3})


def _install_fake_run(monkeypatch, exit_codes):
    """Stub ``subprocess.run`` to pop one exit code per ``fno mux pane`` verb and
    record every argv. Also no-ops the paste->CR settle sleep."""
    calls: list[list[str]] = []

    def _run(argv, **_kwargs):
        calls.append(list(argv))
        code = exit_codes.pop(0) if exit_codes else 0
        return SimpleNamespace(returncode=code, stdout="", stderr="receiving agent not idle")

    monkeypatch.setattr(dispatch.subprocess, "run", _run)
    monkeypatch.setattr(dispatch.time, "sleep", lambda *_a: None)
    return calls


def _paste_call(calls):
    """The stdin paste verb: ``... pane send <id> --stdin [...]``."""
    return next(c for c in calls if "send" in c and "--stdin" in c)


def test_guarded_paste_carries_the_guarded_flag_and_confirms(monkeypatch):
    # claim, paste, CR, release all succeed; guarded is the default.
    calls = _install_fake_run(monkeypatch, [0, 0, 0, 0])
    assert dispatch._mux_pane_send(_entry(), "hi") is True
    assert "--guarded" in _paste_call(calls)


def test_not_idle_paste_stalls_to_durable(monkeypatch, capsys):
    # Guarded paste refused because the recipient's turn is not takeable.
    calls = _install_fake_run(monkeypatch, [0, dispatch._MUX_EXIT_TARGET_NOT_IDLE, 0])
    assert dispatch._mux_pane_send(_entry(), "hi") is False
    # The CR submit never fires once the paste stalls -- no half-sent prompt.
    assert not any("--text" in c for c in calls)
    # The stall reason is surfaced, never swallowed (US5 sibling requirement).
    assert "stalled" in capsys.readouterr().err


def test_unguarded_follow_up_omits_the_flag(monkeypatch):
    # The peer follow-up lane keeps its raw, unguarded channel.
    calls = _install_fake_run(monkeypatch, [0, 0, 0, 0])
    assert dispatch._mux_pane_send(_entry(), "hi", guarded=False) is True
    assert "--guarded" not in _paste_call(calls)
