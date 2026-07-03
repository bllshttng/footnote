"""fno doctor: detect skew between the installed fno and its source checkout.

The ``fno`` on a developer's PATH is a snapshot, not a live view of the repo
(ab-5a1fc285). When a new gate-bearing verb ships (e.g. ``backlog inbox`` in
PR #329), an install that predates it silently fails the documented path. This
command makes that skew detectable and self-explaining, **network-free**.

Two Python-side signals, each degrading to ``unknown`` rather than crying wolf:

1. **Revision compare** (high-signal, when a source checkout is resolvable):
   compare ``~/.fno/installed-rev`` (written by ``fno update``) against
   ``git rev-parse HEAD`` of the resolved source.
2. **Capability probe** (always-available fallback): run ``fno backlog capture
   --help`` against the *installed* CLI; a "No such command" failure proves a
   missing verb regardless of any marker.

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

import json
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
) -> dict[str, Any]:
    """Pure verdict function (no I/O) returning the complete JSON-serializable
    result, so the decision matrix is unit-testable and the output contract is
    assembled in exactly one place.

    Rust staleness is proven only with full evidence: a cargo binary exists,
    the installed-rust-rev marker is known, the crates/ subtree rev is known,
    and they differ. Any missing evidence piece degrades to "not stale" (never
    cry wolf). Rust evidence gaps never upgrade unknown to fresh and never
    block fresh.
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
        "missing_verbs": missing_verbs,
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

    # Informational: the rev the binary self-reports (ab-24a59d50), independent
    # of the installed-rust-rev marker. A non-git build reports None and is
    # silently skipped. Surfaced for visibility; the verdict still keys on the
    # marker, so this line never changes the exit code.
    binary_rev = rust.get("binary_rev")
    if binary_rev is not None:
        src_rev = result.get("rust_source_rev")
        if src_rev is not None and binary_rev == src_rev:
            out(f"fno doctor: rust fno-agents binary self-reports rev {binary_rev[:12]} (matches source).")
        elif src_rev is not None:
            out(
                f"fno doctor: rust fno-agents binary self-reports rev {binary_rev[:12]} "
                f"(source crates/ rev {src_rev[:12]})."
            )
        else:
            out(f"fno doctor: rust fno-agents binary self-reports rev {binary_rev[:12]}.")


# ---------------------------------------------------------------------------
# Command
# ---------------------------------------------------------------------------


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
) -> None:
    """Report skew between the installed fno and its source checkout."""
    from fno import update

    if cost_check:
        # Dedicated mode: the staleness check stays network-free and
        # ccusage-free by default; this opt-in path never mixes its exit
        # semantics with the staleness verdict.
        raise typer.Exit(_cost_check())

    src = _resolve_source(source)
    source_rev = _source_rev(src) if src is not None else None
    marker = _read_marker()
    capture_present = _probe_installed_verb()
    rust = _rust_report()

    rust_src_rev = _rust_source_rev(src)
    cargo_bin_present = _cargo_bin_present()

    result = _verdict(
        source_resolved=src is not None,
        source_rev=source_rev,
        marker=marker,
        capture_present=capture_present,
        rust_binary=rust["binary"],
        rust_installed_rev=rust["revision"],
        rust_source_rev=rust_src_rev,
        cargo_bin_present=cargo_bin_present,
    )

    if json_out:
        # Single JSON object on stdout; human text to stderr (LLM-caller contract).
        typer.echo(json.dumps(result))
        _emit_human(result, src, rust, err=True, cargo_present=cargo_bin_present)
    else:
        _emit_human(result, src, rust, err=False, cargo_present=cargo_bin_present)

    # Report BEFORE delegating: `fno update` execs/replaces this process.
    if fix:
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
