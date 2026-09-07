#!/usr/bin/env python3
"""Backfill cost_usd in ledger.json + graph.json for opus-4.7 entries that
were priced at the old opus-4.0 fallback rate.

Root cause: `scripts/lib/cost_tracker.py` did not recognize `claude-opus-4-7`
as a model key, so `model_tier()` fell through to the opus-4.0 tier
($15 / $75 / $1.50 / $18.75 per million). Opus 4.7 is actually priced
identically to 4.6 ($5 / $25 / $0.50 / $6.25). That is exactly 3x cheaper
across every dimension, and every line item (input, output, cache_read,
cache_create) scales linearly, so any ledger entry priced via the old
fallback can be corrected by dividing `cost_usd` by 3.

Known limitation - web_search: the `web_search` rate ($0.01 per request)
is IDENTICAL between opus-4.0 and opus-4.7. A flat `/3` division slightly
over-corrects web_search cost (makes it 3x too low) for any session that
performed web searches. The per-request charge is small enough that this
is usually negligible ($0.01 per request - e.g., 10 searches under-reports
by $0.067). If exact numbers matter, re-run
`scripts/metrics/session-cost.py --json <session-id>` against the
transcript; the fixed cost_tracker will now price correctly end-to-end.

Graph.json note: graph entries do NOT store per-session model metadata.
Naively dividing every `cost_sessions.cost_usd` by 3 would corrupt any
session that used Sonnet / Haiku / older Opus. Instead, this script
cross-references with the ledger: `patch_ledger` returns the set of
session IDs it identified as opus-4.7, and `patch_graph` only touches
`cost_sessions` entries whose session_id is in that set.

Usage:
    python3 scripts/metrics/backfill-opus47-costs.py            # dry-run
    python3 scripts/metrics/backfill-opus47-costs.py --apply    # write

Safe to re-run: already-corrected entries are detected and skipped. The
idempotency check uses a `cost_backfilled_for_opus47: true` marker stored
on each patched entry.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from pathlib import Path

RATIO = 3.0  # opus-4.0 fallback was exactly 3x opus-4.7 actual pricing
MARKER = "cost_backfilled_for_opus47"


def is_opus_47(model: str | None) -> bool:
    if not model:
        return False
    m = model.lower()
    return "opus-4-7" in m or "opus-4.7" in m


def atomic_write_json(path: Path, data: object) -> None:
    """Write JSON atomically: serialize to a temp file in the same directory,
    fsync, then os.replace onto the target. A crash mid-write leaves the
    original file intact rather than a truncated half-written target.
    """
    serialized = json.dumps(data, indent=2)
    directory = path.parent
    directory.mkdir(parents=True, exist_ok=True)
    # NamedTemporaryFile(delete=False) so we can rename it after closing.
    with tempfile.NamedTemporaryFile(
        mode="w",
        dir=str(directory),
        prefix=f".{path.name}.",
        suffix=".tmp",
        delete=False,
        encoding="utf-8",
    ) as tmp:
        tmp.write(serialized)
        tmp.flush()
        os.fsync(tmp.fileno())
        tmp_path = tmp.name
    try:
        os.replace(tmp_path, path)
    except Exception:
        # Best-effort cleanup of the temp file if the replace fails.
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


def patch_ledger(path: Path, apply: bool) -> tuple[int, int, float, float, set[str]]:
    """Return (total_opus47, patched, old_sum, new_sum, opus47_session_ids).

    `opus47_session_ids` is the union of every `sessions[]` entry from ledger
    rows whose model was opus-4.7. This is what `patch_graph` uses to know
    which graph `cost_sessions` entries belong to opus-4.7 (vs. Sonnet / Haiku /
    older Opus, which must NOT be corrected).
    """
    d = json.loads(path.read_text())
    entries = d.get("entries", [])
    total = patched = 0
    old_sum = new_sum = 0.0
    opus47_sessions: set[str] = set()

    for e in entries:
        if not is_opus_47(e.get("model")):
            continue
        total += 1
        # Always track the session IDs so patch_graph can correct them even
        # on subsequent runs where this specific ledger row is already fixed.
        for sid in e.get("sessions") or []:
            if sid:
                opus47_sessions.add(sid)
        if e.get(MARKER):
            # Already corrected on a prior run. Don't include it in the
            # per-run savings math - savings reflect only THIS run's patches.
            continue
        cost = e.get("cost_usd")
        if cost is None:
            # Nothing to correct; entries that were never priced stay None.
            continue
        old_sum += float(cost)
        new_cost = round(float(cost) / RATIO, 2)
        e["cost_usd"] = new_cost
        e[MARKER] = True
        new_sum += new_cost
        patched += 1

    if apply and patched > 0:
        atomic_write_json(path, d)
    return total, patched, old_sum, new_sum, opus47_sessions


def patch_graph(
    path: Path,
    apply: bool,
    opus47_sessions: set[str],
) -> tuple[int, float, float, int]:
    """Correct graph.json nodes whose `cost_sessions` entries are known to
    belong to opus-4.7 sessions. Cross-references `opus47_sessions` (derived
    from the ledger) so non-opus-4.7 nodes are never touched.

    Uses the MARKER on the node itself for idempotency: a re-run skips nodes
    that have already been corrected, even if some of their sessions reappear
    in the set.

    Returns (patched, old_sum, new_sum, skipped_unknown_sessions).
    `skipped_unknown_sessions` counts cost_sessions rows we left alone because
    the ledger did not identify them as opus-4.7 - useful to surface for
    operator awareness.
    """
    if not path.exists():
        return 0, 0.0, 0.0, 0
    d = json.loads(path.read_text())
    entries = d.get("entries", [])
    patched = 0
    skipped_unknown = 0
    old_sum = new_sum = 0.0

    for e in entries:
        if e.get(MARKER):
            continue
        sessions = e.get("cost_sessions") or []
        touched = False
        for cs in sessions:
            cost = cs.get("cost_usd")
            sid = cs.get("session_id")
            if cost is None:
                continue
            if sid not in opus47_sessions:
                # We can't prove this session was opus-4.7 - skip to avoid
                # corrupting Sonnet / Haiku / older Opus entries that are
                # already correctly priced.
                skipped_unknown += 1
                continue
            old_sum += float(cost)
            cs["cost_usd"] = round(float(cost) / RATIO, 2)
            new_sum += cs["cost_usd"]
            touched = True
        if touched:
            # Recompute the aggregate cost_usd from the corrected sessions.
            e["cost_usd"] = round(sum(cs.get("cost_usd") or 0 for cs in sessions), 2)
            e[MARKER] = True
            patched += 1

    if apply and patched > 0:
        atomic_write_json(path, d)
    return patched, old_sum, new_sum, skipped_unknown


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Write the patched JSON back. Omit for a dry-run.",
    )
    parser.add_argument(
        "--ledger",
        action="append",
        help="Additional ledger path (repeatable). Defaults to ~/.fno/ledger.json "
        "and $PWD/.fno/ledger.json if present.",
    )
    parser.add_argument(
        "--graph",
        help="Graph.json path. Defaults to ~/.fno/graph.json.",
    )
    args = parser.parse_args()

    ledgers: list[Path] = []
    if args.ledger:
        ledgers.extend(Path(p) for p in args.ledger)
    else:
        home = Path.home() / ".fno" / "ledger.json"
        cwd = Path.cwd() / ".fno" / "ledger.json"
        for p in (home, cwd):
            if p.exists() and p not in ledgers:
                ledgers.append(p)

    graph_path = Path(args.graph) if args.graph else Path.home() / ".fno" / "graph.json"

    mode = "APPLY" if args.apply else "DRY-RUN (no writes)"
    print(f"=== opus-4.7 cost backfill ({mode}) ===")
    print()

    grand_total_entries = grand_patched = 0
    grand_old = grand_new = 0.0
    all_opus47_sessions: set[str] = set()

    for lp in ledgers:
        total, patched, old_sum, new_sum, sessions = patch_ledger(lp, args.apply)
        print(f"{lp}")
        print(f"  opus-4.7 entries: {total}")
        print(f"  patched this run: {patched}")
        print(f"  old cost total:   ${old_sum:.2f}")
        print(f"  new cost total:   ${new_sum:.2f}  (savings: ${old_sum - new_sum:.2f})")
        print()
        grand_total_entries += total
        grand_patched += patched
        grand_old += old_sum
        grand_new += new_sum
        all_opus47_sessions.update(sessions)

    graph_patched, graph_old, graph_new, graph_skipped = patch_graph(
        graph_path, args.apply, all_opus47_sessions
    )
    print(f"{graph_path}")
    print(f"  graph nodes patched:       {graph_patched}")
    print(f"  sessions known as opus-4.7: {len(all_opus47_sessions)}")
    if graph_patched:
        print(f"  old cost total:            ${graph_old:.2f}")
        print(f"  new cost total:            ${graph_new:.2f}  (savings: ${graph_old - graph_new:.2f})")
    if graph_skipped:
        print(
            f"  skipped (unknown-model sessions): {graph_skipped} "
            "(session_id not in ledger's opus-4.7 set)"
        )
    print()

    print("=== Summary ===")
    print(f"Ledger entries patched: {grand_patched} / {grand_total_entries}")
    print(f"Ledger cost correction: ${grand_old:.2f} -> ${grand_new:.2f}")
    print(f"Graph nodes patched:    {graph_patched}")
    if not args.apply:
        print()
        print("(dry-run — no files written. Re-run with --apply to persist.)")

    return 0


if __name__ == "__main__":
    sys.exit(main())
