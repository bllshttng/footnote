"""The per-provider budget, and the review route that consumes it (x-c703).

Every assertion here names a POSITIVE marker: a reason string, a resolved
route, a number. None of them asserts that a panel was absent. An absence has
two explanations, the real outcome and "the instrument never ran", and a test
built on one cannot tell them apart - which is the failure that let an
uncapped fan-out read as a pass on the night this node was filed.
"""

from __future__ import annotations

import pytest

from fno.agents.model_routing import ROUTE_PROVIDER_ENV, bind_route_provider
from fno.agents.spawn_gate import provider_lanes_cap
from fno.config import AgentsBlock, ProviderBudget, provider_subagent_budget
from fno.review_capability import (
    ReviewerVerdict,
    SessionCapability,
    detect_session,
    resolve_reviewers,
)


def _budget(**kwargs) -> tuple[int | None, int | None]:
    b = AgentsBlock(**kwargs).max_lanes.get("zai")
    return (None, None) if b is None else (b.lanes, b.subagents)


def _session(**kwargs) -> SessionCapability:
    base = {"harness": "claude", "substrate": "interactive", "attended": True}
    base.update(kwargs)
    return SessionCapability(**base)


def _sigma(session: SessionCapability) -> ReviewerVerdict:
    return resolve_reviewers(["sigma"], session)[0]


# --- the record ------------------------------------------------------------


def test_builtin_zai_budget_carries_both_dimensions():
    assert _budget() == (5, 1)


def test_legacy_scalar_reads_as_lanes_and_keeps_the_builtin_subagents():
    # AC3-HP. An install written before the record widened must still get the
    # protection, or shipping a built-in for a shared account buys nothing.
    assert _budget(max_lanes={"zai": 5}) == (5, 1)


def test_a_written_dimension_wins_over_the_builtin():
    assert _budget(max_lanes={"zai": {"lanes": 9}}) == (9, 1)
    assert _budget(max_lanes={"zai": {"subagents": 4}}) == (5, 4)
    assert _budget(max_lanes={"zai": {"lanes": 2, "subagents": 3}}) == (2, 3)


def test_an_unlisted_provider_is_unbounded_in_both_dimensions():
    # AC3-EDGE. Today's behavior for every dedicated account.
    budgets = AgentsBlock(max_lanes={"openai": 7}).max_lanes
    assert (budgets["openai"].lanes, budgets["openai"].subagents) == (7, None)


def test_an_explicit_empty_table_disables_every_budget():
    assert AgentsBlock(max_lanes={}).max_lanes == {}


@pytest.mark.parametrize(
    "written",
    [
        {"zai": 0},
        {"zai": -1},
        {"zai": True},
        {"zai": {"subagents": 0}},
        {"zai": {"bogus": 1}},
        {"ZAI": 5},
        "broken",
        None,
    ],
)
def test_a_malformed_table_restores_the_safe_builtin(written):
    # Restoring the whole built-in table, rather than dropping one entry, is
    # what keeps a typo from making a shared provider unlimited.
    assert _budget(max_lanes=written) == (5, 1)


# --- the two readers of the record -----------------------------------------


def test_lanes_cap_reads_both_spellings_at_one_seam():
    # The gate's configured path carries the record and its fail-safe path
    # carries the integer. One reader is why they cannot disagree about a cap.
    assert provider_lanes_cap(ProviderBudget(lanes=5, subagents=1)) == 5
    assert provider_lanes_cap(5) == 5
    assert provider_lanes_cap(None) is None
    assert provider_lanes_cap(ProviderBudget(subagents=1)) is None


def test_subagent_budget_fails_open_on_every_unknown():
    assert provider_subagent_budget("zai") == 1
    assert provider_subagent_budget("anthropic") is None
    assert provider_subagent_budget("unknown") is None
    assert provider_subagent_budget(None) is None


# --- the stamp -------------------------------------------------------------


def test_bound_route_stamps_the_provider_into_the_env():
    # AC1-HP. `.provider` serves the parent; only the env key crosses the fork.
    env = bind_route_provider({}, "zai")
    assert env[ROUTE_PROVIDER_ENV] == "zai"
    assert env.provider == "zai"


def test_detect_session_reads_the_stamp_and_describes_it():
    session = detect_session(env={"FNO_ROUTE_PROVIDER": "zai"})
    assert session.provider == "zai"
    assert "provider=zai" in session.describe()


def test_an_unstamped_session_resolves_unknown_and_never_guesses():
    # AC1-EDGE, first half. The stamp is written by the same code that CHOOSES
    # a provider, so no stamp means no fno route, not an unreadable one.
    assert detect_session(env={}).provider == "unknown"


# --- route resolution ------------------------------------------------------


def test_a_budget_of_one_refuses_the_panel_and_names_its_cause():
    # AC2-HP. The reason has to teach the operator the fix, so it carries the
    # provider, the number, and the route that runs instead.
    verdict = _sigma(_session(provider="zai"))
    assert verdict.status == "unavailable"
    assert "zai" in verdict.reason
    assert "subagent budget of 1" in verdict.reason
    assert verdict.resolves_to == "code-review"


def test_the_budget_downgrade_does_not_block_the_run():
    # AC2-EDGE. Fail-closed here wedges every worker on a shared account over a
    # review that is still going to run.
    verdict = _sigma(_session(provider="zai"))
    assert verdict.blocks_autonomy is False
    assert "resolved route: code-review" in verdict.line()


def test_an_unstamped_session_keeps_the_panel():
    # AC1-EDGE, second half.
    assert _sigma(_session(provider="unknown")).status == "satisfiable"


def test_a_provider_with_no_budget_keeps_the_panel():
    # AC3-EDGE.
    verdict = _sigma(_session(provider="anthropic"))
    assert verdict.status == "satisfiable"
    assert verdict.resolves_to is None


def test_a_budget_of_two_or_more_keeps_the_panel(monkeypatch):
    monkeypatch.setattr(
        "fno.review_capability.provider_subagent_budget", lambda _p: 6
    )
    assert _sigma(_session(provider="zai")).status == "satisfiable"


def test_a_non_budget_refusal_never_substitutes():
    # AC2-ERR, the assertion that keeps the downgrade honest. A gemini harness
    # cannot dispatch the panel either, but that is a misconfiguration the
    # operator has to see - running something else there would hide it.
    verdict = _sigma(_session(harness="gemini", provider="unknown"))
    assert verdict.status == "unavailable"
    assert verdict.resolves_to is None
    assert verdict.blocks_autonomy is True
    assert "needs subagent-dispatch, unavailable on" in verdict.reason


def test_the_budget_is_checked_before_the_harness():
    # Reversing the order answers the harness question on exactly the sessions
    # this exists for, and a budgeted claude worker resolves satisfiable again.
    verdict = _sigma(_session(harness="gemini", provider="zai"))
    assert "subagent budget of 1" in verdict.reason
