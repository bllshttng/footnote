"""TTL coalescing cache for `fno do pr status` (load-bearing).

The GraphQL quota is per-USER, and the REST SECONDARY limit counts request
rate, so N sessions polling one PR trip it no matter which transport they
use. Only collapsing those N reads into one helps. This module is that
collapse: one shared row per (repo, PR, HEAD) in a flock-protected file under
the fno do state dir, refreshed at most once per TTL by whichever session missed.
The head is part of the key because a verdict is a fact about one commit: a
row cached for head A must never answer for head B, whose check set may not
exist yet (the operator's court zero-checks fail-open).

A 403 secondary-limit failure poisons the row for a while: `backoff_until`
pushes the next real read out by 2^k * 60s (capped at 900s), and a caller
inside the window is served the last row DEGRADED to unknown/unsettled with
`stale_reason` - never its green verdict, never a fresh-looking row, and
never a silent retry that sustains the very refusal it is waiting out.
Transient (non-secondary) failures are NOT
cached: a loud error must reach every caller, not be replayed from disk.

Code defaults, deliberately not operator config: TTL 60s, backoff base 60s,
cap 900s. Env overrides exist for tests and one-off
tuning: FNO_PR_STATUS_TTL, FNO_PR_STATUS_BACKOFF_CAP, FNO_PR_STATUS_CACHE_DIR.
"""

from __future__ import annotations

import fcntl
import io
import json
import math
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


def _rows_newest_first(slug_key: str, pr: str):
    """Every cached row for (slug_key, pr), newest mtime first. No network.

    Shared by `newest_row_offline` (wants the first readable row) and
    `cached_status`'s head-unreadable arm (wants the first servable row) so
    the candidate-collection-and-sort logic lives in exactly one place.
    """
    candidates = []
    for candidate in cache_dir().glob(f"{slug_key}-{pr}-*.json"):
        try:
            candidates.append((candidate.stat().st_mtime, candidate))
        except OSError:
            continue  # a racing prune won; fewer candidates, not a crash
    for _, candidate in sorted(candidates, reverse=True):
        row = read_row(candidate.stem)
        if row is not None:
            yield row


def newest_row_offline(slug_key: str, pr: str) -> Optional[dict]:
    """The newest cached row for this PR, by mtime. No network, ever.

    Deliberately head-agnostic: the caller has no head and must not fetch
    one. The row may describe a head the PR has since moved past, so every
    caller must render it as "as of <ts>", never as the current verdict.
    """
    return next(_rows_newest_first(slug_key, pr), None)


def _write_row_locked(p: Path, row: dict) -> None:
    # Caller holds the per-key flock around read + write (flock is per-fd, so
    # re-acquiring through a helper here would self-deadlock - write inline).
    tmp = p.with_suffix(".tmp")
    tmp.write_text(json.dumps(row), encoding="utf-8")
    os.replace(tmp, p)


def finite_or_zero(value: object) -> float:
    """`value` as a finite float, 0.0 when absent, unparseable or not finite.

    Public because a cache row is read on more than one path and the guard has
    to travel with it. `fno.graph.board` reads the same row's `ts` through
    `newest_row_offline`, and while this lived as a private row-keyed helper
    that path kept a bare `float()` and crashed on the same values - a guard
    on one of two reachable paths, under a docstring claiming it covered both.

    `json.loads` accepts a bare `NaN` / `Infinity` by default, so a row
    carrying `"ts": Infinity` parses cleanly and survives `float()`.
    """
    try:
        v = float(value or 0)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return 0.0
    return v if math.isfinite(v) else 0.0


def _num(row: dict, key: str) -> float:
    """A numeric field of a cache ROW, guarded by `finite_or_zero`.

    A row written by a different schema (a concurrent fno install polling the
    same PR) must read as a miss, never crash a caller - the same discipline
    `_serve` already applies to a corrupt exit code.

    Finite is necessary and NOT sufficient, which is why callers that hand the
    result to `time` still guard the conversion. `1e18` is perfectly finite
    and still raises out of `time.gmtime`, so a value passing here can fail
    one caller and satisfy another.
    """
    return finite_or_zero(row.get(key))


