"""TTL coalescing cache for `fno do pr status` (load-bearing).

One shared row per (repo, PR, head), flock-protected, refreshed at most once
per TTL, served degraded inside a fleet backoff. The full narrative lives in
docs/architecture/pr-status-verdict.md (`cached_status: the coalescing
chokepoint`).

Code default, deliberately not operator config: TTL 60s. Env overrides exist
for tests and one-off tuning: FNO_PR_STATUS_TTL, FNO_PR_STATUS_CACHE_DIR.
"""

from __future__ import annotations

import fcntl
import io
import json
import math
import os
import sys
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Optional

# Defaults named in the PR body: change them here, never in the
# operator's live config file.
DEFAULT_TTL_SECONDS = 60


def _open_locked_path(lock_path: Path):
    while True:
        handle = open(lock_path, "a+")
        try:
            fcntl.flock(handle, fcntl.LOCK_EX)
            try:
                opened = os.fstat(handle.fileno())
                current = lock_path.stat()
                same_inode = (opened.st_dev, opened.st_ino) == (
                    current.st_dev,
                    current.st_ino,
                )
            except OSError:
                same_inode = False
            if same_inode:
                return handle
            fcntl.flock(handle, fcntl.LOCK_UN)
        except BaseException:
            handle.close()
            raise
        handle.close()


@contextmanager
def _locked_path(lock_path: Path):
    handle = _open_locked_path(lock_path)
    try:
        yield handle
    finally:
        handle.close()


def _ttl() -> int:
    try:
        return max(1, int(os.environ.get("FNO_PR_STATUS_TTL", DEFAULT_TTL_SECONDS)))
    except ValueError:
        return DEFAULT_TTL_SECONDS


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


def _row_paths_newest(slug_key: str, pr: str) -> list[Path]:
    """Every row file for (slug_key, pr), newest mtime first. No network."""
    candidates = []
    for candidate in cache_dir().glob(f"{slug_key}-{pr}-*.json"):
        try:
            candidates.append((candidate.stat().st_mtime, candidate))
        except OSError:
            continue  # a racing prune won; fewer candidates, not a crash
    return [p for _, p in sorted(candidates, reverse=True)]


def _rows_newest_first(slug_key: str, pr: str):
    """Every cached row for (slug_key, pr), newest mtime first. No network."""
    for candidate in _row_paths_newest(slug_key, pr):
        row = read_row(candidate.stem)
        if row is not None:
            yield row


def newest_row_offline(slug_key: str, pr: str) -> Optional[dict]:
    """The newest cached row for this PR, by mtime. No network, ever.

    Deliberately head-agnostic: render it as "as of <ts>", never as the
    current verdict (docs/architecture/pr-status-verdict.md).
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

    Public because a cache row is read on more than one path and the guard
    has to travel with it (docs/architecture/pr-status-verdict.md).
    """
    try:
        v = float(value or 0)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return 0.0
    return v if math.isfinite(v) else 0.0


def _num(row: dict, key: str) -> float:
    """A numeric field of a cache ROW, guarded by `finite_or_zero`."""
    return finite_or_zero(row.get(key))


