#!/usr/bin/env python3
"""Backfill ledger.json + graph.json costs after the cost-accuracy fixes.

Two independent bugs multiplied to ~7.5x inflation on opus-4-8 entries
(and ~2.5-2.8x on every other model):

1. Per-line double counting (all models): Claude Code writes one transcript
   JSONL line per content block, and every line of the same API message
   repeats identical usage. session-cost.py summed usage per line until the
   (message.id, requestId) dedup landed.
2. Unknown-opus pricing fallback (3x on opus-4-8): model_tier() fell
   through to the opus-4.0 tier ($15/$75) for `claude-opus-4-8` ($5/$25
   actual). Same failure as 4.7 (see backfill-opus47-costs.py, the
   precedent for this script).

Correction strategy per ledger entry (idempotent via the `cost_backfill`
marker; entries carrying the marker are skipped on re-runs):

- Surviving transcripts (ALL of the entry's sessions resolve): recompute
  cost/tokens via the fixed parser. Corrects both bugs exactly, for every
  model. Marker: `cost_backfill: recomputed`.
- opus-4-8 entries whose transcripts are gone: divide cost_usd by 3 and
  mark `cost_backfill: pricing_only`. The /3 is exact for the pricing
  component because every line item (input, output, cache_read,
  cache_create) scales linearly between the two tiers; the dedup component
  is unknowable without the transcript, so it is NOT estimated. Known
  limitation (inherited from the 4.7 precedent): the web_search rate is
  identical between tiers, so /3 slightly over-corrects sessions that
  performed web searches ($0.01/request - negligible).
- Other transcript-less entries: skip, marker `cost_backfill:
  no_transcript`. Never guess.

Graph.json: `cost_sessions` rows are cross-referenced by session_id against
the ledger-derived correction map (recomputed -> absolute per-session cost;
pricing-only -> /3), node-level cost_usd aggregates are recomputed, and
unknown sessions are left alone (counted and printed). Graph mutations go
through fno.graph.store.locked_mutate_graph - the lock-protected,
hash-sidecar-respecting canonical write path. `session_id` fields in both
files are never modified (fno.cost._session_cost keys the per-session cost
lookup off session_id).

Concurrency contract: the ledger pass holds the same flock the register
path uses (/tmp/abilities-ledger.lock, see register-task.py), and --apply
refuses to run while live target sessions hold claims (~/.fno/claims)
unless --force is given. Atomic temp+fsync+replace writes remain the
corruption backstop either way.

Usage:
    python3 scripts/metrics/backfill-cost-recompute.py            # dry-run
    python3 scripts/metrics/backfill-cost-recompute.py --apply    # write
"""

from __future__ import annotations

import argparse
import copy
import fcntl
import json
import os
import re
import sys
import tempfile
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent

# The cost helper moved into the fno package (cli/src/fno/cost/_session_cost.py).
# Import it from the package source (this script lives in a checkout).
sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
from fno.cost import _session_cost as session_cost  # noqa: E402

PRICING_RATIO = 3.0  # opus-4.0 fallback was exactly 3x the modern opus tier
MARKER = "cost_backfill"
NODE_MARKER = "cost_backfill_recompute"
LEGACY_OPUS47_MARKER = "cost_backfilled_for_opus47"
# Same lock the register path takes (register-task.py) so a stop-hook
# append can never interleave with this read-modify-write.
LEDGER_LOCK_PATH = Path("/tmp/abilities-ledger.lock")


def is_opus_48(model: str | None) -> bool:
    if not model:
        return False
    m = model.lower()
    return "opus-4-8" in m or "opus-4.8" in m


def atomic_write_json(path: Path, data: object) -> None:
    """Serialize to a temp file in the same directory, fsync, os.replace.

    A crash mid-write leaves the original file intact rather than a
    truncated half-written target (precedent: backfill-opus47-costs.py).
    """
    serialized = json.dumps(data, indent=2)
    directory = path.parent
    directory.mkdir(parents=True, exist_ok=True)
    # try/finally spans the whole write so a failure anywhere (write, flush,
    # fsync, replace) cleans up the delete=False temp file instead of
    # leaving an orphan next to the target.
    tmp_path: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            dir=str(directory),
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
            encoding="utf-8",
        ) as tmp:
            tmp_path = tmp.name
            tmp.write(serialized)
            tmp.flush()
            os.fsync(tmp.fileno())
        os.replace(tmp_path, path)
        tmp_path = None
    finally:
        if tmp_path is not None:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass


def _pid_alive(pid: int) -> bool:
    """True when a process with this pid exists and is ours.

    Any non-zero kill(pid, 0) result - including EPERM - means "not ours":
    a recycled PID owned by another user raises PermissionError but is not
    our worker (matches the repo-wide `kill -0 pid 2>/dev/null` idiom;
    claims in ~/.fno are same-user, so a genuinely-live holder never
    hits EPERM).
    """
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError, OSError):
        return False
    return True


