"""fno update: reinstall the fno CLI from its source path.

Discovers the source via (in priority order):

1. ``--source`` flag override
2. ``FNO_SOURCE`` env var
3. ``~/.fno/source-path`` cache (written on prior successful install)
4. Well-known candidate paths (plugin install, common dev locations)

Then execs ``uv tool install --reinstall <source>`` (or ``pip install --user
--force-reinstall <source>`` if uv is unavailable). Uses ``os.execvp`` so the
installer replaces this Python process cleanly, avoiding the "binary being
replaced while it runs" race.
"""
from __future__ import annotations

import logging
import os
import shlex
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Literal, Optional

import typer

try:
    from fno import paths as _paths
    _CACHE_FILE = _paths.state_dir() / "source-path"
except Exception:
    _CACHE_FILE = Path.home() / ".fno" / "source-path"

# Records the source git rev that the *current* install was built from, so
# `fno doctor` can detect installed-vs-source skew (ab-5a1fc285). Sibling of
# the source-path cache; monkeypatched in tests the same way as _CACHE_FILE.
_INSTALLED_REV_FILE = _CACHE_FILE.parent / "installed-rev"

# Records the last git commit that touched crates/ - the marker doctor.py reads
# to decide whether cargo-installed rust bins are fresh relative to source.
# Stored separately from _INSTALLED_REV_FILE because a Python-only commit must
# not flag the rust bins stale.
_RUST_MARKER_FILE = _CACHE_FILE.parent / "installed-rust-rev"

_log = logging.getLogger(__name__)

RefreshOutcome = Literal[
    "refreshed", "refreshed-no-marker", "fresh", "failed", "dry-run",
    "skipped-no-crate", "skipped-no-binary", "skipped-no-rev", "skipped-no-cargo",
]

_GUARD_MSG = (
    "[fno update] refused: target-state.md shows status: IN_PROGRESS. "
    "Updating mid-loop risks binary skew across subprocesses. "
    "Pass --force to override."
)


def _target_in_progress() -> bool:
    """Return True if target-state.md in the current repo shows status: IN_PROGRESS.

    Uses paths.resolve_repo_root() so FNO_REPO_ROOT env var and git rev-parse
    fallbacks are honoured. Lenient on missing or malformed files (returns False).
    """
    try:
        from fno.paths import resolve_repo_root
        repo_root = resolve_repo_root()
    except Exception:
        return False

    state_path = repo_root / ".fno" / "target-state.md"
    if not state_path.exists():
        return False

    try:
        content = state_path.read_text(encoding="utf-8")
    except OSError as exc:
        # Fail safe: an unreadable state file may hide an active loop.
        # Treat as IN_PROGRESS rather than opening the gate on a filesystem error.
        _log.warning(
            "target-state.md at %s could not be read (%s); assuming IN_PROGRESS for safety",
            state_path,
            exc,
        )
        return True

    # Parse YAML front-matter between the first two `---` lines.
    parts = content.split("---", 2)
    if len(parts) < 3:
        # No proper frontmatter delimiters - treat as not IN_PROGRESS (lenient).
        _log.warning("target-state.md has no YAML front-matter; skipping guard check")
        return False

    frontmatter = parts[1]
    return "status: IN_PROGRESS" in frontmatter


# Search order matters: plugin install first (most users), then dev clone.
_CANDIDATE_PATHS = (
    Path.home() / ".claude" / "plugins" / "fno" / "cli",
    Path.home() / "code" / "me" / "fno" / "cli",
)


class SourceNotFoundError(Exception):
    """Raised when the fno source path cannot be located."""


def _looks_like_abi_source(path: Path) -> bool:
    """True if path contains a pyproject.toml declaring ``[project] name = "fno"``.

    Parses the TOML rather than substring-matching so a stray ``name = "fno"``
    outside the ``[project]`` table (in a dependency list, a tool subsection, etc.)
    cannot false-match. Returns False for any read/parse failure - this is a
    "looks like" check, not a validator.
    """
    pyproject = path / "pyproject.toml"
    if not pyproject.is_file():
        return False
    try:
        with pyproject.open("rb") as f:
            data = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError):
        return False
    project = data.get("project") if isinstance(data, dict) else None
    if not isinstance(project, dict):
        return False
    return project.get("name") == "fno"


