"""fno update: reinstall the fno CLI from its source path.

Discovers the source via (in priority order):

1. ``--source`` flag override
2. ``FNO_SOURCE`` env var
3. ``~/.fno/source-path`` cache (written on prior successful install)
4. Well-known candidate paths (plugin install, common dev locations)

Then execs ``uv tool install --reinstall --refresh <source>`` (or ``pip install --user
--force-reinstall <source>`` if uv is unavailable). Uses ``os.execvp`` so the
installer replaces this Python process cleanly, avoiding the "binary being
replaced while it runs" race.
"""
from __future__ import annotations

import logging
import filecmp
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tomllib
from collections.abc import Callable
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

# Legacy breadcrumb: the last git commit that touched crates/. NO LONGER read by
# any freshness verdict - update's gate and doctor's verdict both
# interrogate the binary's embedded crates_rev now. Still written as an inert
# marker in case an out-of-tree consumer wants it; delete the writes once a grep
# proves none remain.
_RUST_MARKER_FILE = _CACHE_FILE.parent / "installed-rust-rev"

# The fno-agents triad: client + daemon + worker, one crate, three [[bin]]s.
# They MUST stay a coherent same-build set in every install location (the
# resolve_daemon_bin same-dir sibling contract, client.rs) - a mixed-version
# pair is the worse bug, so update syncs all three or none per location.
_TRIAD_STEMS = ("fno-agents", "fno-agents-daemon", "fno-agents-worker")


def _triad_names() -> tuple[str, ...]:
    suffix = ".exe" if os.name == "nt" else ""
    return tuple(f"{stem}{suffix}" for stem in _TRIAD_STEMS)

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


