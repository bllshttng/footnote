"""``fno config plugin install <claude|codex|opencode|agy> [--force]``.

One local-dev install verb for every plugin harness (x-7ca7 task 3.2). It
replaces the per-harness setup verbs (the retired ``fno config setup
codex-plugin``) and installs from the FILTERED stage
(``fno.setup.plugin_stage.build_stage``), never from the repo root: the
harness caches copy whatever they are pointed at, and pointed at the root
they copied 19-22 GB of build output into ``~/.claude`` and ``~/.codex``.

Each install also exports ``CARGO_BUILD_BUILD_DIR`` to the surfaces the
harness spawns shells from (Claude settings ``env``, Codex
``shell_environment_policy``, the user's rc), so harness-shell cargo builds
keep intermediates out of the checkout like every other build.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import tomllib
from pathlib import Path

import typer

plugin_app = typer.Typer(help="Install the footnote plugin into a harness (from the filtered stage)")


# --- stage -------------------------------------------------------------------

def _build_stage() -> Path:
    from fno.setup.plugin_stage import build_stage

    return build_stage()


# --- env exports -------------------------------------------------------------

_BUILD_DIR_KEY = "CARGO_BUILD_BUILD_DIR"
_RC_MARK = "# fno: cargo build-dir (x-7ca7)"


def _build_dir_value() -> str:
    from fno.paths import cargo_build_dir_value

    return cargo_build_dir_value()


def _claude_settings_env() -> Path:
    return Path.home() / ".claude" / "settings.json"


def _export_claude_env() -> None:
    path = _claude_settings_env()
    data: dict = {}
    if path.is_file():
        try:
            loaded = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(loaded, dict):
                data = loaded
        except (ValueError, OSError):
            data = {}
    env = data.get("env")
    if not isinstance(env, dict):
        env = {}
    env[_BUILD_DIR_KEY] = _build_dir_value()
    data["env"] = env
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def _codex_config() -> Path:
    home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser()
    return home / "config.toml"


def _export_codex_env() -> None:
    import tomli_w

    path = _codex_config()
    document: dict = {}
    if path.is_file():
        try:
            document = tomllib.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, tomllib.TOMLDecodeError):
            document = {}
    policy = document.setdefault("shell_environment_policy", {})
    if not isinstance(policy, dict):
        policy = {}
        document["shell_environment_policy"] = policy
    applied = policy.setdefault("set", {})
    if not isinstance(applied, dict):
        applied = {}
        policy["set"] = applied
    applied[_BUILD_DIR_KEY] = _build_dir_value()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(tomli_w.dumps(document), encoding="utf-8")


def _export_rc_env() -> Optional[Path]:
    """Append the export to the user's shell rc (idempotent via the marker)."""
    from fno.setup.starship import default_shell_rc

    rc = default_shell_rc()
    line = f"{_RC_MARK}\nexport {_BUILD_DIR_KEY}=\"{_build_dir_value()}\""
    try:
        existing = rc.read_text(encoding="utf-8") if rc.exists() else ""
    except (OSError, UnicodeDecodeError):
        return None
    if _RC_MARK in existing:
        return rc
    try:
        rc.parent.mkdir(parents=True, exist_ok=True)
        rc.write_text(existing.rstrip("\n") + ("\n" if existing.strip() else "") + line + "\n", encoding="utf-8")
    except OSError:
        return None
    return rc


def _export_env_everywhere() -> str:
    notes = []
    try:
        _export_claude_env()
        notes.append("claude settings env")
    except OSError as exc:
        notes.append(f"claude env FAILED ({exc})")
    try:
        _export_codex_env()
        notes.append("codex shell_environment_policy")
    except OSError as exc:
        notes.append(f"codex env FAILED ({exc})")
    rc = _export_rc_env()
    notes.append(f"rc ({rc})" if rc else "rc skipped")
    return "; ".join(notes)


