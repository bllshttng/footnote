"""CLI-integration installers for `fno setup` (the agent-door opt-in).

A CLI-only install of footnote (`curl fno.sh | sh`, `uv`, `brew`, `cargo`) lands
the `fno` binary but **not** the ``/fno:*`` slash commands - those come from the
Claude Code plugin / Gemini extension / Codex marketplace integration. This
module installs that integration for each CLI the user checks in the setup
wizard. It runs side-effecting installers and writes no settings.yaml config
(that is ``run_wizard``'s job); the two concerns stay cleanly separated.

The core (``run_cli_integration``) is interactive-agnostic, mirroring
``run_wizard``: a ``select_fn`` is injected so the same code drives a terminal
checklist, the Claude Code multi-select UI, and tests. Adapters take an
injectable subprocess runner so tests never shell out for real.
"""
from __future__ import annotations

import json
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

# Marketplace / repo the integrations install from.
_MARKETPLACE = "bllshttng/footnote"
_REPO_URL = "https://github.com/bllshttng/footnote"

# A subprocess runner: takes an argv list (and an optional timeout) and returns
# a CompletedProcess. `...` keeps the optional timeout kwarg in the contract.
Runner = Callable[..., "subprocess.CompletedProcess[str]"]


@dataclass
class IntegrationResult:
    """Outcome of one CLI's integration install."""

    cli: str  # "claude" | "gemini" | "codex" | "opencode"
    label: str  # human name, e.g. "Claude Code"
    status: str  # "installed" | "already-installed" | "manual" | "failed"
    note: str = ""  # detail (e.g. "skills-dir", a manual step, a failure reason)

    @property
    def ok(self) -> bool:
        # "manual" is NOT ok: a step succeeded but the integration is not yet
        # wired up, so it must never print as "installed".
        return self.status in ("installed", "already-installed")


@dataclass
class IntegrationAdapter:
    """One CLI's detection + install triple."""

    cli: str
    label: str
    is_available: Callable[[], bool]
    is_installed: Callable[[], bool]
    install: Callable[[], IntegrationResult]


def _run(cmd: list[str], timeout: int = 120) -> "subprocess.CompletedProcess[str]":
    """Run a command, capturing output, never raising.

    A vanished binary / timeout / OS error becomes a returncode-1 result so
    callers branch on the exit code alone - never on stdout text (a sibling CLI's
    "already installed" wording is not a contract; the exit code is).
    """
    try:
        return subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            errors="replace",  # non-UTF-8 installer output must not raise
            timeout=timeout,
            check=False,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
        return subprocess.CompletedProcess(cmd, returncode=1, stdout="", stderr=str(exc))


def _tail(text: Optional[str], n: int = 200) -> str:
    """Last n chars of an installer's stderr, trimmed, for a one-line reason."""
    if not text:
        return "no output"
    return text.strip()[-n:]


# --- claude -----------------------------------------------------------------

def _claude_skills_dir() -> Path:
    return Path.home() / ".claude" / "skills" / "fno"


def _claude_is_installed(run: Runner) -> bool:
    # The skills-dir fallback drop loads as fno@skills-dir; detect it by the
    # plugin manifest it lands.
    if (_claude_skills_dir() / ".claude-plugin" / "plugin.json").exists():
        return True
    res = run(["claude", "plugin", "list", "--json"])
    if res.returncode != 0:
        return False
    try:
        data = json.loads(res.stdout)
    except (ValueError, TypeError):
        return False
    # `claude plugin list --json` yields objects with an "id" of the form
    # "<plugin>@<marketplace>" (verified 2026-06-22), so footnote is "fno@footnote".
    if isinstance(data, list):
        return any(
            isinstance(p, dict) and str(p.get("id", "")).startswith("fno@")
            for p in data
        )
    return False


def _claude_install(run: Runner) -> IntegrationResult:
    label = "Claude Code"
    # Preferred path: marketplace add + plugin install. Probe for the `plugin`
    # subcommand first - an old `claude` lacks it entirely, route to skills-dir.
    if run(["claude", "plugin", "--help"]).returncode == 0:
        add = run(["claude", "plugin", "marketplace", "add", _MARKETPLACE])
        if add.returncode == 0:
            inst = run(["claude", "plugin", "install", "fno@footnote"])
            if inst.returncode == 0:
                return IntegrationResult("claude", label, "installed")
    # Fallback: clone the plugin into ~/.claude/skills/fno/ -> fno@skills-dir.
    # No postinstall and no `claude plugin update`, but a curl user already has
    # the CLI, so that is acceptable.
    return _claude_skills_dir_install(run)


