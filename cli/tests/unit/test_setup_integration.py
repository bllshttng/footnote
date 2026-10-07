"""Tests for the `fno setup` CLI-integration step (`fno.setup.integration`).

Drives the interactive-agnostic core ``run_cli_integration`` with stub adapters
and a fake subprocess runner so nothing shells out for real. Covers AC1-HP
(select + install), AC1-ERR (one failure does not abort the rest, no false
success), AC1-UI (visible per-CLI lines + a skipped-not-on-PATH note), AC1-EDGE
(already-installed skips, none-available no-ops), AC1-FR (claude skills-dir
fallback when `claude plugin install` is unavailable/errors).
"""
from __future__ import annotations

import json
import shutil
import subprocess

import pytest

from fno.setup import integration as I
from fno.setup.integration import (
    IntegrationAdapter,
    IntegrationResult,
    run_cli_integration,
)


def _adapter(cli, label, *, available=True, installed=False, result=None, calls=None):
    """A stub adapter that records whether install() ran (via ``calls``)."""
    res = result if result is not None else IntegrationResult(cli, label, "installed")

    def _install():
        if calls is not None:
            calls.append(cli)
        return res

    return IntegrationAdapter(
        cli,
        label,
        is_available=lambda: available,
        is_installed=lambda: installed,
        install=_install,
    )


def _collector():
    lines: list[str] = []
    return lines, lines.append


# --- AC1-HP -----------------------------------------------------------------


# --- AC1-ERR ----------------------------------------------------------------

def test_ac1_err_one_failure_does_not_abort_the_rest():
    lines, echo = _collector()
    calls: list[str] = []
    adapters = [
        _adapter(
            "codex",
            "Codex CLI",
            result=IntegrationResult("codex", "Codex CLI", "failed", note="boom"),
            calls=calls,
        ),
        _adapter("gemini", "Gemini CLI", calls=calls),
    ]

    results = run_cli_integration(
        select_fn=lambda opts: ["codex", "gemini"], echo_fn=echo, adapters=adapters
    )

    # both attempted, codex failed, gemini still installed
    assert set(calls) == {"codex", "gemini"}
    by_cli = {r.cli: r for r in results}
    assert by_cli["codex"].status == "failed"
    assert by_cli["gemini"].status == "installed"
    # no false success line for the failed CLI
    assert any("Codex CLI: FAILED" in m for m in lines)
    assert not any("Codex CLI: installed" in m for m in lines)


# --- AC1-UI -----------------------------------------------------------------

def test_ac1_ui_visible_per_cli_feedback_and_skipped_note():
    lines, echo = _collector()
    adapters = [
        _adapter("claude", "Claude Code"),
        _adapter("gemini", "Gemini CLI"),
        _adapter("codex", "Codex CLI", available=False),  # not on PATH
    ]

    run_cli_integration(
        select_fn=lambda opts: ["claude", "gemini"], echo_fn=echo, adapters=adapters
    )

    blob = "\n".join(lines)
    assert "Claude Code: installing..." in blob and "Claude Code: installed" in blob
    assert "Gemini CLI: installing..." in blob and "Gemini CLI: installed" in blob
    # the undetected CLI is named once in a skipped line, not silently dropped
    assert "skipped (not on PATH): Codex CLI" in blob


# --- AC1-EDGE ---------------------------------------------------------------

def test_ac1_edge_already_installed_is_not_reinstalled():
    lines, echo = _collector()
    calls: list[str] = []
    adapters = [_adapter("claude", "Claude Code", installed=True, calls=calls)]

    # even if the user "selects" it, an already-installed CLI is never installed
    results = run_cli_integration(
        select_fn=lambda opts: ["claude"], echo_fn=echo, adapters=adapters
    )

    assert calls == []
    assert results == []
    assert any("Claude Code: already installed" in m for m in lines)
    assert any("nothing to install" in m for m in lines)


# --- AC1-FR (claude skills-dir fallback) ------------------------------------

class _FakeRun:
    """Maps an argv (matched by a substring of the joined command) to a result."""

    def __init__(self, rules):
        # rules: list of (needle, returncode, stdout, stderr)
        self.rules = rules
        self.calls: list[list] = []

    def __call__(self, cmd, timeout=120):
        self.calls.append(cmd)
        joined = " ".join(cmd)
        for needle, rc, out, err in self.rules:
            if needle in joined:
                return subprocess.CompletedProcess(cmd, rc, out, err)
        return subprocess.CompletedProcess(cmd, 0, "", "")


