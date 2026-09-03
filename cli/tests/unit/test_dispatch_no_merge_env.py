"""x-9d11: the dispatch refusal carrier rides resolve_dispatch's env.

The choke point every spawn surface that consumes the resolver's tuple (skill
spawn.sh, the dispatch.py pane, advance's `fno agents spawn`) must set
TARGET_NO_MERGE whenever the resolved command carries the refusal, and must
rewrite the legacy bare `no-merge` token to the flag. Lanes that shell
`fno agents spawn` WITHOUT the resolver (recovery respawn, keep_going) set the
env directly at their spawn sites. Without a carrier, a worker that drops the
flag post-compaction folds no refusal at init and a configured auto-merge
stands unrevoked.

x-8151/d-450caaeb: the vocabulary is the canonical merge_posture table
(authored in the Rust tree, shipped here as generated package data), so these
tests are pure Python - no binary needed - while the Rust engine's tests in
crates/fno-agents/src/merge_posture.rs pin the same semantics from the same
table.
"""

import os

import pytest

from fno.agents.harness_map import apply_merge_posture_env, resolve_dispatch


def test_builtin_no_merge_command_carries_env_refusal():
    dispatch = resolve_dispatch(harness="claude", node_id="x-1", trigger="autonomous")
    assert "--no-merge" in dispatch["command"]
    assert dispatch["env"].get("TARGET_NO_MERGE") == "1"


def test_legacy_bare_token_is_rewritten_and_carries_env():
    dispatch = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/target no-merge {id}",
        trigger="autonomous",
    )
    # Rewritten to the flag spelling: the fold and every env match key on
    # `--no-merge`, so a stale operator template keeps its refusal.
    assert "--no-merge" in dispatch["command"]
    assert " no-merge " not in f" {dispatch['command']} "
    assert dispatch["env"].get("TARGET_NO_MERGE") == "1"


def test_merge_allowed_command_has_no_env_refusal():
    dispatch = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/target {id}",
        trigger="autonomous",
    )
    assert "TARGET_NO_MERGE" not in dispatch["env"]


def test_prose_template_mentioning_no_merge_stays_literal():
    """A non-slash prose template passes through literally: rewriting its
    words would turn a sentence into a merge posture (review round 5)."""
    dispatch = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="Discuss the no-merge rollout plan for {id}",
        trigger="autonomous",
    )
    assert "no-merge" in dispatch["command"]
    assert "--no-merge" not in dispatch["command"]
    assert "TARGET_NO_MERGE" not in dispatch["env"]


def test_non_target_slash_command_args_are_untouched():
    """The rewrite and env carrier are scoped to /target-family commands
    (review round 6): another slash verb's args are instruction text, not
    merge posture."""
    dispatch = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/think summarize the no-merge posture {id}",
        trigger="autonomous",
    )
    assert dispatch["command"] == "/think summarize the no-merge posture x-1"
    assert "TARGET_NO_MERGE" not in dispatch["env"]


# --------------------------------------------------------------------------- #
# x-8151: apply_to_environ - the Python lane's env application over the owner
# --------------------------------------------------------------------------- #


@pytest.fixture(autouse=True)
def _clean_carrier(monkeypatch):
    monkeypatch.delenv("TARGET_NO_MERGE", raising=False)


def test_flag_family_message_arms_carrier_and_returns_prior(monkeypatch):
    monkeypatch.setenv("TARGET_NO_MERGE", "legacy-value")
    prior = apply_merge_posture_env("/target --no-merge x-1")
    assert prior == "legacy-value"
    assert os.environ["TARGET_NO_MERGE"] == "1"


def test_prose_message_clears_nothing(monkeypatch):
    monkeypatch.setenv("TARGET_NO_MERGE", "1")
    prior = apply_merge_posture_env("please discuss the no-merge rollout")
    assert prior == "1"
    assert os.environ["TARGET_NO_MERGE"] == "1"


