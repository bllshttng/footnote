"""A hermetic run must not write to a live operator surface.

On 2026-08-27 six ``think_offered`` rows carrying the fixture node id
``x-2222aaaa`` sat in the developer's live ``.fno/events.jsonl`` and surfaced as
a born-with-why offer for a node that does not exist. The row's ``offer_line``
named ``/x/t.jsonl``, which is the default transcript path of ``_resolved`` in
``test_spawn_think.py`` and appears nowhere else in the repository.

The cause was a private path builder: ``spawn_think._events_path`` composed
``Path.cwd() / ".fno" / "events.jsonl"`` and so consulted neither
``FNO_EVENTS_PATH`` (the journal pin ``fno.hermetic`` exists to set) nor
``FNO_REPO_ROOT`` (which the leaking test's own fixture sets). A worktree's
``.fno/events.jsonl`` is a symlink to the canonical journal, so a worker
running the suite inside a worktree reached the operator's file.

These tests pin the fix at both altitudes: the caller that had the bug, and the
shared seam that stops the next hand-built path instead of letting it land in
production. The notification pair covers the same class on the other
surface a test can reach the operator through.
"""
from __future__ import annotations

import pytest

from fno.events import HermeticEscapeError, _build, append_event
from fno.provenance import spawn_think as st

_DATA = {
    "node_id": "x-2222aaaa",
    "trigger": "birth",
    "presence": "away",
    "resolved": True,
    "offer_line": "/think x-2222aaaa",
    "rank": "env",
}


def _event() -> dict:
    return _build("think_offered", "backlog", dict(_DATA))


# ---------------------------------------------------------------------------
# The caller that had the bug
# ---------------------------------------------------------------------------


def test_unpathed_events_path_delegates_to_the_resolver(monkeypatch, tmp_path):
    """No project_root => the shared resolver decides, never the process cwd.

    Asserted against ``project_events_json`` itself rather than a literal path,
    because that function IS the contract: it consults ``FNO_EVENTS_PATH``, then
    the resolved repo root. A cwd-built path honours neither, which is the bug.
    """
    from fno.paths import project_events_json

    pinned = tmp_path / "sandbox" / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(pinned))
    monkeypatch.chdir(tmp_path)  # a cwd that is NOT the pin's parent
    assert st._events_path(None) == project_events_json() == pinned


def test_unpathed_events_path_ignores_the_process_cwd(monkeypatch, tmp_path):
    """The specific regression: chdir must not steer the journal."""
    from fno.paths import project_events_json

    pinned = tmp_path / "sandbox" / "events.jsonl"
    monkeypatch.setenv("FNO_EVENTS_PATH", str(pinned))
    decoy = tmp_path / "decoy"
    decoy.mkdir()
    monkeypatch.chdir(decoy)
    assert st._events_path(None) != decoy / ".fno" / "events.jsonl"
    assert st._events_path(None) == project_events_json()


def test_explicit_project_root_still_wins_over_the_pin(monkeypatch, tmp_path):
    """A caller holding its own root keeps its own file. One sandbox journal
    serves a whole pytest process, so honouring the pin here would replace that
    caller's file with a bucket every test in the process shares."""
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / "pinned.jsonl"))
    root = tmp_path / "proj"
    assert st._events_path(root) == root / ".fno" / "events.jsonl"


# ---------------------------------------------------------------------------
# The shared seam
# ---------------------------------------------------------------------------


def test_append_event_refuses_a_write_outside_the_sandbox(monkeypatch, tmp_path):
    """The guard that closes the class, for any path builder named or not."""
    home = tmp_path / "home"
    home.mkdir()
    outside = tmp_path / "checkout" / ".fno" / "events.jsonl"
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.setenv("TMPDIR", str(home))
    monkeypatch.delenv("FNO_EVENTS_PATH", raising=False)

    with pytest.raises(HermeticEscapeError) as exc:
        append_event(_event(), outside)
    assert str(outside) in str(exc.value)
    # Refused before the mkdir, so a blocked write leaves no .fno/ behind.
    assert not outside.parent.exists()


def test_append_event_allows_a_write_under_the_sandbox(monkeypatch, tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    inside = home / "proj" / ".fno" / "events.jsonl"
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.setenv("TMPDIR", str(home))

    append_event(_event(), inside)
    assert inside.read_text().strip()


def test_append_event_allows_the_pinned_journal(monkeypatch, tmp_path):
    """The pin's own directory is allowed even when it sits under neither HOME
    nor TMPDIR, because neutralise names it explicitly."""
    home = tmp_path / "home"
    home.mkdir()
    pinned = tmp_path / "elsewhere" / "events.jsonl"
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.setenv("TMPDIR", str(home))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(pinned))

    append_event(_event(), pinned)
    assert pinned.read_text().strip()