def _discover_source(override: Optional[Path] = None) -> Path:
    """Locate the fno CLI source directory.

    The override path (``--source``) is trusted-but-validated: if the user
    explicitly points us at a directory, we surface a precise error when that
    directory doesn't look right, rather than silently falling through to
    other candidates.
    """
    if override is not None:
        path = override.expanduser().resolve()
        if not _looks_like_abi_source(path):
            raise SourceNotFoundError(
                f"--source {path} does not contain a pyproject.toml with "
                "name = 'fno'. Pass a path to the fno CLI source directory."
            )
        return path

    candidates: list[Path] = []

    env_source = os.environ.get("FNO_SOURCE")
    if env_source:
        candidates.append(Path(env_source).expanduser().resolve())

    if _CACHE_FILE.is_file():
        try:
            cached = _CACHE_FILE.read_text(encoding="utf-8").strip()
            if cached:
                candidates.append(Path(cached).expanduser().resolve())
        except OSError:
            pass

    candidates.extend(p.expanduser().resolve() for p in _CANDIDATE_PATHS)

    seen: set[Path] = set()
    for path in candidates:
        if path in seen:
            continue
        seen.add(path)
        if _looks_like_abi_source(path):
            return path

    raise SourceNotFoundError(
        "Could not locate the fno CLI source. Pass --source /path/to/abilities/cli, "
        "set $FNO_SOURCE, or install the fno plugin into "
        "~/.claude/plugins/abilities/."
    )


def _cache_source_path(source: Path) -> None:
    """Write the resolved source to the cache. Best-effort; failures are silent."""
    try:
        _CACHE_FILE.parent.mkdir(parents=True, exist_ok=True)
        _CACHE_FILE.write_text(f"{source}\n", encoding="utf-8")
    except OSError:
        pass


