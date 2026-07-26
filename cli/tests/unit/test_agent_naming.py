"""Contract tests for the single component-aware agent-name owner (x-3218)."""

import pytest

from fno.agents.naming import MAX_LEN, AgentNameError, agent_name, slug_component


def test_slug_component_matches_the_shell_sanitize_pipeline():
    assert slug_component("Path Consolidation: Wave 0!") == "path-consolidation-wave-0"
    assert slug_component("--Leading and trailing--") == "leading-and-trailing"
    assert slug_component(None) == ""
    assert slug_component("a" * 50) == "a" * 30
    # A cut landing on a hyphen trims it rather than leaving a dangling tail.
    assert slug_component("x" * 29 + "-tail") == "x" * 29


def test_plain_node_name():
    assert agent_name("target", "x-3218", slug="spawn-sh-pane-name") == (
        "target-x-3218-spawn-sh-pane-name"
    )
    assert agent_name("target", "x-3218") == "target-x-3218"
    assert agent_name("target", "x-3218", slug="   ") == "target-x-3218"


def test_qualifier_and_discriminator_ordering():
    assert agent_name(
        "think", "x-3218", qualifier="worked", slug="pane-name", discriminator="ab12"
    ) == "think-x-3218-worked-pane-name-ab12"


def test_long_slug_is_trimmed_to_exactly_the_limit():
    name = agent_name("target", "x-" + "0" * 20, slug="c" * 30, discriminator="d" * 12)
    assert len(name) == MAX_LEN
    assert name.startswith("target-x-" + "0" * 20)
    assert name.endswith("-" + "d" * 12)


def test_slug_is_the_only_component_that_gives_way():
    # Required components consume the whole budget: the slug disappears, the
    # load-bearing discriminator survives intact.
    node = "n-" + "9" * 42
    name = agent_name("target", node, slug="human-readable", discriminator="e" * 12)
    assert name == f"target-{node}-" + "e" * 12
    assert len(name) <= MAX_LEN


def test_uniqueness_suffixes_stay_distinct_near_the_limit():
    node = "x-" + "7" * 30
    a = agent_name("think", node, slug="s" * 30, discriminator="aaaaaaaa")
    b = agent_name("think", node, slug="s" * 30, discriminator="bbbbbbbb")
    assert a != b
    assert len(a) == len(b) == MAX_LEN
    assert a.endswith("-aaaaaaaa") and b.endswith("-bbbbbbbb")


def test_identical_components_converge_on_one_name():
    kwargs = dict(prefix="target", node_id="x-3218", slug="dedup token")
    assert agent_name(**kwargs) == agent_name(**kwargs)


def test_over_budget_required_identity_fails_closed():
    node = "n-" + "z" * 70
    with pytest.raises(AgentNameError) as exc:
        agent_name("target", node, slug="anything")
    assert node in str(exc.value)
    assert "64" in str(exc.value)


def test_over_budget_with_discriminator_names_the_node_and_budget():
    node = "n-" + "z" * 45
    with pytest.raises(AgentNameError) as exc:
        agent_name("target", node, discriminator="d" * 20)
    assert node in str(exc.value)


def test_invalid_characters_in_required_components_are_refused():
    with pytest.raises(AgentNameError):
        agent_name("target", "x 3218")
    with pytest.raises(AgentNameError):
        agent_name("tar/get", "x-3218")


def test_empty_required_identity_is_refused():
    with pytest.raises(AgentNameError):
        agent_name("", "")


def test_underscores_survive_in_required_components():
    # The daemon contract allows '_'; only the human slug is hyphen-normalized.
    assert agent_name("target", "x_3218", slug="a_b") == "target-x_3218-a-b"


@pytest.mark.parametrize(
    "kwargs",
    [
        dict(prefix="target", node_id="x-3218", slug="s" * 40),
        dict(prefix="reconcile", node_id="ab-4040eee8", slug="cargo bootstrapper"),
        dict(prefix="think", node_id="x-" + "1" * 35, qualifier="retro", slug="s" * 30),
        dict(prefix="spawn", node_id="x-3218", discriminator="0" * 30),
    ],
)
def test_every_result_satisfies_the_daemon_contract(kwargs):
    import re

    name = agent_name(**kwargs)
    assert re.fullmatch(r"[A-Za-z0-9_-]{1,64}", name), name