# --- stale copies ------------------------------------------------------------

def _remove_stale_copies() -> list[str]:
    removed = []
    candidates = [
        Path.home() / ".gemini" / "config" / "plugins" / "footnote",
        Path.home() / ".codex" / "plugins" / "cache" / "footnote-local",
    ]
    for path in candidates:
        if path.exists():
            shutil.rmtree(path, ignore_errors=True)
            removed.append(str(path))
    return removed


# --- per-harness installs ----------------------------------------------------

def _run_checked(cmd: list[str]) -> str:
    res = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if res.returncode != 0:
        detail = (res.stderr or res.stdout or "").strip()[-400:]
        raise RuntimeError(f"{' '.join(cmd)} exited {res.returncode}: {detail}")
    return (res.stdout or "").strip()


def _install_claude(stage: Path, force: bool) -> str:
    _run_checked(["claude", "plugin", "marketplace", "add", str(stage)])
    if force:
        # `claude plugin install` has no --force; the documented refresh is
        # `claude plugin update` (the doctor staleness remedy names it).
        _run_checked(["claude", "plugin", "update", "fno@footnote"])
        return f"updated fno@footnote from {stage}"
    try:
        _run_checked(["claude", "plugin", "install", "fno@footnote"])
    except RuntimeError as exc:
        if "already installed" not in exc.args[0]:
            raise
        _run_checked(["claude", "plugin", "update", "fno@footnote"])
        return f"updated fno@footnote from {stage}"
    return f"installed fno@footnote from {stage}"


def _install_codex(stage: Path, force: bool) -> str:
    from fno.setup.codex_plugin import CodexPluginError, converge

    try:
        result = converge(channel="dev", refresh=force, source_root=stage)
    except CodexPluginError as exc:
        raise RuntimeError(f"{exc.stage}: {exc.detail}") from exc
    return f"converged {result.plugin_id} {result.version} (action={result.action})"


def _install_opencode(stage: Path, force: bool) -> str:
    del stage, force  # the plugin is one shipped file, not the checkout
    from fno.setup.integration import _opencode_install

    result = _opencode_install()
    if not result.ok:
        raise RuntimeError(result.note or result.status)
    return result.note or "installed"


def _install_agy(stage: Path, force: bool) -> str:
    from fno.setup.integration import _agy_install

    result = _agy_install()
    if result.status == "failed":
        raise RuntimeError(result.note)
    notes = [f"hooks: {result.note or result.status}"]
    cmd = ["agy", "plugin", "install", str(stage)]
    if force:
        cmd.append("--force")
    notes.append(_run_checked(cmd) or "agy plugin installed")
    return "; ".join(notes)


_INSTALLERS = {
    "claude": _install_claude,
    "codex": _install_codex,
    "opencode": _install_opencode,
    "agy": _install_agy,
}


@plugin_app.command("install")
def install(
    harness: str = typer.Argument(..., help="claude | codex | opencode | agy"),
    force: bool = typer.Option(
        False,
        "--force",
        help="Refresh the install even when the harness already has this version.",
    ),
) -> None:
    """Install the footnote plugin from the filtered stage (no build output)."""
    installer = _INSTALLERS.get(harness)
    if installer is None:
        typer.echo(f"unknown harness '{harness}'; want one of: {', '.join(_INSTALLERS)}", err=True)
        raise typer.Exit(2)
    try:
        stage = _build_stage()
        detail = installer(stage, force)
    except (RuntimeError, FileNotFoundError) as exc:
        typer.echo(f"plugin install {harness} FAILED: {exc}", err=True)
        raise typer.Exit(1) from exc
    env_note = _export_env_everywhere()
    stale = _remove_stale_copies()
    typer.echo(f"plugin install {harness}: {detail}")
    typer.echo(f"build-dir env exported to: {env_note}")
    if stale:
        typer.echo(f"removed stale copies: {', '.join(stale)}")