def _source_rev(source: Path) -> Optional[str]:
    """Return ``git rev-parse HEAD`` of the source checkout, or None on failure.

    Network-free. A detached/corrupt/non-git source (or a missing ``git``)
    yields None so the caller records no marker rather than a bogus rev
    (Failure Modes: "preserve a clean exit when git rev-parse fails").
    """
    try:
        result = subprocess.run(
            ["git", "-C", str(source), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, FileNotFoundError):
        return None
    rev = result.stdout.strip()
    if result.returncode == 0 and rev:
        return rev
    return None


def _write_installed_rev(rev: str) -> None:
    """Atomically record ``rev`` to the installed-rev marker. Best-effort.

    Writes a temp file in the marker's own directory then ``os.replace``s it
    into place, so a concurrent ``fno doctor`` read never sees a torn or empty
    value (Invariant: atomic marker write). Used on the Windows path; the Unix
    path chains an equivalent atomic write into the installer via the shell
    (see :func:`_install_then_mark`) because ``os.execvp`` never returns.
    """
    target = _INSTALLED_REV_FILE
    tmp = None
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.parent / f".installed-rev.{os.getpid()}.tmp"
        tmp.write_text(f"{rev}\n", encoding="utf-8")
        os.replace(tmp, target)
    except OSError:
        # Don't leave the temp behind if write_text succeeded but os.replace failed.
        if tmp is not None and tmp.exists():
            try:
                tmp.unlink()
            except OSError:
                pass


def _rust_subtree_rev(source: Path) -> Optional[str]:
    """Return the last commit SHA that touched crates/, or None on any failure.

    Rationale: the marker stores the last commit that TOUCHED crates/, not HEAD,
    so Python-only commits never flag the rust bins as stale. ``source`` is the
    cli/ dir; its parent is the repo root in both dev-clone and plugin layouts.
    Mirror ``_source_rev``'s defensive style exactly - a missing/non-git source
    yields None so the caller records no marker rather than a bogus rev.
    """
    try:
        result = subprocess.run(
            ["git", "-C", str(source.parent), "log", "-1", "--format=%H", "--", "crates/"],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, FileNotFoundError):
        return None
    rev = result.stdout.strip()
    if result.returncode == 0 and rev:
        return rev
    return None


def _read_rust_marker() -> Optional[str]:
    """Return the content of the rust-marker file, or None if missing/empty."""
    try:
        content = _RUST_MARKER_FILE.read_text(encoding="utf-8").strip()
        return content if content else None
    except OSError:
        return None


def _write_rust_marker(rev: str) -> bool:
    """Atomically record ``rev`` to the rust-marker file. Best-effort; never raises.

    Mirrors ``_write_installed_rev``: temp file + ``os.replace`` so a concurrent
    reader never sees a torn or empty value. Cleans up the temp on replace failure.
    Returns True when the marker landed, False on OSError, so callers can
    distinguish refreshed-with-marker from refreshed-without (ab-703f2ed2).
    """
    target = _RUST_MARKER_FILE
    tmp = None
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.parent / f".installed-rust-rev.{os.getpid()}.tmp"
        tmp.write_text(f"{rev}\n", encoding="utf-8")
        os.replace(tmp, target)
        return True
    except OSError:
        if tmp is not None and tmp.exists():
            try:
                tmp.unlink()
            except OSError:
                pass
        return False


def _cargo_installed_bin() -> Optional[Path]:
    """Return the path to the cargo-installed fno-agents binary, or None if absent.

    Deliberately checks the cargo install location (``$CARGO_HOME/bin``), NOT
    ``fno.agents.rust_runtime.resolve_installed_binary()``, because a
    bundled-wheel binary refreshes via pip, not cargo.
    """
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    name = "fno-agents.exe" if os.name == "nt" else "fno-agents"
    candidate = cargo_home / "bin" / name
    return candidate if candidate.is_file() else None


def _cargo_installed_mux() -> Optional[Path]:
    """Return the path to the cargo-installed mux front-door binary (`fno`), or
    None if absent. Same `$CARGO_HOME/bin` location as the fno-agents bins - the
    front door this channel installs. `fno doctor` reuses this via `update`."""
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    name = "fno.exe" if os.name == "nt" else "fno"
    candidate = cargo_home / "bin" / name
    return candidate if candidate.is_file() else None


def _install_mux_front_door(source: Path, install_root: Path, *, dry_run: bool) -> None:
    """Best-effort: install the crates/fno mux binary (`fno` on PATH - the front
    door) alongside the fno-agents bins, into the same --root.

    Called only when the agents leg already decided a refresh is due, so it
    shares that crates/ subtree staleness gate. A failure warns and continues:
    the mux is heavier to build (tokio + alacritty + pty), and an absent/stale
    mux is a front-door problem `fno doctor` surfaces, never a reason to fail the
    Python update. No marker of its own - `fno doctor`'s front-door check keys on
    the binary's presence, not a rev marker.
    """
    crate_dir = source.parent / "crates" / "fno"
    if not crate_dir.is_dir():
        return
    cmd = ["cargo", "install", "--path", str(crate_dir), "--bins", "--root", str(install_root)]
    if dry_run:
        typer.echo(f"Would run: {shlex.join(cmd)}")
        return
    typer.echo(f"fno update: refreshing mux front door: {shlex.join(cmd)}")
    try:
        result = subprocess.run(cmd, check=False)
    except OSError as exc:
        typer.echo(
            f"fno update: WARNING: mux front door install failed to execute ({exc});"
            " `fno` may be absent/stale; continuing",
            err=True,
        )
        return
    if result.returncode != 0:
        typer.echo(
            f"fno update: WARNING: mux front door install failed (exit {result.returncode});"
            " `fno` may be absent/stale; continuing",
            err=True,
        )
        return
    typer.echo("fno update: mux front door refreshed (crates/fno -> `fno`)")


def _refresh_rust_bins(source: Path, *, force: bool = False, dry_run: bool = False) -> RefreshOutcome:
    """Refresh the cargo-installed fno-agents rust bins if stale.

    Returns an outcome string. Every path prints exactly one line (stdout or
    stderr) so callers can assert feedback without parsing silences.

    Outcomes: skipped-no-crate | skipped-no-binary | skipped-no-rev |
              fresh | skipped-no-cargo | dry-run | failed | refreshed |
              refreshed-no-marker (cargo succeeded but no marker landed,
              so the next doctor run still reports rust stale)

    The cargo install root is pinned to the same location that _cargo_installed_bin()
    tested via --root. Without this, CARGO_INSTALL_ROOT can split the tested binary
    location from the install destination, so the marker claims fresh while the tested
    binary stays stale. For the first-install case (no binary detected), --root falls
    back to the CARGO_HOME default so detection and install location stay coherent.
    """
    crate_dir = source.parent / "crates" / "fno-agents"
    if not crate_dir.is_dir():
        typer.echo("fno update: no crates/fno-agents directory found; skipping rust leg")
        return "skipped-no-crate"

    installed_bin = _cargo_installed_bin()
    if installed_bin is None and not force:
        typer.echo(
            "fno update: no cargo-installed fno-agents binary; skipping rust leg"
            " (pass --rust to install)"
        )
        return "skipped-no-binary"

    subtree = _rust_subtree_rev(source)
    if subtree is None and not force:
        typer.echo(
            "fno update: could not determine crates/ subtree rev; skipping rust leg"
        )
        return "skipped-no-rev"
    # When force=True but subtree is None, we continue but remember we cannot write a marker.

    marker = _read_rust_marker()
    if not force and marker is not None and marker == subtree:
        typer.echo(f"fno update: rust bins fresh (rev {subtree[:12]}); skipping cargo install")
        # The agents bins are current, but the mux front door (crates/fno ->
        # `fno`) can still be ABSENT at a fresh marker: the fno->fno-py rename
        # lands fno-py while a fresh-marker `fno update` never installed the mux,
        # stranding the front door (no `fno` on PATH). Heal it additively if
        # missing, so `fno doctor`'s "run fno update" hint is true rather than a
        # dead-end. No-op when there is no crates/fno source. installed_bin is
        # non-None here (passed the `installed_bin is None and not force` gate).
        if _cargo_installed_mux() is None:
            _install_mux_front_door(source, installed_bin.parent.parent, dry_run=dry_run)
        return "fresh"

    if shutil.which("cargo") is None:
        typer.echo(
            "fno update: WARNING: rust bins need refresh but cargo is not on PATH; skipping",
            err=True,
        )
        return "skipped-no-cargo"

    # Derive the install root from the detected binary so the refresh lands in the
    # exact same location that was tested. Binary lives at <root>/bin/<name>, so
    # root = binary.parent.parent. For the first-install case (no binary), fall back
    # to the same CARGO_HOME default that _cargo_installed_bin() probes so detection
    # and install location remain coherent even when CARGO_INSTALL_ROOT is set.
    if installed_bin is not None:
        install_root = installed_bin.parent.parent
    else:
        install_root = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))

    cmd = ["cargo", "install", "--path", str(crate_dir), "--bins", "--root", str(install_root)]

    if dry_run:
        typer.echo(f"Would run: {shlex.join(cmd)}")
        _install_mux_front_door(source, install_root, dry_run=True)
        return "dry-run"

    typer.echo(f"fno update: refreshing rust bins: {shlex.join(cmd)}")
    try:
        result = subprocess.run(cmd, check=False)
    except OSError as exc:
        # TOCTOU after the which() check, permission error, exec format
        # error: fail the leg loudly but never crash the Python update.
        typer.echo(
            f"fno update: WARNING: cargo install failed to execute ({exc});"
            " rust bins NOT refreshed; continuing with Python update",
            err=True,
        )
        return "failed"
    if result.returncode != 0:
        typer.echo(
            f"fno update: WARNING: cargo install failed (exit {result.returncode});"
            " rust bins NOT refreshed; continuing with Python update",
            err=True,
        )
        return "failed"

    # The mux front door (crates/fno -> `fno` on PATH) rides the SAME crates/
    # subtree staleness gate as the agents bins, so refresh it here too. Without
    # this the front door is an orphan: `fno update` rebuilds fno-agents but the
    # `fno` binary this whole channel is about is never installed or refreshed.
    _install_mux_front_door(source, install_root, dry_run=False)

    outcome: RefreshOutcome
    if subtree is None:
        # force=True with an undeterminable rev: bins rebuilt, but with no
        # marker the next doctor run still reports rust stale.
        typer.echo("fno update: rust bins refreshed (marker not written: rev undeterminable)")
        outcome = "refreshed-no-marker"
    elif _write_rust_marker(subtree):
        typer.echo(f"fno update: rust bins refreshed (rev {subtree[:12]})")
        outcome = "refreshed"
    else:
        typer.echo(
            "fno update: WARNING: rust bins refreshed but the marker write"
            " failed; doctor will still report rust stale"
            f" (check {_RUST_MARKER_FILE.parent} permissions)",
            err=True,
        )
        outcome = "refreshed-no-marker"

    # Best-effort daemon advisory: warn if the old binary is still running.
    try:
        pgrep_result = subprocess.run(
            ["pgrep", "-x", "fno-agents-daemon"],
            capture_output=True,
            check=False,
        )
        if pgrep_result.returncode == 0:
            typer.echo(
                "fno update: note: fno-agents-daemon is running the OLD binary;"
                " restart it to pick up the refresh",
                err=True,
            )
    except (OSError, subprocess.SubprocessError):
        pass

    return outcome