def test_append_event_guard_is_inert_outside_a_hermetic_run(monkeypatch, tmp_path):
    """Production keeps writing. The guard keys on the neutralise receipt, so an
    operator's own journal write never meets it."""
    monkeypatch.delenv("FNO_TEST_HERMETIC", raising=False)
    outside = tmp_path / "checkout" / ".fno" / "events.jsonl"
    append_event(_event(), outside)
    assert outside.read_text().strip()


def test_append_event_refuses_a_symlink_that_resolves_outside(monkeypatch, tmp_path):
    """Only the RESOLVED path is judged.

    A worktree's ``.fno/events.jsonl`` is a symlink to the canonical journal,
    which is the whole reason a test run reached the operator's file. Accepting
    the raw path when it merely SITS inside the sandbox reopens exactly that
    door: measured at 200 bytes of a production-shaped row landing in the
    outside file before the guard judged the realpath alone.
    """
    home = tmp_path / "home"
    home.mkdir()
    outside = tmp_path / "checkout" / ".fno"
    outside.mkdir(parents=True)
    target = outside / "events.jsonl"
    link = home / "events.jsonl"
    link.symlink_to(target)

    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.setenv("TMPDIR", str(home))
    monkeypatch.delenv("FNO_EVENTS_PATH", raising=False)

    with pytest.raises(HermeticEscapeError):
        append_event(_event(), link)
    assert not target.exists()


def test_home_is_not_an_allowed_root(monkeypatch, tmp_path):
    """A test that restores the real HOME must not disable the guard.

    ``HOME`` is read at call time, and three live-lane tests in this suite
    restore the real one. Were it a root, the allowed area would widen to the
    whole home directory, which contains both the checkout and ``~/.fno`` - the
    two files this guard exists to protect. Nothing is lost by excluding it: the
    sandbox HOME is a mkdtemp and so already sits under TMPDIR.
    """
    real_home = tmp_path / "real-home"
    real_home.mkdir()
    sandbox = tmp_path / "sandbox"
    sandbox.mkdir()
    live = real_home / ".fno" / "events.jsonl"

    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.setenv("HOME", str(real_home))
    monkeypatch.setenv("TMPDIR", str(sandbox))
    monkeypatch.delenv("FNO_EVENTS_PATH", raising=False)

    with pytest.raises(HermeticEscapeError):
        append_event(_event(), live)
    assert not live.parent.exists()


# ---------------------------------------------------------------------------
# The operator's screen
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("system", "has_notify_send"), [("Darwin", False), ("Linux", True)]
)
def test_notification_is_suppressed_in_a_hermetic_run(
    monkeypatch, system, has_notify_send
):
    """No test may reach the operator's screen, on either notifier.

    Reported as delivered because a caller that branches on a non-zero code
    (``fno.mail.cli`` escalates on one) would otherwise take a path the test
    never asked for.
    """
    import fno.notify._impl as impl

    def _boom(*a, **k):  # noqa: ANN002, ANN003
        raise AssertionError("a hermetic run dispatched a real OS notification")

    monkeypatch.setattr(impl.subprocess, "run", _boom)
    monkeypatch.setattr(impl.platform, "system", lambda: system)
    monkeypatch.setattr(
        impl.shutil, "which", lambda _n: "/usr/bin/notify-send" if has_notify_send else None
    )
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    assert impl.send_notification("t", "m") == (0, "")


def test_hermetic_run_still_degrades_loudly_with_no_notifier(monkeypatch):
    """The suppression sits at the dispatch, not at the top of the function.

    A host with neither notifier must still answer non-zero (AC2-FR) so its
    caller does not claim a surface that never reached the operator. Short-
    circuiting the whole function would have answered "delivered" here.
    """
    import fno.notify._impl as impl

    monkeypatch.setattr(impl.platform, "system", lambda: "Linux")
    monkeypatch.setattr(impl.shutil, "which", lambda _n: None)
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    code, err = impl.send_notification("t", "m")
    assert code == 1 and "no OS notification tool available" in err


def test_notification_dispatches_outside_a_hermetic_run(monkeypatch):
    """Production still notifies; the guard keys on the receipt, nothing else."""
    import fno.notify._impl as impl

    calls: list = []
    monkeypatch.setattr(impl.subprocess, "run", lambda *a, **k: calls.append(a))
    monkeypatch.setattr(impl.platform, "system", lambda: "Darwin")
    monkeypatch.delenv("FNO_TEST_HERMETIC", raising=False)
    assert impl.send_notification("t", "m") == (0, "")
    assert calls, "no dispatch attempted outside a hermetic run"
