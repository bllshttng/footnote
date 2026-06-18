#!/usr/bin/env python3
"""Phase 01 measurement harness: quantify fno CLI overhead in target phases.

This script answers: what fraction of a target phase's wall time is spent in
`fno <verb>` subprocess calls? If the fraction is low (<15%), a daemon adds
minimal value and a simpler lazy-imports refactor is preferred.

Methodology:
  1. Sample 5 representative sessions from ledger.json that include the 'do'
     phase and have real wall-time data (duration_minutes).
  2. Estimate per-session fno call count from known verb frequencies in target
     skill scripts. All fno calls share a common subprocess startup cost.
  3. Time each key fno verb (N=20 runs each) to get a median latency.
  4. Compute fno_wall_seconds = call_count * median_latency_s per session.
  5. Compute ratio = sum(fno_wall_seconds) / sum(phase_wall_seconds).
  6. Write cli/benchmarks/fno_in_target_results.json.

If <3 valid sessions are available, the script halts with a non-zero exit
and emits a help signal (AC1-ERR).

Usage:
    python cli/benchmarks/measure_fno_in_target.py
    python cli/benchmarks/measure_fno_in_target.py --ledger path/to/ledger.json
    python cli/benchmarks/measure_fno_in_target.py --dry-run  # use fixture data
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import warnings
from pathlib import Path
from typing import Any, Literal, TypedDict


# Per-session measurement record. TypedDict (not dataclass) so the existing
# JSON-serialization sites and dict-style access patterns keep working
# without a wrapping/unwrapping shim. The five fields are required.
class SessionData(TypedDict):
    session_id: str
    fno_call_count: int
    fno_wall_seconds: float
    phase_wall_seconds: float
    ratio: float


# The three buckets the decision rule maps to. Narrowed via Literal so a
# typo at a call site (e.g. ``result == "abort_demon"``) is a mypy error
# rather than a quietly-false runtime comparison.
Decision = Literal["abort_daemon", "reads_only_v1", "full_v1"]


# ---------------------------------------------------------------------------
# Repo root discovery
# ---------------------------------------------------------------------------

SCRIPT_DIR = Path(__file__).parent.resolve()
REPO_ROOT = SCRIPT_DIR.parent.parent  # cli/benchmarks -> cli -> repo root


def _resolve_default_ledger() -> Path:
    """Resolve the canonical ledger path via fno.paths if available.

    Falls back to ``REPO_ROOT/.fno/ledger.json`` only when the path
    config can't be loaded (e.g., the harness is run from a checkout that
    doesn't have the cli package installed in the active interpreter).
    Codex review on PR #268 caught the previous hardcoded path that
    ignored ``config.paths.ledger_json`` overrides.
    """
    try:
        from fno.paths import ledger_json as _ledger_json  # type: ignore[import-untyped]
        result = _ledger_json()
        # _ledger_json is dynamically typed; coerce to Path so the caller's
        # return annotation stays honest.
        return Path(result)
    except Exception:
        return REPO_ROOT / ".fno" / "ledger.json"


DEFAULT_LEDGER = _resolve_default_ledger()
DEFAULT_OUTPUT = SCRIPT_DIR / "fno_in_target_results.json"

# ---------------------------------------------------------------------------
# Known fno verb frequencies per full target execution session
# Derived from grep of skills/target/, hooks/, and observed session patterns.
# A "full" session runs: do + review + validate + ship + external + docs.
# Verbs are the subprocess-startup cost carriers; arg differences are minor.
# ---------------------------------------------------------------------------

# (verb_family, expected_calls_per_full_session)
FNO_VERB_FREQUENCIES: list[tuple[str, int]] = [
    ("gate set",        4),   # set quality_check_passed, output_validated, etc.
    ("gate transition", 4),   # emit-gate-transition per phase
    ("pr rebase",       2),   # pre-ship rebase check
    ("pr merge",        1),   # ship phase
    ("backlog done",    1),   # mark node complete at ship
    ("backlog get",     2),   # operator reads node before + after
    ("paths state-dir", 2),   # hook queries state dir
    ("agent whoami",    1),   # pre-phase context check
    ("plan stamp",      1),   # ship phase stamps frontmatter
    ("inbox send",      1),   # cross-project notify at ship
]

TOTAL_CALLS_PER_FULL_SESSION = sum(c for _, c in FNO_VERB_FREQUENCIES)

# Phases that contribute to TOTAL_CALLS_PER_FULL_SESSION. A session that
# completed only `do` should not be charged for review/validate/ship/
# external/docs calls. Codex review on PR #268 caught this.
EXPECTED_PHASES_FULL = ("do", "review", "validate", "ship", "external", "docs")

# ---------------------------------------------------------------------------
# Decision rule
# ---------------------------------------------------------------------------

ABORT_THRESHOLD = 0.15
FULL_V1_THRESHOLD = 0.30


def apply_decision_rule(aggregate_ratio: float) -> Decision:
    """Map aggregate ratio to a decision string.

    Boundaries are inclusive at the upper bucket:
      ratio < 0.15  -> abort_daemon
      0.15 <= ratio < 0.30 -> reads_only_v1
      ratio >= 0.30 -> full_v1

    AC1-EDGE: ratio == 0.15 -> reads_only_v1; ratio == 0.30 -> full_v1
    """
    if aggregate_ratio >= FULL_V1_THRESHOLD:
        return "full_v1"
    if aggregate_ratio >= ABORT_THRESHOLD:
        return "reads_only_v1"
    return "abort_daemon"


# ---------------------------------------------------------------------------
# Session parsing
# ---------------------------------------------------------------------------

# Derived from SessionData.__annotations__ so adding a field to the
# TypedDict automatically extends the runtime check; the two cannot drift.
_REQUIRED_SESSION_FIELDS: tuple[str, ...] = tuple(SessionData.__annotations__)


def parse_session_data(entry: Any) -> SessionData | None:
    """Validate and return a session measurement dict, or None if invalid.

    AC1-FR: invalid/unparseable sessions return None; caller skips and logs.

    Field-type validation is intentionally strict: a ledger entry with
    ``"fno_call_count": "banana"`` or ``"ratio": None`` passes a presence
    check but corrupts downstream arithmetic in ``compute_aggregate_ratio``.
    Type-checking each field upfront keeps the ``SessionData`` cast honest.
    """
    if not isinstance(entry, dict):
        return None
    for field in _REQUIRED_SESSION_FIELDS:
        if field not in entry:
            return None
    if not isinstance(entry["session_id"], str) or not entry["session_id"]:
        return None
    if not isinstance(entry["fno_call_count"], int) or isinstance(entry["fno_call_count"], bool):
        # ``bool`` is a subclass of ``int``; reject it explicitly to avoid
        # ``True`` quietly counting as 1 call.
        return None
    for numeric_field in ("fno_wall_seconds", "phase_wall_seconds", "ratio"):
        value = entry[numeric_field]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return None
    if entry["phase_wall_seconds"] <= 0:
        return None
    # Runtime shape and field types verified above; cast to TypedDict for
    # the type-checker. Existing call sites read the same dict keys verbatim.
    return entry  # type: ignore[return-value]


# ---------------------------------------------------------------------------
# Aggregate ratio
# ---------------------------------------------------------------------------


def compute_aggregate_ratio(sessions: list[SessionData]) -> float:
    """Volume-weighted aggregate ratio: sum(fno) / sum(phase).

    Raises ValueError if sessions list is empty.
    """
    if not sessions:
        raise ValueError("compute_aggregate_ratio: no sessions provided")
    total_fno: float = sum(s["fno_wall_seconds"] for s in sessions)
    total_phase: float = sum(s["phase_wall_seconds"] for s in sessions)
    return total_fno / total_phase


# ---------------------------------------------------------------------------
# Timing
# ---------------------------------------------------------------------------

_CACHED_MEDIAN_MS: float | None = None


PROBE_TIMEOUT_SECONDS = 10.0
MAX_FAILED_PROBES_FRACTION = 0.25


def measure_median_fno_latency_ms(n_runs: int = 20) -> float:
    """Time `fno --help` N times and return the median latency in ms.

    Uses --help as a zero-side-effect probe that exercises the full
    subprocess startup path. The startup cost dominates; argument parsing
    is negligible.

    Failure handling (review: silent-failure-hunter HIGH on PR for
    ab-f0fe4687):
    - ``timeout=PROBE_TIMEOUT_SECONDS`` so a hung ``fno`` invocation
      cannot wedge the harness forever.
    - Probes that return non-zero or time out are dropped, not silently
      averaged into the median. Negative ``returncode`` (signal kill,
      e.g. -9 for SIGKILL) is treated as failure.
    - If more than ``MAX_FAILED_PROBES_FRACTION`` of probes fail, the
      whole measurement is invalid and ``RuntimeError`` aborts the run
      so the operator sees a clear breadcrumb rather than a confidently
      wrong ratio.
    """
    times: list[float] = []
    failures: list[str] = []
    for _ in range(n_runs):
        t0 = time.perf_counter()
        try:
            result = subprocess.run(
                ["fno", "--help"],
                capture_output=True,
                check=False,
                timeout=PROBE_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired:
            failures.append(f"timeout after {PROBE_TIMEOUT_SECONDS}s")
            continue
        except OSError as exc:
            failures.append(f"OSError: {exc!s}")
            continue
        t1 = time.perf_counter()
        if result.returncode != 0:
            stderr_tail = (result.stderr or b"").decode("utf-8", errors="replace")[-200:]
            failures.append(f"returncode={result.returncode} stderr={stderr_tail!r}")
            continue
        times.append((t1 - t0) * 1000)

    if not times:
        raise RuntimeError(
            f"fno probe failed every attempt ({n_runs} runs). "
            f"First failure: {failures[0] if failures else 'unknown'}"
        )
    if len(failures) / n_runs > MAX_FAILED_PROBES_FRACTION:
        raise RuntimeError(
            f"fno probe failed in {len(failures)}/{n_runs} runs "
            f"(threshold {MAX_FAILED_PROBES_FRACTION:.0%}). "
            f"Sample failure: {failures[0]}"
        )

    times.sort()
    # Standard median: average the two middle elements when len is even,
    # pick the single middle when odd. Gemini review on PR #268 caught the
    # earlier upper-middle bias.
    n = len(times)
    return times[n // 2] if n % 2 else (times[n // 2 - 1] + times[n // 2]) / 2


def get_median_fno_latency_ms(n_runs: int = 20) -> float:
    """Return cached or freshly measured median fno latency."""
    global _CACHED_MEDIAN_MS
    if _CACHED_MEDIAN_MS is None:
        _CACHED_MEDIAN_MS = measure_median_fno_latency_ms(n_runs)
    return _CACHED_MEDIAN_MS


# ---------------------------------------------------------------------------
# Ledger loading
# ---------------------------------------------------------------------------

MIN_SAMPLE_SESSIONS = 3


def load_sessions_from_ledger(ledger_path: Path) -> list[dict[str, Any]]:
    """Load and filter ledger.json for sessions suitable for measurement.

    Returns dicts with keys: session_id, duration_minutes, phases_completed, title.
    Only sessions with the 'do' phase and duration > 3 minutes are included.
    """
    with open(ledger_path, encoding="utf-8") as f:
        raw = json.load(f)

    entries = raw if isinstance(raw, list) else raw.get("entries", [])
    seen: set[str] = set()
    good: list[dict[str, Any]] = []

    for e in entries:
        if not isinstance(e, dict):
            continue
        sessions_list = e.get("sessions") or []
        if not sessions_list:
            continue
        full_sid = sessions_list[0]
        if full_sid in seen:
            continue
        dur = e.get("duration_minutes") or 0
        if not dur or dur < 3:
            continue
        phases = e.get("phases_completed") or []
        if "do" not in phases:
            continue
        seen.add(full_sid)
        good.append({
            "session_id": full_sid,
            "duration_minutes": float(dur),
            "phases_completed": list(phases),
            "title": str(e.get("title") or ""),
        })

    return good


# ---------------------------------------------------------------------------
# Main measurement logic
# ---------------------------------------------------------------------------


def build_session_measurement(
    ledger_session: dict[str, Any],
    median_fno_ms: float,
) -> tuple[SessionData, dict[str, Any]] | None:
    """Build a session measurement dict from ledger entry + timing data.

    Returns None with a warning if the session cannot be parsed.
    The fno_call_count is scaled by the fraction of EXPECTED_PHASES_FULL
    actually completed in this session: a `do`-only session contributes
    only `do`-phase calls, not the full 19-call total. Codex review on
    PR #268 caught the previous over-attribution.
    """
    sid = ledger_session.get("session_id")
    dur_minutes = ledger_session.get("duration_minutes")

    if not sid or not dur_minutes or dur_minutes <= 0:
        warnings.warn(f"Skipping session {sid!r}: missing/invalid duration")
        return None

    phase_wall_seconds = dur_minutes * 60.0
    phases_completed = ledger_session.get("phases_completed") or []
    completed_in_full = sum(1 for p in phases_completed if p in EXPECTED_PHASES_FULL)
    if completed_in_full == 0:
        warnings.warn(
            f"Skipping session {sid!r}: no recognized phases in "
            f"phases_completed={phases_completed!r}"
        )
        return None
    phase_fraction = completed_in_full / len(EXPECTED_PHASES_FULL)
    call_count = max(1, round(TOTAL_CALLS_PER_FULL_SESSION * phase_fraction))
    fno_wall_seconds = (call_count * median_fno_ms) / 1000.0

    if phase_wall_seconds <= 0:
        warnings.warn(f"Skipping session {sid!r}: zero phase wall seconds")
        return None

    ratio = fno_wall_seconds / phase_wall_seconds

    # Return the SessionData narrow shape and the output-only extras as a
    # tuple. The previous "build a runtime dict with extras and cast to
    # SessionData" pattern lied to the type-checker -- the static return
    # type said five fields, the runtime value carried eight. A tuple
    # split makes the extras' scope explicit at the call site and keeps
    # ``SessionData`` honest for downstream consumers (compute_aggregate_ratio,
    # parse_session_data) that read only the canonical fields.
    measurement: SessionData = {
        "session_id": sid,
        "fno_call_count": call_count,
        "fno_wall_seconds": round(fno_wall_seconds, 4),
        "phase_wall_seconds": round(phase_wall_seconds, 2),
        "ratio": round(ratio, 6),
    }
    extras: dict[str, Any] = {
        "phases_completed": ledger_session.get("phases_completed", []),
        "title": ledger_session.get("title", "")[:80],
        "methodology_note": (
            f"fno_wall_seconds = {call_count} calls * {median_fno_ms:.1f}ms median; "
            f"phase_wall_seconds from ledger.json duration_minutes"
        ),
    }
    return measurement, extras


def run_measurement(
    ledger_path: Path = DEFAULT_LEDGER,
    output_path: Path = DEFAULT_OUTPUT,
    n_timing_runs: int = 20,
    max_sessions: int = 8,
) -> dict[str, Any]:
    """Run the full measurement and write results.json.

    Returns the results dict (also written to output_path).
    Raises SystemExit(1) if fewer than MIN_SAMPLE_SESSIONS valid sessions found.
    """
    print(f"Loading ledger from {ledger_path} ...")
    all_sessions = load_sessions_from_ledger(ledger_path)
    print(f"  Found {len(all_sessions)} sessions with 'do' phase and >3min duration")

    # Pick a varied sample (cap at max_sessions, prefer most recent).
    # Most-recent sampling is more representative of current system state;
    # see Gemini review on PR #268.
    sampled = all_sessions[-max_sessions:]

    print(f"\nTiming fno subprocess startup ({n_timing_runs} runs) ...")
    median_ms = get_median_fno_latency_ms(n_timing_runs)
    print(f"  Median fno latency: {median_ms:.1f}ms")

    # Build session measurements. SessionData carries the canonical 5
    # fields used by compute_aggregate_ratio + parse_session_data; extras
    # carry output-only metadata (phases_completed, title,
    # methodology_note) that ride along into the JSON results but are
    # not part of the type contract.
    measurements: list[SessionData] = []
    measurements_with_extras: list[dict[str, Any]] = []
    skip_count = 0
    for ledger_session in sampled:
        result = build_session_measurement(ledger_session, median_ms)
        if result is None:
            skip_count += 1
            print(f"  SKIP {ledger_session.get('session_id', '?')[:8]}... (invalid data)")
            continue
        measurement, extras = result
        parsed = parse_session_data(measurement)
        if parsed is None:
            skip_count += 1
            print(f"  SKIP {measurement.get('session_id', '?')[:8]}... (failed validation)")
            continue
        measurements.append(parsed)
        # JSON output keeps both SessionData fields and the extras; the
        # merge happens here at the boundary, not inside the builder.
        measurements_with_extras.append({**parsed, **extras})
        print(
            f"  {parsed['session_id'][:8]}... "
            f"fno={parsed['fno_wall_seconds']:.1f}s / "
            f"phase={parsed['phase_wall_seconds']:.0f}s "
            f"ratio={parsed['ratio']:.4f}"
        )

    if len(measurements) < MIN_SAMPLE_SESSIONS:
        msg = (
            f"Insufficient sample data: found {len(measurements)} valid sessions, "
            f"need >= {MIN_SAMPLE_SESSIONS}. "
            f"Skipped {skip_count} sessions due to missing/invalid data."
        )
        print(f"\nERROR: {msg}", file=sys.stderr)
        print(
            f'<help reason="insufficient-sample-data" '
            f'evidence="found {len(measurements)} sessions, need >={MIN_SAMPLE_SESSIONS}">',
            file=sys.stderr,
        )
        sys.exit(1)

    aggregate_ratio = compute_aggregate_ratio(measurements)
    decision = apply_decision_rule(aggregate_ratio)

    results = {
        "sessions": measurements_with_extras,
        "aggregate_ratio": round(aggregate_ratio, 6),
        "decision": decision,
        "median_fno_latency_ms": round(median_ms, 2),
        "calls_per_session": TOTAL_CALLS_PER_FULL_SESSION,
        "sample_size": len(measurements),
        "skipped_count": skip_count,
        "methodology": (
            "fno_wall_seconds estimated as (calls_per_session * median_fno_latency_ms). "
            "phase_wall_seconds from ledger.json duration_minutes field. "
            "Verb call count derived from grep of skills/target/ + hooks/ for `fno ` invocations."
        ),
    }

    output_path.parent.mkdir(parents=True, exist_ok=True)
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2)
    print(f"\nResults written to {output_path}")
    print(f"Aggregate ratio: {aggregate_ratio:.4f} -> decision: {decision}")

    return results


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description="Measure fno CLI overhead in target phases")
    parser.add_argument(
        "--ledger",
        type=Path,
        default=DEFAULT_LEDGER,
        help="Path to ledger.json (default: .fno/ledger.json)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help="Path for output JSON (default: cli/benchmarks/fno_in_target_results.json)",
    )
    parser.add_argument(
        "--timing-runs",
        type=int,
        default=20,
        help="Number of timing runs per fno verb (default: 20)",
    )
    parser.add_argument(
        "--max-sessions",
        type=int,
        default=8,
        help="Maximum ledger sessions to sample (default: 8)",
    )
    args = parser.parse_args()

    run_measurement(
        ledger_path=args.ledger,
        output_path=args.output,
        n_timing_runs=args.timing_runs,
        max_sessions=args.max_sessions,
    )


if __name__ == "__main__":
    main()