def test_flagless_family_message_clears_inherited_carrier_loudly(monkeypatch, capsys):
    monkeypatch.setenv("TARGET_NO_MERGE", "1")
    prior = apply_merge_posture_env("/target x-1")
    assert prior == "1"
    assert "TARGET_NO_MERGE" not in os.environ
    assert "inherited TARGET_NO_MERGE cleared" in capsys.readouterr().err


def test_bare_token_family_message_neither_arms_nor_clears(monkeypatch):
    monkeypatch.setenv("TARGET_NO_MERGE", "1")
    prior = apply_merge_posture_env("/target no-merge x-1")
    assert prior == "1"
    assert os.environ["TARGET_NO_MERGE"] == "1"


def test_guard_named_flag_is_not_the_carrier():
    prior = apply_merge_posture_env("/target --no-merge-guard x-1")
    assert prior is None
    assert "TARGET_NO_MERGE" not in os.environ


# --------------------------------------------------------------------------- #
# x-8151: merge_posture on resolve_dispatch - the resolver owns the carrier
# for every rung, and the dispatch shell stops re-deriving it
# --------------------------------------------------------------------------- #


def test_posture_no_merge_injects_into_verb_resolved_family_command():
    dispatch = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/target {id}",
        merge_posture="no-merge",
        trigger="autonomous",
    )
    assert dispatch["command"] == "/target --no-merge x-1"
    assert dispatch["env"]["TARGET_NO_MERGE"] == "1"


def test_posture_no_merge_covers_the_namespaced_spellings():
    for cmd in ("/fno:target {id}", "$fno:target {id}"):
        dispatch = resolve_dispatch(
            harness="codex",
            node_id="x-1",
            command=cmd,
            merge_posture="no-merge",
            trigger="autonomous",
        )
        assert "--no-merge" in dispatch["command"], cmd


def test_posture_no_merge_skips_when_flag_already_present():
    dispatch = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/target --no-merge {id}",
        merge_posture="no-merge",
        trigger="autonomous",
    )
    assert dispatch["command"] == "/target --no-merge x-1"


def test_posture_no_merge_leaves_prose_templates_alone():
    dispatch = resolve_dispatch(
        harness="opencode",
        node_id="x-1",
        command="Work on {id} and explain the refusal posture",
        merge_posture="no-merge",
        trigger="autonomous",
    )
    assert "--no-merge" not in dispatch["command"]


def test_posture_allow_strips_flag_and_legacy_token():
    dispatch = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/target --no-merge {id}",
        merge_posture="allow",
        trigger="autonomous",
    )
    assert dispatch["command"] == "/target x-1"
    assert "TARGET_NO_MERGE" not in dispatch["env"]
    legacy = resolve_dispatch(
        harness="codex",
        node_id="x-1",
        command="$fno:target no-merge {id}",
        merge_posture="allow",
        trigger="autonomous",
    )
    assert "no-merge" not in f" {legacy['command']} "
    assert "TARGET_NO_MERGE" not in legacy["env"]


def test_posture_from_config_reads_grant_and_degrades_to_no_merge():
    granted = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/target {id}",
        merge_posture="from-config",
        dispatch_cfg={"auto_merge": True},
        trigger="autonomous",
    )
    assert "--no-merge" not in granted["command"]
    refused = resolve_dispatch(
        harness="claude",
        node_id="x-1",
        command="/target {id}",
        merge_posture="from-config",
        dispatch_cfg={},
        trigger="autonomous",
    )
    assert refused["command"] == "/target --no-merge x-1"


def test_posture_unknown_value_fails_closed():
    from fno.agents.harness_map import DispatchResolveError

    with pytest.raises(DispatchResolveError, match="unknown merge posture"):
        resolve_dispatch(
            harness="claude",
            node_id="x-1",
            merge_posture="merge-now",
            trigger="autonomous",
        )