def test_ac1_fr_falls_back_to_skills_dir_when_plugin_install_errors(tmp_path, monkeypatch):
    dest = tmp_path / "skills-fno"
    monkeypatch.setattr(I, "_claude_skills_dir", lambda: dest)
    run = _FakeRun([
        ("plugin --help", 0, "", ""),
        ("marketplace add", 0, "", ""),
        ("plugin install", 1, "", "marketplace not reachable"),
        ("git clone", 0, "", ""),  # fallback clone succeeds
    ])

    res = I._claude_install(run)

    assert res.status == "installed" and "skills-dir" in res.note
    assert any("git" in c and "clone" in c for c in run.calls)


def test_ac1_fr_reports_failed_only_when_fallback_also_fails(tmp_path, monkeypatch):
    dest = tmp_path / "skills-fno"
    monkeypatch.setattr(I, "_claude_skills_dir", lambda: dest)
    run = _FakeRun([
        ("plugin --help", 0, "", ""),
        ("marketplace add", 0, "", ""),
        ("plugin install", 1, "", "nope"),
        ("git clone", 1, "", "clone failed: no network"),
    ])

    res = I._claude_install(run)

    assert res.status == "failed" and "no network" in res.note


def test_skills_dir_recovers_from_a_stale_partial_clone(tmp_path, monkeypatch):
    # A prior failed clone left dest non-empty but without a valid plugin.json.
    dest = tmp_path / "skills-fno"
    dest.mkdir()
    (dest / "stale").write_text("leftover from a failed clone")
    monkeypatch.setattr(I, "_claude_skills_dir", lambda: dest)
    run = _FakeRun([("git clone", 0, "", "")])

    res = I._claude_skills_dir_install(run)

    # the stale dir was cleared before the retry clone, and the result is honest
    assert not (dest / "stale").exists()
    assert res.status == "installed" and "skills-dir" in res.note


# --- adapter exit-code honesty ----------------------------------------------

def test_install_never_claims_success_on_nonzero_exit():
    run = _FakeRun([("gemini extensions install", 1, "", "network down")])
    res = I._gemini_install(run)
    assert res.status == "failed" and not res.ok


# --- codex: wizard delegates to the verified release convergence ------------


def test_codex_install_failed_on_convergence_error(monkeypatch):
    from fno.setup.codex_plugin import CodexPluginError

    def fail(**_kwargs):
        raise CodexPluginError("marketplace-add", "no such marketplace")

    monkeypatch.setattr("fno.setup.codex_plugin.converge", fail)
    run = _FakeRun([])
    res = I._codex_install(run)
    assert res.status == "failed" and not res.ok
    assert "marketplace-add" in res.note


def test_manual_result_echoes_a_finish_step_not_installed():
    lines, echo = _collector()
    adapters = [
        _adapter(
            "codex",
            "Codex CLI",
            result=IntegrationResult("codex", "Codex CLI", "manual", note="finish in browser"),
        )
    ]
    run_cli_integration(
        select_fn=lambda opts: ["codex"], echo_fn=echo, adapters=adapters
    )
    blob = "\n".join(lines)
    assert "needs a manual finish" in blob and "finish in browser" in blob
    assert "Codex CLI: installed" not in blob


# --- opencode (fno-agents door, x-11ca) --------------------------------------

def test_opencode_is_installed_reads_the_door(monkeypatch):
    """Installed == the door receipt says installed; no local byte-compare."""
    calls = []

    def fake_status():
        calls.append(1)
        return (None, {"status": "installed"})

    monkeypatch.setattr(I, "_opencode_status", fake_status)
    assert I._opencode_is_installed() is True
    assert calls

    monkeypatch.setattr(I, "_opencode_status", lambda: (None, {"status": "partial"}))
    assert I._opencode_is_installed() is False

    monkeypatch.setattr(I, "_opencode_status", lambda: ("binary missing", None))
    assert I._opencode_is_installed() is False


def test_opencode_install_names_kept_user_files(monkeypatch):
    import fno.rust_binary as rb

    monkeypatch.setattr(
        rb,
        "call_binary_json",
        lambda verb, args, **kw: (
            None,
            {
                "status": "partial",
                "written": 3,
                "version": "9.9.9",
                "config_dir": "/tmp/conf",
                "kept": ["skills/decoy/SKILL.md"],
            },
        ),
    )
    res = I._opencode_install()
    assert res.ok
    assert "kept user files: skills/decoy/SKILL.md" in res.note


def test_opencode_install_maps_door_failure(monkeypatch):
    import fno.rust_binary as rb

    monkeypatch.setattr(rb, "call_binary_json", lambda verb, args, **kw: ("no binary", None))
    res = I._opencode_install()
    assert not res.ok
    assert res.status == "failed"
    assert res.note == "no binary"