def _looks_like_fno_source(path: Path) -> bool:
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
        if not _looks_like_fno_source(path):
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
        if _looks_like_fno_source(path):
            return path

    raise SourceNotFoundError(
        "Could not locate the fno CLI source. Pass --source /path/to/fno/cli, "
        "set $FNO_SOURCE, or install the fno plugin into "
        "~/.claude/plugins/fno/."
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
    ``fno.rust_binary.resolve_installed_binary()``, because a
    bundled-wheel binary refreshes via pip, not cargo.
    """
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    name = "fno-agents.exe" if os.name == "nt" else "fno-agents"
    candidate = cargo_home / "bin" / name
    return candidate if candidate.is_file() else None


def _installed_bin_crates_rev(binary: Path, *, timeout: float = 20.0) -> Optional[str]:
    """The clean crates/ subtree rev the installed binary self-reports, or None.

    Runs ``<binary> version --json`` (the build.rs embed) and returns its
    ``crates_rev`` only when the binary answered cleanly AND the build is not
    dirty. Returns None - which the gate treats as STALE, forcing a rebuild -
    for every failure mode: a missing/hung/crashing binary (bounded by
    ``timeout``), a non-zero exit, unparseable or non-dict JSON, a "unknown"
    rev (non-git build), or a dirty tree. Fail toward rebuild, never toward a
    false-fresh skip (the stale-marker gate's exact lie).
    """
    try:
        result = subprocess.run(
            [str(binary), "version", "--json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    stdout = getattr(result, "stdout", None)
    if not stdout:
        return None
    try:
        data = json.loads(stdout)
    except (ValueError, TypeError):
        return None
    if not isinstance(data, dict) or data.get("dirty") is True:
        return None
    rev = data.get("crates_rev")
    if not isinstance(rev, str) or rev in ("", "unknown"):
        return None
    return rev


def _no_rev_reason(binary: Optional[Path], install_root: Path, *, timeout: float = 20.0) -> str:
    """Why ``_installed_bin_crates_rev`` returned None, as a message fragment.

    It collapses five causes into one None, and only one of them is fixed by
    committing - telling a user with a crashing binary to stash their tree is
    the same misdiagnosis this reporting exists to end. Re-probes rather than
    guessing; the probe only runs on the already-failing path.
    """
    if binary is None or not binary.is_file():
        return f"is missing from the install root ({install_root})"
    try:
        result = subprocess.run(
            [str(binary), "version", "--json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return f"hung on `version --json` (>{timeout:g}s)"
    except (OSError, subprocess.SubprocessError) as exc:
        return f"could not be executed ({exc})"
    if result.returncode != 0:
        return f"exited {result.returncode} on `version --json`"
    try:
        data = json.loads(result.stdout)
    except (ValueError, TypeError):
        return "emitted unparseable `version --json` output"
    if not isinstance(data, dict):
        return "emitted unexpected `version --json` output"
    if data.get("dirty") is True:
        return (
            "was built from a dirty crates/ tree - commit or stash your"
            " crates/ changes and re-run"
        )
    return "carries no rev stamp (built outside a git checkout?)"


def _triad_same_build(bindir: Path, subtree: str) -> bool:
    """True iff all three triad bins in ``bindir`` self-report ``crates_rev ==
    subtree`` (and are not dirty). Now that daemon + worker carry a ``version``
    verb too, the fresh fast path can verify the whole triad is the SAME build,
    not merely present: a stale-but-present sibling reports a different (or
    unparseable -> None) rev and forces a rebuild. ``_installed_bin_crates_rev``
    already fails toward rebuild for every error mode, so no extra guarding here.
    """
    return all(
        _installed_bin_crates_rev(bindir / n) == subtree for n in _triad_names()
    )


def _cargo_installed_mux() -> Optional[Path]:
    """Return the path to the cargo-installed mux front-door binary (`fno`), or
    None if absent. Same `$CARGO_HOME/bin` location as the fno-agents bins - the
    front door this channel installs. `fno doctor` reuses this via `update`."""
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    name = "fno.exe" if os.name == "nt" else "fno"
    candidate = cargo_home / "bin" / name
    return candidate if candidate.is_file() else None


def _mux_dir() -> Path:
    """The mux socket dir, matching the Rust `proto::mux_dir()`: `$FNO_MUX_DIR`
    when set (tests point it at a tempdir), else `~/.fno/mux`."""
    override = os.environ.get("FNO_MUX_DIR")
    return Path(override) if override else Path.home() / ".fno" / "mux"


def _live_mux_sessions(
    runner: "Callable[..., subprocess.CompletedProcess[str]]" = subprocess.run,
) -> list[str]:
    """Live mux session names from `fno mux ls --json` (entries with
    `state == "live"`). Bounded + best-effort: no mux binary, a non-zero exit, a
    timeout, or unparseable JSON all yield `[]` (advisory reads never cry wolf)."""
    fno = _cargo_installed_mux() or shutil.which("fno")
    if not fno:
        return []
    try:
        proc = runner(
            [str(fno), "mux", "ls", "--json"],
            capture_output=True, text=True, check=False, timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if proc.returncode != 0:
        return []
    try:
        rows = json.loads(proc.stdout or "[]")
    except (ValueError, TypeError):
        return []
    return [
        r["session"]
        for r in rows
        if isinstance(r, dict) and r.get("state") == "live" and r.get("session")
    ]


def stale_mux_servers(
    runner: "Callable[..., subprocess.CompletedProcess[str]]" = subprocess.run,
) -> list[str]:
    """Live mux sessions on a STALE WIRE VERSION: the running server predates the
    installed binary's ``PROTO_VERSION``, so a new client's handshake is rejected
    and the server is already unreachable (the fix is `fno restart`, which now
    auto-cuts these over). The precise signal is the ``stale`` field
    ``fno mux ls --json`` computes from each server's ``.ver`` sidecar (x-1a85); a
    pre-sidecar server has no ``.ver`` and reads as stale, so the check works
    across the very upgrade that introduces it. This replaces the old
    ``socket mtime < binary mtime`` heuristic, which flagged EVERY server after
    any reinstall (a wire-agnostic false alarm). Best-effort and advisory: any
    missing binary / non-zero exit / unparseable JSON yields ``[]``. `fno doctor`
    renders this; `fno update` nudges on it; `fno restart` auto-restarts it."""
    fno = _cargo_installed_mux() or shutil.which("fno")
    if not fno:
        return []
    try:
        proc = runner(
            [str(fno), "mux", "ls", "--json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if proc.returncode != 0:
        return []
    try:
        rows = json.loads(proc.stdout or "[]")
    except (ValueError, TypeError):
        return []
    return [
        r["session"]
        for r in rows
        if isinstance(r, dict)
        and r.get("state") == "live"
        and r.get("stale")
        and r.get("session")
    ]


def _read_source_wire_version(source: Path) -> Optional[int]:
    """Parse ``PROTO_VERSION`` out of the source checkout's
    ``crates/fno/src/proto.rs``. ``source`` is the ``cli/`` dir (this module's
    discovery convention, ``_discover_source``); its parent is the repo root in
    both dev-clone and plugin layouts. None on any read/parse failure - the
    readiness resolver treats that as a degraded input, never a bogus wire."""
    proto_path = source.parent / "crates" / "fno" / "src" / "proto.rs"
    try:
        text = proto_path.read_text(encoding="utf-8")
    except OSError:
        return None
    match = re.search(r"^pub const PROTO_VERSION: u32 = (\d+);", text, re.MULTILINE)
    return int(match.group(1)) if match else None


def _live_mux_rows(
    runner: "Callable[..., subprocess.CompletedProcess[str]]" = subprocess.run,
) -> Optional[list[dict]]:
    """Live (``state == "live"``) rows from ``fno mux ls --json``, or None on any
    failure (missing binary, non-zero exit, timeout, unparseable JSON) so the
    caller can tell "no live servers" from "could not ask" (AC4-EDGE)."""
    fno = _cargo_installed_mux() or shutil.which("fno")
    if not fno:
        return None
    try:
        proc = runner(
            [str(fno), "mux", "ls", "--json"],
            capture_output=True, text=True, check=False, timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        rows = json.loads(proc.stdout or "[]")
    except (ValueError, TypeError):
        return None
    if not isinstance(rows, list):
        return None
    return [r for r in rows if isinstance(r, dict) and r.get("state") == "live"]


def _live_agent_rows(
    runner: "Callable[..., subprocess.CompletedProcess[str]]" = subprocess.run,
) -> Optional[list[dict]]:
    """Rows from ``fno agents list --json``, or None on any failure - same
    "unavailable vs empty" distinction as :func:`_live_mux_rows`, so a revivable
    count of 0 always means "asked and got zero", never "could not ask"."""
    fno = _cargo_installed_mux() or shutil.which("fno")
    if not fno:
        return None
    try:
        proc = runner(
            [str(fno), "agents", "list", "--json"],
            capture_output=True, text=True, check=False, timeout=15,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        data = json.loads(proc.stdout or "[]")
    except (ValueError, TypeError):
        return None
    if isinstance(data, list):
        rows = data
    elif isinstance(data, dict):
        rows = data.get("agents", [])
    else:
        return None
    return [r for r in rows if isinstance(r, dict)]


def _changelog_subjects(
    installed_rev: str,
    source: Path,
    runner: "Callable[..., subprocess.CompletedProcess[str]]" = subprocess.run,
) -> list[str]:
    """Up to ten commit subjects between ``installed_rev`` and the source
    checkout's HEAD. [] on any git failure (detached source, unknown rev,
    missing git) - the readiness payload never blocks on this."""
    try:
        proc = runner(
            ["git", "-C", str(source), "log", "--no-merges", "--format=%s",
             f"{installed_rev}..HEAD"],
            capture_output=True, text=True, check=False, timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if proc.returncode != 0:
        return []
    return [line for line in proc.stdout.splitlines() if line.strip()][:10]


def _wire_label(wires: list[int]) -> str:
    return "/".join(f"v{w}" for w in wires) if wires else "unknown"


def _build_update_guidance(
    *,
    update_ready: bool,
    revs_known: bool,
    source_rev: Optional[str],
    wire_known: bool,
    wire_bump: bool,
    running_wires: list[int],
    source_wire: Optional[int],
    shells: int,
    shells_ended: int,
    shells_known: bool,
    revivable: int,
    revivable_known: bool,
    degraded_reason: Optional[str],
) -> str:
    """The one guidance line, computed rather than authored. Three
    branches - no bump, bump, degraded - and no fourth. Every branch names a
    count and a positive outcome; the degraded branch treats an unknown wire as
    a bump and, when the shell/worker count itself could not be read (a failed
    `mux ls`/`agents list`, not just an unreadable wire), says "unknown" rather
    than a false zero - a count fno never fetched is not evidence of an empty
    fleet (AC4-EDGE). A degraded input unrelated to the wire (e.g. `agents
    list` failing) must not override an actually-known wire status with
    "unknown, treated as a bump" - that both contradicts the JSON payload's own
    `wire.bump` field and, when the wire is known safe, wrongly tells the
    operator a restart is destructive (P2, codex on PR #881)."""
    rev_label = (source_rev or "unknown")[:8]
    source_label = f"v{source_wire}" if source_wire is not None else "unknown"

    # A degraded input (mux ls, agents list, wire) never overrides a *confidently*
    # known not-ready state - if both revs were read and match, there is no update
    # to warn about, regardless of what else failed to fetch. Only take the
    # degraded branch when readiness itself is uncertain (a rev is unreadable) or
    # an update actually is pending.
    if not update_ready and revs_known:
        return f"up to date at {rev_label} - no update pending, {shells} shell(s) unaffected"

    if degraded_reason:
        shells_label = f"{shells} live shell(s)" if shells_known else "an unknown number of live shells"
        revivable_label = f"{revivable} worker(s)" if revivable_known else "an unknown number of workers"
        wire_label = (
            (f"WIRE BUMP {_wire_label(running_wires)} -> {source_label}" if wire_bump else "wire unchanged")
            if wire_known
            else "wire status unknown, treated as a wire bump"
        )
        return (
            f"update check degraded ({degraded_reason}) - {wire_label}; "
            f"{shells_label} at risk, --revive respawns {revivable_label}"
        )

    if not update_ready:
        return f"up to date at {rev_label} - no update pending, {shells} shell(s) unaffected"

    if wire_bump:
        return (
            f"update ready {rev_label} - WIRE BUMP {_wire_label(running_wires)} -> "
            f"{source_label} - `fno update && fno restart --mux` ends {shells_ended} "
            f"shell(s); --revive respawns {revivable} worker(s)"
        )

    return (
        f"update ready {rev_label} - wire unchanged ({source_label}) - "
        f"detach, `fno update`, reattach; {shells} shell(s) survive"
    )


def update_readiness(
    runner: "Callable[..., subprocess.CompletedProcess[str]]" = subprocess.run,
    source: Optional[Path] = None,
) -> dict:
    """Compute update readiness: whether an install is waiting, whether it would
    break the wire, and the one guidance line an operator sees. The single
    resolver (Locked Decision 1) - the TUI (``crates/fno/src/client.rs``)
    renders this payload and computes nothing itself; ``fno update --check
    --json`` exposes it directly. Every input degrades independently rather
    than raising, so a broken environment still gets a non-empty, honest
    guidance line (AC4-EDGE)."""
    from fno import doctor
    from fno.restart import is_revivable

    degraded: list[str] = []

    installed_rev = doctor._read_marker()
    if installed_rev is None:
        degraded.append("installed rev marker missing")

    resolved_source = doctor._resolve_source(source)
    if resolved_source is None:
        degraded.append("source checkout not resolvable")

    source_rev: Optional[str] = None
    if resolved_source is not None:
        source_rev = doctor._source_rev(resolved_source)
        if source_rev is None:
            degraded.append("source rev unreadable")

    update_ready = bool(installed_rev and source_rev and installed_rev != source_rev)

    source_wire = _read_source_wire_version(resolved_source) if resolved_source else None
    if resolved_source is not None and source_wire is None:
        degraded.append("source PROTO_VERSION unreadable")

    live_rows = _live_mux_rows(runner)
    shells_known = live_rows is not None
    if live_rows is None:
        degraded.append("fno mux ls --json failed")
        live_rows = []
        wire_bump = True  # unknown live state: never assert shells survive.
    else:
        wire_bump = (
            True
            if source_wire is None
            else any(r.get("wire_version") != source_wire for r in live_rows)
        )

    shells = sum(int(r.get("panes") or 0) for r in live_rows)
    sessions = len(live_rows)
    running_wires = sorted(
        {r["wire_version"] for r in live_rows if isinstance(r.get("wire_version"), int)}
    )
    shells_ended = shells if wire_bump else 0

    agent_rows = _live_agent_rows(runner)
    revivable_known = agent_rows is not None
    if agent_rows is None:
        degraded.append("fno agents list --json failed")
        agent_rows = []
    # Same candidate scope as `_revive_orphans`' `pre_live` snapshot
    # (restart.py): only a worker that is actually live now can be orphaned by
    # a restart, so an exited or already-dead row never counts toward
    # `--revive` even when `is_revivable` alone would accept it (P2, codex on
    # PR #881).
    revivable = sum(1 for r in agent_rows if r.get("status") == "live" and is_revivable(r))

    changelog: list[str] = []
    if resolved_source is not None and installed_rev and source_rev:
        changelog = _changelog_subjects(installed_rev, resolved_source, runner)

    degraded_reason = "; ".join(degraded) if degraded else None

    guidance = _build_update_guidance(
        update_ready=update_ready,
        revs_known=installed_rev is not None and source_rev is not None,
        source_rev=source_rev,
        wire_known=shells_known and source_wire is not None,
        wire_bump=wire_bump,
        running_wires=running_wires,
        source_wire=source_wire,
        shells=shells,
        shells_ended=shells_ended,
        shells_known=shells_known,
        revivable=revivable,
        revivable_known=revivable_known,
        degraded_reason=degraded_reason,
    )

    # None (not 0) when the underlying fetch never happened - a count fno never
    # fetched is not evidence of an empty fleet (AC4-EDGE). `guidance` already
    # says "unknown" in prose; the structured fields need the same honesty for a
    # consumer reading them directly instead of parsing that prose.
    return {
        "update_ready": update_ready,
        "installed_rev": installed_rev,
        "source_rev": source_rev,
        "wire": {"running": running_wires, "source": source_wire, "bump": wire_bump},
        "shells": shells if shells_known else None,
        "shells_ended": shells_ended if shells_known else None,
        "sessions": sessions if shells_known else None,
        "revivable": revivable if revivable_known else None,
        "changelog": changelog,
        "guidance": guidance,
        "degraded": degraded_reason,
    }


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


def _triad_install_dirs() -> list[Path]:
    """Deduped install-location dirs that already host >=1 of the triad bins.

    Enumerates the resolver's install locations (bundled ``fno/_bin``, the
    launcher-sibling scripts dir, PATH) plus the uv tool venv bin, and keeps only
    dirs that already host at least one triad binary. NEVER seeds a new location
    (locked decision 4). The cargo bin dir may appear here; the caller skips it.
    """
    names = _triad_names()
    candidates: list[Path] = []
    try:
        from fno.agents import rust_runtime as _rr
        candidates.append(Path(_rr.__file__).resolve().parent.parent / "_bin")
    except Exception:
        pass
    launcher = sys.argv[0] if sys.argv else ""
    if launcher:
        candidates.append(Path(launcher).resolve().parent)
    onpath = shutil.which(names[0])
    if onpath:
        candidates.append(Path(onpath).resolve().parent)
    candidates.append(Path.home() / ".local" / "share" / "uv" / "tools" / "fno" / "bin")

    dirs: list[Path] = []
    seen: set[Path] = set()
    for d in candidates:
        try:
            rd = d.resolve()
        except OSError:
            continue
        if rd in seen:
            continue
        seen.add(rd)
        if rd.is_dir() and any((rd / n).is_file() for n in names):
            dirs.append(rd)
    return dirs


def _sync_triad(cargo_bin_dir: Path, *, dry_run: bool = False) -> None:
    """Propagate the freshly-built triad from ``cargo_bin_dir`` into every OTHER
    live install location that already hosts one of the three bins, so client,
    daemon, and worker stay a coherent same-build set wherever the resolver might
    pick one up (locked decisions 3-4).

    Runs on BOTH the rebuilt and the gate's fresh path so an interrupted prior
    run's other locations still converge (AC2-FR). Per-location atomicity: each
    bin is copied to a temp name then ``os.replace``'d (running processes keep
    their inode; the next spawn gets the new bin). A location that already holds a
    byte-identical triad is skipped so the fresh path stays cheap. A location that
    cannot take the full triad HALTS update loud (``typer.Exit``) naming the
    location and the bins left inconsistent - a mixed-version pair is the worse
    bug, never left silently half-copied (AC2-ERR).
    """
    names = _triad_names()
    sources = {n: cargo_bin_dir / n for n in names}
    if any(not p.is_file() for p in sources.values()):
        # Source root itself is incomplete - nothing coherent to propagate.
        return

    try:
        cargo_resolved = cargo_bin_dir.resolve()
    except OSError:
        cargo_resolved = cargo_bin_dir

    for dest in _triad_install_dirs():
        if dest == cargo_resolved:
            continue
        if not any((dest / n).is_file() for n in names):
            continue  # never seed a location that hosts none of the triad (decision 4)
        if all(
            (dest / n).is_file() and filecmp.cmp(sources[n], dest / n, shallow=False)
            for n in names
        ):
            continue  # already the same build here
        if dry_run:
            typer.echo(f"Would sync fno-agents triad -> {dest}")
            continue
        copied: list[str] = []
        tmp: Optional[Path] = None
        try:
            for n in names:
                tmp = dest / f".{n}.{os.getpid()}.tmp"
                shutil.copy2(sources[n], tmp)
                os.replace(tmp, dest / n)  # consumes tmp; leaves nothing behind
                tmp = None
                copied.append(n)
        except OSError as exc:
            # A copy2 that wrote a partial temp, or an os.replace that failed,
            # leaves the last tmp orphaned in dest - unlink it (same atomic-write
            # cleanup as _write_rust_marker) before failing.
            if tmp is not None:
                try:
                    tmp.unlink(missing_ok=True)
                except OSError:
                    pass
            typer.echo(
                f"fno update: ERROR: triad sync FAILED at {dest} ({exc}). "
                f"Copied {copied or 'none'} before the failure; this location may now "
                "hold a MIXED-VERSION fno-agents triad. Fix the location and re-run "
                "`fno update` (or set FNO_AGENTS_DAEMON_BIN to a coherent triad dir).",
                err=True,
            )
            raise typer.Exit(1)
        typer.echo(f"fno update: synced fno-agents triad -> {dest}")


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

    # Freshness is proven by the BINARY ITSELF, not a marker file. The
    # installed binary bakes in its crates_rev via build.rs; interrogate it and
    # skip cargo only when that rev matches source AND the build is not dirty.
    # A marker could advance past a stale/out-of-band binary and lie "fresh".
    installed_rev = None if installed_bin is None else _installed_bin_crates_rev(installed_bin)
    # The fresh fast path also requires the daemon + worker siblings to be the
    # SAME build as the fresh client, not merely present. All three bins now
    # carry a `version --json` verb, so _triad_same_build interrogates each one's
    # crates_rev: a MISSING or STALE sibling (different/unparseable rev -> None)
    # falls through to cargo, which rebuilds the whole triad coherently. This
    # closes the residual gap where a manually-replaced older sibling beside a
    # fresh client passed a presence-only check and skipped the rebuild.
    if (
        not force
        and installed_bin is not None
        and subtree is not None
        and installed_rev is not None
        and installed_rev == subtree
        and _triad_same_build(installed_bin.parent, subtree)
    ):
        typer.echo(
            f"fno update: rust bins fresh (rev {installed_rev[:12]} from binary);"
            " skipping cargo install"
        )
        # The agents bins are current, but the mux front door (crates/fno ->
        # `fno`) can still be ABSENT or STALE at a fresh triad. Absent: the
        # fno->fno-py rename lands fno-py while a fresh-binary `fno update` never
        # installed the mux. Stale: the mux install is best-effort (a failed
        # build warns and continues), so a prior failure can leave an OLD `fno`
        # beside a fresh triad. Now that crates/fno bakes its own crates_rev,
        # interrogate the installed mux and reinstall when it is missing OR its
        # rev != source - closing the present-but-stale front-door gap a
        # presence-only heal would miss. No-op when there is no crates/fno
        # source. installed_bin is non-None here.
        mux = _cargo_installed_mux()
        if mux is None or _installed_bin_crates_rev(mux) != subtree:
            _install_mux_front_door(source, installed_bin.parent.parent, dry_run=dry_run)
        # Sync even on the fresh path: an interrupted prior run may have left the
        # other install locations behind (AC2-FR). The gate's fresh verdict must
        # NOT short-circuit convergence.
        _sync_triad(installed_bin.parent, dry_run=dry_run)
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

    # Post-deploy verify: interrogate the binary we just deployed. cargo can exit
    # 0 yet leave a stale artifact (a reused build cache, or an install root that
    # is not what the runtime actually resolves) - the marker gate hid exactly
    # this class. HALT loud on mismatch with both revs printed. Skipped
    # only when subtree is undeterminable (force with no git rev to check against).
    if subtree is not None:
        deployed = _cargo_installed_bin()
        verify_rev = None if deployed is None else _installed_bin_crates_rev(deployed)
        if verify_rev != subtree:
            # Distinct failures reach here; a shared message misdiagnoses one as
            # another and sends the reader hunting the install root.
            if verify_rev is None:
                detail = _no_rev_reason(deployed, install_root)
            else:
                detail = (
                    f"reports crates/ rev {verify_rev[:12]}, but source is"
                    f" {subtree[:12]} - the rebuild did not land where the runtime"
                    f" resolves it (install root {install_root})"
                )
            typer.echo(
                f"fno update: ERROR: post-deploy verify FAILED - the deployed fno-agents {detail}."
                " NOT continuing.",
                err=True,
            )
            raise typer.Exit(1)

    # The mux front door (crates/fno -> `fno` on PATH) rides the SAME crates/
    # subtree staleness gate as the agents bins, so refresh it here too. Without
    # this the front door is an orphan: `fno update` rebuilds fno-agents but the
    # `fno` binary this whole channel is about is never installed or refreshed.
    _install_mux_front_door(source, install_root, dry_run=False)

    # Propagate the freshly-built triad to every other live install location so
    # client/daemon/worker stay a coherent set (the same-dir sibling contract).
    # After a successful cargo install the triad lives at <install_root>/bin.
    _sync_triad(install_root / "bin", dry_run=False)

    outcome: RefreshOutcome
    if subtree is None:
        # force=True with an undeterminable rev: bins rebuilt but no marker
        # breadcrumb written (no verdict reads it, so this is cosmetic).
        typer.echo("fno update: rust bins refreshed (marker not written: rev undeterminable)")
        outcome = "refreshed-no-marker"
    elif _write_rust_marker(subtree):
        typer.echo(f"fno update: rust bins refreshed (rev {subtree[:12]})")
        outcome = "refreshed"
    else:
        # Marker write failed, but the deploy already passed post-deploy verify
        # above - the bins ARE repaired, and no verdict reads the legacy marker.
        # So this is a SUCCESSFUL refresh, not `refreshed-no-marker` (which
        # `fno doctor --fix` treats as a failed repair and exits 1). Warn about the
        # cosmetic breadcrumb, return success.
        typer.echo(
            "fno update: note: rust bins refreshed; the legacy marker write failed"
            f" (harmless, no verdict reads it; check {_RUST_MARKER_FILE.parent} permissions)",
            err=True,
        )
        outcome = "refreshed"

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

    # Best-effort mux advisory: a long-running mux server keeps speaking the OLD
    # proto after this refresh (the mux deliberately survives a reinstall), which
    # silently blocks agent dispatch until restarted. Nothing else nudges for it.
    for sess in stale_mux_servers():
        typer.echo(
            f"fno update: note: mux server '{sess}' speaks an OLD wire version"
            " (a new client can't attach it); run 'fno restart' to auto-cut it"
            " over (ends that session's panes)",
            err=True,
        )

    return outcome


_UV_INSTALL_ATTEMPTS = 3


def _uv_retry_sh(cmd: list[str]) -> str:
    """Shell snippet running the uv install with ONE retried failure.

    Retried: only the ENOTEMPTY signature (uv's removal walk racing a
    concurrent importer's bytecode rewrite; docs/architecture/cli-lazy-imports.md).
    Any other failure exits immediately with uv's own code, uv's stderr
    re-printed verbatim. Bounded at ``_UV_INSTALL_ATTEMPTS``. Success is
    accepted only via a positive marker - the ``fno-py`` console script plus
    shipped bytecode under the tool venv - never the exit code alone, and only
    after the same bounded re-check ``_await_binary`` spends on the same file.

    This lives as a shell string (not a Python loop) because the Unix install
    path execs it: uv must be free to replace the venv this interpreter may
    itself be running from, which an in-process ``subprocess.run`` loop would
    reintroduce as a race.

    uv-only: the marker reads ``uv tool dir``, so wrapping the pip fallback
    here would refuse a perfectly good pip install on the very machines that
    have no uv. Callers gate on ``cmd[0] == "uv"``.
    """
    c = shlex.join(cmd)
    verify = (
        '__td=$(NO_COLOR=1 UV_NO_COLOR=1 uv tool dir 2>/dev/null) && '
        '[ -n "$__td" ] && [ -x "$__td/fno/bin/fno-py" ] && '
        '[ -n "$(find "$__td/fno/lib" -name "*.pyc" -print -quit 2>/dev/null)" ]'
    )
    # The same wait `_await_binary` spends, on the verify that runs BEFORE it.
    # uv exits before its own artifacts settle (the console script is absent for
    # ~490ms across an install), so a single-shot verify here refuses a good
    # install, and its `exit 1` short-circuits the whole chain: no marker write,
    # no agent refresh, `_await_binary` never reached. RE-CHECKED rather than
    # slept blind, so a genuinely broken install still fails with the same words.
    # POSIX sh only. 15 * 0.2s = 3s, the budget every provisioning path spends.
    verify_fns = (
        '__fno_verify() { ' + verify + '; }; '
        '__fno_verify_within() { __vn=0; while :; do __fno_verify && return 0; '
        '__vn=$((__vn+1)); [ "$__vn" -gt 15 ] && return 1; sleep 0.2; done; }; '
    )
    return (
        f'{verify_fns}'
        f'__n=0; __e=$(mktemp) || exit 1; '
        f'while :; do __n=$((__n+1)); '
        f'if {c} 2>"$__e"; then rm -f "$__e"; '
        # break, not exit 0: callers chain `&& marker-write && refresh` after
        # this snippet, and an exit here would skip both on every success.
        f'if __fno_verify_within; then break; '
        f'else echo "fno: uv exited 0 but the install does not verify after waiting 3s '
        f'(no fno-py script or no shipped bytecode under the tool venv)" >&2; exit 1; fi; '
        f'else __rc=$?; cat "$__e" >&2; '
        # __sig is the signature verdict for THIS attempt. It gates the race
        # message too: a non-signature failure that happens to land on the
        # last attempt (auth on attempt 3 after two real races) must not send
        # the operator off to kill fno processes.
        f'if grep -q "Directory not empty" "$__e" && grep -q "os error 66" "$__e"; '
        f'then __sig=1; else __sig=0; fi; rm -f "$__e"; '
        f'if [ "$__sig" = 1 ] && [ "$__n" -lt {_UV_INSTALL_ATTEMPTS} ]; then sleep 1; else '
        # Same words the fno.sh / postinstall twins die with, so every
        # provisioning path explains the capped race identically.
        f'if [ "$__sig" = 1 ]; then echo "fno: uv tool install hit the directory race (os error 66) three times. A concurrent fno process is rewriting bytecode into the venv mid-removal. Stop fno processes and re-run." >&2; fi; '
        f'exit "$__rc"; fi; fi; done'
    )


def _install_then_mark(
    install_cmd: list[str],
    rev: str,
    *,
    marker: Path,
    pid: int,
    post_install: Optional[str] = None,
    await_binary: Optional[str] = None,
    install_sh: Optional[str] = None,
) -> str:
    """Build a shell line that installs, then writes the marker iff install succeeds.

    The ``&&`` gates the marker write on a zero install exit (Invariant: no
    marker on a failed/partial update). The temp-file + ``mv`` keeps the write
    atomic for a concurrent ``fno doctor`` reader. Returned as a string so the
    Unix install path can ``execvp`` ``/bin/sh -c <line>`` and still let the
    installer replace this process.

    ``post_install`` runs AFTER a successful install (also gated by ``&&`` on
    the install exit), best-effort (``|| true``) so it can never override a
    successful installer exit. Used to refresh the launchd agents onto the
    freshly-installed binary; they must run the NEW binary, which is why this is
    chained here (after the install) rather than executed in the pre-exec
    interpreter. Multiple commands arrive ``;``-separated, so one failing
    refresh still lets the rest run.
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
    # install_sh overrides the plain join when the caller wrapped the install
    # in its own shell logic (the uv retry/verify wrapper at the real call
    # site); the && gating is identical either way.
    line = f"{install_sh or shlex.join(install_cmd)} && {{ {marker_write} || true; }}"
    if post_install:
        line += f" && {{ {_await_binary(post_install, await_binary)} }}"
    return line


def _await_binary(post_install: str, binary: Optional[str]) -> str:
    """Wrap ``post_install`` in a bounded wait for ``binary`` to exist.

    Measured, not assumed: during ``uv tool install --reinstall`` the console
    script ``<tools>/fno/bin/fno-py`` is deleted and recreated, and the
    ``~/.local/bin`` exposure dangles with it, for roughly half a second. (The
    venv's python3 never disappears, so the shebang interpreter is not the
    problem and pointing at it would fix nothing.) That gap closed only ~40ms
    before uv exited in an idle measurement, so the ``&&`` that already gates
    this chain on uv's exit is technically correct and practically far too tight:
    on a loaded machine the exec still lands in the tail of the gap and dies with
    ``/bin/sh: .../fno-py: No such file or directory``.

    Waiting here is not speculation about an unknown cause. We just ran the
    install ourselves, so the binary is expected to appear; this waits for our
    own artifact rather than guessing at someone else's. If it never shows up,
    say what did not happen and give the two commands to run by hand, because
    silently skipping the refresh is what leaves a launchd agent pinned to the
    old binary - the exact wedge this chain exists to prevent.
    """
    # A brace group needs a terminator before `}`, and a doubled `;;` is a
    # syntax error in sh (it is the case-arm terminator), so normalise exactly
    # one. `sh -n` catches both mistakes; there is a test that runs it.
    body = post_install.rstrip().rstrip(";")
    if not binary:
        # Unchanged pre-existing shape: the trailing `|| true` binds to the last
        # command only, which is what makes each refresh independently
        # best-effort.
        return f" {body} || true;"
    q = shlex.quote
    b = q(binary)
    # Pick the probe from the shape of the path, because `-x` and PATH lookup are
    # not interchangeable. `_resolve_fno_binary` falls back to a BARE `fno-py`
    # when no console script exists yet (pr_watch/cli.py), which is exactly the
    # cold-install case this refresh matters most for -- and `[ -x fno-py ]` tests
    # `$PWD/fno-py`, never PATH. Probing that with `-x` would wait the full
    # ceiling and then skip both refreshes even though the install had just put
    # `fno-py` on PATH. Deciding here rather than in the shell keeps the emitted
    # line simple and needs no shell function.
    probe = f"[ -x {b} ]" if "/" in binary else f"command -v {b} >/dev/null 2>&1"
    # POSIX sh only: no `seq`, no bashisms. 15 * 0.2s = 3s ceiling.
    wait = (
        f"_fno_n=0; while [ $_fno_n -lt 15 ] && ! {probe}; "
        f"do _fno_n=$((_fno_n+1)); sleep 0.2; done;"
    )
    warn = q(
        "fno update: fno-py never reappeared after the install, so the launchd "
        "agents were NOT refreshed onto the new binary. Run by hand: "
        "fno do pr watch refresh; fno backlog groom --refresh-agent"
    )
    return (
        f" {wait} if {probe}; then {{ {body}; }} || true; "
        f"else printf '%s\\n' {warn} >&2; fi;"
    )


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
    check: bool = typer.Option(
        False,
        "--check",
        help="Print update readiness as JSON and exit without installing.",
    ),
) -> None:
    """Reinstall fno from its source directory.

    Picks up local CLI source changes by running ``uv tool install --reinstall``
    (or ``pip install --user --force-reinstall`` if uv is unavailable).
    """
    # Normalize to plain bool: when called directly (not via CLI), Typer Option
    # defaults are OptionInfo objects, not False. Guard against both.
    dry_run = dry_run is True
    force = force is True
    rust = rust is True
    no_rust = no_rust is True
    check = check is True

    if check:
        if dry_run or rust or force:
            raise typer.BadParameter(
                "--check cannot be combined with --dry-run, --rust, or --force"
            )
        typer.echo(json.dumps(update_readiness(source=source)))
        return

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
        # --refresh busts uv's build cache. Without it, a path source at an
        # unchanged version (fno stays 0.2.1 across rebuilds) can reinstall a
        # stale cached wheel that predates newly-added modules, so `fno restart`
        # etc. crash with ModuleNotFoundError even after `fno update`.
        # --compile-bytecode: ship the venv's own .pyc so no later process
        # writes into a tree a reinstall may be deleting
        # (docs/architecture/cli-lazy-imports.md).
        cmd = [
            "uv", "tool", "install",
            "--reinstall", "--refresh", "--compile-bytecode",
            str(resolved),
        ]
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

    # The retry/verify wrapper reads `uv tool dir`, so it applies to the uv
    # command only; the pip fallback runs bare, or its success would be
    # refused by a marker no pip install can produce. None = run bare.
    install_sh = _uv_retry_sh(cmd) if cmd[0] == "uv" else None

    if dry_run:
        # shlex.join shell-escapes each arg so the printed command is safe to
        # paste into a terminal even when the source path contains spaces.
        # The uv path prints the retry snippet, not the bare command: that is
        # what the exec below actually runs, and a receipt that understates it
        # sends an operator back into the unretried failure.
        typer.echo(f"Would run: {install_sh or shlex.join(cmd)}")
        _cache_source_path(resolved)
        return

    _cache_source_path(resolved)

    # Rev we are about to install, recorded so `fno doctor` can later detect
    # installed-vs-source skew. None when the source is not a readable git
    # checkout; the marker is written ONLY on a successful install.
    rev = _source_rev(resolved)

    # Refresh the pr-watch daemon onto the freshly-installed binary. `fno update`
    # only replaces the binary; it never re-renders/reloads the launchd plist, so
    # a migration-update that makes the next tick error wedges the daemon with no
    # self-heal (observed with the config-flatten). Chained AFTER the install so
    # it runs the NEW `fno-py`, and ALWAYS appended: the fresh `pr-watch refresh`
    # self-gates on pr_watch.enabled, so gating here on the OLD binary's config
    # reader would fail closed in the exact migration case this repairs (old
    # reader can't parse the new config -> skip -> daemon stays wedged). Resolve
    # fno-py to an ABSOLUTE, PATH-independent path (a cargo/front-door install may
    # not have fno-py on PATH, and the post-install shell inherits that PATH), and
    # carry it as an argv list so the Windows subprocess quotes it correctly.
    # Every launchd agent fno installs needs this, not just the watcher: each
    # embeds an absolute binary path and none is re-rendered by the install.
    # Each verb self-gates (disabled watcher / no groom plist), so listing one
    # here costs nothing on a machine that does not use it.
    refresh_cmds: list[list[str]] = []
    try:
        from fno.pr_watch.cli import _resolve_fno_binary

        _fno = _resolve_fno_binary()
        refresh_cmds = [
            [_fno, "do", "pr", "watch", "refresh"],
            [_fno, "backlog", "groom", "--refresh-agent"],
        ]
        _await_bin = _fno
    except Exception:
        refresh_cmds = []
        _await_bin = None

    if sys.platform == "win32":
        # On Windows, os.execvp does NOT replace the process: it spawns the
        # installer as a child and terminates the parent with status 0,
        # hiding the install result. Worse, fno.exe is held open by the
        # still-running parent until the parent exits, racing the new
        # install. Use subprocess.run and propagate the real exit code.
        result = subprocess.run(cmd, check=False)
        if result.returncode == 0 and rev:
            _write_installed_rev(rev)
        if result.returncode == 0:
            # Best-effort, mirroring the Unix chain: refresh each agent onto the
            # new binary but never let a failure change the update exit code.
            # List form (no shell) so subprocess handles Windows quoting.
            for _argv in refresh_cmds:
                subprocess.run(_argv, check=False)
        raise typer.Exit(result.returncode)

    # On Unix, execvp replaces this Python process with the installer; uv
    # tool install is then free to replace the fno binary without racing
    # the running interpreter. Because execvp never returns, the installed-rev
    # marker write (and the best-effort pr-watch refresh) are chained onto the
    # installer via the shell so they run iff the install exits 0.
    # `;` not `&&`: the refreshes are independent and best-effort, so a wedged
    # watcher must not skip the groom agent.
    post_install = "; ".join(shlex.join(c) for c in refresh_cmds) or None
    if rev:
        os.execvp(
            "/bin/sh",
            [
                "/bin/sh", "-c",
                _install_then_mark(
                    cmd, rev, marker=_INSTALLED_REV_FILE, pid=os.getpid(),
                    post_install=post_install, await_binary=_await_bin,
                    install_sh=install_sh,
                ),
            ],
        )
    elif post_install:
        # No git rev (source is not a checkout), so no marker write - but still
        # chain the refresh after a successful install via a shell.
        os.execvp(
            "/bin/sh",
            ["/bin/sh", "-c",
             f"{install_sh or shlex.join(cmd)} && "
             f"{{ {_await_binary(post_install, _await_bin)} }}"],
        )
    elif install_sh:
        os.execvp("/bin/sh", ["/bin/sh", "-c", install_sh])
    else:
        os.execvp(cmd[0], cmd)
