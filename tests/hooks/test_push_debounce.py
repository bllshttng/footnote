#!/usr/bin/env python3
"""Tests for the push debounce in hooks/git-protection.py.

Run: python3 tests/hooks/test_push_debounce.py
 or: pytest tests/hooks/test_push_debounce.py

A push while a run is in flight on the branch head only cancels that run and
restarts the wait. The debounce refuses it and names `fno do pr wait`. Two
instruments, because one has a blind spot the other covers: the CI probe reads
`fno do pr status --json`, and the stamp covers the seconds right after a push
when GitHub has not registered the new run yet and the probe honestly reads
`pending: 0`.

Every failure path allows. A broken status reader, a missing `fno`, no PR at
all: a push is never blocked by an instrument that could not answer.
"""

import importlib.util
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
HOOK_PATH = REPO_ROOT / "hooks" / "git-protection.py"

_spec = importlib.util.spec_from_file_location("git_protection", HOOK_PATH)
git_protection = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(git_protection)


class _Harness:
    """Redirect the module's state dir into tmp and stub its status read."""

    def __init__(self, tmp_path, status, monkeypatch):
        monkeypatch.setattr(git_protection, "PUSH_STAMP_DIR", tmp_path / "push-stamps")
        monkeypatch.setattr(
            git_protection, "_read_push_status", lambda cwd=None: status
        )
        self.bypass_events = []
        monkeypatch.setattr(
            git_protection,
            "_emit_push_bypass_event",
            self.bypass_events.append,
        )
        monkeypatch.delenv("FNO_PUSH_NOW", raising=False)


def test_pending_checks_refuse_the_push(tmp_path, monkeypatch):
    _Harness(tmp_path, (1234, 2), monkeypatch)
    reason = git_protection.push_debounce_refusal(
        "git push origin feature/x", "feature/x"
    )
    assert reason is not None
    assert "2 check(s) still running on PR 1234" in reason
    assert "fno do pr wait 1234" in reason


def test_green_checks_allow_the_push(tmp_path, monkeypatch):
    _Harness(tmp_path, (1234, 0), monkeypatch)
    assert (
        git_protection.push_debounce_refusal("git push origin feature/x", "feature/x")
        is None
    )


def test_no_pr_allows_the_push(tmp_path, monkeypatch):
    """`fno do pr status` returning nothing readable is not evidence of a run.

    An unreadable instrument allows: a broken reader must never become a push
    outage. This is the deliberate fail-open, and it is the reason the stamp
    exists as a second, local instrument.
    """
    _Harness(tmp_path, None, monkeypatch)
    assert (
        git_protection.push_debounce_refusal("git push origin feature/x", "feature/x")
        is None
    )


def test_fno_push_now_bypasses_and_records(tmp_path, monkeypatch):
    h = _Harness(tmp_path, (1234, 2), monkeypatch)
    monkeypatch.setenv("FNO_PUSH_NOW", "1")
    assert (
        git_protection.push_debounce_refusal("git push origin feature/x", "feature/x")
        is None
    )
    assert h.bypass_events == ["feature/x"], "the bypass left no journal row"


def test_a_fresh_stamp_refuses_even_when_checks_read_green(tmp_path, monkeypatch):
    """The blind spot the stamp exists for: GitHub has not registered the run
    the last push started, so the probe reads pending: 0 for a live run."""
    _Harness(tmp_path, (1234, 0), monkeypatch)
    git_protection._stamp_push("feature/x")
    reason = git_protection.push_debounce_refusal(
        "git push origin feature/x", "feature/x"
    )
    assert reason is not None
    assert "pushed 0s ago" in reason


def test_an_expired_stamp_allows(tmp_path, monkeypatch):
    _Harness(tmp_path, (1234, 0), monkeypatch)
    git_protection._stamp_push("feature/x")
    stamp = git_protection._push_stamp_path("feature/x")
    old = time.time() - git_protection.PUSH_DEBOUNCE_SECONDS - 1
    import os

    os.utime(stamp, (old, old))
    assert (
        git_protection.push_debounce_refusal("git push origin feature/x", "feature/x")
        is None
    )


def test_a_protected_push_still_refuses_for_the_branch(tmp_path, monkeypatch):
    """The debounce is a pacing rule and must not outrank a safety one: a push
    to main reads the protected-branch refusal, not the CI one."""
    _Harness(tmp_path, (1234, 2), monkeypatch)
    monkeypatch.setattr(git_protection, "load_state", lambda: {})
    monkeypatch.setattr(git_protection, "save_state", lambda state: None)
    monkeypatch.setattr(git_protection, "has_recent_approval", lambda s: False)
    monkeypatch.setattr(git_protection, "check_for_bypass_phrase", lambda s: False)
    verdict = git_protection._evaluate_git_segment(
        "git push origin main", has_approval=False
    )
    assert verdict is not None
    decision, reason = verdict
    assert decision == "deny"
    assert "push debounce" not in reason


def test_a_feature_push_routes_through_the_debounce(tmp_path, monkeypatch):
    _Harness(tmp_path, (1234, 3), monkeypatch)
    verdict = git_protection._evaluate_git_segment(
        "git push origin feature/x", has_approval=False
    )
    assert verdict is not None
    decision, reason = verdict
    assert decision == "deny"
    assert "fno do pr wait 1234" in reason


def test_a_non_push_git_command_is_not_stamped(tmp_path, monkeypatch):
    _Harness(tmp_path, (1234, 3), monkeypatch)
    assert (
        git_protection._evaluate_git_segment("git status", has_approval=False) is None
    )
    assert not git_protection._push_stamp_path("feature/x").exists()


def _main():
    import tempfile

    class _MP:
        """The two monkeypatch methods these tests use, for a bare python3 run."""

        def __init__(self):
            self._undo = []

        def setattr(self, obj, name, value):
            self._undo.append((obj, name, getattr(obj, name)))
            setattr(obj, name, value)

        def setenv(self, key, value):
            import os

            self._undo.append((os.environ, key, os.environ.get(key)))
            os.environ[key] = value

        def delenv(self, key, raising=True):
            import os

            if key in os.environ:
                self._undo.append((os.environ, key, os.environ[key]))
                del os.environ[key]

        def undo(self):
            import os

            for obj, name, old in reversed(self._undo):
                if obj is os.environ:
                    if old is None:
                        os.environ.pop(name, None)
                    else:
                        os.environ[name] = old
                else:
                    setattr(obj, name, old)
            self._undo = []

    failures = 0
    for name, fn in sorted(globals().items()):
        if not name.startswith("test_"):
            continue
        mp = _MP()
        with tempfile.TemporaryDirectory() as td:
            try:
                fn(Path(td), mp)
                print(f"[push-debounce] PASS: {name}")
            except AssertionError as exc:
                failures += 1
                print(f"[push-debounce] FAIL: {name}: {exc}", file=sys.stderr)
            finally:
                mp.undo()
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(_main())