def _install_then_mark(install_cmd: list[str], rev: str, *, marker: Path, pid: int) -> str:
    """Build a shell line that installs, then writes the marker iff install succeeds.

    The ``&&`` gates the marker write on a zero install exit (Invariant: no
    marker on a failed/partial update). The temp-file + ``mv`` keeps the write
    atomic for a concurrent ``fno doctor`` reader. Returned as a string so the
    Unix install path can ``execvp`` ``/bin/sh -c <line>`` and still let the
    installer replace this process.
    """
    q = shlex.quote
    tmp = marker.parent / f".installed-rev.{pid}.tmp"
    # `install && { marker-write || true; }`: the install gates the marker write
    # (no marker on a failed install), but the inner `|| true` keeps a marker-write
    # failure (unwritable ~/.fno, full disk) from overriding a SUCCESSFUL
    # installer exit - the marker is diagnostic-only, mirroring the best-effort
    # Windows path. `&&` binds the install to the brace group, so an install
    # failure still short-circuits to its own non-zero exit.
    marker_write = (
        f"mkdir -p {q(str(marker.parent))} && "
        f"printf '%s\\n' {q(rev)} > {q(str(tmp))} && "
        f"mv {q(str(tmp))} {q(str(marker))}"
    )
    return f"{shlex.join(install_cmd)} && {{ {marker_write} || true; }}"


