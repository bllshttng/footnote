"""Contract tests for the single component-aware agent-name owner (x-3218)."""

import pytest

from fno.agents.naming import (
    MAX_LEN,
    AgentNameError,
    dispatch_agent_name,
    dispatch_sources,
    dispatch_verbs,
    parse_dispatch_agent_name,
    slug_component,
)


def _legacy_name(*argv):
    """The legacy prefix form through the bridge (cmd_name -> binary)."""
    res = _run_name(*argv)
    if res.exit_code != 0:
        raise AgentNameError(res.output)
    return res.stdout.strip()


def test_slug_component_matches_the_shell_sanitize_pipeline():
    assert slug_component("Path Consolidation: Wave 0!") == "path-consolidation-wave-0"
    assert slug_component("--Leading and trailing--") == "leading-and-trailing"
    assert slug_component(None) == ""
    assert slug_component("a" * 50) == "a" * 30
    # A cut landing on a hyphen trims it rather than leaving a dangling tail.
    assert slug_component("x" * 29 + "-tail") == "x" * 29


def test_plain_node_name():
    assert _legacy_name("target", "x-3218", "--slug", "spawn-sh-pane-name") == (
        "target-x-3218-spawn-sh-pane-name"
    )
    assert _legacy_name("target", "x-3218") == "target-x-3218"
    assert _legacy_name("target", "x-3218", "--slug", "   ") == "target-x-3218"


def test_qualifier_and_discriminator_ordering():
    assert _legacy_name(
        "think", "x-3218", "--qualifier", "worked", "--slug", "pane-name",
        "--discriminator", "ab12",
    ) == "think-x-3218-worked-pane-name-ab12"


def test_long_slug_is_trimmed_to_exactly_the_limit():
    name = _legacy_name("target", "x-" + "0" * 20, "--slug", "c" * 30, "--discriminator", "d" * 12)
    assert len(name) == MAX_LEN
    assert name.startswith("target-x-" + "0" * 20)
    assert name.endswith("-" + "d" * 12)


def test_slug_is_the_only_component_that_gives_way():
    # Required components consume the whole budget: the slug disappears, the
    # load-bearing discriminator survives intact.
    node = "n-" + "9" * 42
    name = _legacy_name("target", node, "--slug", "human-readable", "--discriminator", "e" * 12)
    assert name == f"target-{node}-" + "e" * 12
    assert len(name) <= MAX_LEN


def test_uniqueness_suffixes_stay_distinct_near_the_limit():
    node = "x-" + "7" * 30
    a = _legacy_name("think", node, "--slug", "s" * 30, "--discriminator", "aaaaaaaa")
    b = _legacy_name("think", node, "--slug", "s" * 30, "--discriminator", "bbbbbbbb")
    assert a != b
    assert len(a) == len(b) == MAX_LEN
    assert a.endswith("-aaaaaaaa") and b.endswith("-bbbbbbbb")


def test_identical_components_converge_on_one_name():
    # Exact, not f(x) == f(x): the tautology holds for any deterministic
    # implementation, including one that returns the empty string.
    assert _legacy_name("target", "x-3218", "--slug", "dedup token") == "target-x-3218-dedup-token"


def test_over_budget_required_identity_fails_closed():
    node = "n-" + "z" * 70
    with pytest.raises(AgentNameError) as exc:
        _legacy_name("target", node, "--slug", "anything")
    assert node in str(exc.value)
    assert "64" in str(exc.value)


def test_over_budget_with_discriminator_names_the_node_and_budget():
    node = "n-" + "z" * 45
    with pytest.raises(AgentNameError) as exc:
        _legacy_name("target", node, "--discriminator", "d" * 20)
    assert node in str(exc.value)


def test_invalid_characters_in_required_components_are_refused():
    with pytest.raises(AgentNameError):
        _legacy_name("target", "x 3218")
    with pytest.raises(AgentNameError):
        _legacy_name("tar/get", "x-3218")


def test_empty_required_identity_is_refused():
    with pytest.raises(AgentNameError):
        _legacy_name("", "")


def test_underscores_survive_in_required_components():
    # The daemon contract allows '_'; only the human slug is hyphen-normalized.
    assert _legacy_name("target", "x_3218", "--slug", "a_b") == "target-x_3218-a-b"


@pytest.mark.parametrize(
    "argv",
    [
        ["target", "x-3218", "--slug", "s" * 40],
        ["reconcile", "ab-4040eee8", "--slug", "cargo bootstrapper"],
        ["think", "x-" + "1" * 35, "--qualifier", "retro", "--slug", "s" * 30],
        ["spawn", "x-3218", "--discriminator", "0" * 30],
    ],
)
def test_every_result_satisfies_the_daemon_contract(argv):
    import re

    name = _legacy_name(*argv)
    assert re.fullmatch(r"[A-Za-z0-9_-]{1,64}", name), name


