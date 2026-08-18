"""Tests for the `fno pr status` coalescing cache.

The load-bearing claim: N sessions polling one PR issue ONE network read per
TTL (a secondary limit counts request rate, so transport does not save you -
only coalescing does), and the row they share is keyed by HEAD because a
verdict is a fact about one commit. Plus the 403 discipline: a
secondary-limit failure poisons the row with an exponential backoff, and
callers inside the window get the last row DEGRADED to unknown - never its
green verdict, and never a fixed-interval retry that sustains the refusal.
"""
from __future__ import annotations

import json
import time

import pytest

from fno.pr import _cache, _rest, _status


@pytest.fixture
def cache_env(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_PR_STATUS_CACHE_DIR", str(tmp_path / "cache"))
    monkeypatch.setattr(_rest, "_repo_slug", lambda cwd=None: "owner/repo")
    # The review/coverage reads are not under test; stub them as run_status's
    # own tests do.
    monkeypatch.setattr(
        _status, "read_optional_review_state",
        lambda pr, cwd: {"optional_reviews": [], "optional_reviews_unresolved": 0},
    )
    monkeypatch.setattr(
        _status, "read_review_coverage",
        lambda pr, cwd, **kw: {"coverage": "unknown", "reviewed_count": None},
    )
    # The head read cached_status keys the row on: movable per test, countable,
    # and failable to exercise the unreadable-head path.
    head = {"sha": "a" * 40, "reads": 0, "fail": False}

    def fake_info(pr, cwd=None, runner=None, repo=None):
        head["reads"] += 1
        if head["fail"]:
            return None, "HTTP 403: You have exceeded a secondary rate limit"
        return (
            {
                "pr": int(pr),
                "state": "OPEN",
                "head_sha": head["sha"],
                "head_ref": "feature/x",
                "base_ref": "main",
                "mergeable": "MERGEABLE",
                "merged_at": None,
                "url": None,
            },
            "",
        )

    monkeypatch.setattr(_rest, "fetch_pr_info_rest", fake_info)
    return tmp_path / "cache", head


def _row_path(cache_dir, pr="42", sha="a" * 40):
    return cache_dir / f"owner--repo-{pr}-{sha[:12]}.json"


def _fetch_spy(results):
    """A _fetch stand-in replaying `results` and counting invocations."""
    calls = {"n": 0}

    def fetch(pr, cwd):
        calls["n"] += 1
        return results[min(calls["n"] - 1, len(results) - 1)]

    return fetch, calls


_GREEN = (
    {
        "state": "OPEN",
        "headRefOid": "a" * 40,
        "statusCheckRollup": [
            {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}
        ],
    },
    "",
)

_NO_CHECKS = (
    {
        "state": "OPEN",
        "headRefOid": "b" * 40,
        "statusCheckRollup": [],
    },
    "",
)


def test_ttl_hit_makes_zero_check_set_reads(cache_env, monkeypatch, capsys):
    """A TTL hit spends exactly one cheap head read and zero check-set reads:
    the coalescing claim, restated for the head-keyed row. The head read is
    the price of never serving one head's verdict for another."""
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    code1 = _cache.cached_status("42")
    assert code1 == 0
    assert calls["n"] == 1
    capsys.readouterr()
    code2 = _cache.cached_status("42")
    assert code2 == 0
    assert calls["n"] == 1, "TTL hit must not re-read the check set"
    assert head["reads"] == 2, "every call resolves the current head once"
    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "green"
    assert out["cached"] is True


def test_head_change_invalidates_the_row_inside_ttl(cache_env, monkeypatch, capsys):
    """The operator's court zero-checks fail-open: a green row cached for
    head A must never answer after a push moves the PR to head B, whose check
    set may not exist yet. A head change is a cache miss, inside the TTL."""
    cache_dir, head = cache_env
    results = [_GREEN, _NO_CHECKS]
    fetch, calls = _fetch_spy(results)
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    assert calls["n"] == 1
    # Push: the head moves inside the TTL window; head B has zero checks.
    head["sha"] = "b" * 40
    code = _cache.cached_status("42")
    assert code == 3, "an empty check set resolves to unknown, never green"
    assert calls["n"] == 2, "a moved head is a miss even inside the TTL"
    out = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert out["verdict"] == "unknown"
    assert out["settled"] is False
    assert out["ready"] is False


def test_ttl_hit_replays_the_degraded_coverage_note(cache_env, monkeypatch, capsys):
    """A cached degraded row carries its stderr note onto the serve, not only
    onto the one live read that produced it: the coalescing this module exists
    to do must not swallow the human-readable degradation reason."""
    cache_dir, head = cache_env
    monkeypatch.setattr(
        _status,
        "read_review_coverage",
        lambda pr, cwd, **kw: {
            "coverage": "unknown",
            "reviewed_count": None,
            "recompute": "recompute degraded to unknown: GraphQL quota exhausted",
        },
    )
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    capsys.readouterr()
    assert _cache.cached_status("42") == 0
    assert calls["n"] == 1, "the TTL hit serves the row without a check-set read"
    cap = capsys.readouterr()
    assert "degraded to unknown" in cap.err, cap.err


def test_ttl_expiry_rereads(cache_env, monkeypatch, capsys):
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    _cache.cached_status("42")
    # Age the row past the TTL (default 60s).
    row = json.loads(_row_path(cache_dir).read_text())
    row["ts"] = row["ts"] - 3600
    _row_path(cache_dir).write_text(json.dumps(row))
    _cache.cached_status("42")
    assert calls["n"] == 2, "an expired row must be re-read"


def test_ttl_hit_calls_review_thread_reader_once(cache_env, monkeypatch, capsys):
    """Two `cached_status` calls inside the TTL window must invoke the
    review-thread reader exactly once - the coalescing this module exists
    to do must cover the reviewThreads read, not just the CI fetch."""
    cache_dir, head = cache_env
    calls = {"n": 0}

    def counting_reader(pr, cwd):
        calls["n"] += 1
        return {"optional_reviews": [], "optional_reviews_unresolved": 0}

    monkeypatch.setattr(_status, "read_optional_review_state", counting_reader)
    fetch, _ = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)

    assert _cache.cached_status("42") == 0
    capsys.readouterr()
    assert _cache.cached_status("42") == 0
    assert calls["n"] == 1, "TTL hit must not re-invoke the review-thread reader"