def live_claims(claims_dir: Path) -> list[str]:
    """Names of live claim locks (live target sessions).

    Mirrors the TTL-vs-PID dichotomy of fno.claims.core's
    _existing_is_live (which needs pydantic, unavailable to this
    stdlib-only script): a claim carrying `expires_at:` is live until it
    expires; a PID-liveness claim (the default shape target sessions
    write - no expires_at) is live while its `pid:` process exists.
    A claim with neither field is treated as live (conservative refuse;
    --force overrides).
    """
    if not claims_dir.is_dir():
        return []
    now_ms = time.time() * 1000
    live: list[str] = []
    for lock in sorted(claims_dir.glob("*.lock")):
        try:
            text = lock.read_text()
        except OSError:
            continue
        expires = re.search(r"^expires_at:\s*(\d+)\s*$", text, re.MULTILINE)
        if expires:
            if int(expires.group(1)) > now_ms:
                live.append(lock.name)
            continue
        pid_match = re.search(r"^pid:\s*(\d+)\s*$", text, re.MULTILINE)
        if pid_match is None or _pid_alive(int(pid_match.group(1))):
            live.append(lock.name)
    return live


def recompute_entry(entry: dict, entry_seen: set) -> tuple[object, dict[str, float]] | None:
    """Recompute an entry's metrics from its surviving transcripts.

    Returns (combined SessionMetrics, {session_id: cost_usd}) or None when
    any of the entry's sessions lacks a transcript (a partial recompute
    would silently drop the missing sessions' cost - never guess).
    """
    sessions = entry.get("sessions") or []
    if not sessions:
        return None
    paths = [(sid, session_cost.find_transcript(sid)) for sid in sessions]
    if any(path is None for _, path in paths):
        return None

    branch = entry.get("branch")
    all_metrics = []
    per_session: dict[str, float] = {}
    for sid, path in paths:
        m = None
        if branch:
            # Trial parse on a copy: if the branch filter matches nothing,
            # the fallback full parse must not see the trial's keys
            # (mirrors session-cost.py::backfill_tasks_json).
            trial_seen = set(entry_seen)
            branch_metrics = session_cost.parse_transcript(
                path, sid, branch_filter=branch, seen=trial_seen
            )
            if branch_metrics.assistant_messages > 0:
                m = branch_metrics
                entry_seen.update(trial_seen)
        if m is None:
            m = session_cost.parse_transcript(path, sid, seen=entry_seen)
        per_session[sid] = round(m.cost_usd, 2)
        all_metrics.append(m)

    combined = session_cost.merge_metrics(all_metrics)
    return combined, per_session


