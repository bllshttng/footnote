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
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any, Literal, Optional

import typer

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
    abi_bin = shutil.which("fno-py")
    if not abi_bin:
        return "unknown"
    try:
        result = subprocess.run(
            [abi_bin, *_PROBE_VERB, "--help"],
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
        from fno.agents.rust_runtime import resolve_installed_binary

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
        out("fno doctor: installed fno is up to date with source.")
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


# ---------------------------------------------------------------------------
# Command
# ---------------------------------------------------------------------------


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
    toml_trusted = diagnostics.all_toml_footnote_hooks_trusted
    toml_trust = dict(
        zip(
            diagnostics.toml_footnote_state_keys,
            diagnostics.toml_footnote_trusted,
            strict=True,
        )
    )
    duplicate_layers = diagnostics.has_toml_hooks and diagnostics.has_json_hooks

    if diagnostics.errors:
        status = "error"
    elif not toml_wired or not toml_trusted or diagnostics.has_json_hooks:
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
        "footnote_toml_trusted": toml_trusted,
        "footnote_toml_trust": toml_trust,
        "duplicate_layers": duplicate_layers,
        "footnote_json_hooks": list(diagnostics.json_footnote_commands),
        "foreign_json_hooks": list(diagnostics.json_foreign_commands),
        "errors": list(diagnostics.errors),
    }


def _emit_codex_hooks_report(result: dict[str, Any], *, err: bool) -> None:
    """Render one summary plus actionable Codex hook diagnostics."""

    def out(message: str) -> None:
        typer.echo(message, err=err)

    if result["footnote_toml_trusted"]:
        trust = "trusted"
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

    for state_key, present in result["footnote_toml_trust"].items():
        state = "found" if present else "missing"
        out(f"fno doctor: codex hooks: trust state {state}: {state_key}")
    if result["footnote_toml_wired"]:
        if not result["footnote_toml_trusted"]:
            out("fno doctor: codex hooks: approve the footnote SessionStart hook in Codex.")

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
) -> None:
    """Report skew between the installed fno and its source checkout."""
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

    if json_out:
        # Single JSON object on stdout; human text to stderr (LLM-caller contract).
        typer.echo(json.dumps(result))
        _emit_human(result, src, rust, err=True, cargo_present=cargo_bin_present)
    else:
        _emit_human(result, src, rust, err=False, cargo_present=cargo_bin_present)

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

    raise typer.Exit(1 if result["status"] == "stale" else 0)
