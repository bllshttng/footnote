"""CLI-integration installers for `fno config setup` (the agent-door opt-in).

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

# A subprocess runner: takes an argv list plus subprocess options and returns a
# CompletedProcess. `...` keeps the optional kwargs in the contract.
Runner = Callable[..., "subprocess.CompletedProcess[str]"]


@dataclass
class IntegrationResult:
    """Outcome of one CLI's integration install."""

    cli: str  # "claude" | "gemini" | "codex" | "opencode" | "agy"
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


def _run(
    cmd: list[str],
    timeout: int = 120,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
) -> "subprocess.CompletedProcess[str]":
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
            cwd=cwd,
            env=env,
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
    # No `claude plugin update`, but a curl user already has
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
    from fno.setup.codex_plugin import inspect_freshness

    return inspect_freshness(runner=run).get("status") == "fresh"


def _codex_install(run: Runner) -> IntegrationResult:
    label = "Codex CLI"
    from fno.setup.codex_plugin import CodexPluginError, converge

    try:
        result = converge(channel="release", runner=run)
    except CodexPluginError as exc:
        return IntegrationResult("codex", label, "failed", note=str(exc))
    return IntegrationResult(
        "codex",
        label,
        "already-installed" if result.action == "no-op" else "installed",
        note=f"{result.plugin_id} {result.version}; start a new Codex session",
    )


# --- opencode ---------------------------------------------------------------
# OpenCode is a loop-wrapper harness (scripts/lib/driver-opencode.sh). Its
# integration is one fno-agents install (opencode_install.rs) that writes the
# stop bridge, generated fno:<verb> command files, translated agent files and
# the skill trees into the directories OpenCode already scans in the global
# config dir. The Python side reads JSON receipts through the fno-agents door
# and owns no install logic of its own; the receipt decides "installed".


def _opencode_status():
    """One door round-trip: (error, receipt) from the fno-agents opencode arm.

    The flags ride AHEAD of the harness word: a deployed binary older than
    this change parses the first flag as the mode, lands on "unknown
    harness", and refuses - so a stale binary can answer a PROBE with an
    install, never the reverse."""
    from fno.rust_binary import call_binary_json

    return call_binary_json("plugin-install", ["--installed", "--json", "opencode"])


def _opencode_is_installed() -> bool:
    err, receipt = _opencode_status()
    return (
        err is None
        and isinstance(receipt, dict)
        and receipt.get("status") == "installed"
    )


def _opencode_install() -> IntegrationResult:
    label = "OpenCode"
    from fno.rust_binary import call_binary_json

    err, receipt = call_binary_json("plugin-install", ["--json", "opencode"])
    if err is not None:
        return IntegrationResult("opencode", label, "failed", note=str(err))
    if not isinstance(receipt, dict):
        return IntegrationResult(
            "opencode", label, "failed", note="unreadable install receipt"
        )
    kept = receipt.get("kept") or []
    note = "{} file(s) (footnote {}) -> {}".format(
        receipt.get("written", 0),
        receipt.get("version", "?"),
        receipt.get("config_dir", "?"),
    )
    if kept:
        note += "; kept user files: " + ", ".join(str(k) for k in kept)
    status = receipt.get("status")
    return IntegrationResult(
        "opencode",
        label,
        "installed" if status in ("installed", "partial") else "failed",
        note=note,
    )


# --- pi ---------------------------------------------------------------------
# pi is an extension harness (like opencode, not a plugin-marketplace CLI),
# measured 2026-08-28 as shipping no shell hook surface at all: its lifecycle
# boundary is the in-process `pi.on("agent_settled")` extension event. The
# integration is a local-file TypeScript extension copied into pi's global
# extension dir (~/.pi/agent/extensions/ - pi auto-discovers *.ts there, one
# install covers every project). Unlike codex, the installed state is
# verifiable (the file exists and matches the shipped source), so we can
# claim "installed" honestly.

def _pi_extension_src() -> Path:
    return Path(__file__).parent / "assets" / "pi" / "footnote.ts"


def _pi_is_installed() -> bool:
    # True only when the dest exists AND matches the shipped source, so a
    # stale copy reports not-installed; the read is the Rust pi arm.
    from fno.rust_binary import call_binary_json

    _err, payload = call_binary_json(
        "plugin-install",
        ["pi", "--status", "--extension-src", str(_pi_extension_src()), "--json"],
    )
    return bool(payload and payload.get("installed"))


def _pi_install() -> IntegrationResult:
    from fno.rust_binary import call_binary_json

    label = "pi"
    _err, payload = call_binary_json(
        "plugin-install",
        ["pi", "--extension-src", str(_pi_extension_src()), "--json"],
    )
    if payload and payload.get("installed"):
        note = "extension -> {}, skills: {}".format(
            payload.get("dest", "?"), payload.get("skills", "unresolved")
        )
        return IntegrationResult("pi", label, "installed", note=note)
    detail = (payload.get("refused") if payload else _err) or (
        "the fno-agents binary is missing or failed; run fno doctor update --rust"
    )
    return IntegrationResult("pi", label, "failed", note=str(detail))