def patch_ledger(
    path: Path, apply: bool
) -> tuple[dict, dict[str, float], set[str]]:
    """Process one ledger file. Returns (stats, recompute_map, pricing_only_sids).

    recompute_map: session_id -> recomputed absolute cost (for graph rows).
    pricing_only_sids: sessions of /3-corrected opus-4-8 entries.
    """
    try:
        data = json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        print(f"Error: {path} is corrupt: {exc}", file=sys.stderr)
        sys.exit(1)
    entries = data if isinstance(data, list) else data.get("entries", [])

    stats = {
        "total": len(entries),
        "recomputed": 0,
        "pricing_only": 0,
        "no_transcript": 0,
        "already": 0,
        "old_sum": 0.0,
        "new_sum": 0.0,
        "marked": 0,
    }
    recompute_map: dict[str, float] = {}
    pricing_only_sids: set[str] = set()

    for entry in entries:
        marker = entry.get(MARKER)
        if marker:
            # Already-corrected ledger entry: leave it untouched, but still
            # REBUILD its graph-correction info. The ledger and graph passes
            # are not mutually atomic; if a prior --apply died between them,
            # a rerun that dropped marked entries from the correction map
            # could never repair the graph (the unpatched nodes would skip
            # as "unknown sessions" forever). Already-patched graph nodes
            # are protected by NODE_MARKER, so rebuilding is double-apply
            # safe.
            stats["already"] += 1
            if marker == "recomputed":
                rebuilt = recompute_entry(entry, entry_seen=set())
                if rebuilt is not None:
                    _, per_session = rebuilt
                    recompute_map.update(per_session)
                else:
                    # Transcript gone since the original recompute: fall
                    # back to the entry-level cost for the single-session
                    # case; multi-session splits are unknowable.
                    sessions = [s for s in (entry.get("sessions") or []) if s]
                    if len(sessions) == 1 and entry.get("cost_usd") is not None:
                        recompute_map[sessions[0]] = float(entry["cost_usd"])
            elif marker == "pricing_only":
                for sid in entry.get("sessions") or []:
                    if sid:
                        pricing_only_sids.add(sid)
            continue

        recomputed = recompute_entry(entry, entry_seen=set())
        if recomputed is not None:
            combined, per_session = recomputed
            old_cost = float(entry.get("cost_usd") or 0)
            new_cost = round(combined.cost_usd, 2)
            print(
                f"  ~ {entry.get('title', entry.get('session_id', 'Untitled'))}: "
                f"${old_cost:.2f} -> ${new_cost:.2f} (recomputed, "
                f"{session_cost.format_tokens(combined.total_tokens)} tokens)"
            )
            entry["cost_usd"] = new_cost
            entry["tokens_total"] = combined.total_tokens
            entry["cache_read_tokens"] = combined.cache_read_tokens
            entry["compactions"] = combined.compaction_count
            entry[MARKER] = "recomputed"
            stats["recomputed"] += 1
            stats["old_sum"] += old_cost
            stats["new_sum"] += new_cost
            recompute_map.update(per_session)
            continue

        cost = entry.get("cost_usd")
        if is_opus_48(entry.get("model")) and cost is not None and not entry.get(
            LEGACY_OPUS47_MARKER
        ):
            old_cost = float(cost)
            new_cost = round(old_cost / PRICING_RATIO, 2)
            print(
                f"  ~ {entry.get('title', 'Untitled')}: "
                f"${old_cost:.2f} -> ${new_cost:.2f} (pricing-only /3, transcript gone)"
            )
            entry["cost_usd"] = new_cost
            entry[MARKER] = "pricing_only"
            stats["pricing_only"] += 1
            stats["old_sum"] += old_cost
            stats["new_sum"] += new_cost
            for sid in entry.get("sessions") or []:
                if sid:
                    pricing_only_sids.add(sid)
            continue

        # No transcript, not /3-correctable: marker only, never guess.
        entry[MARKER] = "no_transcript"
        stats["no_transcript"] += 1

    stats["marked"] = stats["recomputed"] + stats["pricing_only"] + stats["no_transcript"]
    if apply and stats["marked"] > 0:
        atomic_write_json(path, data)
    return stats, recompute_map, pricing_only_sids


def _patch_graph_entries(
    entries: list[dict],
    recompute_map: dict[str, float],
    pricing_only_sids: set[str],
    counters: dict,
) -> list[dict]:
    """Mutator for locked_mutate_graph: correct cost_sessions rows known to
    the ledger pass, recompute node aggregates, skip unknown sessions."""
    for node in entries:
        if node.get(NODE_MARKER):
            continue
        sessions = node.get("cost_sessions") or []
        touched = False
        for cs in sessions:
            sid = cs.get("session_id")
            cost = cs.get("cost_usd")
            if cost is None:
                continue
            if sid in recompute_map:
                counters["old_sum"] += float(cost)
                cs["cost_usd"] = recompute_map[sid]
                counters["new_sum"] += cs["cost_usd"]
                touched = True
            elif sid in pricing_only_sids:
                counters["old_sum"] += float(cost)
                cs["cost_usd"] = round(float(cost) / PRICING_RATIO, 2)
                counters["new_sum"] += cs["cost_usd"]
                touched = True
            else:
                counters["skipped_unknown"] += 1
        if touched:
            node["cost_usd"] = round(
                sum(float(cs.get("cost_usd") or 0) for cs in sessions), 2
            )
            node[NODE_MARKER] = True
            counters["patched"] += 1
    return entries


