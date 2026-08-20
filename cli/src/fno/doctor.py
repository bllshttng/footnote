"""fno doctor: detect skew between the installed fno and its source checkout.

The ``fno`` on a developer's PATH is a snapshot, not a live view of the repo
(ab-5a1fc285). When a new gate-bearing verb ships (e.g. ``backlog inbox`` in
PR #329), an install that predates it silently fails the documented path. This
command makes that skew detectable and self-explaining, **network-free**.

Python-side signals, each degrading to ``unknown`` rather than crying wolf:

1. **Revision compare** (when a source checkout is resolvable): compare
   ``~/.fno/installed-rev`` (written by ``fno update``) against ``git rev-parse
   HEAD`` of the resolved source.
2. **Capability probe** (always-available fallback): run ``fno backlog capture
   --help`` against the *installed* CLI; a "No such command" failure proves a
   missing verb regardless of any marker.
3. **Content compare** (ground truth, cannot be fooled by a lying marker):
   fingerprint the *installed* ``fno`` package's ``.py`` bytes against the
   source working tree ``uv tool install`` would ship. Signal 1 trusts a marker
   ``fno update`` writes on any zero install exit -- but ``uv`` can exit 0 while
   serving a stale *cached* wheel (a no-op reinstall), leaving month-old bytes on
   disk under a marker that reads HEAD. That false 'fresh' went unnoticed until a
   content compare grounded the verdict on the actual installed bytes.

Plus a Rust-side report: which ``fno-agents`` binary ``auto`` mode would use,
and whether the cargo-installed bins are stale relative to the crates/ subtree
rev. The installed rev now comes from the binary itself -- ``fno-agents version
--json`` reports the crates/ subtree rev baked in by build.rs (ab-716cd330) --
not the ``installed-rust-rev`` marker, so a bare ``cargo install`` (no marker)
is judged correctly. Rust staleness is proven only when full evidence is present
(cargo binary exists, the binary self-reports a crates/ rev, crates/ subtree rev
known); any gap degrades to unknown rather than crying wolf.

--fix now repairs the Rust side directly (ab-a78c9731): a rust-only stale
verdict calls ``update._refresh_rust_bins`` without triggering a full Python
reinstall.

Exit code is non-zero only when staleness is **proven**.
"""
from __future__ import annotations

import ast
import hashlib
import json
import math
import os
import re
import signal
import shutil
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal, Optional

import typer

if TYPE_CHECKING:
    from datetime import datetime

# The verb the narrow capability probe checks for: the newest gate-bearing
# verb. A missing `backlog inbox` (PR #329 gate) was the failure that
# motivated this command; the probe now targets the renamed `backlog capture`
# spelling, which also catches installs that predate the rename (ab-bf7cc0d8).
_PROBE_VERB = ("backlog", "capture")
_PROBE_VERB_LABEL = "backlog capture"

# Three-valued probe outcome made explicit (not Optional[bool]) so call sites
# must distinguish "proven missing" from "could not probe" - an `if not x:`
# would silently conflate them. Mirrors the Literal style in health_monitor.py.
ProbeResult = Literal["present", "missing", "unknown"]
# The verdict's discriminator. Only these three values are ever reachable.
DoctorStatus = Literal["fresh", "stale", "unknown"]


# ---------------------------------------------------------------------------
# Signal collectors (module-level so tests monkeypatch them individually)
# ---------------------------------------------------------------------------


def _read_marker() -> Optional[str]:
    """Return the recorded installed rev, or None if the marker is absent.

    A missing marker (install predates the feature) is "rev unknown", NOT a
    false "fresh" - the caller falls back to the capability probe.
    """
    from fno import update

    try:
        text = update._INSTALLED_REV_FILE.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    return text or None


def _resolve_source(source: Optional[Path]) -> Optional[Path]:
    """Resolve a source checkout via the same precedence as ``fno update``.

    Returns None when no source is resolvable (PyPI install, repo absent), so
    the verdict degrades to ``unknown`` rather than hard-failing.
    """
    from fno import update

    try:
        return update._discover_source(source)
    except update.SourceNotFoundError:
        return None


def _source_rev(source: Path) -> Optional[str]:
    """``git rev-parse HEAD`` of the source (reuses update's network-free probe)."""
    from fno import update

    return update._source_rev(source)


def _deployed_config_keys() -> Optional[frozenset[str]]:
    """Config-schema surface of the RUNNING (deployed) CLI: the FIELD_META keyset.

    ``FIELD_META`` is CI-enforced-complete (one entry per config model leaf), so
    its key set is a faithful schema fingerprint. Imported in-process because
    THIS interpreter IS the deployed CLI. Fail-open to None so a broken import
    never crashes doctor.
    """
    try:
        from fno.config.registry import FIELD_META

        return frozenset(FIELD_META)
    except Exception:
        return None


def _parse_field_meta_keys(source_text: str) -> Optional[frozenset[str]]:
    """Extract ``FIELD_META``'s keys from ``registry.py`` source text via AST.

    Returns None (skip the check) when the text is unparseable OR ``FIELD_META``
    is not a flat literal of constant string keys. A spread (``{**base, ...}``)
    or computed-key form cannot be read completely from the AST, and returning a
    PARTIAL keyset would risk a false 'fresh' (real drift masked because the
    source set is truncated) - so fail to None instead of guessing.
    """
    try:
        tree = ast.parse(source_text)
    except (ValueError, SyntaxError):
        return None
    for node in ast.walk(tree):
        targets: list[str] = []
        if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            targets = [node.target.id]
        elif isinstance(node, ast.Assign):
            targets = [t.id for t in node.targets if isinstance(t, ast.Name)]
        if "FIELD_META" not in targets:
            continue
        if not isinstance(node, (ast.AnnAssign, ast.Assign)):
            continue
        if not isinstance(node.value, ast.Dict):
            # A bare `FIELD_META: dict[...]` annotation (value None) or a computed
            # assignment: keep walking to find the real literal rather than
            # skipping the check. A truly computed-only FIELD_META finds no dict
            # and falls through to None below.
            continue
        keys: set[str] = set()
        for k in node.value.keys:
            # k is None for a `**spread` entry; non-Constant for a computed key.
            if not isinstance(k, ast.Constant) or not isinstance(k.value, str):
                return None
            keys.add(k.value)
        return frozenset(keys)
    return None