# --- agy (Antigravity CLI) --------------------------------------------------
# agy is a native Stop-hook harness (like opencode, not a plugin-marketplace CLI).
# Its hooks are Claude-shaped event names with a Gemini-family wire format, so the
# integration registers footnote's Stop adapter in agy's hooks.json customization
# file (~/.gemini/config/hooks.json, the global dir - one install covers every
# project). The command references the adapter that ships in the plugin
# (hooks/agy-target-stop-hook.sh), resolved via the plugin-root pointer; a CLI-only
# install (uv/curl) carries no hooks/, so it degrades to a "manual" finish rather
# than wiring a path that does not exist.

def _agy_hooks_json() -> Path:
    return Path.home() / ".gemini" / "config" / "hooks.json"


def _agy_adapter_path() -> "Optional[Path]":
    # The adapter ships in the plugin (hooks/), which the uv/curl wheel does NOT
    # carry. resolve_plugin_script always returns a path (last fallback may not
    # exist), so gate on is_file(): None means "not in this install" -> manual.
    from fno.paths import resolve_plugin_script

    p = resolve_plugin_script("hooks/agy-target-stop-hook.sh")
    return p if p.is_file() else None


def _agy_crown_adapter_path() -> "Optional[Path]":
    # Same load-shape as _agy_adapter_path for the PreInvocation crown adapter.
    # agy has no session-start event, so the crown line rides PreInvocation
    # gated on invocationNum == 0 (first model call == session start).
    from fno.paths import resolve_plugin_script

    p = resolve_plugin_script("hooks/agy-crown-inject.sh")
    return p if p.is_file() else None


def _agy_is_installed() -> bool:
    from fno.rust_binary import call_binary_json

    adapter = _agy_adapter_path()
    crown = _agy_crown_adapter_path()
    args = [
        "agy",
        "--hooks-status",
        "--hooks-file",
        str(_agy_hooks_json()),
        "--json",
    ]
    if adapter is not None:
        args += ["--adapter", str(adapter)]
    if crown is not None:
        args += ["--crown", str(crown)]
    error, payload = call_binary_json("plugin-install", args)
    if error is not None or not isinstance(payload, dict):
        return False
    return bool(payload.get("installed"))


def _agy_install() -> IntegrationResult:
    label = "Antigravity CLI"
    adapter = _agy_adapter_path()
    if adapter is None:
        return IntegrationResult(
            "agy",
            label,
            "manual",
            note="adapter ships in the plugin (not this CLI-only install); wire "
            "hooks/agy-target-stop-hook.sh into ~/.gemini/config/hooks.json by hand",
        )
    from fno.rust_binary import call_binary_json

    # Probe first: a stale fno-agents binary IGNORES unknown flags and would
    # fall through to the old full plugin install. A current one answers
    # --hooks-status with a JSON status object.
    hooks_file = _agy_hooks_json()
    error, probe = call_binary_json(
        "plugin-install",
        ["agy", "--hooks-status", "--hooks-file", str(hooks_file), "--json"],
    )
    if error is not None or not isinstance(probe, dict) or "file" not in probe:
        return IntegrationResult(
            "agy",
            label,
            "failed",
            note="the fno-agents binary does not answer --hooks-status; run "
            "`fno doctor update --rust`",
        )
    args = [
        "agy",
        "--hooks",
        "--adapter",
        str(adapter),
        "--hooks-file",
        str(hooks_file),
        "--json",
    ]
    crown = _agy_crown_adapter_path()
    if crown is not None:
        args += ["--crown", str(crown)]
    error, payload = call_binary_json("plugin-install", args)
    if error is not None:
        if "not found" in error:
            note = "fno-agents binary not found; run `fno doctor update --rust`"
        else:
            note = error
        return IntegrationResult("agy", label, "failed", note=note)
    note = "Stop hook installed"
    if isinstance(payload, dict):
        note = payload.get("note") or note
    return IntegrationResult("agy", label, "installed", note=note)


def build_adapters(run: Runner = _run) -> "list[IntegrationAdapter]":
    """The adapter registry: claude (preferred + skills-dir fallback), gemini,
    codex (native marketplace CLIs), opencode (local-file plugin copy), and agy
    (native Stop-hook registration). hermes / openclaw remain absent - their
    install surfaces are unverified, and printing a command that does not exist is
    worse than omitting them (locked decision 4).
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
        IntegrationAdapter(
            "pi",
            "pi",
            is_available=lambda: shutil.which("pi") is not None,
            is_installed=_pi_is_installed,
            install=_pi_install,
        ),
        IntegrationAdapter(
            "agy",
            "Antigravity CLI",
            is_available=lambda: shutil.which("agy") is not None,
            is_installed=_agy_is_installed,
            install=_agy_install,
        ),
    ]


def run_cli_integration(
    *,
    select_fn: "Callable[[list[dict[str, object]]], list[str]]",
    echo_fn: Callable[[str], None] = lambda _m: None,
    adapters: "Optional[list[IntegrationAdapter]]" = None,
) -> "list[IntegrationResult]":
    """Interactive-agnostic core of the ``fno config setup`` CLI-integration step.

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
