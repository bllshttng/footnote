"""fno whoami cost - ledger + budget integration.

Runs the in-package _session_cost module (the former
scripts/metrics/session-cost.py) via `python3 -m fno.cost._session_cost`.
Falls back to direct JSON mutation when the subprocess fails (test environment
/ offline / unsupported flags).

Public API:
  update(session_id, tokens, cost_usd, *, ledger_path, graph_path, node_id,
         provider_id, account_id)
  check_budget(total_cost_usd, budget_cap_usd, estimated_phase_cost) -> bool
"""
from __future__ import annotations

import dataclasses
import json
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

from fno.events import append_event


# ---- update ----

def update(
    session_id: str,
    tokens: int,
    cost_usd: float,
    *,
    ledger_path: Optional[Path] = None,
    graph_path: Optional[Path] = None,
    node_id: Optional[str] = None,
    provider_id: Optional[str] = None,
    account_id: Optional[str] = None,
) -> dict[str, Any]:
    """Append a cost record to ledger.json and optionally to a graph node.

    First runs the in-package _session_cost module via `python3 -m
    fno.cost._session_cost`. Falls back to direct JSON mutation so tests and
    offline environments work.

    Args:
        session_id: The fno session ID.
        tokens: Token count for this session.
        cost_usd: Cost in USD for this session.
        ledger_path: Explicit path to ledger.json (default: ~/.fno/ledger.json).
        graph_path: Path to graph.json (optional, for node cost update).
        node_id: Graph node ID to append cost_sessions to (optional).
        provider_id: ID of the provider this cost is attributed to. When None,
            the entry is "untagged" (back-compat with pre-substrate sessions).
            The key is omitted entirely from the entry (not written as null).
        account_id: Human-friendly account label for the provider. When None,
            the key is omitted entirely from the entry (not written as null).

    Returns:
        {"ok": True, "ledger_path": str, "entry": dict}
    """
    if ledger_path is None:
        from fno import paths as _paths
        ledger_path = _paths.ledger_json()
    ledger_path = Path(ledger_path)
    ledger_path.parent.mkdir(parents=True, exist_ok=True)

    # Build the entry
    entry: dict[str, Any] = {
        "session_id": session_id,
        "tokens": tokens,
        "cost_usd": round(float(cost_usd), 4),
        "timestamp": datetime.now(timezone.utc).isoformat(),
    }
    if provider_id is not None:
        entry["provider_id"] = provider_id
    if account_id is not None:
        entry["account_id"] = account_id

    # Try the in-package _session_cost module first (run via `python3 -m`, so
    # it resolves from the installed wheel with no repo on disk - the whole
    # point of the move). The same interpreter that runs `fno` can always
    # import fno.cost._session_cost, so there is no path/existence gate.
    used_script = False

    # Track exec-level OSError separately from non-zero returncode so the
    # forensic event can distinguish "could not start sys.executable" from
    # "the module ran and failed". Wrapping subprocess.run itself is
    # load-bearing: without it, an OSError (PermissionError, FileNotFoundError
    # on a custom interpreter path, text-file-busy, etc.) propagates out of
    # update() and the direct-JSON ledger fallback never runs.
    script_exec_error: OSError | None = None
    result = None
    try:
        result = subprocess.run(
            [sys.executable, "-m", "fno.cost._session_cost",
             "--session-id", session_id,
             "--tokens", str(tokens),
             "--cost-usd", str(cost_usd),
             "--ledger", str(ledger_path)],
            capture_output=True,
            text=True,
        )
    except OSError as exc:
        # FileNotFoundError is a subclass of OSError; covers both.
        script_exec_error = exc
    else:
        if result.returncode == 0:
            used_script = True

    if not used_script:
        # Surface subprocess failure before the direct-JSON fallback write.
        # `result.returncode != 0` already covers signal-killed (negative)
        # cases per memory feedback_python_subprocess_negative_returncode.
        subprocess_failed = script_exec_error is not None or (result is not None and result.returncode != 0)
        stderr_text: str = ""
        # Stable reference for code paths that use returncode below: synthesize
        # -1 for the exec-failure branch and use the real value for the
        # ran-then-failed branch.
        effective_returncode = -1 if script_exec_error is not None else (result.returncode if result is not None else 0)
        if subprocess_failed:
            raw_stderr_text = str(script_exec_error) if script_exec_error is not None else (result.stderr if result is not None else "") or ""
            _MAX_STDERR_BYTES = 4096
            _TRUNCATION_SUFFIX = "[...truncated]"
            stderr_bytes = raw_stderr_text.encode("utf-8")
            if len(stderr_bytes) > _MAX_STDERR_BYTES:
                # Reserve suffix bytes from the budget so the total payload
                # (snippet + suffix) stays inside the documented 4096-byte
                # cap. Decode with errors="ignore" rather than "replace":
                # "replace" inserts U+FFFD (3 bytes encoded) for partial
                # codepoints and can push the re-encoded result back over the
                # cap when the slice lands mid-codepoint.
                budget = _MAX_STDERR_BYTES - len(_TRUNCATION_SUFFIX.encode("utf-8"))
                stderr_text = stderr_bytes[:budget].decode("utf-8", errors="ignore") + _TRUNCATION_SUFFIX
            else:
                stderr_text = raw_stderr_text
            if stderr_text:
                print(
                    f"cost.py: subprocess failed: returncode={effective_returncode} stderr={stderr_text}",
                    file=sys.stderr,
                )

        # Direct JSON mutation fallback. Capture the outcome so the event's
        # `fallback_succeeded` field reflects what actually happened (the
        # schema description treats it as observable, not a constant).
        fallback_error: Exception | None = None
        try:
            _append_to_ledger(ledger_path, entry)
        except Exception as exc:  # noqa: BLE001
            fallback_error = exc

        if subprocess_failed:
            event_data: dict[str, Any] = {
                "session_id": session_id,
                "returncode": effective_returncode,
                "stderr_snippet": stderr_text,
                "fallback_succeeded": fallback_error is None,
                "cost_usd": entry["cost_usd"],
                "tokens": tokens,
            }
            if provider_id is not None:
                event_data["provider_id"] = provider_id
            if account_id is not None:
                event_data["account_id"] = account_id

            try:
                from fno.events import _build
                event = _build("cost_subprocess_failed", "fno-loop", event_data)
                append_event(event)
            except Exception as exc:  # noqa: BLE001
                print(
                    f"cost.py: event-emit failed (cost_subprocess_failed): {exc}",
                    file=sys.stderr,
                )

        if fallback_error is not None:
            raise fallback_error

    # Update graph node if provided. `graph_updated` is None when no node was
    # named; False means the ledger row landed but the node did not get it, a
    # difference the caller could not previously see.
    graph_updated = None
    if graph_path and node_id:
        graph_updated = _update_graph_node(Path(graph_path), node_id, session_id, cost_usd)

    return {
        "ok": True,
        "ledger_path": str(ledger_path),
        "entry": entry,
        "graph_updated": graph_updated,
    }