def _source_config_keys(source: Optional[Path]) -> Optional[frozenset[str]]:
    """Config-schema surface of the SOURCE checkout: ``FIELD_META`` keys parsed
    from its ``registry.py`` at committed ``HEAD``, without importing it.

    Reads the COMMITTED file (``git show HEAD:...``), not the working tree, to
    match ``_source_rev``'s committed-HEAD semantics: an uncommitted local edit
    to registry.py must not flip the verdict while the sibling rev signal still
    reads fresh. Import-free on purpose (a broken source must never crash doctor,
    and importing the source package would clash with the loaded deployed one).
    Returns None - fail-open, skip the check - when the source is not a
    resolvable git checkout, or the committed file is missing/unparseable.
    """
    if source is None:
        return None
    try:
        result = subprocess.run(
            ["git", "-C", str(source), "show", "HEAD:./src/fno/config/registry.py"],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0 or not result.stdout:
        return None
    return _parse_field_meta_keys(result.stdout)


def _installed_pkg_dir() -> Optional[Path]:
    """Directory of the RUNNING (installed) ``fno`` package - the deployed bytes.

    This interpreter IS the deployed CLI, so ``fno.__file__`` points at the
    installed copy (site-packages after ``uv tool install``). None on any import
    quirk so the content check degrades to skip.
    """
    try:
        import fno

        f = getattr(fno, "__file__", None)
        return Path(f).parent if f else None
    except Exception:
        return None


def _pkg_py_fingerprint(pkg_dir: Path) -> Optional[dict[str, str]]:
    """Map each ``.py`` under ``pkg_dir`` to its content sha256, keyed by relpath.

    None (skip the check) when the dir is missing or any file is unreadable - a
    partial fingerprint could miss real drift and read a false 'fresh'.
    """
    try:
        if not pkg_dir.is_dir():
            return None
        fp: dict[str, str] = {}
        for p in sorted(pkg_dir.rglob("*.py")):
            fp[p.relative_to(pkg_dir).as_posix()] = hashlib.sha256(
                p.read_bytes()
            ).hexdigest()
    except OSError:
        return None
    return fp


def _python_content_drift(source: Optional[Path]) -> Optional[int]:
    """Count of ``.py`` files where the INSTALLED package differs from SOURCE.

    Ground-truths freshness on actual bytes instead of the ``installed-rev``
    marker, which ``fno update`` writes on any zero install exit even when ``uv``
    served a stale cached wheel - the exact way a month-old install hid behind a
    HEAD marker. Compares against the source WORKING TREE (not committed HEAD)
    because ``uv tool install <path>`` ships the working tree: an
    uncommitted-but-updated install then reads fresh, and running from source
    (installed dir == source dir) trivially reports 0. None when undeterminable.
    """
    if source is None:
        return None
    inst = _installed_pkg_dir()
    if inst is None:
        return None
    inst_fp = _pkg_py_fingerprint(inst)
    src_fp = _pkg_py_fingerprint(source / "src" / "fno")
    if inst_fp is None or src_fp is None:
        return None
    return sum(
        1 for k in set(inst_fp) | set(src_fp) if inst_fp.get(k) != src_fp.get(k)
    )


def _probe_installed_verb() -> ProbeResult:
    """Probe whether the *installed* fno exposes the known gate verb.

    Returns "present", "missing" (proven via "No such command"), or "unknown"
    (could not probe - no ``fno-py`` on PATH, or a non-zero exit for some other
    reason). "unknown" never asserts staleness.

    Probes ``fno-py`` (the Python CLI console script), NOT ``fno`` (the Rust mux
    front door): the gate verb is a property of the Python CLI, and probing it
    directly keeps this check working even when the front door binary is not
    installed - the front door only forwards here anyway.
    """
    fno_bin = shutil.which("fno-py")
    if not fno_bin:
        return "unknown"
    try:
        result = subprocess.run(
            [fno_bin, *_PROBE_VERB, "--help"],
            capture_output=True,
            text=True,
            check=False,
            timeout=20,
        )
    except (OSError, subprocess.SubprocessError):
        return "unknown"
    if result.returncode == 0:
        return "present"
    combined = f"{result.stderr or ''}{result.stdout or ''}".lower()
    if "no such command" in combined:
        return "missing"
    # Non-zero for some other reason - do not cry wolf.
    return "unknown"


def _read_rust_marker() -> Optional[str]:
    """Return the installed-rust-rev marker content, or None if missing/empty.

    Thin wrapper around update._read_rust_marker so this collector is
    monkeypatchable at the doctor module level (mirrors _read_marker's style).
    """
    from fno import update

    return update._read_rust_marker()


def _rust_source_rev(source: Optional[Path]) -> Optional[str]:
    """Return the last crates/ subtree commit SHA for the given source, or None.

    None when source is None or when the git probe fails. Wrapper around
    update._rust_subtree_rev so the collector is monkeypatchable.
    """
    if source is None:
        return None
    from fno import update

    return update._rust_subtree_rev(source)


def _cargo_bin_present() -> bool:
    """Return True if the cargo-installed fno-agents binary exists."""
    from fno import update

    return update._cargo_installed_bin() is not None


def _cargo_bin_path() -> Optional[str]:
    """Path to the cargo-installed fno-agents binary, or None.

    This is the binary the rust-stale gate (``_cargo_bin_present``) checks and
    that ``fno doctor --fix`` refreshes, so the verdict's installed rev must come
    from it -- not from ``resolve_installed_binary()``, which can return a
    bundled/launcher sibling when one is present (codex PR #491).
    """
    from fno import update

    cargo_bin = update._cargo_installed_bin()
    return str(cargo_bin) if cargo_bin else None


def _binary_version_json(binary: Optional[str]) -> dict:
    """Parsed ``<binary> version --json`` (the build.rs embed), or ``{}``.

    One subprocess spawn shared by ``_binary_self_rev`` and ``_binary_crates_rev``
    so the ``doctor`` command does not pay for ``version --json`` twice on the
    same binary (gemini PR #491). Any failure (no binary, old binary without the
    verb, non-zero exit, malformed/non-dict JSON) degrades to ``{}`` rather than
    crying wolf. Module-level so tests monkeypatch it like the other collectors.
    """
    if not binary:
        return {}
    try:
        result = subprocess.run(
            [binary, "version", "--json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=20,
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    if result.returncode != 0:
        return {}
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError:
        return {}
    return data if isinstance(data, dict) else {}


def _clean_rev(rev: object) -> Optional[str]:
    """Normalize a self-reported rev: None for missing or the literal "unknown"."""
    if not isinstance(rev, str) or not rev or rev == "unknown":
        return None
    return rev


def _binary_self_rev(binary: Optional[str]) -> Optional[str]:
    """The full HEAD rev (``git_rev``) the fno-agents binary self-reports, or None.

    Baked in by build.rs (ab-24a59d50); surfaced informationally as an identity
    signal. See ``_binary_version_json`` for the failure contract.
    """
    return _clean_rev(_binary_version_json(binary).get("git_rev"))


def _binary_crates_rev(binary: Optional[str]) -> Optional[str]:
    """The crates/ subtree rev (``crates_rev``) the binary self-reports, or None.

    The last commit touching crates/ at the HEAD the binary was built from, baked
    in by build.rs (ab-716cd330). This is the marker-free staleness signal the
    rust verdict keys on: unlike ``installed-rust-rev`` (written only by ``fno
    update``), it is true for ANY install path, including a bare ``cargo
    install``. Its semantics MATCH ``_rust_source_rev`` (both are the crates/
    subtree rev, not HEAD), so the verdict compares apples-to-apples.
    """
    return _clean_rev(_binary_version_json(binary).get("crates_rev"))


def _rust_report() -> dict[str, Optional[str]]:
    """Report which fno-agents binary ``auto`` mode resolves.

    The ``revision`` key carries the verdict-driving rust rev: the crates/
    subtree rev of the CARGO-installed binary (ab-716cd330). It is sourced from
    the cargo binary -- not ``resolve_installed_binary()`` -- because the rust
    gate (``_cargo_bin_present``) and ``--fix`` both target the cargo binary;
    reading the rev from a bundled sibling would misjudge or misrepair (codex
    PR #491). It replaces the old ``installed-rust-rev`` marker, which only
    tracked ``fno update`` cargo installs and missed a bare ``cargo install``.
    ``binary``/``binary_rev`` describe the binary ``auto`` actually runs (display
    + HEAD identity, ab-24a59d50). A probe error degrades to None rather than
    aborting the verdict.
    """
    binary: Optional[Path] = None
    try:
        from fno.rust_binary import resolve_installed_binary

        binary = resolve_installed_binary()
    except Exception:
        binary = None
    binary_str = str(binary) if binary else None
    cargo_str = _cargo_bin_path()
    # Spawn `version --json` once when the resolved and cargo binaries are the
    # same path (the common case -> gemini PR #491); only probe twice when they
    # genuinely diverge (a bundled sibling alongside a cargo install).
    resolved_ver = _binary_version_json(binary_str)
    cargo_ver = (
        resolved_ver if cargo_str == binary_str else _binary_version_json(cargo_str)
    )
    return {
        "binary": binary_str,
        "revision": _clean_rev(cargo_ver.get("crates_rev")),
        "binary_rev": _clean_rev(resolved_ver.get("git_rev")),
    }


def _plugin_registry_path() -> Path:
    """The claude plugin install registry (module-level so tests can stub it)."""
    return Path.home() / ".claude" / "plugins" / "installed_plugins.json"


def _plugin_cache_report() -> dict[str, Optional[str]]:
    """Freshness of the deployed CLAUDE plugin cache the hooks run from.

    ``fno doctor`` already owns source-vs-installed staleness for the wheel and
    the cargo bins, but not for ``~/.claude/plugins/cache/footnote``: the copy
    ``hooks/helpers/init-target-state.sh`` (resolved via CLAUDE_PLUGIN_ROOT)
    actually executes in every Claude session. A cache pinned to a pre-feature
    sha ships hooks that predate provenance writers while every Python-side
    check reads green - the exact gap that left armed manifests reporting
    ``auto_merge_source: unknown`` after x-9d11.

    Uses the module's staleness vocabulary: ``fresh`` when the pinned sha IS
    the source HEAD, ``stale`` when the sha is a proven ancestor of HEAD (and
    not HEAD), ``unknown`` when the installed-plugins file is missing, the sha
    is unknown to this clone, or git is unavailable. Never asserts staleness on
    absent evidence (same rule as the exit-code contract at module top).
    """
    report: dict[str, Optional[str]] = {
        "status": "unknown",
        "sha": None,
        "installed_at": None,
        "detail": None,
    }
    try:
        registry = _plugin_registry_path()
        data = json.loads(registry.read_text(encoding="utf-8"))
        plugins = data.get("plugins") if isinstance(data, dict) else None
        entries = (
            plugins.get("fno@footnote") if isinstance(plugins, dict) else None
        ) or []
        entry = entries[0] if isinstance(entries, list) and entries else {}
    except (OSError, ValueError, IndexError, AttributeError, TypeError, KeyError):
        # A hand-edited, corrupted, or future-version registry is exactly the
        # broken install this advisory leg exists to describe: any malformed
        # shape degrades to unknown, never a traceback through doctor_command's
        # unwrapped call sites.
        report["detail"] = "no installed_plugins.json entry for fno@footnote"
        return report
    sha = entry.get("gitCommitSha")
    if not sha:
        report["detail"] = "installed_plugins.json carries no gitCommitSha"
        return report
    report["sha"] = sha
    report["installed_at"] = entry.get("installedAt")

    src = _resolve_source(None)
    if src is None:
        report["detail"] = "no source checkout to compare against"
        return report
    try:
        head = subprocess.run(
            ["git", "-C", str(src), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=15,
        )
        if head.returncode != 0:
            report["detail"] = "git rev-parse failed in the source checkout"
            return report
        if head.stdout.strip() == sha:
            report["status"] = "fresh"
            return report
        ancestor = subprocess.run(
            ["git", "-C", str(src), "merge-base", "--is-ancestor", sha, "HEAD"],
            capture_output=True,
            timeout=15,
        )
        if ancestor.returncode == 0:
            report["status"] = "stale"
        else:
            # Not HEAD and not an ancestor: a foreign sha. Unknown, never
            # stale-on-absent-evidence.
            report["detail"] = "pinned sha is not known as an ancestor of HEAD"
    except (OSError, subprocess.SubprocessError):
        report["detail"] = "git unavailable"
    return report


# ---------------------------------------------------------------------------
# Cost cross-check (--cost-check, opt-in - ab-c0f92987)
# ---------------------------------------------------------------------------
#
# Compares our session-cost.py math against ccusage (the community reference
# that dedups transcript lines and tracks pricing) for one recent session.
# Opt-in only: doctor's default run stays network-free and never assumes
# ccusage is installed. Three outcomes:
#   OK      divergence <= threshold          -> exit 0
#   WARN    divergence  > threshold          -> exit 1 (doctor warning state)
#   SKIPPED prerequisites missing / errors   -> exit 0, one info line
#
# The collectors below are module-level so tests monkeypatch them
# individually (same style as the staleness signal collectors above).

_COST_DIVERGENCE_THRESHOLD = 0.10  # relative divergence that flips OK -> WARN

_SESSION_UUID_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
)


def _find_transcript_for(session_id: str) -> Optional[Path]:
    """Locate a transcript JSONL by session UUID across ~/.claude/projects."""
    if not _SESSION_UUID_RE.match(session_id):
        return None
    base = Path.home() / ".claude" / "projects"
    if not base.is_dir():
        return None
    for project_dir in base.iterdir():
        candidate = project_dir / f"{session_id}.jsonl"
        if candidate.exists():
            return candidate
    return None


def _find_recent_session_with_transcript() -> Optional[tuple[str, Path]]:
    """Most recent ledger-registered session whose transcript survives."""
    from fno import paths as _paths

    try:
        data = json.loads(_paths.ledger_json().read_text())
    except (OSError, json.JSONDecodeError):
        return None
    entries = data if isinstance(data, list) else data.get("entries", [])
    for entry in reversed(entries):
        for sid in reversed(entry.get("sessions") or []):
            transcript = _find_transcript_for(str(sid))
            if transcript is not None:
                return str(sid), transcript
    return None


def _run_session_cost(session_id: str) -> Optional[float]:
    """Our number: run the in-package _session_cost --json via `python3 -m`."""
    try:
        result = subprocess.run(
            [sys.executable, "-m", "fno.cost._session_cost", "--json", session_id],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
        if result.returncode != 0:
            return None
        cost = json.loads(result.stdout).get("cost_usd")
        return float(cost) if cost is not None else None
    except (OSError, subprocess.SubprocessError, json.JSONDecodeError, ValueError):
        return None


def _run_ccusage(session_id: str) -> tuple[Optional[float], Optional[str]]:
    """ccusage's number for the session, or (None, skip-reason)."""
    ccusage_bin = shutil.which("ccusage")
    if not ccusage_bin:
        return None, "ccusage not installed"
    try:
        result = subprocess.run(
            [ccusage_bin, "session", "--json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return None, f"ccusage failed to run: {exc}"
    if result.returncode != 0:
        return None, f"ccusage exited {result.returncode}"
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
        return None, "ccusage emitted unparseable output"
    if isinstance(payload, dict):
        sessions = payload.get("sessions") or payload.get("data") or []
    else:
        sessions = payload if isinstance(payload, list) else []
    for item in sessions:
        if not isinstance(item, dict):
            continue
        item_id = item.get("sessionId") or item.get("session_id") or item.get("id")
        # ccusage's session key is not pinned across versions: some emit the
        # bare transcript UUID, others a project-qualified path ending in
        # it. UUIDs do not collide, so suffix matching stays precise.
        if not isinstance(item_id, str) or not (
            item_id == session_id or item_id.endswith(session_id)
        ):
            continue
        # Cost key drift across ccusage versions: totalCost is current;
        # the rest are observed/plausible variants kept for liberality.
        for key in ("totalCost", "total_cost", "costUSD", "cost_usd", "cost"):
            value = item.get(key)
            if isinstance(value, (int, float)):
                return float(value), None
        return None, "ccusage session row carries no cost field"
    return None, "session not present in ccusage output"


def _cost_check() -> int:
    """Run the cost cross-check. Returns the process exit code."""

    def skip(reason: str) -> int:
        typer.echo(f"fno doctor: cost-check skipped ({reason}).")
        return 0

    found = _find_recent_session_with_transcript()
    if found is None:
        return skip("no completed session with a surviving transcript")
    session_id, _transcript = found

    ours = _run_session_cost(session_id)
    if ours is None:
        return skip(f"fno.cost._session_cost unavailable or failed for {session_id}")

    theirs, reason = _run_ccusage(session_id)
    if theirs is None:
        return skip(reason or "ccusage unavailable")

    if theirs == 0:
        divergence = 0.0 if ours == 0 else float("inf")
    else:
        divergence = abs(ours - theirs) / theirs

    pct = f"{divergence * 100:.1f}%" if divergence != float("inf") else "inf"
    if divergence <= _COST_DIVERGENCE_THRESHOLD:
        typer.echo(
            f"fno doctor: cost-check OK: session {session_id} "
            f"ours=${ours:.2f} ccusage=${theirs:.2f} divergence={pct}"
        )
        return 0
    typer.echo(
        f"fno doctor: cost-check WARN: session {session_id} "
        f"ours=${ours:.2f} ccusage=${theirs:.2f} divergence={pct} "
        f"(> {_COST_DIVERGENCE_THRESHOLD * 100:.0f}% - pricing table or "
        "dedup drift; see scripts/lib/cost_tracker.py)"
    )
    return 1


# ---------------------------------------------------------------------------
# Mux front door health (x-c267)
# ---------------------------------------------------------------------------


def _cargo_installed_mux() -> Optional[Path]:
    """Path to the cargo-installed mux front-door binary (`fno`), or None.

    Thin wrapper around ``update._cargo_installed_mux`` (single source of truth,
    shared with `fno update`'s install path) so this collector stays patchable.
    Probes the default ``$CARGO_HOME/bin``; a custom-``--root`` install is caught
    instead by the ``which("fno")`` + mux-verb probe in ``_mux_front_door_report``.
    """
    from fno import update

    return update._cargo_installed_mux()


def _probe_is_mux(fno_path: str) -> bool:
    """True if the `fno` at ``fno_path`` responds to a mux-only verb - i.e. it is
    the Rust mux front door, not some other binary named `fno`. Runs
    ``fno mux ls --json`` (read-only, no TTY; the Python CLI has no `mux`
    subcommand and fails "No such command"). Bounded + best-effort: any error or
    non-zero exit -> False, so it never cries wolf or hangs the doctor."""
    try:
        result = subprocess.run(
            [fno_path, "mux", "ls", "--json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0


def _mux_front_door_report() -> dict[str, Any]:
    """Report whether the Rust mux binary owns `fno` on PATH (advisory only).

    ``mux_front_door`` is one of:
    - ``active``: `fno` on PATH IS the mux - it either resolves to the
      cargo-installed mux binary, or (custom ``--root`` / non-default
      ``CARGO_HOME``) answers a mux-only verb.
    - ``shadowed``: the mux is cargo-installed but `fno` on PATH is not it (a
      Python binary, another `fno`) or is off PATH - so bare `fno` will not
      launch the mux.
    - ``not-installed``: no cargo-installed mux AND `fno` on PATH is not a mux.

    Never changes the verdict status or exit code: a front-door setup problem is
    distinct from source-vs-installed staleness.
    """
    mux = _cargo_installed_mux()
    path_fno = shutil.which("fno")
    path_is_mux = path_fno is not None and (
        (mux is not None and Path(path_fno).resolve() == mux.resolve())
        or _probe_is_mux(path_fno)
    )
    if path_is_mux:
        state = "active"
    elif mux is not None:
        state = "shadowed"
    else:
        state = "not-installed"
    return {
        "mux_binary": str(mux) if mux else None,
        "path_fno": path_fno,
        "mux_front_door": state,
    }


# Runtime files no code writes anymore (Group 3 GC wave: convo-signals
# capture, tasks.json/md migration, evals-history, metrics.jsonl analytics).
# Purely informational - never changes doctor's status or exit code.
_ORPHAN_BASENAMES = (
    "convo-signals.jsonl",
    "tasks.json",
    "tasks.md",
    "evals-history.jsonl",
    "metrics.jsonl",
)


def _orphan_report() -> list[str]:
    """Leftover files from deleted capture/migration paths.

    Checks the default global state dir (``~/.fno``, not a configured
    override - this is a lightweight advisory check, not a path-config-aware
    operation) and the project ``.fno/`` dir. Returns an empty list on a
    clean machine, or if either dir can't be resolved (e.g. cwd deleted out
    from under a running shell); never raises.
    """
    dirs: list[Path] = []
    for get_dir in (Path.home, Path.cwd):
        try:
            dirs.append(get_dir() / ".fno")
        except OSError:
            continue

    found: set[str] = set()
    for d in dirs:
        for name in _ORPHAN_BASENAMES:
            p = d / name
            if p.exists():
                found.add(str(p))
    return sorted(found)


def _pr_watch_liveness() -> dict[str, Any]:
    """Ground-truth liveness verdict for the global PR-watch agent (x-e106).

    Advisory: never changes doctor's status/exit. Degrades to ``unknown``
    (silent) rather than crying wolf when the check itself can't run.
    """
    try:
        from fno.pr_watch import _install as m

        return m.liveness_report_live()
    except Exception:
        # Same dict shape as liveness_report so a future non-.get() reader
        # cannot KeyError on the exception path.
        return {
            "enabled": False,
            "verdict": "unknown",
            "detail": "",
            "fix": None,
            "loaded": False,
            "last_tick": None,
        }


_FD_SOFT_FLOOR = 1024


def _parse_launchctl_maxfiles(stdout: str) -> Optional[int]:
    """Soft limit from a `launchctl limit maxfiles` line, or None.

    Pure function over the `maxfiles 256 unlimited` shape so the verdict is
    unit-testable without a Mac.
    """
    for line in stdout.splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[0] == "maxfiles":
            try:
                return int(fields[1])
            except ValueError:
                return None
    return None


def _fd_limit_report() -> dict[str, Any]:
    """Soft RLIMIT_NOFILE of THIS process, beside the launchd session default.

    Reporting one number reproduces the disagreement this check exists to end:
    a login shell reads 1048576 while every launchd-spawned worker on the same
    machine runs at 256, and both readings are correct. The limit belongs to
    the launch context, not the machine. Advisory: never changes status/exit.
    """
    import resource

    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    report: dict[str, Any] = {
        "soft": soft,
        "hard": "unlimited" if hard == resource.RLIM_INFINITY else hard,
        "threshold": _FD_SOFT_FLOOR,
        "launchd_soft": None,
        "kern_maxfiles": None,
    }
    if sys.platform == "darwin":
        probe = _bounded_command(["launchctl", "limit", "maxfiles"])
        if probe and probe[0] == 0:
            report["launchd_soft"] = _parse_launchctl_maxfiles(probe[1])
        kern = _bounded_command(["sysctl", "-n", "kern.maxfiles"])
        if kern and kern[0] == 0:
            try:
                report["kern_maxfiles"] = int(kern[1].strip())
            except ValueError:
                pass
    # Linux spells RLIM_INFINITY as -1, which isinstance(int) happily passes;
    # an unlimited soft limit is the healthiest reading, never "low".
    healthy = resource.RLIM_INFINITY
    measured = [
        v
        for v in (report["soft"], report["launchd_soft"])
        if isinstance(v, int) and v != healthy
    ]
    report["verdict"] = "low" if any(v <= _FD_SOFT_FLOOR for v in measured) else "ok"
    return report


def _groom_health() -> dict[str, Any]:
    """Freshness of the daily grooming pass, plus whether its agent is installed.

    Four grooming surfaces have now shipped and never run, every time because
    nothing reported the silence. This is the report.
    """
    try:
        from fno.backlog.groom import GROOM_LABEL, GROOM_STALE_HOURS, groom_staleness

        state, hours = groom_staleness()
        plist = Path.home() / "Library" / "LaunchAgents" / f"{GROOM_LABEL}.plist"
        return {
            "state": state,
            "hours": hours,
            "stale": state == "never" or (state == "ran" and hours is not None and hours > GROOM_STALE_HOURS),
            "agent_installed": plist.exists(),
        }
    except Exception:  # noqa: BLE001 - an alarm that crashes doctor helps nobody
        return {"state": "unknown", "hours": None, "stale": False, "agent_installed": False}


def _archive_id_collisions() -> dict[str, Any]:
    """Ids present in BOTH the working graph and the archive (x-f69b).

    Each is a real collision, not a duplicate: the id generator only checked
    the working graph, so a freed id gets reminted while the archive still
    holds a different node under the same id. A repeat sweep frees more ids,
    so this count grows on its own; it changes doctor's exit code rather than
    reporting quietly.

    The archive is read via ``_read_json``, NOT ``read_graph``: the read path
    swallows corruption to an empty list, which would report 0 collisions and
    exit green in exactly the state where the ids cannot be checked.
    """
    try:
        from fno.graph.store import GraphCorruptError, _apply_graph_defaults, _read_json
        from fno.paths import graph_archive_json
        from fno.tracker.metadata import read_entries

        archive_path = graph_archive_json()
        if not archive_path.exists():
            return {"count": 0, "ids": []}
        # Guarded metadata read: this alarm compares the LOCAL store against
        # its LOCAL archive, which is default-backend machinery; an external
        # selection degrades to the silent count-0 path through the except.
        working_ids = {
            nid for e in read_entries("doctor")
            if isinstance(e, dict) and isinstance(nid := e.get("id"), str)
        }
        try:
            archive_entries = _apply_graph_defaults(_read_json(archive_path))
        except GraphCorruptError:
            return {"count": 0, "ids": [], "unreadable": True}
        archive_ids = {
            nid for e in archive_entries
            if isinstance(e, dict) and isinstance(nid := e.get("id"), str)
        }
        collisions = sorted(working_ids & archive_ids)
        return {"count": len(collisions), "ids": collisions}
    except Exception:  # noqa: BLE001 - an alarm that crashes doctor helps nobody
        return {"count": 0, "ids": []}


def _post_merge_sync_health() -> dict[str, Any]:
    """Is the canonical checkout current with recently-merged PRs?

    The failure class here is "the daemon runs but does nothing" - five merges
    went unsynced while process-liveness checks all read green. Only outcome
    truth (sync markers, commit distance) can see it, which is what the
    predicate below reads. Read-only, so it reports regardless of
    ``post_merge.auto_run``: reporting is not acting.
    """
    try:
        from fno.pr._sync_canonical import sync_staleness

        # fetch=True: doctor is the human-facing report and runs interactively,
        # so it can afford the fetch that makes the behind-count trustworthy.
        # Without it a merge aged out of the gh window leaves no markerless row
        # and a stale remote-tracking ref reads as zero behind - the outage goes
        # invisible exactly when it has lasted longest.
        st = sync_staleness(fetch=True)
        return {
            "state": st.state,
            "stale": st.state == "stale",
            "behind": st.behind,
            "detail": st.detail,
        }
    except Exception:  # noqa: BLE001 - an alarm that crashes doctor helps nobody
        return {"state": "unknown", "stale": False, "behind": None, "detail": ""}


def _launch_agent_failures() -> dict[str, Any]:
    """Every ``sh.fno.*`` LaunchAgent whose LAST EXIT was nonzero.

    Generic over the label prefix rather than groom-specific: two unrelated fno
    agents were dead and silent when this was written, so one loop is both
    smaller and wider than a bespoke check per agent. Column 2 is the last exit,
    not current state - a ``-`` in column 1 is normal for a periodic job.
    """
    if sys.platform != "darwin" or not shutil.which("launchctl"):
        return {"applicable": False, "dead": []}
    try:
        proc = subprocess.run(
            ["launchctl", "list"], capture_output=True, text=True, timeout=10
        )
    except Exception:  # noqa: BLE001 - an unrunnable probe must not fabricate an alarm
        return {"applicable": False, "dead": []}
    if proc.returncode != 0:
        return {"applicable": False, "dead": []}

    dead: list[dict[str, Any]] = []
    for line in (proc.stdout or "").splitlines():
        cols = line.split("\t")
        if len(cols) < 3 or not cols[2].startswith("sh.fno."):
            continue
        try:
            status = int(cols[1])
        except ValueError:
            continue  # "-" or a header; only a numeric exit proves a failure
        if status != 0:
            dead.append({"label": cols[2].strip(), "exit": status})
    return {"applicable": True, "dead": dead}


# --------------------------------------------------------------------------
# Silent-switch legibility (x-8cd5 Wave 6). Fail-safe defaults compose to
# inertness, and inertness is invisible because every component behaves as
# designed: a disabled drain with missions queued looks identical to a clean
# queue with nothing to do. The symmetric risk is a default-on/armed switch
# silently taking an irreversible action (auto-merge). The rule, recorded in
# both directions: any default-off switch that can silently produce inaction
# owes a doctor line; any default-on switch that can silently take an
# irreversible action owes one too. Advisory only, never changes the exit code.
# --------------------------------------------------------------------------


def _mission_active_count() -> int:
    """Backlog epics carrying ``mission_active`` (the drain's input set).

    Delegates to the canonical fail-safe reader so a torn graph reads as 0
    rather than crashing the report. A hand-rolled json walk raised TypeError on
    ``{"entries": null}`` because the null value iterated outside the try, which
    broke this function's own ``never crashes`` promise."""
    try:
        from fno.active_backlog import _active_missions

        return len(_active_missions())
    except Exception:  # noqa: BLE001 - advisory; never crash doctor
        return 0


def _auto_merge_armed_manifests() -> dict[str, int]:
    """Worktree manifests whose resolved ``auto_merge_approved`` is true,
    counted by ``auto_merge_source`` (x-9d11).

    Each is one run's standing merge authority; the count is the legibility
    point (an armed manifest set against an operator who expects to review),
    and the per-source breakdown answers the operator's first question - WHICH
    layer set each posture. A manifest predating the source field counts under
    ``unknown``, never a guessed origin. Scans BOTH worktree homes: the
    fno-managed base (``paths.worktrees_base()``) and the harness-native
    ``<repo>/.claude/worktrees`` (the default policy's location, which the fno
    base does not cover, so without it the count reads 0 on a default-config
    machine). Uses ``parse_target_state`` so a quoted ``"true"`` coerces
    instead of string-matching to nothing. Fixed-depth globs avoid descending
    into each worktree's own tree the way rglob would.
    """
    bases: list[Path] = []
    try:
        from fno import paths as _paths

        bases.append(_paths.worktrees_base())
    except Exception:  # noqa: BLE001 - advisory; never crash doctor
        pass
    try:
        repo = _resolve_source(None)
        if repo:
            bases.append(Path(repo) / ".claude" / "worktrees")
    except Exception:  # noqa: BLE001 - advisory; never crash doctor
        pass

    try:
        from fno.cost._register import parse_target_state
    except Exception:
        return {}

    counts: dict[str, int] = {}
    seen: set[Path] = set()
    for base in bases:
        if not base.is_dir():
            continue
        # Two layouts live under these bases: <base>/<name>/.fno/target-state.md
        # (harness-native) and <base>/<repo>/<name>/.fno/target-state.md
        # (fno-managed). Fixed-depth globs skip each worktree's interior.
        for pattern in ("*/.fno/target-state.md", "*/*/.fno/target-state.md"):
            for mf in base.glob(pattern):
                real = mf.resolve()
                if real in seen:
                    continue
                seen.add(real)
                try:
                    state = parse_target_state(str(mf))
                    if state.get("auto_merge_approved") is True:
                        source = state.get("auto_merge_source") or "unknown"
                        counts[str(source)] = counts.get(str(source), 0) + 1
                except (OSError, ValueError):
                    continue
    return counts


def _read_posture_stamp() -> Optional[dict[str, Any]]:
    """Advisory provenance written by ``fno posture apply``; None if absent.

    Doctor may DISPLAY this; config resolution must never read it, or the
    applied posture becomes a resolve-time layer (the trap ``fno posture`` was
    designed to avoid)."""
    try:
        from fno import paths as _paths

        p = _paths.state_dir() / "posture.json"
        if not p.is_file():
            return None
        data = json.loads(p.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else None
    except Exception:  # noqa: BLE001 - advisory; never crash doctor
        return None


def _silent_switch_report(plugin_cache: Optional[dict[str, Any]] = None) -> dict[str, Any]:
    """Both directions of the silent-switch rule.

    Direction "inaction": a default-off switch silently producing nothing while
    work waits (drain off + missions queued; think_spawn off). Direction
    "irreversible": a default-on/armed switch that can merge a green PR with no
    operator (auto_merge.enabled, auto_merge.grant, armed manifests). Each
    finding names the switch, a count where one exists, and the exact command.
    """
    try:
        from fno.config import load_settings

        s = load_settings()
    except Exception:  # noqa: BLE001 - a config that won't load is not doctor's alarm
        s = None

    def _leaf(block: Any, attr: str) -> Optional[bool]:
        v = getattr(block, attr, None)
        return bool(v) if isinstance(v, bool) else None

    ab = _leaf(getattr(s, "active_backlog", None), "enabled")
    ts = _leaf(getattr(s, "think_spawn", None), "enabled")
    am = _leaf(getattr(s, "auto_merge", None), "enabled")
    # Actor scope (x-4be1): the grant key replaces the dispatch.auto_merge
    # bool. Read through getattr so a stub settings object in tests degrades
    # to None (not armed) rather than raising.
    grant = getattr(getattr(s, "auto_merge", None), "grant", None)

    findings: list[dict[str, Any]] = []
    missions = _mission_active_count()
    # Direction 1: default-off -> silent inaction.
    if ab is False and missions > 0:
        findings.append(
            {
                "direction": "inaction",
                "switch": "active_backlog.enabled",
                "count": missions,
                "count_label": "mission_active epic(s) queued",
                "command": "fno config set active_backlog.enabled true",
            }
        )
    if ts is False:
        findings.append(
            {
                "direction": "inaction",
                "switch": "think_spawn.enabled",
                "command": "fno config set think_spawn.enabled true",
            }
        )
    # Direction 2: armed -> silent irreversible action.
    if am is True:
        findings.append(
            {
                "direction": "irreversible",
                "switch": "auto_merge.enabled",
                "command": "fno config set auto_merge.enabled false",
            }
        )
    if grant == "dispatch":
        findings.append(
            {
                "direction": "irreversible",
                "switch": "auto_merge.grant",
                "command": "fno config set auto_merge.grant none",
            }
        )
    # A manifest's per-run approval is inert while the kill-switch
    # (auto_merge.enabled) is off: the sanctioned merge path checks that switch
    # first, so a green PR cannot merge unattended no matter how many manifests
    # carry auto_merge_approved. Counting them as an active irreversible risk in
    # that state is a false alarm; the kill-switch-off inaction finding above
    # already names that state. Only when the switch is ON do armed manifests
    # become a live, silent irreversible action worth a doctor line.
    armed = _auto_merge_armed_manifests()
    if armed and am is True:
        total = sum(armed.values())
        # Name WHICH layer set each posture (x-9d11): "24 manifests" hides that
        # 12 came from config and 12 from an env grant; the breakdown is the
        # actionable half of the count.
        breakdown = ", ".join(
            f"{n} {src}" for src, n in sorted(armed.items(), key=lambda kv: -kv[1])
        )
        finding = {
            "direction": "irreversible",
            "switch": "auto_merge_approved (worktree manifests)",
            "count": total,
            "count_label": f"manifest(s): {breakdown}",
            "command": "fno config set auto_merge.enabled false",
        }
        # An ``unknown`` count is answerable, not fated (x-4be1): a proven-stale
        # deployed plugin cache is a LIKELY cause (a cache pinned before the
        # provenance writer cannot stamp manifests), and ``fno update`` is the
        # fix for that cause. Only speak when the plugin-cache signal PROVES
        # stale, and say "likely" - ancestor-ness alone does not prove the
        # pinned sha predates the writer, and a pre-provenance manifest with a
        # fresh cache stays a bare unknown (never guess an origin).
        if armed.get("unknown"):
            cache = plugin_cache if plugin_cache is not None else _plugin_cache_report()
            if cache.get("status") == "stale":
                sha = str(cache.get("sha") or "")[:12]
                when = str(cache.get("installed_at") or "")[:10] or "?"
                finding["cause"] = (
                    f"deployed plugin cache is stale ({sha}, {when}); likely "
                    "predates the auto_merge_source writer. Fix: fno update"
                )
        findings.append(finding)
    return {"findings": findings, "posture": _read_posture_stamp()}


def _bounded_command(argv: list[str]) -> Optional[tuple[int, str, str]]:
    try:
        proc = subprocess.Popen(
            argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
            text=True,
        )
        stdout, stderr = proc.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.communicate()
        return None
    except OSError:
        return None
    return proc.returncode, stdout, stderr


def _git_checkout_identity(path: Path) -> Optional[tuple[Path, Path]]:
    result = _bounded_command(
        [
            "git",
            "-C",
            str(path),
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ]
    )
    if result is None or result[0] != 0:
        return None
    lines = result[1].splitlines()
    if len(lines) != 2:
        return None
    return Path(lines[0]).resolve(), Path(lines[1]).resolve()


def _preamble_budget_line(
    source_root: Optional[Path],
    *,
    cwd: Optional[Path] = None,
) -> Optional[str]:
    """Return the quiet report only inside the resolved Footnote checkout."""
    if source_root is None:
        return None
    if cwd is None:
        try:
            cwd = Path.cwd()
        except OSError:
            return None
    source = _git_checkout_identity(source_root)
    current = _git_checkout_identity(cwd)
    if source is None or current is None or source[1] != current[1]:
        return None
    root = current[0]
    gate = root / "scripts" / "ci" / "check-preamble-budget.sh"
    if not gate.is_file():
        return None

    result = _bounded_command(["bash", str(gate), "--quiet", str(root)])
    if result is None:
        return "preamble: unavailable (check did not complete)"
    line = next(
        (line for line in result[1].splitlines() if line.startswith("preamble:")),
        None,
    )
    if line is not None:
        return line
    if result[0] != 0:
        detail = next((line.strip() for line in result[2].splitlines() if line.strip()), "")
        if detail:
            return f"preamble: unavailable ({detail[:160]})"
        return f"preamble: unavailable (check exited {result[0]})"
    return "preamble: unavailable (check emitted no report)"


# ---------------------------------------------------------------------------
# Verdict
# ---------------------------------------------------------------------------


def _verdict(
    *,
    source_resolved: bool,
    source_rev: Optional[str],
    marker: Optional[str],
    capture_present: ProbeResult,
    rust_binary: Optional[str] = None,
    rust_installed_rev: Optional[str] = None,
    rust_source_rev: Optional[str] = None,
    cargo_bin_present: bool = False,
    deployed_config_keys: Optional[frozenset[str]] = None,
    source_config_keys: Optional[frozenset[str]] = None,
    content_drift_count: Optional[int] = 0,
) -> dict[str, Any]:
    """Pure verdict function (no I/O) returning the complete JSON-serializable
    result, so the decision matrix is unit-testable and the output contract is
    assembled in exactly one place.

    Rust staleness is proven only with full evidence: a cargo binary exists,
    the installed-rust-rev marker is known, the crates/ subtree rev is known,
    and they differ. Any missing evidence piece degrades to "not stale" (never
    cry wolf). Rust evidence gaps never upgrade unknown to fresh and never
    block fresh.

    Config-schema drift follows the same full-evidence rule: only when BOTH
    keysets are known and the source defines keys the deployed CLI lacks is the
    Python schema proven stale. A deployed CLI AHEAD of source is never stale.
    """
    missing_verbs: list[str] = []
    python_stale = False
    status: DoctorStatus

    if capture_present == "missing":
        # Capability probe proved a missing verb - stale regardless of marker.
        python_stale = True
        missing_verbs = [_PROBE_VERB_LABEL]
        status = "stale"
    elif source_resolved and source_rev is not None and marker is not None:
        if marker == source_rev:
            status = "fresh"
        else:
            python_stale = True
            status = "stale"
    else:
        # No source, undeterminable source rev, or no marker to compare against.
        # Cannot prove stale; must not cry wolf (and must not claim false fresh).
        status = "unknown"

    # Config-schema drift: the deployed FIELD_META keyset is a schema fingerprint.
    # Source keys the deployed CLI lacks mean the install predates a config block
    # and silently mis-mints IDs - a stale the rev/verb signals miss (they read
    # "unknown" when the install predates the rev marker). Full evidence only;
    # proven drift upgrades even an "unknown" status to stale.
    missing_config_keys: list[str] = []
    if deployed_config_keys is not None and source_config_keys is not None:
        missing_config_keys = sorted(source_config_keys - deployed_config_keys)
    if missing_config_keys:
        python_stale = True
        status = "stale"

    # Content drift: the authoritative Python signal. Installed .py bytes differ
    # from the source the updater would install -> stale regardless of what the
    # marker claims (this is what catches a cache-hit reinstall the marker lies
    # about). A None count means the check could not run; only a positive count
    # is stale, so a deployed CLI byte-identical to source (0) never flips.
    content_stale = content_drift_count is not None and content_drift_count > 0
    if content_stale:
        python_stale = True
        status = "stale"

    # If the authoritative content check could not run, a "fresh" verdict would
    # rest only on the installed-rev marker - the signal that lies about a
    # cache-hit reinstall. Downgrade to unknown rather than assert a marker-only
    # fresh (the module's never-claim-false-fresh rule). Only bites the rare
    # resolved-source-but-unreadable-bytes case: a source-absent install is
    # already unknown, and a normal install yields a count (0 or N), never None.
    # Unlike config-schema drift (a supplementary check that skips on None), the
    # content signal is authoritative, so its absence is not neutral for a fresh.
    # The param DEFAULTS to 0 (no content concern) so a caller uninterested in
    # content is neutral; an EXPLICIT None means _python_content_drift genuinely
    # could not run - only that downgrades.
    content_indeterminate = content_drift_count is None
    if content_indeterminate and status == "fresh":
        status = "unknown"

    # Rust staleness: requires full evidence. Partial evidence is never stale.
    rust_stale = (
        cargo_bin_present
        and rust_installed_rev is not None
        and rust_source_rev is not None
        and rust_installed_rev != rust_source_rev
    )

    # Fold rust staleness into overall status.
    if rust_stale and status != "stale":
        status = "stale"

    return {
        "status": status,
        "python_stale": python_stale,
        "rust_stale": rust_stale,
        "content_stale": content_stale,
        "content_drift_count": content_drift_count,
        "content_indeterminate": content_indeterminate,
        "missing_verbs": missing_verbs,
        "missing_config_keys": missing_config_keys,
        "source_rev": source_rev,
        "installed_rev": marker,
        "rust_binary": rust_binary,
        "rust_installed_rev": rust_installed_rev,
        "rust_source_rev": rust_source_rev,
    }


def _emit_human(
    result: dict[str, Any],
    src: Optional[Path],
    rust: dict[str, Optional[str]],
    *,
    err: bool,
    cargo_present: bool = False,
) -> None:
    out = (lambda m: typer.echo(m, err=True)) if err else typer.echo
    status = result["status"]
    if status == "fresh":
        # Naming WHICH source is load-bearing. The comparison is against
        # `git rev-parse HEAD` of the RESOLVED source checkout, which is the
        # canonical clone sitting on the default branch. Unmerged work in a
        # feature worktree is invisible to it, so this verdict answers "am I
        # behind the default branch" and never "does this binary carry the
        # change I just wrote". Anyone editing fno itself reads the first as the
        # second and then verifies their branch against a binary without it.
        # The command named here must actually load the other checkout. `fno` is
        # the Rust front door and forwards every non-mux verb to the ABSOLUTE
        # installed fno-py (cli/pyproject.toml), so cd-ing into a worktree and
        # typing `fno` runs the installed build again and the branch stays
        # untested. Measured: one scoped call reported 196 inspected lines via
        # `fno` inside the checkout against 0 via uv run. Advice that does not
        # work is the same defect as a receipt that lies.
        out(
            "fno doctor: installed fno is up to date with source at "
            f"{src or 'the resolved source checkout'} "
            f"(rev {result.get('source_rev') or 'unknown'}). "
            "Unmerged work in another branch or worktree is not included, and "
            "`fno` forwards to this installed build from anywhere. To exercise "
            "another checkout run: cd <checkout>/cli && uv run fno-py <verb>"
        )
    elif status == "stale":
        # A missing-verb verdict can be stale with no resolved source (src is
        # None), so fall back to a readable label rather than printing "behind None".
        src_label = src or "source"
        if result["missing_verbs"]:
            out(
                f"fno doctor: installed fno is behind {src_label} "
                f"(missing: {', '.join(result['missing_verbs'])}). "
                "Run `fno update` (or `fno doctor --fix`)."
            )
        elif result.get("content_stale"):
            # Authoritative signal: installed bytes differ from source. Named
            # first because it catches the lying-marker case the rev check below
            # would otherwise report as fresh (a cache-hit reinstall).
            n = result.get("content_drift_count")
            out(
                f"fno doctor: installed fno is STALE - {n} .py file(s) on disk differ "
                f"from {src_label} (a cache-hit reinstall can leave old bytes while the "
                "installed-rev marker still reads HEAD). Run `fno update` (or `fno doctor --fix`)."
            )
        elif result.get("missing_config_keys"):
            # Config-schema drift is the more actionable signal (it names a
            # missing key), so it leads - but a deployed-behind install is usually
            # ALSO rev-behind, so append the rev delta when known rather than drop
            # the diagnostic the plain python_stale branch would have shown.
            keys = result["missing_config_keys"]
            msg = (
                f"fno doctor: Python config schema is STALE (deployed is missing "
                f"{len(keys)} config key(s), e.g. {keys[0]})."
            )
            inst, srcrev = result["installed_rev"], result["source_rev"]
            if inst is not None and srcrev is not None and inst != srcrev:
                msg += f" Installed rev {inst} != source {srcrev}."
            out(msg + " Run `fno update` (or `fno doctor --fix`).")
        elif result["python_stale"]:
            out(
                f"fno doctor: installed fno is behind {src_label} "
                f"(installed rev {result['installed_rev']} != source {result['source_rev']}). "
                "Run `fno update` (or `fno doctor --fix`)."
            )
        else:
            # Rust-only stale. Branch structurally guarantees non-None (rust_stale
            # requires both rust_installed_rev and rust_source_rev to be set).
            ri = result["rust_installed_rev"]
            rs = result["rust_source_rev"]
            out(
                f"fno doctor: rust bins STALE "
                f"(installed {ri[:12]} != source {rs[:12]}). "
                "Run fno update (or fno doctor --fix)."
            )
    elif (
        result.get("content_indeterminate")
        and result.get("installed_rev") is not None
        and result.get("installed_rev") == result.get("source_rev")
    ):
        out(
            "fno doctor: status unknown - the installed-rev marker matches HEAD, but the "
            "content check could not read the installed/source .py bytes to confirm it (the "
            "marker alone can lie about a cache-hit reinstall). Check file permissions, or "
            "run `fno update` to be safe."
        )
    elif src is None:
        out("fno doctor: status unknown (no source checkout to compare against).")
    else:
        out(
            "fno doctor: status unknown "
            "(installed rev undeterminable; capability probe found no missing verbs)."
        )

    rust_bin_path = rust["binary"]

    # Key the rust binary line on the VERDICT, not on raw marker comparison, so a
    # non-cargo binary (bundled wheel/PATH) with a leftover marker mismatch never
    # prints STALE when the JSON verdict has rust_stale: false.
    if rust_bin_path is None and not cargo_present:
        out("fno doctor: rust fno-agents binary: not found (cargo leg not applicable).")
    elif result["rust_stale"]:
        # rust_stale: True -> proven stale (full evidence, cargo bin present, mismatch)
        ri = result.get("rust_installed_rev")
        rs = result.get("rust_source_rev")
        bin_label = rust_bin_path or "(cargo-installed)"
        out(
            f"fno doctor: rust fno-agents binary: {bin_label} "
            f"rust bins STALE (installed {(ri or '')[:12]} != source {(rs or '')[:12]}). "
            "Run fno update (or fno doctor --fix)."
        )
    elif not cargo_present:
        # Binary resolved but not cargo-installed: the verdict still gates on a
        # cargo-installed binary, so a bundled/PATH binary is left untracked.
        out(
            f"fno doctor: rust fno-agents binary: {rust_bin_path} "
            "revision not tracked (no cargo-installed binary; "
            "the rust staleness check applies to the cargo-installed binary only)."
        )
    else:
        # cargo_present is True and rust_stale is False
        ri = result.get("rust_installed_rev")
        rs = result.get("rust_source_rev")
        bin_label = rust_bin_path or "(cargo-installed)"
        if ri is not None and rs is not None and ri == rs:
            out(f"fno doctor: rust fno-agents binary: {bin_label} rust bins fresh (rev {ri[:12]}).")
        elif ri is None:
            out(
                f"fno doctor: rust fno-agents binary: {bin_label} "
                "rust revision unknown (binary does not self-report a crates/ rev; "
                "rebuild via fno update)."
            )
        else:
            out(f"fno doctor: rust fno-agents binary: {bin_label} rust revision unknown.")

    # Build provenance ONLY: the HEAD (git_rev) the binary was built at
    # (ab-24a59d50). This is a DIFFERENT quantity from the crates/ subtree rev the
    # freshness verdict compares, so it must never be framed as a source mismatch:
    # a python-only commit advancing HEAD past the last crates/ change would
    # otherwise print a bogus "(source crates/ rev ...)" line beside a "rust bins
    # fresh" verdict - the exact self-contradiction from the stale-deploy incident.
    # Freshness is decided solely by crates_rev vs crates_rev above.
    binary_rev = rust.get("binary_rev")
    if binary_rev is not None:
        out(
            f"fno doctor: rust fno-agents binary built at HEAD {binary_rev[:12]} "
            "(build provenance)."
        )

    # Mux front-door health (x-c267): does bare `fno` launch the mux? Advisory.
    fd_state = result.get("mux_front_door")
    if fd_state == "active":
        out(f"fno doctor: mux front door: `fno` -> {result.get('mux_binary')} (active).")
    elif fd_state == "not-installed":
        out(
            "fno doctor: mux front door: crates/fno not cargo-installed; bare `fno` will "
            "not launch the mux. Run `fno update` (or cargo install --path crates/fno)."
        )
    elif fd_state == "shadowed":
        where = result.get("path_fno") or "nothing on PATH"
        out(
            f"fno doctor: mux front door: installed at {result.get('mux_binary')} but `fno` "
            f"on PATH resolves to {where}; the mux is shadowed. Ensure the cargo bin dir "
            "precedes any Python `fno` on PATH."
        )

    # Running-process freshness (x-e6dd): a mux server that predates the installed
    # binary is still speaking the old proto - it survives an upgrade by design and
    # silently blocks agent dispatch until restarted. Advisory only.
    for sess in result.get("mux_server_stale") or []:
        out(
            f"fno doctor: mux server '{sess}' is running an older build than the installed "
            "`fno`; run `fno restart --mux` to cut it over (ends live sessions)."
        )

    # Orphan files from deleted capture/migration paths (Group 3 GC). Advisory.
    orphans = result.get("orphan_files") or []
    if orphans:
        out(
            f"fno doctor: found {len(orphans)} orphaned file(s) from removed "
            f"capture paths (safe to delete): {', '.join(orphans)}"
        )

    # PR-watch liveness (x-e106). Advisory: only speak up when the enabled
    # watcher is not actually running, or is freshly installed and pending.
    pw = result.get("pr_watch") or {}
    pw_verdict = pw.get("verdict")
    if pw_verdict == "dead":
        fix = pw.get("fix") or "fno pr-watch install"
        out(
            f"fno doctor: pr-watch enabled but not running ({pw.get('detail')}); "
            f"run `{fix}`, then verify with `fno pr-watch status`."
        )
    elif pw_verdict == "healthy-pending":
        out(f"fno doctor: pr-watch installed, awaiting first tick ({pw.get('detail')}).")

    # Open-file soft limit (advisory): name BOTH numbers and which launch
    # context each belongs to, or the warning repeats the trap it diagnoses.
    fd = result.get("fd_limit") or {}
    if fd.get("verdict") == "low":
        floor = fd.get("threshold")
        soft = fd.get("soft")
        launchd = fd.get("launchd_soft")
        if launchd is None:
            # No launchd probe ran (non-darwin): generic raise advice only,
            # never launchctl/LaunchDaemon lines on a platform without them.
            out(
                f"fno doctor: open-file soft limit is {soft} in THIS process "
                f"(floor {floor})."
            )
            out(
                "Raise it in the launch context that starts this process "
                "(shell profile, service unit, or container limits)."
            )
        else:
            if isinstance(soft, int) and isinstance(floor, int) and soft <= floor:
                out(
                    f"fno doctor: open-file soft limit is {soft} in THIS process "
                    f"(floor {floor})."
                )
            else:
                out(
                    f"fno doctor: open-file soft limit is {soft} in THIS process, but "
                    f"launchd's session default is {launchd} (floor {floor})."
                )
            kern = fd.get("kern_maxfiles")
            kern_note = (
                f"kern.maxfiles is {kern}, so the kernel is not the constraint"
                if isinstance(kern, int)
                else "the kernel's own ceiling is far higher than either number"
            )
            out(
                "The limit is inherited from the launch context, not the machine: "
                f"{kern_note}, and a login shell can read a healthy number while "
                "every spawned worker starves."
            )
            out("Raise it for launchd children: sudo launchctl limit maxfiles 65536 unlimited")
            out(
                "The raise only reaches NEWLY launched processes, so restart the "
                "affected sessions afterwards. Persistence across reboot needs a "
                "LaunchDaemon plist."
            )

    dl = result.get("dead_letter") or {}
    if dl.get("drain_hook_wired") is False:
        out(
            "fno doctor: a2a drain-self SessionStart hook is NOT wired; durable "
            "mail to this claude env will strand (senders see 'queued' forever). "
            "Wire inject-mail-drain-session-start.sh into hooks.json SessionStart."
        )
    stale = dl.get("stale_unread") or []
    if stale:
        handles = ", ".join(sorted({s["handle"] for s in stale}))
        out(
            f"fno doctor: {len(stale)} unread a2a message(s) stranded on the bus "
            f"for a dead handle ({handles}); no live session will ever drain them."
        )

    mb = result.get("managed_block") or {}
    if mb.get("stale"):
        out(
            f"fno doctor: {mb['file']} footnote block is v{mb['stamped']} "
            f"(current v{mb['current']}); re-run `fno setup` to refresh it."
        )

    surf = result.get("harness_surface") or {}
    oc = surf.get("opencode")
    if oc == "stale":
        out(
            "fno doctor: opencode footnote plugin is STALE (drifted from the "
            "shipped source); re-run `fno setup` to refresh it."
        )
    elif oc == "missing":
        out(
            "fno doctor: opencode is set up but its footnote plugin is missing; "
            "re-run `fno setup` to install it."
        )
    _emit_codex_context_window(result, out=out)
    dupes = surf.get("codex_marketplace_duplicates") or []
    if dupes:
        out(
            f"fno doctor: codex has footnote registered {len(dupes)} times as a "
            f"marketplace source ({', '.join(dupes)}); remove the extras with "
            "`codex plugin marketplace remove <name>`."
        )
    plugin = surf.get("codex_plugin") or {}
    plugin_status = plugin.get("status")
    if plugin_status == "fresh":
        out(
            "fno doctor: codex plugin: fresh "
            f"(channel={plugin.get('channel')} version={plugin.get('cache_version')} "
            f"digest={str(plugin.get('cache_digest') or '')[:12]})."
        )
    elif plugin_status:
        detail = plugin.get("issue") or plugin_status
        enabled = ", ".join(plugin.get("enabled_plugin_ids") or []) or "none"
        out(
            f"fno doctor: codex plugin: {str(plugin_status).upper()} "
            f"({detail}; enabled={enabled}); run `{plugin.get('remedy')}`."
        )
    # Agent health (x-1c7b). Grooming freshness is advisory - a fresh install has
    # legitimately never groomed - but a nonzero-exit agent reddens the exit code
    # below, because "installed" has repeatedly not meant "running".
    gr = result.get("groom") or {}
    if gr.get("state") == "never":
        if gr.get("agent_installed"):
            remedy = " despite an installed agent; check ~/.fno/groom.err.log."
        elif sys.platform == "darwin":
            remedy = "; run `fno backlog groom --install-agent` to schedule it daily."
        else:
            # --install-agent is launchd-only and would report `unsupported`.
            remedy = "; schedule `fno backlog groom` daily (see docs/backlog-usage.md)."
        out("fno doctor: backlog grooming has NEVER run" + remedy)
    elif gr.get("stale"):
        out(
            f"fno doctor: backlog grooming last ran {gr['hours']:.0f}h ago "
            "(the daily pass is not running); check `launchctl list | grep sh.fno.groom` "
            "and ~/.fno/groom.err.log."
        )

    ids = result.get("archive_id_collisions") or {}
    if ids.get("unreadable"):
        out(
            "fno doctor: graph-archive.json is corrupt; node id collisions "
            "could not be checked. Restore it from the .bak read_graph left, "
            "or rebuild it, then re-run doctor."
        )
    if ids.get("count"):
        shown = ", ".join(ids["ids"][:10])
        more = f" (+{ids['count'] - 10} more)" if ids["count"] > 10 else ""
        out(
            f"fno doctor: {ids['count']} node id(s) collide between the working "
            f"graph and the archive: {shown}{more}; run "
            "`fno backlog archive-dedupe-ids --apply` to remint the archived side."
        )

    # Canonical-sync freshness. Advisory like grooming: the alarm
    # exists because process-liveness reads green through this exact failure.
    pms = result.get("post_merge_sync") or {}
    if pms.get("stale"):
        out(
            f"fno doctor: post-merge sync STALE - the canonical checkout is not "
            f"synced with recent merges ({pms.get('detail')}); run "
            "`fno pr sync-canonical --pr-number <n>` and check "
            "~/.fno/pr-watcher.err.log."
        )
    elif pms.get("state") == "unknown":
        # "Could not tell" must not read as "fine". An unauthenticated gh was
        # part of the outage this check exists to catch.
        out(
            "fno doctor: post-merge sync UNKNOWN - could not read merge state "
            f"({pms.get('detail') or 'gh unavailable or unauthenticated'}); "
            "run `gh auth status`."
        )

    agents = result.get("launch_agents") or {}
    if not agents.get("applicable"):
        # No launchctl (Linux) or an unrunnable probe. Say so rather than let a
        # silent scan read as a clean bill of health.
        out("fno doctor: LaunchAgent health: not applicable (no launchctl on this host).")
    for entry in agents.get("dead") or []:
        out(
            f"fno doctor: LaunchAgent {entry['label']} last exited {entry['exit']} "
            "(it is installed but failing); check its log under ~/.fno/ and re-run "
            "`fno update` if the entry point moved."
        )

    # Silent-switch legibility (x-8cd5 Wave 6): the applied posture, then both
    # directions of the rule. Advisory; never changes the exit code.
    ss = result.get("silent_switches") or {}
    stamp = ss.get("posture") or {}
    if stamp.get("posture"):
        out(
            f"fno doctor: applied posture: {stamp.get('posture')} "
            f"(applied {stamp.get('applied_at', '?')}, scope {stamp.get('scope', '?')})."
        )
    for f in ss.get("findings") or []:
        sw = f.get("switch", "")
        cmd = f.get("command", "")
        count = f.get("count")
        label = f.get("count_label", "")
        if f.get("direction") == "inaction":
            clause = f" but {count} {label} waiting" if count else " (idle)"
            out(
                f"fno doctor: {sw} is OFF{clause}; nothing is happening. "
                f"Run `{cmd}` to enable."
            )
        else:  # irreversible
            clause = f" ({count} {label})" if count else ""
            out(
                f"fno doctor: {sw} is ARMED{clause}; a green PR can merge "
                f"unattended. Run `{cmd}` to disarm."
            )
        if f.get("cause"):
            out(f"  cause: {f['cause']}")

    # Deployed claude plugin cache (x-4be1): the hooks actually executed by
    # Claude sessions. Advisory, same vocabulary as the wheel/rust legs.
    pc = result.get("plugin_cache") or {}
    if pc.get("status") == "stale":
        sha = str(pc.get("sha") or "")[:12]
        when = str(pc.get("installed_at") or "")[:10] or "?"
        out(
            f"fno doctor: deployed claude plugin cache STALE (pinned {sha}, "
            f"{when}; hooks run pre-HEAD bytes). Run `fno update`."
        )
    elif pc.get("status") == "fresh":
        out("fno doctor: deployed claude plugin cache: fresh (pinned at source HEAD).")

    if surf.get("codex_hooks_dual"):
        out(
            "fno doctor: codex hooks load from both config.toml and hooks.json; "
            "run `fno setup cli-hooks-codex --migrate-legacy-hooks-json` to "
            "converge (or `fno doctor --codex-hooks` for detail)."
        )

    # Advisory legacy pre-push hook (destination-ref gate): names the remedy
    # verb. Only the legacy install prints; absent/foreign/unchecked are
    # silent. Never changes status/exit.
    if (result.get("pre_push_hook") or {}).get("status") == "legacy":
        out(
            "fno doctor: the installed pre-push hook gates on the pushing "
            "checkout's branch, so it refuses every push from a canonical "
            "checkout on main. Run `bash scripts/install-pre-push-hook.sh` "
            "to replace it (the old file is backed up)."
        )

    # Plugin hook launch probe (x-d991): a hook command that cannot start fails
    # open in Codex with zero signal; each row is one guard that was absent.
    ph: dict[str, Any] = result.get("plugin_hooks") or {}
    if ph.get("applicable"):
        for merr in ph.get("manifest_errors") or []:
            out(
                f"fno doctor: hook manifest {merr.get('issue')}: {merr.get('path')} "
                f"[{merr.get('manifest')}] - that harness loads no hooks (fail-open)."
            )
        for miss in ph.get("missing_scripts") or []:
            out(
                f"fno doctor: hook references MISSING script {miss.get('script')} "
                f"[{miss.get('manifest')}/{miss.get('event')}] - the guard fails "
                "open with no signal."
            )
        for failure in ph.get("failures") or []:
            stderr = failure.get("stderr") or ""
            tail = f" - {stderr[:160]}" if stderr else ""
            out(
                f"fno doctor: hook LAUNCH FAILED (rc={failure.get('rc')}): "
                f"{failure.get('resolved')} [{failure.get('manifest')}/{failure.get('event')}]{tail}"
            )
    elif ph.get("reason"):
        out(f"fno doctor: plugin hook probe skipped ({ph['reason']}).")

    # Codex hook layer split (x-d991): a foreign hook in ~/.codex/hooks.json is
    # not footnote's to remove, but the split itself is the diagnostic gap.
    foreign = surf.get("codex_hooks_foreign_json") or []
    if foreign:
        out(
            f"fno doctor: codex hooks: {len(foreign)} foreign hook(s) in "
            "~/.codex/hooks.json (informational; run `fno doctor --codex-hooks` "
            "for the per-command config.toml/hooks.json breakdown)."
        )

    cas = result.get("codex_app_server") or {}
    if not cas.get("present"):
        out(
            "fno doctor: codex app-server daemon not running (no control socket); "
            "live mail to codex sessions demotes to durable. Start it BEFORE the "
            "codex TUI: `codex app-server daemon start` (or "
            "`codex app-server daemon bootstrap` for durable SSH-driven use). A "
            "session launched before the daemon cannot receive live mail without a restart."
        )


# ---------------------------------------------------------------------------
# Command
# ---------------------------------------------------------------------------


def _codex_context_window_report(app_server_present: bool | None = None) -> dict[str, Any]:
    """What context window a codex thread on the configured model actually gets.

    Codex resolves the window as ``min(config.model_context_window,
    max_context_window) * effective_context_window_percent / 100``, where both
    right-hand values come from ``$CODEX_HOME/models_cache.json``.  The TUI
    footer echoes the CONFIGURED number, so a config asking for 1M reads as 1M
    while every turn runs smaller - the gap this reports.

    Scope is the model config.toml selects.  ``-m`` and a profile both override
    it per thread, so this describes the default, never "every thread".

    The cached ``max_context_window`` is served per fetching client - both its
    surface and its originator, since ``codex exec`` and ``codex app-server``
    are served differently under one originator - while the cache records
    neither and every launcher shares one copy.  So the last fetch sets the cap
    for every thread started since.  That cap is per MODEL, not per file: one
    fetch here holds gpt-5.6-sol at 272000 beside gpt-5.4 at 1000000, so no
    single file-wide tier label is truthful.  Read-only.

    A silent path returns a ``reason`` rather than ``{}`` so ``--json`` says
    which of "nothing to report" and "could not tell" happened."""
    import tomllib

    codex_home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser()
    try:
        with (codex_home / "config.toml").open("rb") as handle:
            config = tomllib.load(handle)
        cache = json.loads((codex_home / "models_cache.json").read_text())
    except Exception as exc:  # noqa: BLE001 - doctor stays advisory on an unreadable home
        return {"reason": f"unreadable-codex-home: {str(exc)[-200:]}"}

    # A `profile` key redirects model selection wholesale, so reading the
    # top-level `model` past one describes a model no thread runs.  A profile
    # sets an arbitrary subset though: name the profile key only once that
    # table actually carried `model`, else the provenance itself is a lie.
    # The window has the same exposure as the slug: a profile that supplies it
    # leaves the reader grepping config.toml for a number that is not there.
    source = "model"
    window_source = "model_context_window"
    profile = config.get("profile")
    if isinstance(profile, str):
        # A malformed `profiles` (a scalar, or a scalar entry) must return a
        # reason, not raise: the caller swallows exceptions, which drops the
        # key from --json and loses the "could not tell" the reason exists for.
        profiles = config.get("profiles")
        table = profiles.get(profile) if isinstance(profiles, dict) else None
        if not isinstance(table, dict):
            table = {}
        if "model" in table:
            source = f"profiles.{profile}.model"
        if "model_context_window" in table:
            window_source = f"profiles.{profile}.model_context_window"
        config = {**config, **table}

    configured = config.get("model_context_window")
    slug = config.get("model")
    # TOML allows a bool here, and `isinstance(True, int)` is True, so an
    # unguarded check reports `model_context_window=True` as a real window.
    if not isinstance(configured, int) or isinstance(configured, bool):
        # Deliberately silent.  `overstated` measures a promise the USER wrote
        # against what runs; with nothing configured there is no such promise,
        # and what an unconfigured footer displays is unverified here.  Do not
        # "fix" this into a percent-shrink nag for every codex install.
        return {"reason": "no-configured-window"}
    if not isinstance(slug, str):
        return {"reason": "no-configured-model"}
    # Same hardening as the profiles branch above: a malformed shape must
    # return a reason, since the caller swallows a raise and drops the key.
    models = cache.get("models") if isinstance(cache, dict) else None
    if not isinstance(models, list):
        return {"reason": "cache-schema-drift: models"}
    entry = next(
        (m for m in models if isinstance(m, dict) and m.get("slug") == slug), None
    )
    if entry is None:
        return {"reason": f"model-not-in-cache: {slug}"}
    # Accept a float everywhere, not just for the percent.  Rejecting a float
    # cap silently substitutes the base and reports a clamp that is not real,
    # which is a false positive in the exact direction this check exists for.
    cap = entry.get("max_context_window")
    base = entry.get("context_window")
    percent = entry.get("effective_context_window_percent")
    def _number(
        value: object, *, lo: float, hi: float, keep_float: bool = False
    ) -> Optional[float]:
        """The ONE gate every cached number goes through, RANGE INCLUDED.

        Per-field guards were the recurring defect here: bool, then non-finite,
        then out-of-range each got closed on one field and left open on its
        neighbour, and every miss either raised into a caller that swallows it
        or printed an impossible window as fact.  The bounds are arguments, so
        adding a field cannot forget them.  The int branch returns before any
        float coercion, because `math.isfinite` coerces and an oversized JSON
        integer raises `OverflowError` there."""
        if isinstance(value, bool):
            return None
        if isinstance(value, int):
            number: float = value
        elif isinstance(value, float) and math.isfinite(value):
            number = value if keep_float else int(value)
        else:
            return None
        return number if lo <= number <= hi else None

    # No real window is zero, negative, or astronomical: the largest codex
    # documents is about 1.05M, so a gigatoken ceiling rejects a corrupt cache
    # without ever rejecting a served one.
    WINDOW_CEILING = 1_000_000_000
    cap = _number(cap, lo=1, hi=WINDOW_CEILING)
    base = _number(base, lo=1, hi=WINDOW_CEILING)
    # No cap means no computable window.  Substituting the base produced a
    # number the cache never carried, and the line then had to disclaim the
    # very cap it quoted.  Say the cache cannot answer instead.
    if cap is None:
        return {"reason": f"cache-schema-drift: no max_context_window for {slug}"}
    base_synthesized = base is None
    # Narrowed on `base` itself, not through the flag: mypy cannot follow the
    # indirection, and the resulting `int > None` error is a real one.
    if base is None:
        base = cap
    # A missing percent must not silence a clamp that needs no percent to
    # compute.  Carry it as None and let the line omit the claim, rather than
    # defaulting to a number upstream did not send.  A float is legitimate
    # here, since 95.0 is served.  `lo=1` for the same reason the window
    # fields use it: a zero percent is an impossible window, and rejecting it
    # falls through to the honest 100% assumption.
    percent = _number(percent, lo=1, hi=100, keep_float=True)

    effective = int(min(configured, cap) * (100 if percent is None else percent) // 100)
    return {
        "model": slug,
        "model_source": source,
        "window_source": window_source,
        "configured": configured,
        "max_context_window": cap,
        "base_synthesized": base_synthesized,
        # Omitted, not filled in, for the same reason the cap is: a consumer
        # reading it alone to price a base-tier fetch would see a loss of zero.
        **({} if base_synthesized else {"context_window": base}),
        "effective": effective,
        "percent": percent,
        # The footer lies whenever the effective window is short of the
        # configured one, and the percent shrink alone is enough to do it.
        "overstated": effective < configured,
        # The cached cap is load-bearing both when it clamps and when a RAISED
        # cap is the only reason it does not.  gpt-5.4 sits at base 272000 and
        # cap 1000000: a configured 400000 escapes the clamp solely because the
        # last fetch won the raised cap, and reverts to 258400 without it.
        # With no base in the entry, the cap alone carries the clamp, so
        # measure against whichever of the two the cache actually gave.  Both
        # halves are needed: a cap BELOW the base clamps while `configured >
        # base` stays False, and the operator then loses tokens to a stale cap
        # with nothing in the line to say so.
        "leans_on_cached_cap": configured > base or cap < configured,
        # Every app-server fetch lands the base cap, whatever clientInfo name
        # it presents, so a live daemon keeps pulling a raised cap back down.
        "app_server_running": (
            bool(_codex_app_server_report().get("present"))
            if app_server_present is None
            else bool(app_server_present)
        ),
        "cache_fetched_at": cache.get("fetched_at"),
        "cache_client_version": cache.get("client_version"),
    }


def _emit_codex_context_window(result: dict[str, Any], *, out) -> None:
    """Name the real window when the configured one overstates it."""
    report = (result.get("harness_surface") or {}).get("codex_context_window") or {}
    if not (report.get("overstated") or report.get("leans_on_cached_cap")):
        return
    # `:g` because a float percent is legitimate: 95.0 must read as 95.
    kept = "" if report.get("percent") is None else f" (codex keeps {report['percent']:g}%)"
    line = (
        f"fno doctor: codex {report['model_source']}={report['model']} with "
        f"{report['window_source']}={report['configured']} runs at an effective "
        f"{report['effective']}{kept}."
    )
    # Only claim the footer lies once it actually does.  A raised cap can be
    # load-bearing while the effective window still equals the configured one.
    if report.get("overstated"):
        line += " The TUI footer shows the configured value, not this one."
    # The cached cap is load-bearing whenever the configured value clears the
    # model's base, whether the cap clamped it or a raised cap spared it.
    if report.get("leans_on_cached_cap"):
        cap = report["max_context_window"]
        # Absent when the cache carried no base; the cap stands in for the
        # arithmetic, and `base_synthesized` keeps it out of the prose.
        base = report.get("context_window", cap)
        # Only claim a fetch time and a client when the cache recorded them:
        # an older or hand-written cache carries neither, and the clause was
        # printing "(fetched None by codex None)" as if it did.
        stamp = report.get("cache_fetched_at")
        client = report.get("cache_client_version")
        if stamp and client:
            fetched = f"(fetched {stamp} by codex {client})"
        elif stamp:
            fetched = f"(fetched {stamp})"
        elif client:
            fetched = f"(written by codex {client})"
        else:
            fetched = "(the cache records no fetch time)"
        provenance = (
            f"{fetched}. That cap is served per fetching client, "
            "surface and originator both, and the cache records neither"
        )
        if cap > base:
            # `percent` is None on cache schema drift, so apply the same 100%
            # assumption `effective` already uses instead of multiplying by it.
            pct = report.get("percent")
            base_tier = base if pct is None else int(base * pct // 100)
            line += (
                f" models_cache.json holds {report['model']} at {cap}, above its {base} "
                f"base {provenance}. A base-tier fetch drops this to {base_tier}."
            )
        elif cap == base and not report.get("base_synthesized"):
            line += (
                f" models_cache.json caps {report['model']} at {cap}, its base "
                f"{provenance}, so whichever launcher fetched last set it for every "
                "thread started since."
            )
        else:
            # A cap BELOW base is upstream nonsense, and a missing base leaves
            # nothing to call it.  State the number, claim nothing about it.
            line += (
                f" models_cache.json puts {report['model']} at {cap} {provenance}, "
                "so whichever launcher fetched last set it for every thread "
                "started since."
            )
        # Belongs on every branch, and most of all on the RAISED-cap one: the
        # daemon is the thing that pulls a raised cap back down, so the run
        # about to lose the difference is the one that needs telling.
        if report.get("app_server_running"):
            # Deliberately no remedy.  The only lever that raises the cap is
            # stopping this daemon, and `_codex_app_server_report` right above
            # requires the same socket for live mail to a codex peer.  Naming a
            # fix here means telling the operator to break that.
            line += (
                " A codex app-server daemon is live here and every app-server "
                "fetch lands the base cap, so it will keep pulling this back down."
            )
    out(line)


def _codex_hooks_report() -> dict[str, Any]:
    """Inspect Codex's user-level hook layers without running doctor collectors."""
    from fno.setup.cli_hooks import inspect_codex_hooks

    codex_home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser()
    config_path = codex_home / "config.toml"
    hooks_json_path = codex_home / "hooks.json"
    diagnostics = inspect_codex_hooks(
        config_path=config_path,
        hooks_json_path=hooks_json_path,
    )
    toml_wired = bool(diagnostics.toml_footnote_commands)
    toml_verified = diagnostics.all_toml_footnote_hooks_verified
    toml_trust = dict(
        zip(
            diagnostics.toml_footnote_state_keys,
            (
                "recorded-unverified" if recorded else "missing"
                for recorded in diagnostics.toml_footnote_state_recorded
            ),
            strict=True,
        )
    )
    duplicate_layers = diagnostics.has_toml_hooks and diagnostics.has_json_hooks

    if diagnostics.errors:
        status = "error"
    elif not toml_wired or not toml_verified or diagnostics.has_json_hooks:
        status = "warn"
    else:
        status = "ok"

    return {
        "status": status,
        "preferred_layer": "config.toml",
        "state": diagnostics.state,
        "config_path": str(config_path),
        "hooks_json_path": str(hooks_json_path),
        "footnote_toml_wired": toml_wired,
        "footnote_toml_trust_verified": toml_verified,
        "footnote_toml_trust": toml_trust,
        "duplicate_layers": duplicate_layers,
        "footnote_json_hooks": list(diagnostics.json_footnote_commands),
        "foreign_json_hooks": list(diagnostics.json_foreign_commands),
        "errors": list(diagnostics.errors),
    }


def _codex_app_server_report() -> dict[str, Any]:
    """Whether the codex app-server control socket exists.

    The socket at ``$CODEX_HOME/app-server-control/app-server-control.sock``
    exists only while a codex app-server daemon runs (``codex app-server daemon
    start``). Absent it, live mail to a codex session demotes to durable, so a
    plain ``fno doctor`` names the fix for hand-started sessions no spawn
    preflight can reach."""
    codex_home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser()
    socket_path = codex_home / "app-server-control" / "app-server-control.sock"
    return {"present": socket_path.exists(), "socket_path": str(socket_path)}


def _codex_version() -> Optional[str]:
    """The installed codex CLI's own version string, or None on any miss.

    Delegates to the memoized :func:`fno.agents.mux_spawn._codex_cli_version`
    rather than shelling out a second time: two independent `codex --version`
    probes with different timeouts and parsing could otherwise silently
    disagree on what "the installed codex" is.
    """
    from fno.agents.mux_spawn import _codex_cli_version

    version = _codex_cli_version()
    if version is None:
        return None
    return "codex-cli %d.%d.%d" % version


#: The production binding window (mirrors the codex harness contract's
#: 60000ms timeout_ms). A module constant, not an inline literal, so a test
#: can shrink it rather than spend real wall-clock time on a negative probe.
_CODEX_BIND_CANARY_WINDOW_S = 60.0


def _codex_bind_report() -> dict[str, Any]:
    """Spawn one throwaway codex pane and time which oracle binds it.

    The lane canary beside ``--codex-hooks``: the pane-binding defect this
    canary exists to catch was an upstream codex upgrade silently breaking
    the fallback lane, invisible until a rate-limited primary made it
    load-bearing. This reports a POSITIVE marker - a returned session id and
    which oracle produced it - never the absence of an error line, so a
    future regression reads as a version change on a red canary rather than
    a mystery some weeks later.

    Drives the exact production binding sequence (``_await_pane_binding`` +
    ``_make_codex_bind_probe``) rather than a hand-rolled poll loop, so a
    future regression in that sequence's probe order, deadline handling, or
    pane-death detection shows up here too instead of passing silently.
    """
    import time
    import uuid as _uuid

    from fno.agents.mux_spawn import (
        _await_pane_binding,
        _codex_session_ids_loaded,
        _lookup_child_pid,
        _make_codex_bind_probe,
        _reap_spawned_pane,
        _run_mux,
        build_pane_argv,
        resolve_mux_session,
    )

    version = _codex_version()
    cwd = Path.cwd()
    session = resolve_mux_session()
    name = f"codex-bind-canary-{_uuid.uuid4().hex[:8]}"
    argv = build_pane_argv("codex", "", cwd, True, None, name=name)
    # None (daemon unreachable at this instant) is passed through as-is; the
    # probe below refuses to correlate against a fabricated empty baseline.
    baseline_ids = _codex_session_ids_loaded(cwd)
    spawn_started_ms = int(time.time() * 1000)
    proc = _run_mux(
        ["mux", "pane", "run", "--session", session, "--cwd", str(cwd), "--", *argv],
        subprocess.run,
    )
    if proc.returncode != 0:
        return {
            "bound": False,
            "oracle": None,
            "elapsed_s": 0.0,
            "codex_version": version,
            "error": (proc.stderr or proc.stdout or "no output").strip(),
        }
    try:
        pane_id = int((proc.stdout or "").strip().splitlines()[-1])
    except (ValueError, IndexError):
        # No pane id to reap by (same unrecoverable shape mux_spawn's own
        # dispatch raises on): the canary pane may exist without a way to
        # name it, so point at the manual cleanup path instead of a silent
        # leak.
        return {
            "bound": False,
            "oracle": None,
            "elapsed_s": 0.0,
            "codex_version": version,
            "error": (
                f"unparseable pane run output {proc.stdout!r}; a pane may "
                f"exist without a registry row - inspect with 'fno mux pane "
                f"ls --session {session}'"
            ),
        }

    mux = {"session": session, "pane_id": pane_id}
    child_pid = _lookup_child_pid(session, pane_id, subprocess.run)
    started = time.monotonic()
    oracle_used: list = []
    if child_pid is not None:
        binding = _await_pane_binding(
            mux,
            _make_codex_bind_probe(
                cwd=cwd,
                spawn_started_ms=spawn_started_ms,
                child_pid=child_pid,
                codex_sessions_dir=None,
                daemon_baseline_ids=baseline_ids,
                mux=mux,
                runner=subprocess.run,
                oracle_used=oracle_used,
            ),
            runner=subprocess.run,
            window_s=_CODEX_BIND_CANARY_WINDOW_S,
            label="codex-bind-canary",
        )
        session_id = binding.session_id
    else:
        session_id = None
    elapsed = time.monotonic() - started
    _reap_spawned_pane(session, pane_id, subprocess.run)
    return {
        "bound": session_id is not None,
        "oracle": oracle_used[0] if oracle_used else None,
        "elapsed_s": round(elapsed, 2),
        "codex_version": version,
        "error": None if session_id else "neither oracle bound within the window",
    }


def _emit_codex_bind_report(result: dict[str, Any]) -> None:
    if result["bound"]:
        typer.echo(
            f"fno doctor: codex bind: ok, oracle={result['oracle']} "
            f"elapsed={result['elapsed_s']}s codex={result['codex_version'] or 'unknown'}"
        )
    else:
        typer.echo(
            f"fno doctor: codex bind: FAILED codex={result['codex_version'] or 'unknown'}: "
            f"{result['error']}"
        )


def _emit_codex_hooks_report(result: dict[str, Any], *, err: bool) -> None:
    """Render one summary plus actionable Codex hook diagnostics."""

    def out(message: str) -> None:
        typer.echo(message, err=err)

    trust_states = set(result["footnote_toml_trust"].values())
    if result["footnote_toml_trust_verified"]:
        trust = "verified"
    elif "recorded-unverified" in trust_states:
        trust = "recorded-unverified"
    elif result["footnote_toml_wired"]:
        trust = "missing"
    else:
        trust = "n/a"
    out(
        f"fno doctor: codex hooks: {result['status']} preferred=config.toml; "
        f"footnote SessionStart={'wired' if result['footnote_toml_wired'] else 'missing'}; "
        f"trust={trust}; layers={result['state']}."
    )

    for error in result["errors"]:
        out(f"fno doctor: codex hooks: parse error: {error}")

    for state_key, state in result["footnote_toml_trust"].items():
        out(f"fno doctor: codex hooks: trust state {state}: {state_key}")
    if result["footnote_toml_wired"]:
        if not result["footnote_toml_trust_verified"]:
            if "recorded-unverified" in trust_states:
                out(
                    "fno doctor: codex hooks: approval record found, but its "
                    "trusted_hash was not locally verified; confirm it in Codex."
                )
            else:
                out(
                    "fno doctor: codex hooks: approve the footnote SessionStart "
                    "hook in Codex."
                )

    if result["duplicate_layers"]:
        out(
            "fno doctor: codex hooks: loading hooks from both "
            f"{result['hooks_json_path']} and {result['config_path']}; "
            "config.toml is preferred."
        )

    if result["footnote_json_hooks"]:
        out(
            "fno doctor: codex hooks: run "
            "`fno setup cli-hooks-codex --migrate-legacy-hooks-json` to remove only "
            "footnote-owned legacy JSON hooks."
        )

    for command in result["foreign_json_hooks"]:
        out(
            "fno doctor: codex hooks: foreign legacy JSON hook preserved: "
            f"{command}; manually consolidate it into {result['config_path']} if desired."
        )

    if not result["footnote_toml_wired"] and not result["errors"]:
        command = "fno setup cli-hooks-codex"
        if result["footnote_json_hooks"]:
            command += " --migrate-legacy-hooks-json"
        out(f"fno doctor: codex hooks: run `{command}` to wire the preferred TOML hook.")


# --------------------------------------------------------------------------
# Dead-letter visibility (US7, x-605c): the durable floor is silent quicksand
# if a recipient's drain is unwired -- senders see `queued (durable)` + exit 0
# forever. Two advisory findings, never blocking: (a) a claude env whose
# `drain-self` SessionStart hook is not wired, (b) unread bus mail past a
# threshold addressed to a handle with no live session.
# --------------------------------------------------------------------------

_DEAD_LETTER_AGE_HOURS = 24.0
# The bare short-id is the only drainable session address. Retired prefixed
# recipients remain in this health-only pattern so pre-flip dead mail surfaces.
def _a2a_handle_re() -> "re.Pattern[str]":
    """A session address: the bare short-id, or a retired ``<harness>-<short8>``.

    The retired form is still MATCHED here on purpose - mail queued to one before
    the flip is undeliverable, so the dead-letter report is the only thing that
    surfaces it. Prefixes come from the harness map so adding a harness cannot
    silently drop it out of the scan.
    """
    from fno.agents.harness_map import known_harnesses

    return re.compile(rf"^(?:(?:{'|'.join(known_harnesses())})-)?[0-9a-fA-F]{{6,}}$")


_A2A_HANDLE_RE = _a2a_handle_re()


def _plugin_hooks_json() -> Optional[Path]:
    """Locate the claude plugin's hooks.json (CLAUDE_PLUGIN_ROOT, else source)."""
    root = os.environ.get("CLAUDE_PLUGIN_ROOT")
    if root:
        p = Path(root) / "hooks" / "hooks.json"
        if p.exists():
            return p
    src = _resolve_source(None)
    if src is not None:
        p = src / "hooks" / "hooks.json"
        if p.exists():
            return p
    return None


def _drain_hook_wired(hooks_json: Optional[Path] = None) -> Optional[bool]:
    """True/False if the claude ``drain-self`` SessionStart hook is/isn't wired;
    ``None`` when the hooks config can't be located (advisory -- don't guess)."""
    path = hooks_json or _plugin_hooks_json()
    if path is None or not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return None
    hooks = data.get("hooks") if isinstance(data, dict) else None
    starts = hooks.get("SessionStart") if isinstance(hooks, dict) else None
    if not isinstance(starts, list):
        starts = []
    cmds: list[str] = []
    for h in starts:
        for c in (h.get("hooks") or []) if isinstance(h, dict) else []:
            if isinstance(c, dict):
                cmds.append(str(c.get("command", "")))
    return any("mail-drain" in c or "drain-self" in c for c in cmds)


def _parse_bus_ts(ts: str) -> Optional[datetime]:
    from datetime import datetime, timezone

    try:
        return datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except (ValueError, TypeError):
        return None


def _stranded(m, *, cutoff, ref) -> bool:
    """Whether an unread envelope has passed its terminal TTL.

    A US6-stamped envelope carries a per-owner ``ttl_at`` (a dead-letter's is its
    birth, so it surfaces immediately; a wake-daemon's is +6h); use it. Legacy
    mail with no ``ttl_at`` falls back to the blanket age cutoff."""
    meta = getattr(m, "meta", None) or {}
    ttl_raw = meta.get("ttl_at")
    if ttl_raw:
        ttl = _parse_bus_ts(str(ttl_raw))
        if ttl is not None:
            return ref >= ttl
    ts = _parse_bus_ts(getattr(m, "ts", ""))
    return ts is not None and ts <= cutoff


def _drained_msg_ids() -> set[str]:
    """msg_ids carrying an ``agent_mail_drained`` receipt (W1.1).

    Read once per sweep so the dead-letter sweep prefers a positive drain marker
    over cursor inference: a message with a marker was drained and never
    escalates, while cursor logic stays as the fallback for legacy mail written
    before the marker existed. A torn or unreadable log reads as empty, so the
    sweep degrades to cursor-only (its prior behavior) rather than crashing or
    silently clearing its findings.
    """
    from fno.paths import state_dir

    path = state_dir() / "events.jsonl"
    ids: set[str] = set()
    try:
        # Stream line-by-line: the events log grows unboundedly, so never slurp
        # it whole just to collect drained ids (mirrors gate_escape.py's reader).
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if "agent_mail_drained" not in line:
                    continue
                try:
                    rec = json.loads(line)
                except (ValueError, TypeError):
                    continue
                if not isinstance(rec, dict):
                    continue
                if rec.get("kind") == "agent_mail_drained":
                    mid = rec.get("msg_id")
                    if isinstance(mid, str) and mid:
                        ids.add(mid)
    except OSError:
        return ids
    return ids


def _stale_dead_letters(
    *,
    max_age_hours: float = _DEAD_LETTER_AGE_HOURS,
    now: Optional[datetime] = None,
) -> list[dict]:
    """Unread bus mail past its terminal TTL, addressed to a session handle.

    Keys on TTL expiry + the per-recipient drain cursor (``scan_unread``), NOT
    roster liveness (AC8-FR): a wedged session listed live whose drain never
    advanced its cursor still escalates once its ``ttl_at`` passes, while a
    session that genuinely drained (cursor advanced) never surfaces. Each finding
    carries the US6 ``owner`` so downstream surfaces can name the terminal class."""
    from datetime import datetime, timedelta, timezone

    from fno.bus.cursor import scan_unread
    from fno.bus.log import iter_messages

    ref = now or datetime.now(tz=timezone.utc)
    cutoff = ref - timedelta(hours=max_age_hours)

    recips: set[str] = set()
    try:
        for m in iter_messages(warn=False):
            to = getattr(m, "to", "") or ""
            # A project broadcast is never a dead session handle, and an all-hex
            # project name matches the bare-handle shape. Only the prefixed form
            # was unambiguous, so widening the pattern makes to_kind load-bearing.
            if getattr(m, "to_kind", "name") == "project":
                continue
            # An owner-stamped durable envelope carries its own terminal class and
            # ttl_at, so sweep its recipient whatever the handle shape - a
            # registered-agent name like `alpha` is not hex but is still a real
            # addressee. The regex stays as the fallback for legacy handle mail
            # that predates the US6 stamp.
            meta = getattr(m, "meta", None) or {}
            if meta.get("owner") or _A2A_HANDLE_RE.match(to):
                recips.add(to)
    except Exception:  # noqa: BLE001 — a torn bus contributes no findings
        return []

    # Positive drain marker (W1.1): a message whose id has an agent_mail_drained
    # receipt was drained (possibly under a different address form, or after a
    # discarded injection) and never escalates. The cursor logic below stays as
    # the fallback for legacy mail written before the marker existed, so the
    # sweep asserts a presence first and an absence only where no better
    # evidence can exist.
    drained = _drained_msg_ids()
    out: list[dict] = []
    for handle in sorted(recips):
        try:
            unread = scan_unread(handle, warn=False)
        except Exception:  # noqa: BLE001
            continue
        for m in unread:
            if getattr(m, "id", "") in drained:
                continue
            if _stranded(m, cutoff=cutoff, ref=ref):
                meta = getattr(m, "meta", None) or {}
                out.append(
                    {
                        "handle": handle,
                        "msg_id": getattr(m, "id", ""),
                        "ts": getattr(m, "ts", ""),
                        "owner": meta.get("owner"),
                    }
                )
    return out


def _dead_letter_report() -> dict:
    """Advisory dead-letter findings for `fno doctor` (US7). Never blocks."""
    return {
        "drain_hook_wired": _drain_hook_wired(),
        "stale_unread": _stale_dead_letters(),
    }


def _managed_block_report() -> dict:
    """Advisory: flag a host AGENTS.md/CLAUDE.md footnote block older than the
    current template (US8). Reports only when a block actually exists - a repo
    that never opted in is silent. Never blocks/exits."""
    try:
        from fno.setup.managed_block import BLOCK_VERSION, stamped_version

        # Walk up to the repo root so `fno doctor` from a subdirectory still finds
        # the host file (the block lives at the repo root, not the cwd).
        root = Path.cwd()
        for parent in [root, *root.parents]:
            if (parent / ".git").exists() or (parent / ".fno").is_dir():
                root = parent
                break
        for name in ("AGENTS.md", "CLAUDE.md"):
            p = root / name
            if not p.is_file():
                continue
            try:
                v = stamped_version(p.read_text(encoding="utf-8"))
            except OSError:
                continue
            if v is None:
                continue
            return {
                "file": name,
                "stamped": v,
                "current": BLOCK_VERSION,
                "stale": v < BLOCK_VERSION,
            }
    except Exception:
        pass
    return {}


# --------------------------------------------------------------------------
# Per-harness surface freshness (x-3248 Change 5): `fno update` refreshes only
# the shared CLI/wheel; the codex marketplace plugin and the opencode local
# plugin are separate surfaces with their own refresh verbs. Report-and-point
# only (no auto-fix): the refresh action differs per harness and stays manual.
# --------------------------------------------------------------------------


def _codex_marketplace_duplicates(list_output: str) -> list[str]:
    """Footnote marketplace source names in `codex plugin marketplace list`
    output, returned ONLY when footnote is registered more than once (the
    duplicate the user sees as duplicate skills). Pure text parser: the caller
    feeds it captured stdout, so tests need no live codex. A single legitimate
    registration returns ``[]``."""
    names: list[str] = []
    for line in list_output.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        name, root = parts[0], parts[1]
        if name.upper() == "MARKETPLACE":  # header row
            continue
        if "footnote" in f"{name} {root}".lower():
            names.append(name)
    return names if len(names) > 1 else []


def _harness_surface_report() -> dict[str, Any]:
    """Advisory per-harness surface findings. Quiet by default: reports only an
    actionable problem (an opencode plugin present-but-stale, or codex footnote
    registered twice). Never blocks/exits; a missing harness is simply silent."""
    report: dict[str, Any] = {}
    try:
        from fno.setup.integration import (
            _opencode_is_installed,
            _opencode_plugin_dest,
            _opencode_plugins_dir,
        )

        # Only when opencode is actually set up (its plugins dir exists), so a
        # non-opencode user is never nagged.
        if _opencode_plugins_dir().exists():
            if not _opencode_plugin_dest().exists():
                report["opencode"] = "missing"
            elif not _opencode_is_installed():
                report["opencode"] = "stale"
    except Exception:
        pass

    codex_home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser()
    plugin_state_exists = (
        (codex_home / "footnote" / "plugin-channel.json").is_file()
        or (codex_home / "footnote" / "rollback-failure.json").is_file()
        or (codex_home / "plugins" / "cache" / "footnote").exists()
        or (codex_home / "plugins" / "cache" / "footnote-dev").exists()
    )
    if shutil.which("codex") is not None or plugin_state_exists:
        try:
            from fno.setup.codex_plugin import inspect_freshness

            report["codex_plugin"] = inspect_freshness()
        except Exception as exc:  # noqa: BLE001 - doctor remains advisory
            report["codex_plugin"] = {
                "status": "unknown",
                "issue": "inspection-failed",
                "detail": str(exc)[-500:],
                "remedy": "fno setup codex-plugin --channel release --refresh",
            }

    # Surface codex hooks dual-representation in the MAIN run too, not only the
    # opt-in `--codex-hooks` mode: a plain `fno doctor` should point at the heal.
    try:
        codex = _codex_hooks_report()
        if codex.get("duplicate_layers") and codex.get("footnote_json_hooks"):
            report["codex_hooks_dual"] = True
        # A foreign hook in ~/.codex/hooks.json is informational only - footnote
        # owns config.toml and must not remove another tool's hook - but a layer
        # split is exactly the unanswerable "which hook ran, from where?" this
        # node exists to close, so surface it rather than stay silent (x-d991).
        if codex.get("foreign_json_hooks"):
            report["codex_hooks_foreign_json"] = codex["foreign_json_hooks"]
    except Exception:
        pass

    # Codex app-server daemon socket (the live-mail prerequisite for a codex
    # peer). Advisory: a plain `fno doctor` names the fix for the hand-started
    # sessions no spawn preflight can reach.
    try:
        report["codex_app_server"] = _codex_app_server_report()
    except Exception:
        pass

    # Gated like the codex plugin check above: a leftover ~/.codex from an
    # uninstalled codex must not nag a user who no longer runs it.
    if shutil.which("codex") is not None:
        try:
            # Reuse the socket probe already stored above: a second stat can
            # disagree with it when the daemon starts between the two calls.
            report["codex_context_window"] = _codex_context_window_report(
                app_server_present=(report.get("codex_app_server") or {}).get("present")
            )
        except Exception:
            pass

    return report


# ---------------------------------------------------------------------------
# Plugin hook launch probe: the live fail-open detector (x-d991)
# ---------------------------------------------------------------------------
#
# A hook whose command cannot resolve (PLUGIN_ROOT unset/empty, or a script
# removed) fails OPEN in Codex: it exits 127/2, the session continues, and the
# guard was absent with no signal. Verifying the hand-expanded absolute path
# always passes, because that path cannot reproduce a placeholder-expansion
# failure; only running the configured string through ``$SHELL -lc`` (the path
# codex-rs/hooks/src/engine/command_runner.rs::default_shell_command takes) can.
# This is that check.

_HOOK_PROBE_TIMEOUT = 8.0

# Probe launch is restricted to read-only gate events. SessionStart/Stop and the
# other stateful events MUTATE ~/.fno when executed: inject-mail-drain advances
# the real mail cursor, reconcile consumes notices, claim-heartbeat writes. A
# probe that ran them would damage the live session, and a temp cwd does not
# isolate that (the state lives under HOME/FNO_HOME, not cwd). PreToolUse gates
# only read (graph.json, worktree location, git state) and exit, so launching
# them is side-effect-free and is exactly the fail-open class this probe exists
# to detect. Every event still gets an existence check; only the launch is gated.
_PROBE_LAUNCH_EVENTS = ("PreToolUse",)

# (name, repo-relative path, the plugin-root env var the manifest's commands
# expand). Root is resolved PER manifest so a broken Codex PLUGIN_ROOT is not
# masked by a healthy CLAUDE_PLUGIN_ROOT (or vice versa).
_PROBE_MANIFESTS = (
    ("hooks.json", "hooks/hooks.json", "CLAUDE_PLUGIN_ROOT"),
    ("codex-hooks.json", "hooks/codex-hooks.json", "PLUGIN_ROOT"),
)


def _resolve_for_display(command: str, root_value: str) -> str:
    """The command with plugin-root placeholders expanded, for failure reports.

    The diagnostic difficulty this whole probe addresses is that the configured
    string and the executed string differ; reporting the expanded form shows the
    path the shell actually tried (empty when the root did not resolve)."""
    expanded = root_value or ""
    return command.replace("${PLUGIN_ROOT}", expanded).replace(
        "${CLAUDE_PLUGIN_ROOT}", expanded
    )


def _referenced_hook_scripts(command: str, root: Path) -> list[Path]:
    """Every ``${PLUGIN_ROOT}/...`` path token in a hook command, resolved under
    ``root``. Used for the interpreter-agnostic missing-script check: a vanished
    .py exits 2 under python3 (not 127), so the launch exit code alone cannot
    prove a script is absent."""
    expanded = command.replace("${PLUGIN_ROOT}", str(root)).replace(
        "${CLAUDE_PLUGIN_ROOT}", str(root)
    )
    return [
        Path(tok.rstrip(';"'))
        for tok in re.findall(rf"{re.escape(str(root))}\S+", expanded)
    ]


# Interpreters a hook command may invoke directly. A bare path (no interpreter)
# is exec'd by the harness, so it needs the execute bit instead.
_HOOK_INTERPRETERS = ("bash", "sh", "python3", "python", "node")


def _command_launch_problems(command: str, root: Path) -> list[str]:
    """Ways a command would fail to START, checked without executing it: a
    missing interpreter, or a bare-invoked script that is not executable.

    Stateful hooks are not launched (they mutate ~/.fno), so for their events
    this static check is the only launchability coverage. A leading
    ``env VAR=val`` prefix is skipped to reach the real first token."""
    import shlex

    expanded = command.replace("${PLUGIN_ROOT}", str(root)).replace(
        "${CLAUDE_PLUGIN_ROOT}", str(root)
    )
    try:
        tokens = shlex.split(expanded)
    except ValueError:
        return []  # unparseable; the launch or existence check is the authority
    i = 0
    if tokens and tokens[0] == "env":
        i = 1
        while i < len(tokens) and "=" in tokens[i] and not tokens[i].startswith("-"):
            i += 1
    if i >= len(tokens):
        return []
    head = tokens[i]
    if "/" not in head and head in _HOOK_INTERPRETERS:
        if not shutil.which(head):
            return [f"interpreter '{head}' not on PATH"]
        return []
    # Bare script path: the harness exec's it directly, so it must be executable.
    script = Path(head)
    if script.is_file() and not os.access(script, os.X_OK):
        return [f"bare-invoked script not executable: {head}"]
    return []


def _manifest_event_commands(data: Any, *, source: str):
    """Yield ``(event, command)`` for every hook in a parsed manifest, preserving
    the event name. Bulletproof against structurally malformed-but-valid JSON
    (the caller owns the hard malformed-manifest signal): every level is
    type-checked, so no shape raises during iteration."""
    if not isinstance(data, dict):
        return
    hooks = data.get("hooks")
    if not isinstance(hooks, dict):
        return
    for event, groups in hooks.items():
        if event == "state" or not isinstance(groups, list):
            continue
        for group in groups:
            if not isinstance(group, dict):
                continue
            entries = group.get("hooks")
            if not isinstance(entries, list):
                continue
            for hook in entries:
                if not isinstance(hook, dict):
                    continue
                command = hook.get("command")
                if isinstance(command, str) and command:
                    yield event, command


def _is_hook_launch_failure(result: dict[str, Any]) -> bool:
    """A genuine launch failure: anything other than a clean exit 0.

    Only read-only PreToolUse gates are launched, under a sandboxed env where
    they have nothing to protect, so a benign probe payload must exit 0 (allow).
    rc None (the launcher would not start, timed out, or could not create a temp
    cwd) is a failure, not a silent green: a guard that does not run is exactly
    the fail-open this probe exists to catch."""
    return result.get("rc") != 0


def _launch_plugin_hook(
    command: str, *, root_value: str, shell: str, root_var: str
) -> dict[str, Any]:
    """Launch one read-only gate hook through the real harness launch path.

    Reproduces Codex's ``default_shell_command`` (``$SHELL -lc <command>``),
    setting ONLY ``root_var`` (the env var this manifest's commands expand), so a
    manifest that uses the WRONG placeholder (e.g. a codex command referencing
    ``${CLAUDE_PLUGIN_ROOT}``) fails here as it would in production, instead of
    being masked by the other harness's var. Only called for read-only events
    (see ``_PROBE_LAUNCH_EVENTS``); stateful hooks are never executed by the
    probe.

    Runs from a throwaway temp cwd with fno + claude-project state sandboxed to
    it. Process-group-bounded so a hung hook cannot wedge the probe; never raises
    (temp creation and launch errors return ``rc=None``, which the predicate
    treats as a failure)."""
    import tempfile

    resolved = _resolve_for_display(command, root_value)
    try:
        workdir = tempfile.mkdtemp(prefix="fno-hook-probe.")
    except OSError as exc:
        return {"command": command, "resolved": resolved, "rc": None,
                "stderr": f"temp creation failed: {exc}"[:300]}
    payload = (
        '{"session_id":"fno-doctor","cwd":'
        + json.dumps(workdir)
        + ',"tool_name":"Bash","tool_input":{"command":"git status --short"}}'
    )
    # Set ONLY this manifest's root var (matching the real harness); drop the
    # others so a wrong-placeholder command is not masked. Sandbox fno +
    # claude-project state to the temp cwd: a gate can mutate on the probe
    # payload (git-protection.py deletes an expired approve_no_verify.flag it
    # resolves under FNO_HOME), and without this the probe writes to the live
    # ~/.fno. HOME stays real so interpreters/binaries resolve.
    env = {
        **os.environ,
        "FNO_HOME": workdir,
        "CLAUDE_PROJECT_DIR": workdir,
    }
    for v in ("PLUGIN_ROOT", "CLAUDE_PLUGIN_ROOT", "PLUGIN_DATA", "CLAUDE_PLUGIN_DATA"):
        env.pop(v, None)
    env[root_var] = root_value
    rc: Optional[int]
    stderr = ""
    try:
        proc = subprocess.Popen(
            [shell, "-lc", command],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=workdir,
            env=env,
            start_new_session=True,
            text=True,
        )
    except OSError as exc:
        shutil.rmtree(workdir, ignore_errors=True)
        return {"command": command, "resolved": resolved, "rc": None,
                "stderr": f"launch failed: {exc}"[:300]}
    try:
        _, err = proc.communicate(input=payload, timeout=_HOOK_PROBE_TIMEOUT)
        rc = proc.returncode
        stderr = (err or "").strip()
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.communicate()
        rc = None
        stderr = "timed out"
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    return {
        "command": command,
        "resolved": resolved,
        "rc": rc,
        "stderr": stderr[-300:],
    }


def _find_plugin_root_with_hooks() -> Optional[Path]:
    """The plugin root that actually carries ``hooks/hooks.json``.

    Prefers an explicit env root containing the manifests; else the resolved
    source, walking up to the directory that carries ``hooks/`` - the resolved
    source can be the Python project dir (e.g. ``cli/``) rather than the plugin
    root, and reading manifests from ``cli/hooks/`` would silently iterate zero
    commands (a false green)."""
    for var in ("CLAUDE_PLUGIN_ROOT", "PLUGIN_ROOT"):
        val = os.environ.get(var)
        if val and (Path(val) / "hooks" / "hooks.json").is_file():
            return Path(val)
    src = _resolve_source(None)
    if src is not None:
        for parent in [src, *src.parents]:
            if (parent / "hooks" / "hooks.json").is_file():
                return parent
    return None


def _probe_root_value(root_var: str, manifest_root: Path) -> str:
    """The plugin-root value to launch a manifest's commands with.

    Honors an explicit env var (even EMPTY) for the manifest's OWN root var, so
    ``PLUGIN_ROOT= fno doctor`` reproduces the live Codex fail-open rather than
    silently falling back to a good root; a healthy CLAUDE_PLUGIN_ROOT cannot
    mask a broken PLUGIN_ROOT because each manifest resolves independently. No
    explicit env -> launch against the root the manifests were read from."""
    if root_var in os.environ:
        return os.environ[root_var]
    return str(manifest_root)


def _plugin_hooks_launch_report() -> dict[str, Any]:
    """Check every plugin hook command: existence for all, plus a real
    ``$SHELL -lc`` launch for the read-only gate events. Report any that cannot
    start, any referenced script that is missing, and any manifest that is
    absent or unparseable on the resolved install.

    Advisory (loud on failure, never changes the staleness exit code): the hard
    gate is the CI test, and this is the live-install detector, because the
    failure is environmental and a CI runner cannot reproduce it. Stateful hooks
    are NOT executed (they mutate ~/.fno); they get an existence check only."""
    shell = os.environ.get("SHELL")
    if not shell:
        return {"applicable": False, "reason": "$SHELL unset"}

    manifest_root = _find_plugin_root_with_hooks()
    if manifest_root is None:
        return {"applicable": False, "reason": "no plugin root with hooks/ resolved"}

    launches: list[dict[str, Any]] = []
    missing: list[dict[str, str]] = []
    manifest_errors: list[dict[str, str]] = []
    for name, rel, root_var in _PROBE_MANIFESTS:
        path = manifest_root / rel
        root_value = _probe_root_value(root_var, manifest_root)
        # A missing or unparseable manifest means that harness loads NO hooks -
        # a fail-open the CI validity test cannot see on a corrupted or partial
        # live install. Surface it instead of reading as a healthy zero checks.
        if not path.is_file():
            manifest_errors.append(
                {"manifest": name, "issue": "missing", "path": str(path)}
            )
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            manifest_errors.append(
                {"manifest": name, "issue": f"unparseable: {exc}"[:200], "path": str(path)}
            )
            continue
        # Valid JSON but structurally wrong (e.g. ``{"hooks": []}``) means that
        # harness registers no real hooks either - same fail-open class as a
        # missing manifest, not a healthy zero checks.
        if not isinstance(data, dict) or not isinstance(data.get("hooks"), dict):
            manifest_errors.append(
                {"manifest": name,
                 "issue": "malformed: top-level 'hooks' object missing or wrong type",
                 "path": str(path)}
            )
            continue
        # Nested malformation: an event whose groups are not an array (the silent
        # skip in _manifest_event_commands would otherwise read as zero commands,
        # a false green). Deeper shape errors are skipped without raising.
        bad_events = [
            ev for ev, grp in data["hooks"].items()
            if ev != "state" and not isinstance(grp, list)
        ]
        if bad_events:
            manifest_errors.append(
                {"manifest": name,
                 "issue": "malformed: event group(s) not arrays: "
                          + ", ".join(bad_events[:3]),
                 "path": str(path)}
            )
            continue
        for event, command in _manifest_event_commands(data, source=name):
            for script in _referenced_hook_scripts(command, manifest_root):
                if not script.is_file():
                    missing.append(
                        {"manifest": name, "event": event, "script": str(script)}
                    )
            if event in _PROBE_LAUNCH_EVENTS:
                result = _launch_plugin_hook(
                    command, root_value=root_value, shell=shell, root_var=root_var
                )
                result["manifest"] = name
                result["event"] = event
                launches.append(result)
            else:
                # Stateful events are not executed (they mutate ~/.fno); check
                # they can still START - missing interpreter, or a bare-invoked
                # script without the execute bit - so a 126/127 fail-open there
                # is not silently green.
                for problem in _command_launch_problems(command, manifest_root):
                    missing.append(
                        {"manifest": name, "event": event, "script": problem}
                    )

    failures = [r for r in launches if _is_hook_launch_failure(r)]
    return {
        "applicable": True,
        "shell": shell,
        "manifest_root": str(manifest_root),
        "launched": len(launches),
        "failed": len(failures),
        "failures": failures,
        "missing_scripts": missing,
        "manifest_errors": manifest_errors,
    }


def _pre_push_hook_report(src: Optional[Path]) -> dict[str, Any]:
    """Advisory: a legacy pre-push hook that gates on the pushing checkout's
    current branch refuses every push from a checkout on main, whatever the
    destination ref. Detection is delegated to the installer's ``--check`` so
    there is exactly one implementation of "is the installed hook the bad
    one". Never changes status/exit; an absent or foreign hook is not a
    defect and reports nothing."""
    candidates = []
    if src is not None:
        candidates.append(src / "scripts" / "install-pre-push-hook.sh")
    candidates.append(
        Path(__file__).resolve().parents[3] / "scripts" / "install-pre-push-hook.sh"
    )
    script = next((c for c in candidates if c.is_file()), None)
    if script is None:
        return {"status": "unchecked"}
    try:
        proc = subprocess.run(
            ["bash", str(script), "--check"],
            capture_output=True,
            text=True,
            timeout=15,
            cwd=str(Path.cwd()),
        )
    except Exception:
        return {"status": "unchecked"}
    out = (proc.stdout or "") + (proc.stderr or "")
    if "legacy" in out:
        return {"status": "legacy"}
    return {"status": "ok"}


def doctor_command(
    fix: bool = typer.Option(
        False,
        "--fix",
        help="If stale, run `fno update` for Python staleness (honors the IN_PROGRESS guard). "
        "For rust-only staleness, calls the rust refresh helper directly (no full Python reinstall).",
    ),
    json_out: bool = typer.Option(
        False,
        "--json", "-J",
        help="Emit a single JSON object on stdout; human/metadata text goes to stderr.",
    ),
    source: Optional[Path] = typer.Option(
        None,
        "--source",
        help="Path to the fno source checkout to compare against (auto-detected if omitted).",
    ),
    cost_check: bool = typer.Option(
        False,
        "--cost-check",
        help="Cross-check session-cost.py against ccusage for one recent session "
        "(opt-in; gracefully skips when ccusage is not installed). "
        "Exit 1 only on proven divergence.",
    ),
    codex_hooks: bool = typer.Option(
        False,
        "--codex-hooks",
        help="Inspect Codex user-level SessionStart hook layers and trust (advisory).",
    ),
    codex_bind: bool = typer.Option(
        False,
        "--codex-bind",
        help="Spawn one throwaway codex pane and report which binding oracle "
        "caught it (rollout-fd, daemon, or neither); the pane-binding lane canary.",
    ),
    context_audit: bool = typer.Option(
        False,
        "--context-audit",
        hidden=True,
        help="Emit the read-only Footnote context census/compiler report.",
    ),
    context_harness: str = typer.Option(
        "all",
        "--context-harness",
        hidden=True,
        help="Context audit harness: claude, codex, gemini, or all.",
    ),
    context_entry: str = typer.Option(
        "all",
        "--context-entry",
        hidden=True,
        help="Context audit entry state: startup, resume, clear, post_compact, or all.",
    ),
    context_budget: int = typer.Option(
        32_768,
        "--context-budget",
        hidden=True,
        help="Maximum compiled context packet bytes.",
    ),
    context_host: Optional[Path] = typer.Option(
        None,
        "--context-host",
        hidden=True,
        help="Host project root whose Footnote-owned native directives are audited.",
    ),
) -> None:
    """Report skew between the installed fno and its source checkout."""
    if context_audit:
        if fix or cost_check or codex_hooks or codex_bind:
            raise typer.BadParameter(
                "--context-audit cannot be combined with --fix, --cost-check, "
                "--codex-hooks, or --codex-bind"
            )
        from fno.context_audit import (
            SUPPORTED_ENTRY_STATES,
            SUPPORTED_HARNESSES,
            build_context_report,
        )

        harnesses = (
            SUPPORTED_HARNESSES
            if context_harness == "all"
            else (context_harness,)
        )
        entry_states = (
            SUPPORTED_ENTRY_STATES if context_entry == "all" else (context_entry,)
        )
        if context_harness != "all" and context_harness not in SUPPORTED_HARNESSES:
            raise typer.BadParameter(
                "--context-harness must be claude, codex, gemini, or all"
            )
        if context_entry != "all" and context_entry not in SUPPORTED_ENTRY_STATES:
            raise typer.BadParameter(
                "--context-entry must be startup, resume, clear, post_compact, or all"
            )
        if context_budget < 1:
            raise typer.BadParameter("--context-budget must be at least 1")

        plugin_root = source or Path.cwd()
        host_root = context_host or plugin_root
        report = build_context_report(
            host_root,
            plugin_root=plugin_root,
            harnesses=harnesses,
            entry_states=entry_states,
            packet_budget_bytes=context_budget,
            node_count=1,
        )
        if json_out:
            typer.echo(json.dumps(report))
        else:
            typer.echo("fno doctor --context-audit")
            for cell in report["cells"]:
                compiled = cell["compiled"]
                packet = compiled["packet"]
                typer.echo(
                    f"  {cell['harness']}/{cell['entry_state']}: "
                    f"{packet['bytes']}/{packet['budget_bytes']} bytes; "
                    f"{len(compiled['duplicates'])} duplicate group(s); "
                    f"{len(compiled['conflicts'])} conflict(s); "
                    f"{len(compiled['omitted_sources'])} omitted"
                )
        raise typer.Exit(0)

    if codex_hooks:
        if fix or source is not None or cost_check:
            raise typer.BadParameter("--codex-hooks may only be combined with --json")
        result = _codex_hooks_report()
        if json_out:
            typer.echo(json.dumps(result))
            _emit_codex_hooks_report(result, err=True)
        else:
            _emit_codex_hooks_report(result, err=False)
        raise typer.Exit(0)

    if codex_bind:
        if fix or source is not None or cost_check:
            raise typer.BadParameter("--codex-bind may only be combined with --json")
        result = _codex_bind_report()
        if json_out:
            typer.echo(json.dumps(result))
        else:
            _emit_codex_bind_report(result)
        raise typer.Exit(0 if result["bound"] else 1)

    if cost_check:
        # Dedicated mode: the staleness check stays network-free and
        # ccusage-free by default; this opt-in path never mixes its exit
        # semantics with the staleness verdict.
        raise typer.Exit(_cost_check())

    from fno import update

    src = _resolve_source(source)
    source_rev = _source_rev(src) if src is not None else None
    marker = _read_marker()
    capture_present = _probe_installed_verb()
    rust = _rust_report()

    rust_src_rev = _rust_source_rev(src)
    cargo_bin_present = _cargo_bin_present()

    deployed_config_keys = _deployed_config_keys()
    source_config_keys = _source_config_keys(src)
    content_drift = _python_content_drift(src)

    result = _verdict(
        source_resolved=src is not None,
        source_rev=source_rev,
        marker=marker,
        capture_present=capture_present,
        rust_binary=rust["binary"],
        rust_installed_rev=rust["revision"],
        rust_source_rev=rust_src_rev,
        cargo_bin_present=cargo_bin_present,
        deployed_config_keys=deployed_config_keys,
        source_config_keys=source_config_keys,
        content_drift_count=content_drift,
    )
    # Advisory front-door fields (x-c267); never change status/exit.
    result.update(_mux_front_door_report())
    # Advisory process-freshness (x-e6dd): a long-running mux server still on the
    # OLD proto after an upgrade. Binary staleness is above; this is the running
    # PROCESS. Never changes status/exit.
    from fno import update as _update

    result["mux_server_stale"] = _update.stale_mux_servers()

    # Advisory orphan-file check (Group 3 GC); never changes status/exit.
    result["orphan_files"] = _orphan_report()

    # Advisory PR-watch liveness (x-e106): enabled-but-dead ran silently for
    # weeks with zero signal; the verdict derives from tick recency (ground
    # truth), never from config alone. Never changes status/exit.
    result["pr_watch"] = _pr_watch_liveness()

    # Advisory open-file limit visibility: a launchd child starves at 256 while
    # a login shell reads 1048576 and both are correct. Never changes
    # status/exit.
    result["fd_limit"] = _fd_limit_report()

    # Advisory dead-letter visibility (US7): an unwired drain hook + stale bus
    # mail to a dead handle are silent quicksand. Never changes status/exit.
    result["dead_letter"] = _dead_letter_report()

    # Advisory codex app-server daemon socket: the live-mail prerequisite for a
    # codex peer. Names the fix for hand-started sessions no spawn preflight
    # reaches. Never changes status/exit.
    try:
        result["codex_app_server"] = _codex_app_server_report()
    except Exception:
        pass

    # Advisory managed-block staleness (US8): a host AGENTS.md/CLAUDE.md footnote
    # block older than the current template. Never changes status/exit.
    result["managed_block"] = _managed_block_report()

    # Advisory per-harness surface freshness (x-3248): codex/opencode plugin
    # surfaces `fno update` does not cover. Never changes status/exit.
    result["harness_surface"] = _harness_surface_report()

    # Advisory plugin hook launch probe (x-d991): a hook command that cannot
    # resolve fails open in Codex with no signal. This launches every configured
    # hook through the real ``$SHELL -lc`` path and reports any that cannot
    # start. Loud on failure; never changes the staleness exit code.
    result["plugin_hooks"] = _plugin_hooks_launch_report()

    # Advisory legacy pre-push hook: gates on the pushing checkout's branch,
    # so it refuses every push from a canonical checkout on main. Never
    # changes status/exit.
    result["pre_push_hook"] = _pre_push_hook_report(src)

    # Agent health (x-1c7b): grooming freshness is advisory, but a nonzero-exit
    # LaunchAgent DOES change the exit code - an installed-but-dead agent is
    # exactly the silence this check exists to break.
    result["groom"] = _groom_health()
    result["archive_id_collisions"] = _archive_id_collisions()
    result["post_merge_sync"] = _post_merge_sync_health()
    result["launch_agents"] = _launch_agent_failures()

    # Advisory silent-switch legibility (x-8cd5 Wave 6): default-off switches
    # silently producing inaction + default-on/armed switches silently merging.
    # Never changes status/exit. The plugin-cache report is computed ONCE and
    # shared with the silent-switch pass (its unknown-manifest cause reads it):
    # two invocations would run the registry read and the git probes twice and
    # could disagree about the same cache within one report.
    plugin_cache = _plugin_cache_report()
    result["silent_switches"] = _silent_switch_report(plugin_cache=plugin_cache)

    # Advisory deployed-plugin-cache freshness (x-4be1): the hooks Claude
    # sessions actually run. Never changes status/exit.
    result["plugin_cache"] = plugin_cache

    if json_out:
        # Single JSON object on stdout; human text to stderr (LLM-caller contract).
        typer.echo(json.dumps(result))
        _emit_human(result, src, rust, err=True, cargo_present=cargo_bin_present)
    else:
        _emit_human(result, src, rust, err=False, cargo_present=cargo_bin_present)
        if not fix:
            preamble_line = _preamble_budget_line(src)
            if preamble_line is not None:
                typer.echo(preamble_line)

    # Report BEFORE delegating: `fno update` execs/replaces this process.
    if fix:
        # Heal a dead pr-watch first: the verdict's own fix is the bounce, and a
        # python_stale --fix execs `fno update` below and never returns, so act
        # on it here. Advisory - never changes doctor's exit code (a dead
        # watcher and a stale binary are distinct concerns).
        pw = result.get("pr_watch") or {}
        if pw.get("verdict") == "dead" and not json_out:
            from fno.pr_watch._install import _LAUNCH_AGENTS_DIR, heal_watcher

            hmsg, _ = heal_watcher(launch_agents_dir=_LAUNCH_AGENTS_DIR)
            typer.echo(f"fno doctor: --fix pr-watch heal: {hmsg}", err=True)

        # Install the groom agent when the pass has never run and nothing is
        # scheduled to run it. The receipt already reports the BOOTSTRAP result
        # rather than the plist write, so a plist that lands but fails to load
        # reads `failed` here too. Advisory, like the pr-watch heal above.
        # darwin-gated: off launchd this only ever returns `unsupported`, so an
        # unguarded call is a warning on every --fix that nothing can act on.
        gr = result.get("groom") or {}
        if (
            sys.platform == "darwin"
            and gr.get("stale")
            and not gr.get("agent_installed")
            and not json_out
        ):
            from fno.backlog.groom import install_groom_agent

            receipt = install_groom_agent()
            typer.echo(
                f"fno doctor: --fix groom agent: {receipt.get('status')}"
                f" ({receipt.get('detail') or receipt.get('plist')})",
                err=True,
            )

        if json_out:
            # Preserve the single-JSON-object stdout contract: any repair
            # operation prints to stdout, so skip under --json. Covers both
            # python_stale and rust_stale paths.
            typer.echo(
                "fno doctor: --fix skipped under --json (would pollute the JSON stdout); "
                "run `fno doctor --fix` without --json to repair.",
                err=True,
            )
        elif result["python_stale"]:
            typer.echo("fno doctor: --fix running `fno update`...", err=True)
            # Delegate to update (its own IN_PROGRESS guard applies). Its new
            # rust leg refreshes both Python and Rust. On Unix this execs and
            # never returns; the post-update marker then matches HEAD.
            update.update_command(source=source, dry_run=False, force=False)
            return
        elif result["rust_stale"]:
            # Rust-only stale: call the refresh helper directly (no needless
            # Python reinstall). src cannot be None here because rust_stale
            # requires rust_source_rev, which requires a resolved source.
            if update._target_in_progress():
                typer.echo(
                    "fno doctor: --fix refused: target-state.md shows status: IN_PROGRESS. "
                    "Refreshing rust bins mid-loop risks binary skew; "
                    "run `fno update --force` after the loop, or to override now.",
                    err=True,
                )
                raise typer.Exit(1)
            assert src is not None, "rust_stale True but src is None - logic error"
            outcome = update._refresh_rust_bins(src, force=False, dry_run=False)
            if outcome == "refreshed":
                typer.echo("fno doctor: rust bins refreshed successfully.", err=True)
                raise typer.Exit(0)
            elif outcome == "fresh":
                # A concurrent refresh can land between the verdict read and
                # the repair; the goal state is achieved either way.
                typer.echo(
                    "fno doctor: rust bins already fresh (refreshed concurrently);"
                    " nothing to fix.",
                    err=True,
                )
                raise typer.Exit(0)
            elif outcome == "refreshed-no-marker":
                # Bins rebuilt, but no marker landed (ab-703f2ed2): the
                # stale verdict will not converge - the next doctor run
                # still reports rust stale. Exit nonzero so loop callers
                # don't believe the repair worked.
                typer.echo(
                    "fno doctor: rust bins refreshed but the marker was not"
                    " written; the stale verdict will not converge."
                    " Check ~/.fno permissions and rerun `fno doctor`.",
                    err=True,
                )
                raise typer.Exit(1)
            else:
                typer.echo(f"fno doctor: rust refresh outcome: {outcome}.", err=True)
                raise typer.Exit(1)
        else:
            typer.echo("fno doctor: nothing to fix.", err=True)

    dead_agents = bool((result.get("launch_agents") or {}).get("dead"))
    id_collisions = bool(
        (result.get("archive_id_collisions") or {}).get("count")
        or (result.get("archive_id_collisions") or {}).get("unreadable")
    )
    raise typer.Exit(1 if result["status"] == "stale" or dead_agents or id_collisions else 0)
