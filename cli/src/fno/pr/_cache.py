"""TTL coalescing cache for `fno pr status` (load-bearing).

The GraphQL quota is per-USER, and the REST SECONDARY limit counts request
rate, so N sessions polling one PR trip it no matter which transport they
use. Only collapsing those N reads into one helps. This module is that
collapse: one shared row per (repo, PR) in a flock-protected file under the
fno state dir, refreshed at most once per TTL by whichever session missed.

A 403 secondary-limit failure poisons the row for a while: `backoff_until`
pushes the next real read out by 2^k * 60s (capped at 900s), and a caller
inside the window is served the last good verdict stamped `stale_reason` -
never a fresh-looking row, and never a silent retry that sustains the very
refusal it is waiting out. Transient (non-secondary) failures are NOT
cached: a loud error must reach every caller, not be replayed from disk.

Code defaults, deliberately not operator config: TTL 60s, backoff base 60s,
cap 900s. Env overrides exist for tests and one-off
tuning: FNO_PR_STATUS_TTL, FNO_PR_STATUS_BACKOFF_CAP, FNO_PR_STATUS_CACHE_DIR.
"""
from __future__ import annotations

import fcntl
import io
import json
import os
import sys
import time
from pathlib import Path
from typing import Optional

# Defaults named in the PR body: change them here, never in the
# operator's live config file.
DEFAULT_TTL_SECONDS = 60
DEFAULT_BACKOFF_CAP_SECONDS = 900


def _ttl() -> int:
    try:
        return max(1, int(os.environ.get("FNO_PR_STATUS_TTL", DEFAULT_TTL_SECONDS)))
    except ValueError:
        return DEFAULT_TTL_SECONDS


def _backoff_cap() -> int:
    try:
        v = int(os.environ.get("FNO_PR_STATUS_BACKOFF_CAP", DEFAULT_BACKOFF_CAP_SECONDS))
    except ValueError:
        v = DEFAULT_BACKOFF_CAP_SECONDS
    return max(60, v)


def cache_dir() -> Path:
    env = os.environ.get("FNO_PR_STATUS_CACHE_DIR")
    if env:
        return Path(env)
    from fno import paths

    return paths.state_dir() / "cache" / "pr-status"


def read_row(key: str) -> Optional[dict]:
    """The cached row for `key`, or None when absent/corrupt. A corrupt row
    reads as a miss, never as an error: the network read is the truth."""
    try:
        row = json.loads((cache_dir() / (key + ".json")).read_text(encoding="utf-8"))
        return row if isinstance(row, dict) else None
    except (OSError, json.JSONDecodeError):
        return None


def _write_row_locked(p: Path, row: dict) -> None:
    # Caller holds the per-key flock around read + write (flock is per-fd, so
    # re-acquiring through a helper here would self-deadlock - write inline).
    tmp = p.with_suffix(".tmp")
    tmp.write_text(json.dumps(row), encoding="utf-8")
    os.replace(tmp, p)


def _serve(row: dict, *, stale: bool) -> int:
    out = dict(row.get("output") or {})
    if not out:
        # Nothing servable ever landed in the row (a first-read secondary
        # failure). Fall through to a live read rather than fabricate one.
        return -1
    exit_raw = row.get("exit")
    try:
        code = 4 if exit_raw is None else int(exit_raw)
    except (TypeError, ValueError):
        # A row written by a different schema (a concurrent fno install
        # polling the same PR) is corrupt, same as an unparseable file:
        # a miss, never a crash. Checked BEFORE the stdout write below, so
        # a corrupt exit code falls through to one clean live read rather
        # than a served line followed by a second, live-read line.
        return -1
    if stale:
        out["stale_reason"] = (
            "secondary rate limit backoff - serving the last cached verdict, "
            "not a fresh read"
        )
    out["cached"] = True
    sys.stdout.write(json.dumps(out) + "\n")
    # A degraded-coverage note must survive the coalescing this module exists
    # to do: without this, the note reaches only the one session whose live
    # read produced the row and none of the serves that follow it.
    from fno.pr._status import coverage_recompute_note

    coverage_recompute_note(out.get("review_coverage") or {})
    return code