def test_new_verdict_replaces_the_row(cache_env, monkeypatch, capsys):
    cache_dir, head = cache_env
    results = [_GREEN, (dict(_GREEN[0], statusCheckRollup=[
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}
    ]), "")]
    fetch, _ = _fetch_spy(results)
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    row = json.loads(_row_path(cache_dir).read_text())
    row["ts"] -= 3600
    _row_path(cache_dir).write_text(json.dumps(row))
    assert _cache.cached_status("42") == 1
    out2 = json.loads(capsys.readouterr().out.splitlines()[-1])
    assert out2["verdict"] == "red"


def test_superseded_head_row_is_pruned(cache_env, monkeypatch, capsys):
    """One row per PR: after a head change writes a fresh verdict, the old
    head's row and lock are gone, so the cache cannot grow a row per push."""
    cache_dir, head = cache_env
    results = [_GREEN, _GREEN]
    fetch, _ = _fetch_spy(results)
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    old_row, old_lock = (
        _row_path(cache_dir),
        cache_dir / "owner--repo-42-aaaaaaaaaaaa.lock",
    )
    assert old_row.exists()
    head["sha"] = "b" * 40
    assert _cache.cached_status("42") == 0
    assert not old_row.exists(), "the superseded head's row must not linger"
    assert not old_lock.exists(), "the superseded head's lock must not linger"
    assert _row_path(cache_dir, sha="b" * 40).exists()


