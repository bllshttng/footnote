"""Tests for the `fno do pr status` coalescing cache.

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
from fno.pr._proc import Result

# Verbatim as measured 2026-08-24T01:01:17Z during a live secondary refusal:
# GitHub's own wording contains NO "secondary", so the refusal this suite
# simulates must come through the REAL classifier against that body - a
# paraphrase containing the phrase would gate on wording this fix removes.
_VERBATIM_403 = (
    "gh: API rate limit exceeded for user ID 4994564. If you reach out to "
    "GitHub Support for help, please include the request ID "
    "FAEB:283161:6EF36:99B72:6A8B97DD ... Terms of Service (...) (HTTP 403)"
)


def _secondary_reason():
    """A refusal reason carrying the structured verdict, built the way the
    live path builds it: the verbatim 403 body classified against an exempt
    bucket that still reads healthy (core 4980/5000 - the measured shape)."""

    def runner(cmd, cwd=None, timeout=None):
        return Result(
            0,
            json.dumps(
                {
                    "resources": {
                        "core": {"remaining": 4980, "limit": 5000, "reset": 4102444800},
                        "graphql": {"remaining": 4446, "limit": 5000, "reset": 4102444800},
                    }
                }
            ),
            "",
        )

    reason = _rest._rest_reason(Result(1, "", _VERBATIM_403), runner=runner)
    assert reason.rate_limit_class == "secondary"
    return reason


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
    # and failable to exercise the unreadable-head path. `fail_reason` decides
    # whether a failure carries the structured secondary verdict.
    head = {
        "sha": "a" * 40,
        "reads": 0,
        "fail": False,
        "fail_reason": "HTTP 403: You have exceeded a secondary rate limit",
    }

    def fake_info(pr, cwd=None, runner=None, repo=None):
        head["reads"] += 1
        if head["fail"]:
            return None, head["fail_reason"]
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
    err = (None, _secondary_reason())
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


def test_ttl_hit_serves_the_human_verdict_line(cache_env, monkeypatch, capsys):
    """AC5-EDGE: the serve is the second path a reader arrives on, so it
    prints the same human line the live read prints, rendered from the
    SERVED payload - asserted against the serve output, not run_status, or
    the second path stays unproven."""
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    capsys.readouterr()
    assert _cache.cached_status("42") == 0
    assert calls["n"] == 1, "the serve under test is a TTL hit, not a live read"
    cap = capsys.readouterr()
    out = json.loads(cap.out)
    assert out["cached"] is True
    assert cap.err.split("\n", 1)[0] == _status.verdict_line(out)


def test_stale_serve_renders_the_degraded_line(cache_env, monkeypatch, capsys):
    """AC5-EDGE: a degraded serve renders unknown, unsettled and NOT-ready,
    and the blockers clause carries stale_reason - the stale arm rewrites
    `ready` without touching `ready_blockers`, so without the reason in the
    clause the line would read NOT-ready beside `no blockers`."""
    cache_dir, head = cache_env
    err = (None, _secondary_reason())
    fetch, calls = _fetch_spy([_GREEN, err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    row = json.loads(_row_path(cache_dir).read_text())
    row["ts"] -= 3600
    _row_path(cache_dir).write_text(json.dumps(row))
    assert _cache.cached_status("42") == 4
    capsys.readouterr()
    assert _cache.cached_status("42") == 3
    cap = capsys.readouterr()
    out = json.loads(cap.out)
    line = cap.err.split("\n", 1)[0]
    assert line == _status.verdict_line(out)
    assert " unknown " in line
    assert "unsettled" in line
    assert "NOT-ready" in line
    assert "stale_serve" in line
    assert "secondary rate limit" in line


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


def test_head_read_refused_by_secondary_limit_arms_the_window(
    cache_env, monkeypatch, capsys
):
    """The p0 chain: the head read is the first network call a waiter makes,
    so its refusal is the moment the window must open. Without arming here the
    head-unreadable arm serves the newest row degraded and returns BEFORE the
    locked-miss writer ever runs, so every waiter re-attempts the head read
    each tick at full rate - the fixed-interval retry that sustains a
    secondary window - while the zero-network pre-check reads a backoff_until
    nothing ever wrote."""
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    capsys.readouterr()
    head["fail"] = True
    head["fail_reason"] = _secondary_reason()
    assert _cache.cached_status("42") == 3
    row = json.loads(_row_path(cache_dir).read_text())
    assert row["backoff_until"] > time.time(), "the head refusal must arm the window"
    assert row["fail_count"] == 1
    assert row["output"]["verdict"] == "green", "the last good verdict survives"
    capsys.readouterr()
    reads_after_refusal = head["reads"]
    # Inside the window the pre-check short-circuits: the fresh row serves
    # verbatim (x-4eac semantics) and the head read itself never fires.
    assert _cache.cached_status("42") == 0
    out = json.loads(capsys.readouterr().out)
    assert out["cached"] is True
    assert head["reads"] == reads_after_refusal, "no head read inside the window"
    assert calls["n"] == 1, "the window must not re-read the check set"


def test_head_read_refused_without_the_verdict_arms_nothing(
    cache_env, monkeypatch, capsys
):
    """A head failure that is NOT a classified secondary limit (plain prose,
    no structured field) must not arm a window: gating on prose here would
    rebuild the coupling this fix removes."""
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42") == 0
    capsys.readouterr()
    head["fail"] = True  # default fail_reason is plain prose, no class
    assert _cache.cached_status("42") == 3
    row = json.loads(_row_path(cache_dir).read_text())
    assert not row.get("backoff_until"), "prose alone must not arm a window"
    # And the next tick still spends its head read (no window to ride out).
    reads_before = head["reads"]
    assert _cache.cached_status("42") == 3
    assert head["reads"] == reads_before + 1


def test_backoff_is_exponential_and_capped(cache_env, monkeypatch, capsys):
    cache_dir, head = cache_env
    monkeypatch.setattr(_cache, "_backoff_cap", lambda: 900)
    err = (None, _secondary_reason())
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
    err = (None, _secondary_reason())
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


def test_a_served_row_names_its_age_and_the_head_it_was_computed_at(
    cache_env, monkeypatch, capsys
):
    """`cached: true` alone is decorative and every consumer ignored it.

    It says the answer is second-hand but not how second-hand, so a reader
    cannot judge whether the staleness matters for their question. The served
    line must carry the age and the head the verdict was computed at.
    """
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    _cache.cached_status("42")
    capsys.readouterr()

    _cache.cached_status("42")
    out = json.loads(capsys.readouterr().out)
    assert out["cached"] is True
    assert calls["n"] == 1, "the second call must be a hit, not a live read"
    # `head` IS the computed-at head. There is deliberately no second field
    # carrying the same value under another name.
    assert out["head"] == "a" * 40
    assert "cached_head" not in out
    assert isinstance(out["cached_age_seconds"], int)
    assert out["cached_at"].endswith("Z")


def test_refresh_ignores_a_fresh_row_and_reads_live(cache_env, monkeypatch, capsys):
    """`--refresh` is the sanctioned escape from a verdict a caller distrusts.

    Before it existed the only option was raw `gh api`, which is what a king
    did on PR 994 after the cache reported red on a PR GitHub called clean.
    A refresh inside the TTL must spend a real read and print a line with no
    `cached` marker on it.
    """
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    _cache.cached_status("42")
    capsys.readouterr()

    code = _cache.cached_status("42", refresh=True)
    out = json.loads(capsys.readouterr().out)
    assert code == 0
    assert calls["n"] == 2, "--refresh must not be served from the row"
    assert "cached" not in out


def test_refresh_replaces_the_row_it_bypassed(cache_env, monkeypatch, capsys):
    """The fresh read is not thrown away: the next ordinary caller inherits it.

    A refresh that read live and then left the distrusted row on disk would
    make every sibling session re-run the escape hatch.
    """
    cache_dir, head = cache_env
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: _GREEN)
    _cache.cached_status("42")
    _row_path(cache_dir).write_text(json.dumps({
        "ts": time.time(),
        "exit": 1,
        "output": {"verdict": "red", "settled": True, "head": "a" * 40},
    }))
    capsys.readouterr()

    _cache.cached_status("42", refresh=True)
    capsys.readouterr()
    code = _cache.cached_status("42")
    out = json.loads(capsys.readouterr().out)
    assert code == 0
    assert out["verdict"] == "green"
    assert out["cached"] is True


def test_refresh_with_an_unreadable_head_goes_loud_not_stale(
    cache_env, monkeypatch, capsys
):
    """With no readable head there is no fresher answer than the live read.

    Serving a degraded row here would answer the exact question `--refresh`
    was raised to refuse, and it would do it while looking like compliance.
    """
    cache_dir, head = cache_env
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: _GREEN)
    _cache.cached_status("42")
    capsys.readouterr()

    head["fail"] = True
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    code = _cache.cached_status("42", refresh=True)
    out = json.loads(capsys.readouterr().out)
    assert calls["n"] == 1, "an unreadable head must still produce a live read"
    assert code == 0
    assert "cached" not in out
    assert "stale_reason" not in out


def test_a_refused_refresh_never_deepens_the_backoff_window(
    cache_env, monkeypatch, capsys
):
    """`--refresh` punches through a live backoff window; it must not double it.

    The degraded `unknown` serve inside a backoff window is exactly when an
    operator reaches for the escape hatch, so the collision is the modal case,
    not an edge one. Letting the refused read escalate `fail_count` walks the
    whole fleet's wait toward the 900s cap one keystroke at a time - the
    "retry that sustains the very refusal it is waiting out" this module
    exists to refuse.
    """
    cache_dir, head = cache_env
    err = (None, _secondary_reason())
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: _GREEN)
    _cache.cached_status("42")
    p = _row_path(cache_dir)
    row = json.loads(p.read_text())
    row["ts"] -= 3600  # past the TTL, so the next call is a real miss
    p.write_text(json.dumps(row))

    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: err)
    _cache.cached_status("42")
    opened = json.loads(p.read_text())
    assert opened["fail_count"] == 1
    window = opened["backoff_until"]

    capsys.readouterr()
    fetch, calls = _fetch_spy([err])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _cache.cached_status("42", refresh=True) == 4
    assert calls["n"] == 1, "the escape hatch still spends its one read"
    after = json.loads(p.read_text())
    assert after["fail_count"] == 1, "a refused refresh must not escalate"
    assert after["backoff_until"] == pytest.approx(window, abs=1.0)
    assert after["output"] == opened["output"], "the last good verdict survives"


def test_an_unknown_flag_is_refused_whatever_its_dash_count(monkeypatch):
    """The refusal must cover EVERY flag shape, not only the two-dash one.

    A guard on one of the reachable spellings reads as protection and ships
    green while the rest stay broken. Split on `--` alone, `-x` fell into
    neither the flag set nor the argument list, so it was read as the PR
    number and spent a live `gh` read - the silent drop the refusal exists to
    refuse.
    """
    monkeypatch.setattr(
        _cache, "cached_status",
        lambda *a, **k: pytest.fail("an unknown flag must never reach the cache"),
    )
    assert _status.main(["-x", "42"]) == 2
    assert _status.main(["--refesh", "42"]) == 2
    assert _status.main(["--refresh"]) == 2


def test_a_known_flag_still_parses_to_the_pr_number(cache_env, monkeypatch, capsys):
    """The refusal above must not eat the flags that do exist."""
    cache_dir, head = cache_env
    fetch, calls = _fetch_spy([_GREEN, _GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    assert _status.main(["42", "--no-cache"]) == 0
    assert _status.main(["--refresh", "42"]) == 0
    capsys.readouterr()
    assert calls["n"] == 2, "both spellings must bypass the row"


def test_a_non_finite_row_number_reads_as_a_miss_not_a_crash(cache_env, monkeypatch, capsys):
    """`json.loads` accepts a bare `Infinity`, and `float()` happily keeps it.

    So a foreign-schema row parsed clean, survived the guard, and then raised
    `OverflowError` out of `time.gmtime` - the crash `_num`'s own docstring
    promised to stop, on the path it claimed to cover. A guard that raises
    where its stated invariant reaches is decorative.
    """
    assert _cache._num({"ts": float("inf")}, "ts") == 0.0
    assert _cache._num({"ts": float("-inf")}, "ts") == 0.0
    assert _cache._num({"ts": float("nan")}, "ts") == 0.0

    cache_dir, head = cache_env
    cache_dir.mkdir(parents=True, exist_ok=True)
    # Written the way a foreign writer would: bare Infinity, which json
    # accepts by default on the way back in.
    _row_path(cache_dir).write_text(
        '{"ts": Infinity, "exit": 0, "output": {"verdict": "green", "head": "a"}}'
    )
    fetch, calls = _fetch_spy([_GREEN])
    monkeypatch.setattr(_status, "_fetch", fetch)
    # No traceback: the row is unusable, so the live read decides.
    assert _cache.cached_status("42") == 0
    assert calls["n"] == 1


def test_a_window_expiring_during_the_read_still_holds_the_refresh(
    cache_env, monkeypatch, capsys
):
    """`held` is decided from the PRE-read clock, never the post-read one.

    `run_status` can burn tens of seconds before a secondary-limit refusal and
    the shortest window is 60s. Deciding after the read let a window that
    expired mid-call flip `held` false, so the refused `--refresh` doubled the
    fleet's wait - the exact harm the comment beside it refuses.
    """
    cache_dir, head = cache_env
    err = (None, _secondary_reason())
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: _GREEN)
    _cache.cached_status("42")
    p = _row_path(cache_dir)
    row = json.loads(p.read_text())
    row["ts"] -= 3600
    p.write_text(json.dumps(row))
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: err)
    _cache.cached_status("42")
    opened = json.loads(p.read_text())
    assert opened["fail_count"] == 1
    capsys.readouterr()

    # A CONTROLLED clock, because the real one cannot be made to cross a 60s
    # window inside a test. Writing the expiry to the row mid-read does not
    # work either: `prior_until` is read from the in-memory row before the
    # call, so that version of this test passed against the unfixed code -
    # a test that cannot fail, which is the defect this file exists to catch.
    row = json.loads(p.read_text())
    t0 = row["backoff_until"] - 30  # 30s of window left when the read starts
    clock = iter([t0, t0, t0 + 120])  # pre-lock, pre-read, post-read
    last = [t0 + 120]

    def fake_clock():
        try:
            last[0] = next(clock)
        except StopIteration:
            pass
        return last[0]

    monkeypatch.setattr(_cache.time, "time", fake_clock)
    monkeypatch.setattr(_status, "_fetch", lambda pr, cwd: err)
    assert _cache.cached_status("42", refresh=True) == 4
    after = json.loads(p.read_text())
    assert after["fail_count"] == 1, "a refused refresh must not escalate"
    assert after["backoff_until"] <= row["backoff_until"], "nor extend the window"


def test_an_out_of_range_finite_ts_reads_as_a_miss_not_a_crash(cache_env, monkeypatch, capsys):
    """Finite is what the row guard promises. It is not what `time` requires.

    1e18 raises OSError and 1e300 raises OverflowError out of `time.gmtime`,
    so the non-finite fix closed one number and left the next one open. Same
    crash, one guard narrower.
    """
    assert _cache.finite_or_zero(1e18) == 1e18, "finite stays finite"
    for ts in (1e18, 1e300):
        row = {"ts": ts, "exit": 0, "output": {"verdict": "green", "head": "a"}}
        code = _cache._serve(row, stale=False)
        assert code == 0
        out = json.loads(capsys.readouterr().out)
        assert out["cached_at"] is None, "an unconvertible stamp reads as absent"
        assert out["cached_age_seconds"] is None


def test_the_row_guard_is_shared_with_every_reader_of_the_row():
    """`fno.graph.board` reads the SAME row and used to keep a bare float().

    A guard on one of two reachable paths, under a docstring claiming it
    covered both. The claim is now true because the guard is shared, not
    because the docstring was reworded.
    """
    from datetime import datetime, timezone

    from fno.graph import board

    now = datetime.now(timezone.utc)
    for bad in (float("inf"), float("nan")):
        assert board._age_from_epoch(bad, now) == "unknown age"


def test_a_future_stamp_is_not_an_age_on_either_board_reader():
    """`finite_or_zero` closes non-finite, not out-of-range.

    `1e18` is perfectly finite, so it passed that guard, went hugely negative
    against `now`, and `_format_age` floored it at zero - rendering a garbage
    stamp as "1m ago", freshly verified. The bound therefore lives in
    `_format_age`, which is the one place BOTH board readers pass through; the
    epoch reader alone is one of two reachable paths.

    The tolerance is deliberate and pinned here so it cannot be quietly
    widened: the board samples `now` once and then reads rows, so a row written
    concurrently is a second or two ahead and must still read as fresh.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph import board

    now = datetime.now(timezone.utc)

    assert board._age_from_epoch(1e18, now) == "unknown age"
    assert board._age_from_epoch(1e300, now) == "unknown age"
    # Inside the tolerance: a concurrent writer, not a garbage stamp.
    assert board._age_from_epoch(now.timestamp() + 2, now) == "1m ago"
    # Outside it: no longer explicable as clock jitter.
    assert board._age_from_epoch(now.timestamp() + 600, now) == "unknown age"
    # A real age still reads as one.
    assert board._age_from_epoch(now.timestamp() - 7200, now) == "2h ago"

    # The sibling reader shares the bound, which is the whole point of moving
    # it into _format_age.
    assert board._age_from_iso((now + timedelta(hours=3)).isoformat(), now) == "unknown age"
    assert board._age_from_iso((now + timedelta(seconds=2)).isoformat(), now) == "1m ago"
    assert board._age_from_iso((now - timedelta(hours=3)).isoformat(), now) == "3h ago"