# --- pi (Rust pi arm of plugin-install; the agent dir pi itself reads) -------

@pytest.fixture
def pi_agent_env(tmp_path, monkeypatch):
    """A relocated agent dir via PI_CODING_AGENT_DIR, plus a scratch HOME.

    The install must honor the agent dir pi actually reads and write nothing
    under the ambient HOME's ~/.pi.
    """
    from fno.rust_binary import find_dev_binary

    binary = find_dev_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    # call_binary_json resolves through $FNO_AGENTS_BIN first, so the tests
    # exercise the dev binary, which carries the pi arm.
    monkeypatch.setenv("FNO_AGENTS_BIN", str(binary))
    agent = tmp_path / "agent"
    monkeypatch.setenv("PI_CODING_AGENT_DIR", str(agent))
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    return agent


@pytest.mark.dev_build
def test_pi_install_copies_extension_and_is_installed(tmp_path, pi_agent_env):
    assert I._pi_is_installed() is False

    res = I._pi_install()
    assert res.ok and res.cli == "pi"

    dest = pi_agent_env / "extensions" / "footnote.ts"
    assert dest.exists()
    assert dest.read_text(encoding="utf-8") == I._pi_extension_src().read_text(
        encoding="utf-8"
    )
    # Nothing under the scratch HOME's default pi tree: the agent dir is
    # where pi itself reads, not $HOME/.pi.
    assert not (tmp_path / "home" / ".pi").exists()
    assert I._pi_is_installed() is True


def test_adapters_registered_and_pi_gated_on_path():
    adapters = I.build_adapters()
    assert {a.cli for a in adapters} == {"claude", "gemini", "codex", "opencode", "pi", "agy"}
    adapter = next(a for a in adapters if a.cli == "pi")
    # Availability rides shutil.which("pi"), which is machine-dependent; the
    # contract asserted here is the gate's SHAPE, not this machine's answer.
    assert adapter.is_available() == (shutil.which("pi") is not None)
    assert I._pi_extension_src().is_file(), "the shipped artifact must exist"


# --- agy (native Stop-hook registration) -------------------------------------

@pytest.fixture
def agy_rust_door(monkeypatch):
    """Pin the agy hooks door to THIS checkout's fno-agents build.

    call_binary_json resolves through $FNO_AGENTS_BIN first, so the tests
    exercise the dev binary, and skip where this checkout has none (the same
    contract the native_backlog_door fixture implements)."""
    from fno.rust_binary import find_dev_binary

    binary = find_dev_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    monkeypatch.setenv("FNO_AGENTS_BIN", str(binary))


def _fake_agy_adapter(tmp_path, monkeypatch):
    """Point _agy_adapter_path at a real tmp file so install is deterministic
    (independent of whether the test env can resolve the real plugin root).
    The crown and guard adapters resolve to None for the same reason: no test
    may depend on what the machine's plugin stage happens to ship."""
    adapter = tmp_path / "plugin" / "hooks" / "footnote-agy-target-stop-hook.sh"
    adapter.parent.mkdir(parents=True, exist_ok=True)
    adapter.write_text("#!/usr/bin/env bash\n", encoding="utf-8")
    monkeypatch.setattr(I, "_agy_adapter_path", lambda: adapter)
    monkeypatch.setattr(I, "_agy_crown_adapter_path", lambda: None)
    monkeypatch.setattr(I, "_agy_guard_adapter_path", lambda: None)
    return adapter


@pytest.mark.dev_build
def test_agy_install_registers_stop_hook_and_is_installed(tmp_path, monkeypatch, agy_rust_door):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.chdir(tmp_path)  # keep any workspace writes inside the tmp tree
    adapter = _fake_agy_adapter(tmp_path, monkeypatch)

    assert I._agy_is_installed() is False

    res = I._agy_install()
    assert res.ok and res.cli == "agy"

    hooks = I._agy_hooks_json()
    data = json.loads(hooks.read_text(encoding="utf-8"))
    stop = data["footnote"]["Stop"]
    assert stop[0]["command"] == str(adapter)
    assert stop[0]["type"] == "command"
    assert I._agy_is_installed() is True
    # No workspace breadcrumb: agy parses AGENTS.md natively, and `.agent/`
    # singular is not an agy path at all.
    assert not (tmp_path / ".agent").exists()


def test_agy_install_manual_when_adapter_absent(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setattr(I, "_agy_adapter_path", lambda: None)
    res = I._agy_install()
    # A CLI-only install can't wire a path that doesn't exist -> manual, never ok.
    assert res.status == "manual" and not res.ok
