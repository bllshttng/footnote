"""Tests for the `fno pr status` coalescing cache.

The load-bearing claim: N sessions polling one PR issue ONE network read per
TTL (a secondary limit counts request rate, so transport does not save you -
only coalescing does). Plus the 403 discipline: a secondary-limit failure
poisons the row with an exponential backoff and callers inside the window get
the last verdict stamped `stale_reason`, never a fresh-looking row and never
a fixed-interval retry that sustains the refusal.
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
    return tmp_path / "cache"


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
        "statusCheckRollup": [
            {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}
        ],
    },
    "",
)


def test_ttl_hit_makes_zero_network_calls(cache_env, monkeypatch, capsys):
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    code1 = _cache.cached_status("42")
    assert code1 == 0
    assert calls["n"] == 1
    capsys.readouterr()
    code2 = _cache.cached_status("42")
    assert code2 == 0
    assert calls["n"] == 1, "TTL hit must make no network call"
    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "green"
    assert out["cached"] is True


def test_ttl_hit_replays_the_degraded_coverage_note(cache_env, monkeypatch, capsys):
    """A cached degraded row carries its stderr note onto the serve, not only
    onto the one live read that produced it: the coalescing this module exists
    to do must not swallow the human-readable degradation reason."""
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
    assert calls["n"] == 1, "the TTL hit serves the row without a network call"
    cap = capsys.readouterr()
    assert "degraded to unknown" in cap.err, cap.err


def test_ttl_expiry_rereads(cache_env, monkeypatch, capsys):
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    _cache.cached_status("42")
    # Age the row past the TTL (default 60s).
    row = json.loads((cache_env / "owner--repo-42.json").read_text())
    row["ts"] = row["ts"] - 3600
    (cache_env / "owner--repo-42.json").write_text(json.dumps(row))
    _cache.cached_status("42")
    assert calls["n"] == 2, "an expired row must be re-read"


def test_new_verdict_replaces_the_row(cache_env, monkeypatch, capsys):
    results = [_GREEN, (dict(_GREEN[0], statusCheckRollup=[
        {"name": "ci", "status": "COMPLETED", "conclusion": "FAILURE"}
    ]), "")]
    fetch, _ = _fetch_spy(results)
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    row = json.loads((cache_env / "owner--repo-42.json").read_text())
    row["ts"] -= 3600
    (cache_env / "owner--repo-42.json").write_text(json.dumps(row))
    assert _cache.cached_status("42") == 1
    out2 = json.loads(capsys.readouterr().out.splitlines()[-1])
    assert out2["verdict"] == "red"


def test_secondary_limit_failure_sets_backoff_and_serves_stale(cache_env, monkeypatch, capsys):
    err = (None, "HTTP 403: You have exceeded a secondary rate limit | back off")
    fetch, calls = _fetch_spy([_GREEN, err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    # Expire the good row, then hit the secondary limit once.
    row = json.loads((cache_env / "owner--repo-42.json").read_text())
    row["ts"] -= 3600
    (cache_env / "owner--repo-42.json").write_text(json.dumps(row))
    assert _cache.cached_status("42") == 4
    capsys.readouterr()
    # Inside the backoff window: NO new read, last verdict served + stamped.
    assert _cache.cached_status("42") == 0
    assert calls["n"] == 2, "a backoff window must not re-read"
    out = json.loads(capsys.readouterr().out)
    assert out["verdict"] == "green"
    assert "stale_reason" in out and "secondary rate limit" in out["stale_reason"]
    assert out["cached"] is True


def test_backoff_is_exponential_and_capped(cache_env, monkeypatch, capsys):
    monkeypatch.setattr(_cache, "_backoff_cap", lambda: 900)
    err = (None, "HTTP 403: You have exceeded a secondary rate limit | back off")
    fetch, _ = _fetch_spy([err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    for _ in range(6):
        # Forcing a fresh miss each round: expire the row and clear the
        # servable output so the backoff write path (not stale serving) runs.
        p = cache_env / "owner--repo-42.json"
        if p.exists():
            row = json.loads(p.read_text())
            row["ts"] -= 3600
            row["output"] = None
            p.write_text(json.dumps(row))
        _cache.cached_status("42")
        capsys.readouterr()
    row = json.loads((cache_env / "owner--repo-42.json").read_text())
    # 2^0..2^4 minutes of failures: k capped at 8 -> 2^8*60 > 900 -> 900.
    assert row["fail_count"] == 6
    assert row["backoff_until"] - row["ts"] <= 900


def test_transient_failure_is_never_cached(cache_env, monkeypatch, capsys):
    err = (None, "could not resolve to a PullRequest")
    fetch, calls = _fetch_spy([err, err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 4
    assert _cache.cached_status("42") == 4
    assert calls["n"] == 2, "a transient error must reach every caller uncached"
    assert not (cache_env / "owner--repo-42.json").exists()
    capsys.readouterr()


def test_first_read_secondary_failure_stays_loud(cache_env, monkeypatch, capsys):
    """No prior verdict exists: the backoff row serves the ERROR row (verdict
    error, settled false), never a fabricated green."""
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
    cache_env.mkdir(parents=True, exist_ok=True)
    (cache_env / "owner--repo-42.json").write_text(json.dumps({
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