def update_command(
    source: Optional[Path] = typer.Option(
        None,
        "--source",
        help="Path to the fno CLI source (auto-detected if omitted).",
    ),
    dry_run: bool = typer.Option(
        False,
        "--dry-run", "-N",
        help="Print the install command without running it.",
    ),
    force: bool = typer.Option(
        False,
        "--force", "-F",
        help="Skip the IN_PROGRESS guard and update even during an active target loop.",
    ),
    rust: bool = typer.Option(
        False,
        "--rust",
        help="Force the cargo rust-bins refresh (also installs when no binary exists yet).",
    ),
    no_rust: bool = typer.Option(
        False,
        "--no-rust",
        help="Skip the cargo rust-bins refresh leg.",
    ),
) -> None:
    """Reinstall fno from its source directory.

    Picks up local CLI source changes by running ``uv tool install --reinstall``
    (or ``pip install --user --force-reinstall`` if uv is unavailable).
    """
    # Normalize to plain bool: when called directly (not via CLI), Typer Option
    # defaults are OptionInfo objects, not False. Guard against both.
    rust = rust is True
    no_rust = no_rust is True

    if rust and no_rust:
        typer.echo("fno update: --rust and --no-rust are mutually exclusive", err=True)
        raise typer.Exit(2)

    if _target_in_progress() and not force:
        typer.echo(_GUARD_MSG, err=True)
        raise typer.Exit(1)

    try:
        resolved = _discover_source(source)
    except SourceNotFoundError as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(1)

    typer.echo(f"Reinstalling fno from {resolved}")

    if not no_rust:
        # Outcome string deliberately dropped (locked decision 4:
        # warn-and-continue). The helper prints one line per path, and on
        # Unix execvp below replaces this process, so an exit-code channel
        # for the rust leg is unreachable from here anyway. `fno doctor
        # --fix` is the caller that branches on the outcome.
        _refresh_rust_bins(resolved, force=rust, dry_run=dry_run)

    if shutil.which("uv"):
        cmd = ["uv", "tool", "install", "--reinstall", str(resolved)]
    elif shutil.which("pip"):
        cmd = [
            sys.executable,
            "-m",
            "pip",
            "install",
            "--user",
            "--force-reinstall",
            str(resolved),
        ]
    else:
        typer.echo("Neither `uv` nor `pip` is available on PATH.", err=True)
        raise typer.Exit(1)

    if dry_run:
        # shlex.join shell-escapes each arg so the printed command is safe to
        # paste into a terminal even when the source path contains spaces.
        typer.echo(f"Would run: {shlex.join(cmd)}")
        _cache_source_path(resolved)
        return

    _cache_source_path(resolved)

    # Rev we are about to install, recorded so `fno doctor` can later detect
    # installed-vs-source skew. None when the source is not a readable git
    # checkout; the marker is written ONLY on a successful install.
    rev = _source_rev(resolved)

    if sys.platform == "win32":
        # On Windows, os.execvp does NOT replace the process: it spawns the
        # installer as a child and terminates the parent with status 0,
        # hiding the install result. Worse, fno.exe is held open by the
        # still-running parent until the parent exits, racing the new
        # install. Use subprocess.run and propagate the real exit code.
        result = subprocess.run(cmd, check=False)
        if result.returncode == 0 and rev:
            _write_installed_rev(rev)
        raise typer.Exit(result.returncode)

    # On Unix, execvp replaces this Python process with the installer; uv
    # tool install is then free to replace the fno binary without racing
    # the running interpreter. Because execvp never returns, the installed-rev
    # marker write is chained onto the installer via the shell so it runs iff
    # the install exits 0 (marker-only-on-success without regaining control).
    if rev:
        os.execvp(
            "/bin/sh",
            ["/bin/sh", "-c", _install_then_mark(cmd, rev, marker=_INSTALLED_REV_FILE, pid=os.getpid())],
        )
    else:
        os.execvp(cmd[0], cmd)