def patch_graph(
    path: Path,
    apply: bool,
    recompute_map: dict[str, float],
    pricing_only_sids: set[str],
) -> dict:
    """Patch graph cost_sessions via the canonical locked write path."""
    counters = {"patched": 0, "old_sum": 0.0, "new_sum": 0.0, "skipped_unknown": 0}
    if not path.exists():
        return counters

    if not apply:
        # Dry-run: simulate against a deep copy, never write.
        try:
            raw = json.loads(path.read_text())
        except json.JSONDecodeError as exc:
            print(f"Error: {path} is corrupt: {exc}", file=sys.stderr)
            sys.exit(1)
        entries = raw if isinstance(raw, list) else raw.get("entries", [])
        _patch_graph_entries(
            copy.deepcopy(entries), recompute_map, pricing_only_sids, counters
        )
        return counters

    # locked_mutate_graph is the canonical graph write surface: flock on
    # /tmp/abilities-graph.lock, timestamped backup, SHA256 sidecar, and
    # derived-view re-render. Imported from the in-repo source tree so the
    # script works without the cli package installed system-wide.
    sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
    from fno.graph.store import locked_mutate_graph

    locked_mutate_graph(
        path,
        lambda entries: _patch_graph_entries(
            entries, recompute_map, pricing_only_sids, counters
        ),
    )
    return counters


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Recompute historical ledger/graph costs after the transcript-dedup "
            "and version-aware-pricing fixes (dry-run by default)."
        )
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Write the patched JSON back. Omit for a dry-run.",
    )
    parser.add_argument(
        "--ledger",
        action="append",
        help="Ledger path (repeatable). Defaults to ~/.fno/ledger.json "
        "and $PWD/.fno/ledger.json if present.",
    )
    parser.add_argument(
        "--graph",
        help="Graph.json path. Defaults to ~/.fno/graph.json.",
    )
    parser.add_argument(
        "--claims-dir",
        help="Claims directory checked for live target sessions. "
        "Defaults to ~/.fno/claims.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Run --apply even while live target sessions hold claims "
        "(risks racing a stop-hook ledger append).",
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
    claims_dir = (
        Path(args.claims_dir) if args.claims_dir else Path.home() / ".fno" / "claims"
    )

    if args.apply and not args.force:
        claims = live_claims(claims_dir)
        if claims:
            print(
                "Error: live target-session claims exist; a stop-hook ledger "
                "append could race this backfill. Wait for the sessions to "
                "finish or re-run with --force.\n  live claims: "
                + ", ".join(claims),
                file=sys.stderr,
            )
            return 1

    mode = "APPLY" if args.apply else "DRY-RUN (no writes)"
    print(f"=== cost recompute backfill ({mode}) ===")
    print()

    grand = {
        "total": 0,
        "recomputed": 0,
        "pricing_only": 0,
        "no_transcript": 0,
        "already": 0,
        "old_sum": 0.0,
        "new_sum": 0.0,
    }
    recompute_map: dict[str, float] = {}
    pricing_only_sids: set[str] = set()

    # Hold the register path's ledger lock across the whole read-modify-write
    # so a concurrent stop-hook append cannot be lost.
    lock_fd = os.open(str(LEDGER_LOCK_PATH), os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        for lp in ledgers:
            print(f"{lp}")
            stats, rmap, psids = patch_ledger(lp, args.apply)
            patched = stats["recomputed"] + stats["pricing_only"]
            print(f"  entries:            {stats['total']}")
            print(f"  recomputed:         {stats['recomputed']}")
            print(f"  pricing-only (/3):  {stats['pricing_only']}")
            print(f"  no-transcript:      {stats['no_transcript']} (marker only, cost unchanged)")
            print(f"  already corrected:  {stats['already']}")
            print(f"  patched this run:   {patched}")
            print(f"  cost correction:    ${stats['old_sum']:.2f} -> ${stats['new_sum']:.2f}")
            print()
            for key in ("total", "recomputed", "pricing_only", "no_transcript", "already"):
                grand[key] += stats[key]
            grand["old_sum"] += stats["old_sum"]
            grand["new_sum"] += stats["new_sum"]
            recompute_map.update(rmap)
            pricing_only_sids.update(psids)
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)

    if args.apply:
        # The ledger and graph passes are atomic individually but not with
        # respect to each other. A crash from here on leaves the ledger
        # committed and the graph pending - re-running is safe (markers
        # skip corrected ledger entries; the graph map is rebuilt).
        print("ledger pass committed; patching graph (re-run safe if interrupted)")
        print()
    graph_stats = patch_graph(graph_path, args.apply, recompute_map, pricing_only_sids)
    print(f"{graph_path}")
    print(f"  graph nodes patched:  {graph_stats['patched']}")
    if graph_stats["patched"]:
        print(
            f"  cost correction:      ${graph_stats['old_sum']:.2f} -> "
            f"${graph_stats['new_sum']:.2f}"
        )
    if graph_stats["skipped_unknown"]:
        print(
            f"  skipped (unknown sessions): {graph_stats['skipped_unknown']} "
            "(session_id not in this run's correction map)"
        )
    print()

    print("=== Summary ===")
    print(f"Ledger entries patched: {grand['recomputed'] + grand['pricing_only']} / {grand['total']}")
    print(f"  recomputed: {grand['recomputed']}  pricing-only: {grand['pricing_only']}  "
          f"no-transcript: {grand['no_transcript']}  already: {grand['already']}")
    print(f"Ledger cost correction: ${grand['old_sum']:.2f} -> ${grand['new_sum']:.2f}")
    print(f"Graph nodes patched:    {graph_stats['patched']}")
    print(f"patched this run: {grand['recomputed'] + grand['pricing_only']}")
    if not args.apply:
        print()
        print("(dry-run - no files written. Re-run with --apply to persist.)")

    return 0


if __name__ == "__main__":
    sys.exit(main())