def test_stale_serve_makes_no_failure_diagnosis(capsys, tmp_path, monkeypatch):
    """A degraded serve declares the check set unreadable; carrying the row's
    `failures` beside that verdict would assert a diagnosis the same payload
    calls unverifiable (verdict_line prints it, failures_note re-states it)."""
    monkeypatch.setenv("FNO_PR_STATUS_CACHE_DIR", str(tmp_path))
    from fno.pr import _cache

    row = {
        "ts": 1.0,
        "exit": 0,
        "output": {
            "pr": "9",
            "verdict": "red",
            "settled": True,
            "green": False,
            "head": "abc",
            "checks": {"total": 2},
            "failures": [
                {"check": "smoke", "step": "Lint", "first_error": "E1 boom"}
            ],
        },
        "fail_count": 0,
        "backoff_until": 0,
    }
    code = _cache._serve(row, stale=True)
    cap = capsys.readouterr()
    assert code == 3
    assert "unknown" in cap.out
    assert "failing:" not in cap.err
    assert "smoke failed" not in cap.err
    assert '"failures"' not in cap.out


def test_live_backoff_window_serves_degraded_with_zero_network(capsys, tmp_path, monkeypatch):
    """x-4eac: inside a secondary-rate-limit window the HEAD read is itself
    the refused call, so a waiter's tick must answer from the row without
    touching GitHub. A fresh (< TTL) row does NOT short-circuit: a push keeps
    being noticed on the next tick, exactly as before."""
    import json as _json
    import time as _time

    monkeypatch.setenv("FNO_PR_STATUS_CACHE_DIR", str(tmp_path))
    from fno.pr import _cache, _rest

    (tmp_path / "Owner--Repo-9-abc123def000.json").write_text(
        _json.dumps(
            {
                "ts": _time.time() - 120,
                "exit": 0,
                "output": {
                    "pr": "9",
                    "verdict": "green",
                    "settled": True,
                    "green": True,
                    "head": "abc123def000",
                    "checks": {"total": 1},
                },
                "fail_count": 1,
                "backoff_until": _time.time() + 300,
            }
        )
    )

    def boom(*a, **k):
        raise AssertionError("head read fired inside a live backoff window")

    monkeypatch.setattr(_rest, "fetch_pr_info_rest", boom)
    monkeypatch.setattr(_rest, "_repo_slug", lambda cwd, runner=None: "Owner/Repo")
    rc = _cache.cached_status("9")
    cap = capsys.readouterr()
    assert rc == 3
    assert "unknown" in cap.out
    assert "secondary rate limit backoff" in cap.out