def _claude_skills_dir_install(run: Runner) -> IntegrationResult:
    label = "Claude Code"
    dest = _claude_skills_dir()
    if (dest / ".claude-plugin" / "plugin.json").exists():
        return IntegrationResult("claude", label, "already-installed", note="skills-dir")
    # A prior clone that failed/timed out leaves a non-empty dest without a valid
    # plugin.json; `git clone` refuses to write into it. Clear the stale dir so a
    # re-run recovers (idempotency + transient-failure robustness).
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    # A full-repo shallow clone over a slow link can outrun the default 120s, so
    # give the one network-heavy step more room before it fails closed.
    clone = run(["git", "clone", "--depth", "1", _REPO_URL, str(dest)], timeout=300)
    if clone.returncode == 0:
        return IntegrationResult(
            "claude", label, "installed", note="skills-dir; no `claude plugin update`"
        )
    return IntegrationResult("claude", label, "failed", note=_tail(clone.stderr))


# --- gemini -----------------------------------------------------------------

def _gemini_is_installed(run: Runner) -> bool:
    res = run(["gemini", "extensions", "list"])
    if res.returncode != 0:
        return False
    return "footnote" in (res.stdout or "")


def _gemini_install(run: Runner) -> IntegrationResult:
    label = "Gemini CLI"
    res = run(["gemini", "extensions", "install", _REPO_URL])
    if res.returncode == 0:
        return IntegrationResult("gemini", label, "installed")
    return IntegrationResult("gemini", label, "failed", note=_tail(res.stderr))


# --- codex ------------------------------------------------------------------

def _codex_is_installed(run: Runner) -> bool:
    # `codex plugin marketplace add` only registers a marketplace SOURCE; the
    # plugin is installed separately (from Codex's plugin browser). A listed
    # marketplace is therefore NOT proof the integration is wired up, and codex
    # has no verified non-interactive installed-plugin query. So we never claim
    # codex is installed: the step is re-offered each run, and re-running the
    # (idempotent) marketplace add is harmless.
    return False


def _codex_install(run: Runner) -> IntegrationResult:
    label = "Codex CLI"
    # The only verified non-interactive step is registering the marketplace
    # source; the actual plugin install is done from Codex's plugin browser
    # (developers.openai.com/codex/plugins). So we register the source and report
    # a MANUAL finish - never "installed", which would be a false success.
    res = run(["codex", "plugin", "marketplace", "add", _MARKETPLACE])
    if res.returncode == 0:
        return IntegrationResult(
            "codex",
            label,
            "manual",
            note="marketplace registered; install the footnote plugin from "
            "Codex's plugin browser to finish",
        )
    return IntegrationResult("codex", label, "failed", note=_tail(res.stderr))


# --- opencode ---------------------------------------------------------------
# OpenCode is a loop-wrapper harness (scripts/lib/driver-opencode.sh), not a
# native plugin-marketplace CLI. Its integration is a local-file plugin copied
# into OpenCode's plugin dir - no npm publish needed (OpenCode loads .js files
# from ~/.config/opencode/plugins/ directly). Unlike codex, the installed state
# is verifiable (the file exists and matches the shipped source), so we can
# claim "installed" honestly.

def _opencode_plugin_src() -> Path:
    return Path(__file__).parent / "assets" / "opencode" / "footnote.js"


def _opencode_plugins_dir() -> Path:
    return Path.home() / ".config" / "opencode" / "plugins"


def _opencode_plugin_dest() -> Path:
    return _opencode_plugins_dir() / "footnote.js"


def _opencode_is_installed() -> bool:
    # Installed == the dest file exists AND matches the shipped source, so a
    # stale copy (older footnote) reports not-installed and gets refreshed.
    dest = _opencode_plugin_dest()
    if not dest.exists():
        return False
    try:
        return dest.read_text(encoding="utf-8") == _opencode_plugin_src().read_text(
            encoding="utf-8"
        )
    except OSError:
        return False