def _append_to_ledger(ledger_path: Path, entry: dict[str, Any]) -> None:
    """Append entry to ledger.json atomically."""
    from filelock import FileLock
    import tempfile
    import os

    lock_path = str(ledger_path) + ".lock"
    with FileLock(lock_path, timeout=10):
        if ledger_path.exists():
            try:
                raw = json.loads(ledger_path.read_text(encoding="utf-8"))
                # Tolerate both shapes:
                #   - bare list: written by pre-fix cost.py
                #   - {"entries": [...]} dict: written by register-task.py and
                #     canonical post-fix cost.py
                if isinstance(raw, list):
                    entries = raw
                elif isinstance(raw, dict):
                    entries = raw.get("entries", [])
                    if not isinstance(entries, list):
                        entries = []
                else:
                    entries = []
            except json.JSONDecodeError:
                # Preserve corrupt file for forensics BEFORE starting fresh.
                # Without this, the next write silently truncates the entire
                # history to a single row - catastrophic data loss pattern.
                import shutil
                import sys
                backup = ledger_path.with_suffix(
                    ledger_path.suffix + f".corrupt-{int(time.time())}"
                )
                try:
                    shutil.copy2(ledger_path, backup)
                    print(
                        f"cost._append_to_ledger: corrupt ledger backed up to "
                        f"{backup}; starting fresh ledger. Inspect backup to "
                        f"recover lost entries.",
                        file=sys.stderr,
                    )
                except OSError as e:
                    print(
                        f"cost._append_to_ledger: ledger corrupt AND backup "
                        f"failed ({e}); starting fresh ledger WITHOUT backup. "
                        f"History is lost.",
                        file=sys.stderr,
                    )
                entries = []
        else:
            entries = []

        entries.append(entry)
        # Always write canonical dict shape so register-task.py and
        # session-cost.py (which use data.get("entries", [])) can read it.
        content = json.dumps({"entries": entries}, indent=2)

        with tempfile.NamedTemporaryFile(
            mode="w", dir=ledger_path.parent,
            prefix=f".{ledger_path.name}.", suffix=".tmp",
            delete=False, encoding="utf-8",
        ) as tmp:
            tmp.write(content)
            tmp_path = Path(tmp.name)
        os.replace(tmp_path, ledger_path)


