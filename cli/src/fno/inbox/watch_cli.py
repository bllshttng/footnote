"""Launchd plist renderer and Typer subapp for the abi-watch daemon.

Task 5.2: render_plist helper.
Task 5.3: install/uninstall/status Typer verbs.
"""
import os
import re
import subprocess
from pathlib import Path

import typer

_PROJECT_RE = re.compile(r"^[a-zA-Z0-9_-]+$")

# Path from this file: cli/src/fno/inbox/watch_cli.py
# parents[0] = inbox/
# parents[1] = abilities/
# parents[2] = src/
# parents[3] = cli/
# parents[4] = repo root
_REPO_ROOT = Path(__file__).parents[4]
_TEMPLATE_PATH = _REPO_ROOT / "scripts" / "templates" / "com.fno.watch.plist"
_ABI_WATCH_PATH = _REPO_ROOT / "scripts" / "abi-watch.sh"


def render_plist(project: str, repo_root: Path) -> str:
    """Render the launchd plist template for a given project and repo root.

    Args:
        project: Project name. Must match ``^[a-zA-Z0-9_-]+$``.
        repo_root: Absolute path to the repository root.

    Returns:
        Rendered plist XML as a string.

    Raises:
        ValueError: If ``project`` contains shell-special characters.
    """
    if not _PROJECT_RE.match(project):
        raise ValueError(
            f"project name must match [a-zA-Z0-9_-]+: {project!r}"
        )

    template = _TEMPLATE_PATH.read_text(encoding="utf-8")

    rendered = (
        template
        .replace("{{PROJECT}}", project)
        .replace("{{REPO_ROOT}}", str(repo_root))
        .replace("{{HOME}}", str(Path.home()))
        .replace("{{FNO_WATCH_PATH}}", str(_ABI_WATCH_PATH))
    )
    return rendered


# ---------------------------------------------------------------------------
# Typer subapp (Task 5.3)
# ---------------------------------------------------------------------------

watch_app = typer.Typer(
    name="watch",
    help="Manage the per-project headless mail-drain daemon.",
    no_args_is_help=True,
)


def _launch_agents_dir() -> Path:
    """Resolve the LaunchAgents dir, honoring FNO_LAUNCH_AGENTS_DIR for tests."""
    override = os.environ.get("FNO_LAUNCH_AGENTS_DIR")
    if override:
        return Path(override)
    return Path.home() / "Library" / "LaunchAgents"


def _plist_path(project: str) -> Path:
    return _launch_agents_dir() / f"com.fno.watch.{project}.plist"


# Legacy launchd label used before the OSS de-branding rename (PR #428).
# install/uninstall/status must still find and unload it so a watcher
# installed under the old name is not orphaned (stale daemon keeps running)
# or double-loaded alongside the new one. Codex review on PR #428.
_LEGACY_LABEL = "com.bllshttng.fno.watch"


def _legacy_plist_path(project: str) -> Path:
    return _launch_agents_dir() / f"{_LEGACY_LABEL}.{project}.plist"


def _cleanup_legacy_watcher(project: str) -> bool:
    """Unload + remove a pre-rename watcher plist if present.

    Returns True if a legacy plist was found and removed. Best-effort: a
    failed unload still proceeds to unlink so a half-loaded legacy agent
    cannot wedge install/uninstall.
    """
    legacy = _legacy_plist_path(project)
    if not legacy.exists():
        return False
    subprocess.run(
        ["launchctl", "unload", str(legacy)], check=False, capture_output=True, text=True
    )
    legacy.unlink(missing_ok=True)
    return True


def _resolve_project_and_root() -> tuple[str, Path]:
    repo_root = Path.cwd().resolve()
    project = repo_root.name
    return project, repo_root


@watch_app.command("install")
def cmd_install() -> None:
    """Create and load the launchd plist for this project."""
    from fno.inbox.settings import read_watch_settings

    project, repo_root = _resolve_project_and_root()
    settings = read_watch_settings(repo_root)
    if not settings.enabled:
        typer.echo(
            "config.inbox.watch.enabled must be true; edit settings.yaml or run /setup",
            err=True,
        )
        raise typer.Exit(code=1)

    # Remove any pre-rename watcher first so we don't double-load two daemons
    # for the same repo (Codex review, PR #428).
    if _cleanup_legacy_watcher(project):
        typer.echo(f"removed legacy watcher: {_legacy_plist_path(project)}")

    plist_path = _plist_path(project)
    plist_path.parent.mkdir(parents=True, exist_ok=True)
    rendered = render_plist(project, repo_root)
    plist_path.write_text(rendered, encoding="utf-8")
    res = subprocess.run(
        ["launchctl", "load", str(plist_path)], check=False, capture_output=True, text=True
    )
    if res.returncode != 0:
        typer.echo(
            f"launchctl load failed (rc={res.returncode}): {res.stderr.strip()}", err=True
        )
        plist_path.unlink(missing_ok=True)
        raise typer.Exit(code=2)
    typer.echo(f"installed: {plist_path}")


@watch_app.command("uninstall")
def cmd_uninstall() -> None:
    """Unload and remove the launchd plist for this project."""
    project, _ = _resolve_project_and_root()
    plist_path = _plist_path(project)
    removed_any = False
    if plist_path.exists():
        res = subprocess.run(
            ["launchctl", "unload", str(plist_path)], check=False, capture_output=True, text=True
        )
        if res.returncode != 0:
            typer.echo(
                f"launchctl unload failed (rc={res.returncode}): {res.stderr.strip()}", err=True
            )
            raise typer.Exit(code=2)
        plist_path.unlink()
        typer.echo(f"uninstalled: {plist_path}")
        removed_any = True
    # Also remove a pre-rename watcher so the old daemon doesn't keep running
    # after a de-branding upgrade (Codex review, PR #428).
    if _cleanup_legacy_watcher(project):
        typer.echo(f"removed legacy watcher: {_legacy_plist_path(project)}")
        removed_any = True
    if not removed_any:
        typer.echo(f"already uninstalled: {plist_path}")


@watch_app.command("status")
def cmd_status() -> None:
    """Report whether the daemon is loaded and show the most recent log line."""
    project, repo_root = _resolve_project_and_root()
    plist_path = _plist_path(project)

    res = subprocess.run(
        ["launchctl", "list", f"com.fno.watch.{project}"],
        capture_output=True,
        text=True,
        check=False,
    )
    loaded = "yes" if res.returncode == 0 else "no"

    legacy_res = subprocess.run(
        ["launchctl", "list", f"{_LEGACY_LABEL}.{project}"],
        capture_output=True,
        text=True,
        check=False,
    )
    if legacy_res.returncode == 0:
        typer.echo(
            f"warning: legacy watcher {_LEGACY_LABEL}.{project} still loaded; "
            "run 'fno watch uninstall' to remove it",
            err=True,
        )

    log_path = repo_root / ".fno" / "abi-watch.log"
    last_line = ""
    if log_path.exists():
        try:
            with log_path.open("rb") as f:
                f.seek(0, 2)
                size = f.tell()
                read_back = min(size, 4096)
                f.seek(size - read_back)
                tail = f.read().decode("utf-8", errors="replace")
                last_line = tail.strip().splitlines()[-1] if tail.strip() else ""
        except OSError:
            last_line = ""

    typer.echo(f"loaded: {loaded}")
    typer.echo(f"plist: {plist_path}")
    if last_line:
        typer.echo(f"last log: {last_line}")