def _opencode_install() -> IntegrationResult:
    label = "OpenCode"
    src = _opencode_plugin_src()
    dest = _opencode_plugin_dest()
    try:
        src_text = src.read_text(encoding="utf-8")
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(src_text, encoding="utf-8")
    except OSError as exc:
        return IntegrationResult("opencode", label, "failed", note=str(exc))
    return IntegrationResult("opencode", label, "installed", note=f"plugin -> {dest}")


def build_adapters(run: Runner = _run) -> "list[IntegrationAdapter]":
    """The adapter registry: claude (preferred + skills-dir fallback), gemini,
    codex (native marketplace CLIs), and opencode (local-file plugin copy).
    hermes / openclaw remain absent - their install surfaces are unverified, and
    printing a command that does not exist is worse than omitting them (locked
    decision 4).
    """
    return [
        IntegrationAdapter(
            "claude",
            "Claude Code",
            is_available=lambda: shutil.which("claude") is not None,
            is_installed=lambda: _claude_is_installed(run),
            install=lambda: _claude_install(run),
        ),
        IntegrationAdapter(
            "gemini",
            "Gemini CLI",
            is_available=lambda: shutil.which("gemini") is not None,
            is_installed=lambda: _gemini_is_installed(run),
            install=lambda: _gemini_install(run),
        ),
        IntegrationAdapter(
            "codex",
            "Codex CLI",
            is_available=lambda: shutil.which("codex") is not None,
            is_installed=lambda: _codex_is_installed(run),
            install=lambda: _codex_install(run),
        ),
        IntegrationAdapter(
            "opencode",
            "OpenCode",
            is_available=lambda: shutil.which("opencode") is not None,
            is_installed=_opencode_is_installed,
            install=_opencode_install,
        ),
    ]


def run_cli_integration(
    *,
    select_fn: "Callable[[list[dict[str, object]]], list[str]]",
    echo_fn: Callable[[str], None] = lambda _m: None,
    adapters: "Optional[list[IntegrationAdapter]]" = None,
) -> "list[IntegrationResult]":
    """Interactive-agnostic core of the ``fno setup`` CLI-integration step.

    Detects agent CLIs on PATH, pre-marks already-installed integrations, asks
    ``select_fn`` which of the not-yet-installed CLIs to wire up, and runs each
    selected installer - echoing a visible result line for every one (no silent
    installs). Returns the per-CLI ``IntegrationResult`` list.

    ``select_fn(options) -> [cli]`` where ``options`` is a list of
    ``{"cli", "label", "installed"}`` dicts (already-installed rows are passed so
    the UI can grey them out; selecting one is a no-op).
    """
    adapters = build_adapters() if adapters is None else adapters

    available = [a for a in adapters if a.is_available()]
    unavailable = [a for a in adapters if a not in available]
    if unavailable:
        echo_fn(
            "  skipped (not on PATH): "
            + ", ".join(a.label for a in unavailable)
        )
    if not available:
        echo_fn("  no agent CLIs detected on PATH - skipping integration step.")
        return []

    options = []
    for a in available:
        installed = a.is_installed()
        options.append({"cli": a.cli, "label": a.label, "installed": installed})
        if installed:
            echo_fn(f"  {a.label}: already installed")

    installed_clis = {o["cli"] for o in options if o["installed"]}
    selected = set(select_fn(options))
    # Invariant: install only a CHECKED, AVAILABLE, NOT-already-installed CLI.
    to_install = [o["cli"] for o in options if o["cli"] in selected and o["cli"] not in installed_clis]

    if not to_install:
        echo_fn("  nothing to install.")
        return []

    by_cli = {a.cli: a for a in available}
    results = []
    for cli in to_install:
        adapter = by_cli[cli]
        echo_fn(f"  {adapter.label}: installing...")
        # One installer's failure must never abort the rest (Errors).
        res = adapter.install()
        results.append(res)
        if res.ok:
            detail = f" ({res.note})" if res.note else ""
            echo_fn(f"  {adapter.label}: installed{detail}")
        elif res.status == "manual":
            # A step succeeded but a manual finish is required - say so plainly,
            # never "installed".
            echo_fn(f"  {adapter.label}: needs a manual finish - {res.note}")
        else:
            echo_fn(f"  {adapter.label}: FAILED ({res.note})")
    return results