# ---------------------------------------------------------------------------
# The x-84b2 dispatch-provenance vocabulary
# ---------------------------------------------------------------------------


def test_dispatch_name_canonical_and_manual_forms():
    assert dispatch_agent_name("ab", "bp", "x-84b2", slug="ab-names") == (
        "ab-bp-x-84b2-ab-names"
    )
    # Attended launch: no source segment.
    assert dispatch_agent_name(None, "t", "x-84b2", slug="ab-names") == (
        "t-x-84b2-ab-names"
    )
    # The verb slot is positional: th is both a source and a verb.
    assert dispatch_agent_name("th", "th", "x-1", qualifier="retro") == (
        "th-th-x-1-retro"
    )


def test_dispatch_name_maps_words_and_refuses_unknowns():
    # The binary maps work-verb words; only unknown words and codes refuse.
    with pytest.raises(AgentNameError, match="unknown dispatch source"):
        dispatch_agent_name("xx", "t", "x-1")
    with pytest.raises(AgentNameError, match="unknown dispatch verb"):
        dispatch_agent_name("ab", "impeccable", "x-1")


def test_dispatch_name_budget_keeps_source_verb_identity_whole():
    node = "x-" + "0" * 40
    name = dispatch_agent_name("ab", "t", node, slug="c" * 30)
    assert name.startswith(f"ab-t-{node}-")
    assert len(name) <= MAX_LEN
    with pytest.raises(AgentNameError):
        dispatch_agent_name("ab", "t", "x-" + "0" * 70)


def test_parse_canonical_and_manual_names():
    parsed = parse_dispatch_agent_name("ab-bp-x-84b2-footprint-pid")
    assert parsed is not None
    assert (parsed.source, parsed.verb, parsed.node, parsed.tail) == (
        "ab", "bp", "x-84b2", "footprint-pid",
    )
    manual = parse_dispatch_agent_name("t-x-84b2-name")
    assert manual is not None
    assert (manual.source, manual.verb, manual.node) == (None, "t", "x-84b2")


def test_parse_node_prefix_colliding_with_a_code_stays_positional():
    # A configured node prefix that IS a source code: the second token decides.
    parsed = parse_dispatch_agent_name("ab-bp-ab-4040eee8-cargo")
    assert parsed is not None
    assert (parsed.source, parsed.verb, parsed.node) == ("ab", "bp", "ab-4040eee8")
    manual = parse_dispatch_agent_name("t-ab-4040eee8-cargo")
    assert manual is not None
    assert (manual.source, manual.verb, manual.node) == (None, "t", "ab-4040eee8")
    # A verb-code prefix with a node whose prefix is a source code must not
    # read as sourced: the second token (ab) is not a verb.
    manual2 = parse_dispatch_agent_name("bp-ab-4040eee8")
    assert manual2 is not None
    assert (manual2.source, manual2.verb, manual2.node) == (None, "bp", "ab-4040eee8")


def test_parse_typed_identities_are_not_nodes():
    for name in ("gr-th-backlog-20260910", "ev-th-evals-42", "rec-t-session-abc123-x"):
        parsed = parse_dispatch_agent_name(name)
        assert parsed is not None, name
        assert parsed.node is None, name


def test_parse_legacy_names_resolve_through_the_window():
    legacy = parse_dispatch_agent_name("target-x-3218-spawn")
    assert (legacy.source, legacy.verb, legacy.node, legacy.tail) == (None, "t", "x-3218", "spawn")
    think = parse_dispatch_agent_name("think-x-3218-retro")
    assert (think.verb, think.node) == ("th", "x-3218")


def test_parse_junk_names_return_none():
    assert parse_dispatch_agent_name("reconcile-ab-4040eee8-cargo") is None
    assert parse_dispatch_agent_name("j-x-3218-2") is None
    assert parse_dispatch_agent_name("fno agents pane") is None
    assert parse_dispatch_agent_name("") is None
    assert parse_dispatch_agent_name(None) is None
    assert parse_dispatch_agent_name("t") is None


def test_source_and_verb_vocabularies_do_not_collide_within_a_slot():
    # Every source is also a legal FIRST token only when followed by a verb;
    # the one shared code (th) must resolve by position.
    assert "th" in dispatch_sources() and "th" in dispatch_verbs()
    assert parse_dispatch_agent_name("th-th-x-1").source == "th"
    assert parse_dispatch_agent_name("th-x-1").verb == "th"


# ---------------------------------------------------------------------------
# The `fno agents name` bridge contract (what the shell dispatchers branch on)
# ---------------------------------------------------------------------------