_COST_NDIGITS = 4


def _as_cost(value: Any) -> float:
    """Coerce a stored cost to a float; junk reads as 0.0 rather than raising.

    Costs come off disk, so bool/None/str all reach the sum. `graph/triage.py`
    excludes bools for the same reason: `float(True)` is a silent 1.0.
    """
    if value is None or isinstance(value, bool):
        return 0.0
    try:
        return float(value)
    except (TypeError, ValueError):
        return 0.0


def upsert_cost_session(node: dict[str, Any], session_id: str, cost_usd: float) -> None:
    """Record one session's cost on a graph node, replacing any prior row.

    A session's cost is a LEVEL that gets re-reported (a resumed run, a second
    `fno whoami cost` call for the same session), not an increment to accumulate.
    Appending and re-summing double-counts it, so the row for `session_id` is
    replaced in place when it already exists. `cost_usd` is then re-derived
    from the rows, never incremented.

    Rounding is fixed for every writer. A per-caller precision made one node's
    total depend on which surface wrote it last; a receipt wanting cents formats
    its own output instead of changing what is stored.

    Legacy and hand-edited graphs carry a non-list `cost_sessions`, non-dict
    rows, and string costs - `graph/triage.py` defends against those same shapes
    on the read side. Drop what cannot be a cost row rather than raising: this
    runs at session finalize, where an exception loses the attribution entirely.
    """
    rows = node.get("cost_sessions")
    rows = [r for r in rows if isinstance(r, dict)] if isinstance(rows, list) else []
    row = {
        "session_id": session_id,
        "cost_usd": round(_as_cost(cost_usd), _COST_NDIGITS),
        "timestamp": datetime.now(timezone.utc).isoformat(),
    }
    for existing in rows:
        if existing.get("session_id") == session_id:
            existing.update(row)
            break
    else:
        rows.append(row)
    node["cost_sessions"] = rows
    node["cost_usd"] = round(sum(_as_cost(r.get("cost_usd")) for r in rows), _COST_NDIGITS)


def _update_graph_node(graph_path: Path, node_id: str, session_id: str, cost_usd: float) -> bool:
    """Record this session's cost on the graph node (upsert, not append).

    Returns True when the cost landed on the node, False when it was skipped.
    Cost attribution is best-effort and must never abort the caller, so every
    failure degrades to stderr plus a False rather than an exception.

    Goes through locked_mutate_graph rather than writing graph.json directly.
    A hand-rolled writer here took a DIFFERENT lockfile than the canonical one
    (which resolves symlinks first, and footnote's worktree setup symlinks
    `.fno/`), so the two writers did not mutually exclude; it also rewrote a
    legacy root-list graph.json as an empty `{"entries": []}` and left the
    sha256 sidecar stale, which made the next reader raise and every later cost
    attribution silently skip.
    """
    from fno.graph.store import locked_mutate_graph

    if not graph_path.exists():
        # The likeliest cause is a worktree whose `.fno/` symlink was never
        # healed by setup-worktree.sh, so say it rather than skipping in silence.
        print(
            f"cost._update_graph_node: no graph at {graph_path}; "
            "cost not attributed",
            file=sys.stderr,
        )
        return False

    found = False

    def mutator(entries):
        nonlocal found
        for node in entries:
            if isinstance(node, dict) and node.get("id") == node_id:
                upsert_cost_session(node, session_id, cost_usd)
                found = True
                break
        return entries

    try:
        locked_mutate_graph(graph_path, mutator)
    except SystemExit:
        # locked_mutate_graph exits on an unreadable graph. That is right for a
        # backlog command and wrong here: a corrupt graph must not take down the
        # session whose cost we are recording. It has already explained itself
        # on stderr, and it leaves the file untouched.
        print(
            f"cost._update_graph_node: graph unreadable at {graph_path}; "
            "cost attribution skipped for this session",
            file=sys.stderr,
        )
        return False
    except Exception as exc:
        # locked_mutate_graph does far more than the mutator after it returns
        # (backup, atomic write, sidecar, md render, status recompute), and any
        # of it can raise on a full disk or a read-only .fno. "Best-effort" has
        # to mean every failure, or the promise above is one the code breaks.
        print(
            f"cost._update_graph_node: {type(exc).__name__} writing {graph_path}: {exc}; "
            "cost attribution skipped for this session",
            file=sys.stderr,
        )
        return False

    if not found:
        print(
            f"cost._update_graph_node: node {node_id} not found in {graph_path}; "
            "cost not attributed",
            file=sys.stderr,
        )
    return found


