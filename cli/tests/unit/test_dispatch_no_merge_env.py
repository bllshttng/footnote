"""x-9d11: the dispatch refusal carrier rides resolve_dispatch's env.

The choke point every spawn surface that consumes the resolver's tuple (skill
spawn.sh, the dispatch.py pane, advance's `fno agents spawn`) must set
TARGET_NO_MERGE whenever the resolved command carries the refusal, and must
rewrite the legacy bare `no-merge` token to the flag. Lanes that shell
`fno agents spawn` WITHOUT the resolver (recovery respawn, keep_going) set the
env directly at their spawn sites. Without a carrier, a worker that drops the
flag post-compaction folds no refusal at init and a configured auto-merge
stands unrevoked.
"""

from fno.agents.harness_map import resolve_dispatch


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
