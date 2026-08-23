"""x-3855: raw `gh pr merge` authorization resolves the one authoritative
posture - live config armed or the run's spawn-time env grant - instead of
the manifest snapshot alone. Before this, the hook authorized on a manifest
whose `true` merely mirrored config an operator had since flipped off, making
the raw path the weaker gate (the sanctioned verb refused what raw gh merged).
"""
import importlib.util
from pathlib import Path

import pytest

_HOOK = Path(__file__).resolve().parents[3] / "hooks" / "git-protection.py"


@pytest.fixture()
def gp(monkeypatch, tmp_path):
    """The hook module, imported from path (it is a script, not a package).

    Top level is imports, constants, and defs under a __main__ guard, so
    exec_module runs no hook logic. The resolver subprocess is stubbed per
    test: the module's own subprocess is patched, not the global one.
    """
    assert _HOOK.exists(), f"hook missing: {_HOOK}"
    spec = importlib.util.spec_from_file_location("git_protection_under_test", _HOOK)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    monkeypatch.chdir(tmp_path)
    return mod


class _Resolver:
    """Stub `fno config get auto_merge.enabled` with a canned answer."""

    def __init__(self, answer, returncode=0):
        self.answer = answer
        self.returncode = returncode
        self.calls = 0
        self.last_cmd = None

    def __call__(self, cmd, **kwargs):
        self.calls += 1
        self.last_cmd = list(cmd)

        class _R:
            pass

        r = _R()
        r.returncode = self.returncode
        r.stdout = self.answer + "\n"
        r.stderr = ""
        return r


def _fm(approved="true", source="config"):
    return {"auto_merge_approved": approved, "auto_merge_source": source}


def test_live_switch_asks_the_resolver(gp, tmp_path, monkeypatch):
    resolver = _Resolver("True")
    monkeypatch.setattr(gp.subprocess, "run", resolver)
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is True
    assert resolver.calls == 1
    # The resolver CLI answers the key; a local TOML parse is the x-93ff trap.
    assert resolver.last_cmd[:4] == ["fno", "config", "get", "auto_merge.enabled"]


def test_live_switch_disarmed_refuses_manifest_true(gp, tmp_path, monkeypatch):
    """The x-2270 doctrine at the raw path: a snapshot whose true mirrored
    config must not outlive the operator flipping the live switch off."""
    monkeypatch.setattr(gp.subprocess, "run", _Resolver("False"))
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is False


def test_resolver_failure_fails_closed(gp, tmp_path, monkeypatch):
    def boom(cmd, **kwargs):
        raise RuntimeError("fno missing")

    monkeypatch.setattr(gp.subprocess, "run", boom)
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is False


def test_env_grant_stamp_arms_without_the_resolver(gp, tmp_path, monkeypatch):
    resolver = _Resolver("False")
    monkeypatch.setattr(gp.subprocess, "run", resolver)
    assert (
        gp._live_merge_switch_armed(tmp_path, _fm(source="env-target-auto-merge"))
        is True
    )
    assert resolver.calls == 0, "the spawn-time grant satisfies the arm alone"


def test_merge_allowed_declines_when_live_switch_disarmed(gp, monkeypatch, tmp_path):
    """The two-factor authorize path consults the live switch: a disarmed
    config returns None (not authorized), so the deny path explains instead
    of raw-merging past the disarm."""
    state_file = tmp_path / ".fno" / "target-state.md"
    state_file.parent.mkdir(parents=True)
    state_file.write_text(
        "---\nsession_id: s1\nauto_merge_approved: true\n"
        "auto_merge_source: config\nexternal_review_passed: skipped\n---\n",
        encoding="utf-8",
    )
    fm = {
        "auto_merge_approved": "true",
        "auto_merge_source": "config",
        "external_review_passed": "skipped",
        "session_id": "s1",
    }
    monkeypatch.setattr(
        gp, "_get_active_target_session", lambda prefer_pr=0: (state_file, fm, tmp_path)
    )
    monkeypatch.setattr(gp, "_targets_other_repo", lambda command: False)
    monkeypatch.setattr(gp, "_parse_merge_pr", lambda command: 42)
    monkeypatch.setattr(gp.subprocess, "run", _Resolver("False"))
    assert gp._check_pr_merge_allowed("gh pr merge 42") is None


def test_merge_allowed_authorizes_when_live_switch_armed(gp, monkeypatch, tmp_path):
    state_file = tmp_path / ".fno" / "target-state.md"
    fm = {
        "auto_merge_approved": "true",
        "auto_merge_source": "config",
        "external_review_passed": "skipped",
        "session_id": "s1",
    }
    monkeypatch.setattr(
        gp, "_get_active_target_session", lambda prefer_pr=0: (state_file, fm, tmp_path)
    )
    monkeypatch.setattr(gp, "_targets_other_repo", lambda command: False)
    monkeypatch.setattr(gp, "_parse_merge_pr", lambda command: 42)
    monkeypatch.setattr(gp.subprocess, "run", _Resolver("True"))
    reason = gp._check_pr_merge_allowed("gh pr merge 42")
    assert reason is not None and "skipped" in reason