def cached_status(pr: str, cwd: Optional[str] = None) -> int:
    """`fno pr status` through the coalescing cache: the CLI chokepoint.

    TTL-hit or backoff-window callers get the shared row and make zero
    network calls (which also skips the review/coverage reads, themselves
    GraphQL). Everyone else runs the real verb once and refreshes the row.
    """
    from fno.pr._rest import _repo_slug
    from fno.pr._status import run_status

    slug = _repo_slug(cwd)
    if not slug or not str(pr).strip().isdigit():
        # No local repo context (nothing to key the row on), or a non-numeric
        # PR argument (the REST reader's own contract; letting it through would
        # make the raw string a filesystem path component under cache_dir()).
        # Serve uncached rather than key every caller onto one global row.
        return run_status(pr, cwd)
    key = f"{slug.replace('/', '--')}-{pr}"

    def _servable(row: Optional[dict], at: float) -> int:
        """Fast-path serve: fresh row, else a row inside its backoff window.
        -1 when the caller must do (or wait on) a live read."""
        if not row:
            return -1
        if at - float(row.get("ts") or 0) < _ttl():
            return _serve(row, stale=False)
        if float(row.get("backoff_until") or 0) > at:
            return _serve(row, stale=True)
        return -1

    now = time.time()
    code = _servable(read_row(key), now)
    if code >= 0:
        return code

    # Miss: run the verb ONCE under the per-key lock. Synchronized pollers
    # (the watcher fleet sleeps 60s in near-lockstep, TTL is 60s, so they all
    # miss together) queue here: the winner refreshes while the losers wait,
    # then re-read the now-fresh row and serve it - the collapse the module
    # exists to deliver. Without the lock every one of them would run the
    # full read (REST + the GraphQL review reads inside run_status).
    lock_path = cache_dir() / (key + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    p = cache_dir() / (key + ".json")
    with open(lock_path, "a+") as lf:
        fcntl.flock(lf, fcntl.LOCK_EX)
        try:
            row = read_row(key)
            code = _servable(row, time.time())
            if code >= 0:
                return code

            # Capture the one JSON line the verb prints so the row holds
            # exactly what a caller saw (verdict, checks, coverage - all of
            # it; partial caching would let a hit serve a mixed row).
            buf = io.StringIO()
            real_stdout = sys.stdout
            sys.stdout = buf
            try:
                code = run_status(pr, cwd)
            finally:
                sys.stdout = real_stdout
            line = buf.getvalue()
            sys.stdout.write(line)
            try:
                output = json.loads(line) if line.strip() else None
            except json.JSONDecodeError:
                output = None

            now = time.time()
            reason = str((output or {}).get("reason", "")).lower()
            if code == 4 and output is not None and "secondary rate limit" in reason:
                fails = int((row or {}).get("fail_count") or 0) + 1
                backoff = min(2 ** min(fails - 1, 8) * 60, _backoff_cap())
                # Keep the last GOOD verdict for stale serving - its exit code
                # too, so the served JSON and the process exit never disagree;
                # with none, keep the error row itself (loud: verdict error).
                had_prior = (
                    (row or {}).get("exit") not in (4, None) and (row or {}).get("output")
                )
                _write_row_locked(
                    p,
                    {
                        # ts stays the LAST SUCCESSFUL read's stamp: a failed
                        # read must not make an old verdict look freshly
                        # verified, or the stale marker never fires inside
                        # the backoff window.
                        "ts": (row or {}).get("ts") if had_prior else now,
                        "exit": (row or {}).get("exit") if had_prior else 4,
                        "output": (row or {}).get("output") if had_prior else output,
                        "fail_count": fails,
                        "backoff_until": now + backoff,
                    },
                )
                return code

            if code != 4 and output is not None:
                # Success only: the row is replaced wholesale - a new head sha
                # never merges into an old verdict - and any backoff clears. A
                # TRANSIENT failure writes nothing, so the next caller re-reads
                # immediately instead of replaying an error from disk.
                _write_row_locked(
                    p,
                    {"ts": now, "exit": code, "output": output,
                     "fail_count": 0, "backoff_until": 0},
                )
            return code
        finally:
            fcntl.flock(lf, fcntl.LOCK_UN)
