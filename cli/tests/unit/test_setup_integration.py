"""Tests for the `fno setup` CLI-integration step (`fno.setup.integration`).

Drives the interactive-agnostic core ``run_cli_integration`` with stub adapters
and a fake subprocess runner so nothing shells out for real. Covers AC1-HP
(select + install), AC1-ERR (one failure does not abort the rest, no false
success), AC1-UI (visible per-CLI lines + a skipped-not-on-PATH note), AC1-EDGE
(already-installed skips, none-available no-ops), AC1-FR (claude skills-dir
fallback when `claude plugin install` is unavailable/errors).
"""
from __future__ import annotations

import subprocess

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

def test_ac1_hp_select_and_install():
    lines, echo = _collector()
    calls: list[str] = []
    adapters = [_adapter("claude", "Claude Code", installed=False, calls=calls)]

    results = run_cli_integration(
        select_fn=lambda opts: ["claude"], echo_fn=echo, adapters=adapters
    )

    assert calls == ["claude"]
    assert len(results) == 1 and results[0].ok and results[0].status == "installed"
    assert any("Claude Code: installed" in m for m in lines)


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


def test_ac1_edge_no_cli_available_skips_with_one_note():
    lines, echo = _collector()
    adapters = [
        _adapter("claude", "Claude Code", available=False),
        _adapter("gemini", "Gemini CLI", available=False),
    ]

    results = run_cli_integration(
        select_fn=lambda opts: ["claude"], echo_fn=echo, adapters=adapters
    )

    assert results == []
    assert any("no agent CLIs detected on PATH" in m for m in lines)


def test_unchecked_cli_is_never_installed():
    calls: list[str] = []
    adapters = [_adapter("claude", "Claude Code", calls=calls)]
    results = run_cli_integration(
        select_fn=lambda opts: [], echo_fn=lambda _m: None, adapters=adapters
    )
    assert calls == [] and results == []


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


def test_ac1_fr_old_claude_without_plugin_subcommand_routes_to_skills_dir(tmp_path, monkeypatch):
    dest = tmp_path / "skills-fno"
    monkeypatch.setattr(I, "_claude_skills_dir", lambda: dest)
    run = _FakeRun([
        ("plugin --help", 1, "", "unknown command 'plugin'"),
        ("git clone", 0, "", ""),
    ])

    res = I._claude_install(run)

    assert res.status == "installed" and "skills-dir" in res.note
    # never attempted the preferred marketplace/install path
    assert not any("marketplace add" in " ".join(c) for c in run.calls)


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


def test_claude_is_installed_detects_fno_at_footnote_in_json(tmp_path, monkeypatch):
    monkeypatch.setattr(I, "_claude_skills_dir", lambda: tmp_path / "absent")
    run = _FakeRun([
        ("plugin list", 0, '[{"id": "fno@footnote", "enabled": true}]', ""),
    ])
    # skills-dir not present, so it falls through to the JSON probe
    assert I._claude_is_installed(run) is True


def test_claude_is_installed_false_on_malformed_json(tmp_path, monkeypatch):
    monkeypatch.setattr(I, "_claude_skills_dir", lambda: tmp_path / "absent")
    run = _FakeRun([("plugin list", 0, "not json", "")])
    assert I._claude_is_installed(run) is False


# --- codex: marketplace-add is not a full install (no false success) --------

def test_codex_install_reports_manual_not_installed():
    run = _FakeRun([("marketplace add", 0, "", "")])
    res = I._codex_install(run)
    # marketplace registration succeeded, but the plugin still needs a manual
    # finish, so this must NOT read as installed.
    assert res.status == "manual" and not res.ok
    assert "plugin browser" in res.note


def test_codex_install_failed_on_nonzero_marketplace_add():
    run = _FakeRun([("marketplace add", 1, "", "no such marketplace")])
    res = I._codex_install(run)
    assert res.status == "failed" and not res.ok


def test_codex_is_installed_never_claims_installed_from_marketplace():
    # a listed marketplace is not proof the plugin is wired up
    run = _FakeRun([("marketplace list", 0, "footnote", "")])
    assert I._codex_is_installed(run) is False


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


# --- opencode (local-file plugin install, x-6007) ---------------------------

def test_opencode_install_copies_plugin_and_is_installed(tmp_path, monkeypatch):
    # Redirect HOME so the plugin lands under a temp ~/.config/opencode/plugins/.
    monkeypatch.setenv("HOME", str(tmp_path))

    assert I._opencode_is_installed() is False

    res = I._opencode_install()
    assert res.ok and res.cli == "opencode"

    dest = I._opencode_plugin_dest()
    assert dest.exists()
    assert dest.read_text(encoding="utf-8") == I._opencode_plugin_src().read_text(
        encoding="utf-8"
    )
    assert I._opencode_is_installed() is True


def test_opencode_is_installed_false_when_stale(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    dest = I._opencode_plugin_dest()
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text("// stale older footnote plugin\n", encoding="utf-8")
    assert I._opencode_is_installed() is False


def test_opencode_adapter_registered():
    clis = {a.cli for a in I.build_adapters()}
    assert "opencode" in clis