def _serve(row: dict, *, stale: bool) -> int:
    """Print one cached row and return its exit code (-1 = not servable).

    Serves verbatim or, when `stale` or the head is unverified, degraded to
    unknown/unsettled. The WHEN and AT-WHAT-HEAD contract and the guards are
    in docs/architecture/pr-status-verdict.md (`cached_status`).
    """
    out = dict(row.get("output") or {})
    if not out:
        return -1  # nothing servable landed: fall through to a live read
    exit_raw = row.get("exit")
    try:
        code = 4 if exit_raw is None else int(exit_raw)
    except (TypeError, ValueError):
        return -1  # a foreign-schema row is a miss, checked BEFORE any write
    if stale or row.get("head_unverified"):
        # Fail-closed stale serve (operator's court): unverifiable green is
        # degraded, and the failure diagnosis goes with it.
        out["stale_verdict"] = out.get("verdict")
        out["verdict"] = "unknown"
        out["green"] = False
        out["settled"] = False
        out["ready"] = False
        # The gate answers in a stale row are history, not verdicts (x-53c5):
        # replaying an old blocker beside an unreadable read names a hold the
        # world may have already released. One honest word instead.
        out["ready_blockers"] = ["status_stale"]
        out.pop("failures", None)
        out["stale_reason"] = (
            "secondary rate limit backoff - the check set is unreadable, so "
            "this is the last cached row degraded to unknown, not a verdict"
        )
        code = 3
    out["cached"] = True
    ts = _num(row, "ts")
    try:
        out["cached_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts)) if ts else None
    except (OSError, OverflowError, ValueError):
        out["cached_at"] = None
        ts = 0.0
    out["cached_age_seconds"] = int(max(0.0, time.time() - ts)) if ts else None
    # Payload-keyed renderers: the degraded arm rewrote the row in place, so
    # the serve tells that degraded truth with no second implementation, and
    # a degraded-coverage note survives the coalescing.
    from fno.pr._status import (
        coverage_recompute_note,
        failures_note,
        rerun_recovery_note,
        verdict_line,
    )

    sys.stderr.write(verdict_line(out) + "\n")
    sys.stdout.write(json.dumps(out) + "\n")
    coverage_recompute_note(out.get("review_coverage") or {})
    failures_note(out)
    rerun_recovery_note(out)
    return code


def _merge_decision_key(slug_key: str, pr: str, info: dict, cwd: Optional[str]) -> str:
    """The row key, minted by the authorized-merge owner from every fact its
    decision reads (head, PR state, hold word, live merge-slot rows, review
    evidence at the head). A hold release, a slot move, a merge, or a fresh
    attestation changes the key, so a pre-change row can never serve inside
    the TTL (x-53c5). An unreachable owner falls back to the head-only key:
    the cache keeps working, with today's staleness window and no worse."""
    from fno.rust_binary import verb_call

    try:
        out = verb_call(
            "authorized-merge",
            {
                "op": "status-cache-key",
                "cwd": cwd or os.getcwd(),
                "pr": int(pr),
                "head_sha": str(info["head_sha"]),
                "pr_state": str(info.get("state") or ""),
                "slug": slug_key,
            },
            timeout=60,
        )
        key = out.get("key") if isinstance(out, dict) else None
        if isinstance(key, str) and key:
            return key
    except Exception:  # noqa: BLE001 - degraded keying, never a wrong row
        pass
    return f"{slug_key}-{pr}-{str(info['head_sha'])[:12]}"


def cached_status(pr: str, cwd: Optional[str] = None, *, refresh: bool = False) -> int:
    """`fno do pr status` through the coalescing cache: the CLI chokepoint.

    Head-keyed rows, one read per TTL, backoff degradation, and the `--refresh`
    escape are documented in docs/architecture/pr-status-verdict.md
    (`cached_status: the coalescing chokepoint`).
    """
    from fno.pr._quota import backoff_live
    from fno.pr._rest import _repo_slug, fetch_pr_info_rest
    from fno.pr._status import run_status

    slug = _repo_slug(cwd)
    if not slug or not str(pr).strip().isdigit():
        # No repo context or a non-numeric PR: serve uncached rather than key
        # every caller onto one global row (the raw string would become a
        # filesystem path component under cache_dir()).
        return run_status(pr, cwd)

    slug_key = slug.replace("/", "--")
    if refresh:
        # A budget note must never name a probe this read did not make.
        import fno.pr._quota as _quota

        _quota.LAST_BUDGET = None
    # Backoff pre-check, zero network: inside a live refusal every waiter's
    # tick short-circuits to the newest cached row (verbatim fresh, degraded
    # stale) instead of re-attempting the held head read.
    if not refresh and backoff_live():
        for row in _rows_newest_first(slug_key, pr):
            code = _serve(row, stale=time.time() - _num(row, "ts") >= _ttl())
            if code >= 0:
                return code
            break  # newest row only
    info, _head_reason = fetch_pr_info_rest(pr, cwd=cwd)
    if info is None:
        if refresh:
            # The caller asked for truth, not a row: the loud live read.
            return run_status(pr, cwd)
        # Head unreadable (refusal, network): fail CLOSED - the newest row
        # degraded, or the loud live read when there is no row at all.
        for row in _rows_newest_first(slug_key, pr):
            code = _serve(row, stale=True)
            if code >= 0:
                return code
        return run_status(pr, cwd)
    key = _merge_decision_key(slug_key, pr, info, cwd)

    def _servable(row: Optional[dict], at: float) -> int:
        """Fast-path serve: a fresh row answers verbatim; anything staler
        than the TTL sends the caller to a live read. -1 when the caller
        must do (or wait on) one."""
        if not row:
            return -1
        if at - _num(row, "ts") < _ttl():
            return _serve(row, stale=False)
        return -1

    now = time.time()
    code = -1 if refresh else _servable(read_row(key), now)
    if code >= 0:
        return code

    # Miss: run the verb ONCE under the per-key lock; the queued pollers
    # re-read the fresh row after (docs/architecture/pr-status-verdict.md).
    lock_path = cache_dir() / (key + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    p = cache_dir() / (key + ".json")
    with _locked_path(lock_path) as lf:
        try:
            row = read_row(key)
            code = -1 if refresh else _servable(row, time.time())
            if code >= 0:
                return code

            # Capture the one JSON line the verb prints so the row holds
            # exactly what a caller saw.
            buf = io.StringIO()
            real_stdout = sys.stdout
            sys.stdout = buf
            try:
                # This HEAD's previous payload: detail and rerun facts are
                # reused within one head only (docs, `Reuse across reads`).
                code = run_status(pr, cwd, prior=(row or {}).get("output"))
            finally:
                sys.stdout = real_stdout
            line = buf.getvalue()
            sys.stdout.write(line)
            try:
                output = json.loads(line) if line.strip() else None
            except json.JSONDecodeError:
                output = None

            now = time.time()
            if code != 4 and output is not None:
                # Success only, replaced wholesale: a transient failure writes
                # nothing, and a new head sha never merges into an old verdict.
                _write_row_locked(
                    p,
                    {
                        "ts": now,
                        "exit": code,
                        "output": output,
                    },
                )
                # One row per PR: superseded heads' rows (and locks) go now.
                for old in p.parent.glob(f"{slug_key}-{pr}-*"):
                    if old not in (p, lock_path):
                        old.unlink(missing_ok=True)
            return code
        finally:
            fcntl.flock(lf, fcntl.LOCK_UN)