def _serve(row: dict, *, stale: bool) -> int:
    """Print one cached row and return its exit code (-1 = not servable).

    The served line says WHEN and at WHAT HEAD it was computed, not merely
    that it came from a cache. `cached: true` alone is decorative: it tells a
    reader the answer is second-hand but gives no way to judge whether the
    staleness matters for their question, so every consumer ignored it. The
    row's own `head` is the head the verdict was computed at; on the
    head-unreadable path that can be a head the PR has since moved past, and
    `cached_age_seconds` is the number that says how far past.

    Deliberately NO `cached_head`: it was a verbatim copy of `head` under a
    second name, which adds no fact a reader did not have and gives the one
    fact two places to drift apart. `head` already documents itself as the
    commit this verdict describes.
    """
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
        # Fail-closed stale serve (operator's court): inside a backoff window
        # the fresh check set is UNREADABLE, so the row's green is a fact
        # about a past read, not about the head now. Degrade the served line
        # to unknown/unsettled/not-ready - a watcher grepping settled:true
        # waits out the window instead of waking on unverifiable green.
        out["stale_verdict"] = out.get("verdict")
        out["verdict"] = "unknown"
        out["green"] = False
        out["settled"] = False
        out["ready"] = False
        # The failure diagnosis goes with it: a payload that declares the
        # check set unreadable must not also print `failing:` slots and
        # per-check notes asserting a diagnosis it just called unverifiable.
        out.pop("failures", None)
        out["stale_reason"] = (
            "secondary rate limit backoff - the check set is unreadable, so "
            "this is the last cached row degraded to unknown, not a verdict"
        )
        code = 3
    out["cached"] = True
    ts = _num(row, "ts")
    # A finite `ts` can still be outside the platform's time_t range: 1e18
    # raises OSError and 1e300 raises OverflowError out of `gmtime`. Finite is
    # what the row guard can promise, and it is not what `time` requires, so
    # the conversion is guarded where it happens rather than by widening
    # `finite_or_zero` into a timestamp validator it is not.
    try:
        out["cached_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts)) if ts else None
    except (OSError, OverflowError, ValueError):
        out["cached_at"] = None
        ts = 0.0
    out["cached_age_seconds"] = int(max(0.0, time.time() - ts)) if ts else None
    # The serve is the second path a reader arrives on, so it prints the
    # same human line the live read does, from the served payload: the
    # degraded arm above rewrote the row in place, and a payload-keyed
    # renderer tells that degraded truth with no second implementation.
    # A degraded-coverage note must survive the coalescing this module
    # exists to do: without this, the note reaches only the one session
    # whose live read produced the row and none of the serves that follow.
    from fno.pr._status import coverage_recompute_note, failures_note, verdict_line

    sys.stderr.write(verdict_line(out) + "\n")
    sys.stdout.write(json.dumps(out) + "\n")
    coverage_recompute_note(out.get("review_coverage") or {})
    failures_note(out)
    return code


def cached_status(pr: str, cwd: Optional[str] = None, *, refresh: bool = False) -> int:
    """`fno do pr status` through the coalescing cache: the CLI chokepoint.

    `refresh=True` (`fno do pr status --refresh`) is the sanctioned escape: no row
    is served, the live read runs, and the fresh row replaces whatever was
    there - WHEN the head is readable. With no readable head there is no head
    to key a row on, so the live read still runs and no row is written at all;
    that arm also sits outside the per-key flock, so it is the one --refresh
    shape that does not coalesce. Before it existed a caller who distrusted a
    cached verdict had no option at all - `--help` listed none - and had to drop to raw `gh api`,
    which is what a king did on PR 994 after the cache reported red on a PR
    GitHub called clean. It is a MANUAL verb: it defeats the coalescing this
    module exists to do, so never put it in a watcher loop.

    Rows are keyed by (repo, PR, head): a verdict is a fact about ONE commit,
    and serving a green row cached for head A after a push moved the PR to
    head B answered "settled" for a head whose check set was still empty (the
    operator's court zero-checks finding). One cheap REST read buys the
    current head on every call; the expensive reads (check-runs, status,
    reviews, coverage) still collapse to one per TTL. Backoff-window callers
    get the last row DEGRADED to unknown (see `_serve`), never its verdict.
    """
    from fno.pr._rest import _repo_slug, fetch_pr_info_rest
    from fno.pr._status import run_status

    slug = _repo_slug(cwd)
    if not slug or not str(pr).strip().isdigit():
        # No local repo context (nothing to key the row on), or a non-numeric
        # PR argument (the REST reader's own contract; letting it through would
        # make the raw string a filesystem path component under cache_dir()).
        # Serve uncached rather than key every caller onto one global row.
        return run_status(pr, cwd)

    slug_key = slug.replace("/", "--")
    info, _head_reason = fetch_pr_info_rest(pr, cwd=cwd)
    if info is None:
        if refresh:
            # The caller asked for truth, not a row. With no readable head
            # there is no fresher answer than the loud live read - serving a
            # degraded row here would answer the question --refresh was
            # raised to refuse.
            return run_status(pr, cwd)
        # Head unreadable (secondary window, network): fail CLOSED. Serve the
        # PR's newest existing row degraded (unknown, unsettled - keeps the
        # zero-network collapse without ever answering green off data nobody
        # can verify); with no row at all, the loud live read decides.
        for row in _rows_newest_first(slug_key, pr):
            code = _serve(row, stale=True)
            if code >= 0:
                return code
        return run_status(pr, cwd)
    key = f"{slug_key}-{pr}-{str(info['head_sha'])[:12]}"

    def _servable(row: Optional[dict], at: float) -> int:
        """Fast-path serve: fresh row, else a row inside its backoff window.
        -1 when the caller must do (or wait on) a live read."""
        if not row:
            return -1
        if at - _num(row, "ts") < _ttl():
            return _serve(row, stale=False)
        if _num(row, "backoff_until") > at:
            return _serve(row, stale=True)
        return -1

    now = time.time()
    code = -1 if refresh else _servable(read_row(key), now)
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
            code = -1 if refresh else _servable(row, time.time())
            if code >= 0:
                return code

            # Capture the one JSON line the verb prints so the row holds
            # exactly what a caller saw (verdict, checks, coverage - all of
            # it; partial caching would let a hit serve a mixed row).
            # The clock the backoff decision uses, read BEFORE the network
            # call. `run_status` can burn tens of seconds before a
            # secondary-limit refusal, and the shortest window is 60s, so a
            # window that expires DURING the read flipped `held` false and let
            # the refused refresh double the fleet's wait - the exact harm the
            # comment below refuses.
            before_read = time.time()
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
                # A manual --refresh punches THROUGH a live backoff window (the
                # window may have cleared server-side, and the escape hatch is
                # worth the one read). Being refused by it must not DEEPEN it:
                # the fleet's wait is not the refresher's to double, and an
                # operator who retries the hatch would otherwise walk the whole
                # window to the 900s cap - the very "retry that sustains the
                # refusal it is waiting out" this module refuses to be.
                prior_until = _num(row or {}, "backoff_until")
                held = refresh and prior_until > before_read
                fails = int(_num(row or {}, "fail_count")) + (0 if held else 1)
                # Held: the window is written back VERBATIM, never
                # recomputed as `now + remaining`. The old form leaned on
                # `now + (prior_until - now)` cancelling exactly, and that
                # identity died the moment `held` moved to the pre-read
                # clock: a read spanning the expiry then pushed the window
                # PAST where it stood, extending the fleet's wait by the
                # duration of the read.
                backoff = None if held else min(2 ** min(fails - 1, 8) * 60, _backoff_cap())
                # Keep the last GOOD verdict for stale serving - its exit code
                # too, so the served JSON and the process exit never disagree;
                # with none, keep the error row itself (loud: verdict error).
                had_prior = (row or {}).get("exit") not in (4, None) and (row or {}).get("output")
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
                        "backoff_until": prior_until if backoff is None else now + backoff,
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
                    {
                        "ts": now,
                        "exit": code,
                        "output": output,
                        "fail_count": 0,
                        "backoff_until": 0,
                    },
                )
                # One row per PR: a served verdict must describe the current
                # head, so superseded heads' rows (and locks) go now, not on
                # a periodic sweep nobody would write.
                for old in p.parent.glob(f"{slug_key}-{pr}-*"):
                    if old not in (p, lock_path):
                        old.unlink(missing_ok=True)
            return code
        finally:
            fcntl.flock(lf, fcntl.LOCK_UN)