def test_secondary_limit_failure_sets_backoff_and_serves_degraded(cache_env, monkeypatch, capsys):
    """Inside a backoff window the fresh check set is UNREADABLE, so the last
    row is served degraded: verdict unknown, settled/green/ready false, exit
    3 - the prior verdict survives only under stale_verdict. A watcher
    grepping settled:true must wait out the window, not wake on green."""
    cache_dir, head = cache_env
    err = (None, "HTTP 403: You have exceeded a secondary rate limit | back off")
    fetch, calls = _fetch_spy([_GREEN, err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    # Expire the good row, then hit the secondary limit once.
    row = json.loads(_row_path(cache_dir).read_text())
    row["ts"] -= 3600
    _row_path(cache_dir).write_text(json.dumps(row))
    assert _cache.cached_status("42") == 4
    capsys.readouterr()
    # Inside the backoff window: NO new check-set read, last row served degraded.
    assert _cache.cached_status("42") == 3
    assert calls["n"] == 2, "a backoff window must not re-read the check set"
    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "unknown"
    assert out["stale_verdict"] == "green"
    assert out["settled"] is False
    assert out["green"] is False
    assert out["ready"] is False
    assert "secondary rate limit" in out["stale_reason"]
    assert out["cached"] is True


def test_head_unreadable_serves_newest_row_degraded(cache_env, monkeypatch, capsys):
    """Head read failing (secondary window / network): fail CLOSED. The newest
    existing row is served degraded with zero network, never its green."""
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    capsys.readouterr()
    head["fail"] = True
    assert _cache.cached_status("42") == 3
    assert calls["n"] == 1, "an unreadable head must not trigger a check-set read"
    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "unknown"
    assert out["settled"] is False
    assert out["ready"] is False


def test_head_unreadable_with_no_row_goes_loud(cache_env, monkeypatch, capsys):
    """Nothing servable exists and the head cannot be resolved: the loud live
    read decides; no verdict is fabricated from disk."""
    cache_dir, head = cache_env
    head["fail"] = True
    err = (None, "could not resolve to a PullRequest")
    fetch, calls = _fetch_spy([err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 4
    assert calls["n"] == 1
    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "error"
    assert out["settled"] is False


def test_backoff_is_exponential_and_capped(cache_env, monkeypatch, capsys):
    cache_dir, head = cache_env
    monkeypatch.setattr(_cache, "_backoff_cap", lambda: 900)
    err = (None, "HTTP 403: You have exceeded a secondary rate limit | back off")
    fetch, _ = _fetch_spy([err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    for _ in range(6):
        # Forcing a fresh miss each round: expire the row and clear the
        # servable output so the backoff write path (not degraded serving) runs.
        p = _row_path(cache_dir)
        if p.exists():
            row = json.loads(p.read_text())
            row["ts"] -= 3600
            row["output"] = None
            p.write_text(json.dumps(row))
        _cache.cached_status("42")
        capsys.readouterr()
    row = json.loads(_row_path(cache_dir).read_text())
    # 2^0..2^4 minutes of failures: k capped at 8 -> 2^8*60 > 900 -> 900.
    assert row["fail_count"] == 6
    assert row["backoff_until"] - row["ts"] <= 900


def test_transient_failure_is_never_cached(cache_env, monkeypatch, capsys):
    cache_dir, head = cache_env
    err = (None, "could not resolve to a PullRequest")
    fetch, calls = _fetch_spy([err, err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 4
    assert _cache.cached_status("42") == 4
    assert calls["n"] == 2, "a transient error must reach every caller uncached"
    assert not _row_path(cache_dir).exists()
    capsys.readouterr()


def test_first_read_secondary_failure_stays_loud(cache_env, monkeypatch, capsys):
    """No prior verdict exists: the backoff row serves the ERROR row (verdict
    error, settled false), never a fabricated green."""
    cache_dir, head = cache_env
    err = (None, "HTTP 403: You have exceeded a secondary rate limit | back off")
    fetch, calls = _fetch_spy([err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 4
    capsys.readouterr()
    assert _cache.cached_status("42") == 4  # inside backoff, no re-read
    assert calls["n"] == 1
    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "error"
    assert out["settled"] is False


def test_corrupt_cached_exit_code_falls_through_to_a_live_read(cache_env, monkeypatch, capsys):
    """A row written by a different schema (a concurrent fno install) has a
    non-numeric `exit`. A crash is not an option; neither is silently serving
    the corrupt row - it must read as a miss and produce exactly one fresh
    JSON line, not the corrupt line followed by a second live one."""
    cache_dir, head = cache_env
    cache_dir.mkdir(parents=True, exist_ok=True)
    _row_path(cache_dir).write_text(json.dumps({
        "ts": time.time(),
        "exit": "bad-schema",
        "output": {"verdict": "green", "settled": True},
    }))
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    code = _cache.cached_status("42")
    assert code == 0
    assert calls["n"] == 1, "a corrupt exit code must fall through to a live read"
    lines = capsys.readouterr().out.strip().splitlines()
    assert len(lines) == 1, f"expected exactly one JSON line, got: {lines}"
    assert json.loads(lines[0])["verdict"] == "green"