# ---- check_budget ----

def check_budget(
    *,
    total_cost_usd: float,
    budget_cap_usd: Optional[float],
    estimated_phase_cost: float = 0.0,
) -> bool:
    """Return True if running the next phase would exceed the budget cap.

    The check fires BEFORE the phase runs (not after), per spec.

    Args:
        total_cost_usd: Accumulated cost so far.
        budget_cap_usd: Cap in USD. None means no cap.
        estimated_phase_cost: Estimated cost of the upcoming phase.

    Returns:
        True if (total + estimated) >= cap (should pause).
        False if no cap set or within budget.
    """
    if budget_cap_usd is None:
        return False
    return float(total_cost_usd) + float(estimated_phase_cost) >= float(budget_cap_usd)


# ---- per-turn attribution (failover phase 02) ----

def compute_per_turn_attribution(
    *,
    sidecar_path: Path,
) -> dict[str, dict[str, int]]:
    """Read the per-turn attribution sidecar and return a per-provider rollup.

    Phase 02 of provider rotation failover (ab-9728b70b). Cost callers
    that previously assumed one provider per session can now ask "how
    many turns did each provider produce in this session?" without
    importing turn_attribution directly.

    The math that converts these counts to per-provider USD is deferred
    to Spec 2.5 (rate-card × token math); v0 only surfaces the breakdown.

    Returns an empty dict for legacy sessions (sidecar missing) so
    callers can fall back to the active-at-compute attribution that
    cost.py.update already supports.
    """
    from fno.turn_attribution import summarize_per_provider

    return summarize_per_provider(sidecar_path=sidecar_path)


# ---- per-provider sub-cap (failover phase 03 task 3.2) ----

def compute_per_provider_cost(
    *,
    total_session_cost_usd: float,
    sidecar_path: Path,
) -> dict[str, float]:
    """Approximate per-provider cost for one session.

    v0 math: session_cost × (turns_on_provider / total_turns).
    Per-segment math (rate × tokens per segment) is Spec 2.5; v0's job
    is to bound damage with a "cheap and approximate" sub-cap, not to
    produce penny-perfect attribution.

    Returns an empty dict for legacy sessions (sidecar missing or
    empty) so callers can fall back to no per-provider check.
    """
    from fno.turn_attribution import summarize_per_provider

    summary = summarize_per_provider(sidecar_path=sidecar_path)
    if not summary:
        return {}
    total_turns = sum(s["turns"] for s in summary.values())
    if total_turns <= 0:
        return {}
    return {
        provider_id: float(total_session_cost_usd) * (s["turns"] / total_turns)
        for provider_id, s in summary.items()
    }


@dataclasses.dataclass(frozen=True)
class PerProviderCapResult:
    """Outcome of ``check_per_provider_caps``.

    The first provider to exceed its cap (sorted by provider id for
    determinism in mixed-trip cases) is reported as ``tripped_provider``.
    The session-level cap fires before this check in the stop hook;
    when both trip, session wins per spec EDGE1.
    """

    tripped: bool
    tripped_provider: str | None = None
    tripped_amount_usd: float | None = None
    tripped_cap_usd: float | None = None


def check_per_provider_caps(
    *,
    per_provider_cost: dict[str, float],
    caps_by_provider: dict[str, float],
) -> PerProviderCapResult:
    """Check each provider's accumulated cost against its sub-cap.

    Args:
        per_provider_cost: ``{provider_id: usd}`` from
            ``compute_per_provider_cost``.
        caps_by_provider: ``{provider_id: cap_usd}``. Providers absent
            from this dict have no per-provider cap (only the session
            cap applies). Set ``cost_cap_usd_per_session`` on a provider
            record in settings.yaml to populate this.

    Returns:
        ``PerProviderCapResult``. ``tripped`` iff any provider's cost
        is at-or-above its cap.
    """
    # Deterministic order so two simultaneously-tripping providers don't
    # produce flapping reports across runs.
    for provider_id in sorted(caps_by_provider.keys()):
        cap = caps_by_provider[provider_id]
        spent = per_provider_cost.get(provider_id, 0.0)
        if spent >= cap:
            return PerProviderCapResult(
                tripped=True,
                tripped_provider=provider_id,
                tripped_amount_usd=spent,
                tripped_cap_usd=cap,
            )
    return PerProviderCapResult(tripped=False)
