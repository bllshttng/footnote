"""`fno do pr review-hold`: the verb behind the hold, for callers outside Python.

`check` carries the same exit contract as `coverage-check` - 0 clear, 3 held
with the refusal on stderr, 4 a NAMED instrument failure - because the callers
that cannot import fno read the two the same way.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.pr import _review_hold
from fno.pr.cli import pr_app


runner = CliRunner()


@pytest.fixture(autouse=True)
def _claims_in_tmp(monkeypatch, tmp_path):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))


def _invoke(*args):
    return runner.invoke(pr_app, ["review-hold", *args])


def test_check_is_clear_when_nothing_is_running(monkeypatch, tmp_path):
    monkeypatch.setattr(
        "fno.pr._merge._pr_head_ref_and_oid", lambda pr, repo: ("feature/x-a089", "abc123", "OPEN")
    )
    monkeypatch.setattr(
        _review_hold, "review_hold_refusal", lambda *a, **kw: None
    )
    result = _invoke("check", "42", "--repo", str(tmp_path))
    assert result.exit_code == 0
    assert "no review in flight on feature/x-a089" in result.stdout


def test_check_exits_3_while_a_review_is_running(monkeypatch, tmp_path):
    monkeypatch.setattr(
        "fno.pr._merge._pr_head_ref_and_oid", lambda pr, repo: ("feature/x-a089", "abc123", "OPEN")
    )
    monkeypatch.setattr(
        _review_hold,
        "review_hold_refusal",
        lambda *a, **kw: "review_in_flight: held by reviewer:sess-1",
    )
    result = _invoke("check", "42", "--repo", str(tmp_path))
    assert result.exit_code == 3


def test_check_exits_4_when_the_probe_itself_could_not_run(monkeypatch, tmp_path):
    """Exit 4 is a dead instrument, never a verdict. A caller reading exit 3 as
    "held" must not also read a failed head lookup as one."""
    monkeypatch.setattr("fno.pr._merge._pr_head_ref_and_oid", lambda pr, repo: None)
    result = _invoke("check", "42", "--repo", str(tmp_path))
    assert result.exit_code == 4


def test_check_without_a_pr_number_is_a_usage_error(tmp_path):
    assert _invoke("check", "--repo", str(tmp_path)).exit_code == 1


def test_acquire_then_release_round_trips(tmp_path):
    from fno.claims.core import claim_status

    key = _review_hold.review_hold_key("feature/x-a089")
    acquired = _invoke(
        "acquire", "--branch", "feature/x-a089", "--head", "abc123",
        "--holder", "reviewer:sess-1", "--verb", "/code-review",
    )
    assert acquired.exit_code == 0
    assert claim_status(key, root=tmp_path)["state"] == "live"

    released = _invoke("release", "--branch", "feature/x-a089")
    assert released.exit_code == 0
    assert "released" in released.stdout
    assert claim_status(key, root=tmp_path)["state"] == "free"

    # Say which happened. A "released" printed over an absent hold is the
    # absence-as-success shape, on the one recovery command an operator has.
    again = _invoke("release", "--branch", "feature/x-a089")
    assert again.exit_code == 0
    assert "no hold on" in again.stdout


def test_acquire_carries_review_invocation_id_in_hold_metadata(tmp_path):
    from fno.claims.core import claim_status

    result = _invoke(
        "acquire",
        "--branch",
        "feature/x-a089",
        "--head",
        "abc123",
        "--holder",
        "reviewer:sess-1",
        "--invocation-id",
        "ri-test-4",
    )

    assert result.exit_code == 0
    hold = claim_status(_review_hold.review_hold_key("feature/x-a089"), root=tmp_path)
    assert hold["metadata"]["invocation_id"] == "ri-test-4"


def test_a_failed_acquire_still_exits_zero(monkeypatch, tmp_path):
    """Registration must never block a review from starting: an unheld review is
    still covered by the worktree layer, and a review that refuses to start
    because a lockfile write failed is strictly worse."""
    monkeypatch.setattr(_review_hold, "acquire_review_hold", lambda *a, **kw: None)
    result = _invoke(
        "acquire", "--branch", "feature/x-a089", "--head", "abc123", "--holder", "r"
    )
    assert result.exit_code == 0


def test_acquire_honors_an_explicit_ttl(tmp_path):
    from fno.claims.core import claim_status

    _invoke(
        "acquire", "--branch", "feature/x-a089", "--head", "abc123",
        "--holder", "reviewer:sess-1", "--ttl-minutes", "5",
    )
    row = claim_status(_review_hold.review_hold_key("feature/x-a089"), root=tmp_path)
    assert row["expires_at"] - row["acquired_at"] == 5 * 60_000


def test_acquire_needs_a_branch_and_a_holder(tmp_path):
    assert _invoke("acquire", "--head", "abc123").exit_code == 1
    assert _invoke("acquire", "--branch", "feature/x-a089").exit_code == 1


def test_release_needs_only_the_branch(tmp_path):
    """The refusal an operator reads prints exactly `--branch <b>`. Requiring a
    holder too would make the one documented recovery command exit 1."""
    assert _invoke("release").exit_code == 1
    assert _invoke("release", "--branch", "feature/x-a089").exit_code == 0


def test_release_clears_a_hold_taken_by_someone_else(tmp_path):
    """The acquire side names the harness session; every release side derives
    its own string. `release_claim` no-ops SILENTLY on a mismatch, so a
    holder-matched release left the hold sitting for the full TTL with a
    "released" receipt printed over it."""
    from fno.claims.core import claim_status

    _review_hold.acquire_review_hold(
        "feature/x-a089", head="abc123", holder="review-session:some-other-session",
        root=tmp_path,
    )
    result = _invoke("release", "--branch", "feature/x-a089")
    assert result.exit_code == 0
    assert "released" in result.stdout
    assert claim_status(_review_hold.review_hold_key("feature/x-a089"), root=tmp_path)["state"] == "free"


def test_an_unknown_action_is_a_usage_error(tmp_path):
    assert _invoke("frobnicate", "--branch", "b", "--holder", "h").exit_code == 1
