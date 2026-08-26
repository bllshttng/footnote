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
    exec_module runs no hook logic. Tests write real `.fno/config.toml`
    layers and exercise the real resolver, never a re-parsed fake.
    """
    assert _HOOK.exists(), f"hook missing: {_HOOK}"
    spec = importlib.util.spec_from_file_location("git_protection_under_test", _HOOK)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    monkeypatch.chdir(tmp_path)
    return mod


def _arm(repo, enabled):
    d = repo / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    # TOML booleans are lowercase; a Python bool's repr would be malformed
    # config, which the resolver correctly degrades to defaults (off).
    (d / "config.toml").write_text(
        f"[auto_merge]\nenabled = {str(enabled).lower()}\n"
    )


def _fm(approved="true", source="config"):
    return {"auto_merge_approved": approved, "auto_merge_source": source}


def test_live_switch_arms_from_project_config(gp, tmp_path):
    _arm(tmp_path, True)
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is True


def test_live_switch_disarmed_refuses_manifest_true(gp, tmp_path):
    """The x-2270 doctrine at the raw path: a snapshot whose true mirrored
    config must not outlive the operator flipping the live switch off."""
    _arm(tmp_path, False)
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is False


def test_unreadable_config_fails_closed(gp, tmp_path):
    d = tmp_path / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    (d / "config.toml").write_text("[auto_merge]\nenabled = unclosed [")
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is False


def test_env_grant_stamp_arms_without_the_switch(gp, tmp_path):
    _arm(tmp_path, False)
    assert (
        gp._live_merge_switch_armed(tmp_path, _fm(source="env-target-auto-merge"))
        is True
    )


def test_resolver_cli_fallback_when_fno_not_importable(gp, tmp_path, monkeypatch):
    """The hook interpreter may not carry the package; the resolver CLI is
    the fallback, and its failure fails closed."""
    import builtins

    real_import = builtins.__import__

    def no_fno_config(name, *a, **k):
        if name == "fno.config":
            raise ImportError("simulated absent package")
        return real_import(name, *a, **k)

    monkeypatch.setattr(builtins, "__import__", no_fno_config)

    class _R:
        returncode = 0
        stdout = "True\n"
        stderr = ""

    monkeypatch.setattr(gp.subprocess, "run", lambda cmd, **kw: _R())
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is True

    def boom(cmd, **kw):
        raise RuntimeError("fno missing")

    monkeypatch.setattr(gp.subprocess, "run", boom)
    assert gp._live_merge_switch_armed(tmp_path, _fm()) is False


def _session(tmp_path, source="config", enabled=True):
    state_file = tmp_path / ".fno" / "target-state.md"
    state_file.parent.mkdir(parents=True, exist_ok=True)
    state_file.write_text(
        "---\nsession_id: s1\nauto_merge_approved: true\n"
        f"auto_merge_source: {source}\nexternal_review_passed: skipped\n---\n",
        encoding="utf-8",
    )
    _arm(tmp_path, enabled)
    fm = {
        "auto_merge_approved": "true",
        "auto_merge_source": source,
        "external_review_passed": "skipped",
        "session_id": "s1",
    }
    return state_file, fm


def test_merge_allowed_declines_when_live_switch_disarmed(gp, monkeypatch, tmp_path):
    """The two-factor authorize path consults the live switch: a disarmed
    config returns None (not authorized), so the deny path explains instead
    of raw-merging past the disarm."""
    state_file, fm = _session(tmp_path, enabled=False)
    monkeypatch.setattr(
        gp, "_get_active_target_session", lambda prefer_pr=0: (state_file, fm, tmp_path)
    )
    monkeypatch.setattr(gp, "_targets_other_repo", lambda command: False)
    monkeypatch.setattr(gp, "_parse_merge_pr", lambda command: 42)
    assert gp._check_pr_merge_allowed("gh pr merge 42") is None


def test_merge_allowed_authorizes_when_live_switch_armed(gp, monkeypatch, tmp_path):
    state_file, fm = _session(tmp_path, enabled=True)
    monkeypatch.setattr(
        gp, "_get_active_target_session", lambda prefer_pr=0: (state_file, fm, tmp_path)
    )
    monkeypatch.setattr(gp, "_targets_other_repo", lambda command: False)
    monkeypatch.setattr(gp, "_parse_merge_pr", lambda command: 42)
    reason = gp._check_pr_merge_allowed("gh pr merge 42")
    assert reason is not None and "skipped" in reason


# ---- the coverage veto's refusal set (AC7-ERR / AC7-EDGE) ----
#
# The verified trap: `if proc.returncode != 3: return None` fails OPEN on
# every exit but 3, so an IMPOSSIBLE verdict (exit 5) would silently permit
# the exact merge it exists to stop. The set is {3, 5}; the fail-open posture
# for genuine machinery failure (missing binary, timeout, an older
# deployment's unknown-command exit) is deliberate and stays.


class _Proc:
    def __init__(self, returncode, stderr=""):
        self.returncode = returncode
        self.stderr = stderr
        self.stdout = ""


def test_ac7_err_exit_five_denies_with_the_verbs_first_line(gp, monkeypatch):
    monkeypatch.setattr(
        gp.subprocess,
        "run",
        lambda cmd, **kw: _Proc(
            5,
            "review coverage is impossible to satisfy by further review: 3 "
            "review rounds used (max 2) with blocking finding(s) still "
            "non-terminal (h.py:302:security)\nsecond line\n",
        ),
    )
    refusal = gp._fno_veto_refusal(["pr", "coverage-check", "42"], timeout=25, fallback="fb")
    assert refusal is not None
    assert refusal.startswith("review coverage is impossible to satisfy")
    assert "3 review rounds used" in refusal


def test_ac7_err_exit_three_still_denies(gp, monkeypatch):
    """The ordinary refusal keeps its behavior - the widening never narrows."""
    monkeypatch.setattr(
        gp.subprocess, "run", lambda cmd, **kw: _Proc(3, "uncovered: 0 reviewed\n")
    )
    assert gp._fno_veto_refusal(["pr", "coverage-check", "42"], 25, "fb") == (
        "uncovered: 0 reviewed"
    )


def test_ac7_edge_exit_127_fails_open(gp, monkeypatch):
    monkeypatch.setattr(gp.subprocess, "run", lambda cmd, **kw: _Proc(127, ""))
    assert gp._fno_veto_refusal(["pr", "coverage-check", "42"], 25, "fb") is None


def test_ac7_edge_timeout_fails_open(gp, monkeypatch):
    def boom(cmd, **kw):
        raise gp.subprocess.TimeoutExpired(cmd, 25)

    monkeypatch.setattr(gp.subprocess, "run", boom)
    assert gp._fno_veto_refusal(["pr", "coverage-check", "42"], 25, "fb") is None


def test_ac7_edge_exit_four_stays_an_instrument_failure(gp, monkeypatch):
    """UNANSWERED is not a verdict; the probe died, so the veto opens."""
    monkeypatch.setattr(
        gp.subprocess, "run", lambda cmd, **kw: _Proc(4, "pr head fetch failed\n")
    )
    assert gp._fno_veto_refusal(["pr", "coverage-check", "42"], 25, "fb") is None