def _run_name(*args):
    """The routed verb replaces the process under CliRunner, so the bridge
    contract runs against the binary directly (the same surface shells hit)."""
    import subprocess

    from fno import rust_binary

    binary = rust_binary.resolve_binary()
    assert binary is not None, "no fno-agents binary on this lane"
    proc = subprocess.run(
        [str(binary), "name", *args], capture_output=True, text=True, timeout=30
    )

    class _Res:
        exit_code = proc.returncode
        stdout = proc.stdout
        stderr = proc.stderr
        output = proc.stdout + proc.stderr

    return _Res()


def test_bridge_prints_the_name_and_exits_zero():
    res = _run_name("target", "x-3218", "--slug", "Path Consolidation: Wave 0")
    assert res.exit_code == 0
    assert res.stdout.strip() == "target-x-3218-path-consolidation-wave-0"


def test_bridge_dispatch_form_prints_the_canonical_name():
    res = _run_name("", "x-84b2", "--source", "ab", "--verb", "bp", "--slug", "Ab Names")
    assert res.exit_code == 0
    assert res.stdout.strip() == "ab-bp-x-84b2-ab-names"
    manual = _run_name("", "x-84b2", "--verb", "t")
    assert manual.exit_code == 0
    assert manual.stdout.strip() == "t-x-84b2"


def test_bridge_single_positional_binds_the_node():
    """One positional is the node: Click would bind it to the legacy prefix
    slot and strand every natural dispatch-form caller on a usage error."""
    res = _run_name("x-84b2", "--source", "kl", "--verb", "th", "--slug", "walk")
    assert res.exit_code == 0
    assert res.stdout.strip() == "kl-th-x-84b2-walk"
    bare = _run_name("x-84b2")
    assert bare.exit_code == 2  # a node alone is a usage error: no default verb


def test_bridge_dispatch_form_refusals():
    assert _run_name("", "x-1", "--source", "ab").exit_code == 2  # --source needs --verb
    assert _run_name("legacy", "x-1", "--verb", "t").exit_code == 2  # not both forms
    # An unknown word refuses (exit 3), never defaulting to t; a work-verb
    # word maps through the vocabulary owner.
    assert _run_name("", "x-1", "--verb", "impeccable").exit_code == 3
    word = _run_name("", "x-1", "--verb", "target")
    assert word.exit_code == 0 and word.stdout.strip() == "t-x-1"
    assert _run_name("", "x-1", "--source", "zz", "--verb", "t").exit_code == 3


def test_bridge_refusal_exit_code_is_three_not_two():
    """The load-bearing constant: 3 is the refusal, 2 is Click's usage error.

    Both shell dispatchers branch on 3 alone. If this collapses to 2, an `fno`
    too old to know this verb (Click answers "no such command" with 2) reads as
    "node unrepresentable" and refuses every dispatch instead of degrading to
    the fallback assembly. Asserting the exact code is the only thing that
    catches that: the downstream receipt lines are indistinguishable, because
    the degraded path has its own over-64 refusal that prints a similar message.
    """
    from fno.agents.cli import NAME_REFUSED_EXIT

    assert NAME_REFUSED_EXIT == 3
    assert NAME_REFUSED_EXIT != 2, "2 is Click's usage/unknown-command exit"

    res = _run_name("target", "n-" + "z" * 70)
    assert res.exit_code == NAME_REFUSED_EXIT
    assert res.exit_code == 3


def test_bridge_unknown_verb_exits_two_the_stale_install_signal():
    """The other half of the contract: what a stale `fno` actually returns."""
    from typer.testing import CliRunner

    from fno.cli import app

    res = CliRunner().invoke(app, ["agents", "no-such-naming-verb", "x-1"])
    assert res.exit_code == 2


def test_bridge_refusal_message_is_one_quote_free_line():
    """dispatch-node.sh relays this into a `reason="..."` field with a grammar."""
    res = _run_name("target", "n-" + "z" * 70)
    combined = (res.stdout or "") + (getattr(res, "stderr", "") or "")
    assert "64" in combined
    body = combined.strip()
    assert '"' not in body, f"a double quote would break the outcome-line grammar: {body!r}"


def test_max_len_matches_the_daemon_contract():
    """MAX_LEN is a copy of the Rust validator's limit; pin them together.

    On a `--node` spawn nothing downstream re-checks 64 (the Python path allows
    128), so the generator is the only enforcement in production. A silent drift
    between these two numbers reopens exactly the hole this module closed.
    """
    import re as _re
    from pathlib import Path

    state = Path(__file__).resolve().parents[3] / "crates/fno-agents/src/state.rs"
    if not state.is_file():
        import pytest as _pytest

        _pytest.skip("rust crate not present in this checkout")
    src = state.read_text()
    fn = src[src.index("fn is_valid_registry_label") : src.index("fn is_valid_registry_label") + 400]
    found = _re.search(r"len\(\)\s*<=\s*(\d+)", fn)
    assert found, f"could not locate the registry length check in: {fn[:200]!r}"
    assert int(found.group(1)) == MAX_LEN
