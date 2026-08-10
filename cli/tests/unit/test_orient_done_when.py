"""The orienter's `done-when:` line must name every gate loop-check enforces.

The wedge this pins (x-0322): with `config.review.peers = ["codex"]` and no
`peer_identity`, loop-check synthesizes a composite local reviewer `peer` and
holds the loop until a head-pinned `review_attestation` for it exists. The
orienter read `github_apps` only, so it announced `reviewed by [none (PR + CI
only)]` -- the session planned its whole run believing there was no review
gate, shipped, promised, and only then discovered an attestation nothing in its
plan produced.

`reviewers` has the same shape and the same failure, so both are covered here.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.target.orient import _done_when_line


@pytest.fixture
def repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """A repo root whose only config layer is its own `.fno/config.toml`."""
    (tmp_path / ".fno").mkdir()
    empty_global = tmp_path / "global.toml"
    empty_global.write_text("")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(empty_global))

    def _write(body: str) -> Path:
        (tmp_path / ".fno" / "config.toml").write_text(body)
        return tmp_path

    return _write


def test_no_gate_config_still_reads_pr_and_ci_only(repo):
    root = repo("")
    # A stock install carries PR + CI; the self-review floor for a code payload
    # appends after it (tested below), so assert the base as a prefix.
    assert _done_when_line({}, root).startswith(
        "PR + CI green + reviewed by [none (PR + CI only)]"
    )


def test_identity_free_peers_announce_the_local_peer_gate_and_its_producer(repo):
    # `opencode` arms the gate but `/review peer` cannot drive it, so it is not
    # offered as a choice - naming the one peer that runs is the whole point.
    root = repo('[review]\npeers = ["codex", "opencode"]\n')
    line = _done_when_line({}, root)
    assert "peer -> /fno:review peer codex --attest (cross-model only)" in line, line
    # The old string is the lie: it told the session there was nothing to run.
    assert "none (PR + CI only)" not in line, line


def test_peer_identity_keeps_peers_on_the_github_login_carrier(repo):
    # A shared identity makes every peer a posted-review login, not a local
    # attestation -- loop-check resolves no `peer` reviewer, so neither do we.
    # Asserting only the absence of the local gate would pass while the line
    # still read "none (PR + CI only)" for a repo whose login gate is live, so
    # the login itself must be named.
    root = repo('[review]\npeers = ["codex"]\npeer_identity = "fno-peer-bot"\n')
    line = _done_when_line({}, root)
    assert "peer ->" not in line, line
    assert "reviewed by [fno-peer-bot (post: /fno:review peer <pr#> codex --post; cross-model only)]" in line, line


def test_per_entry_identity_is_a_login_while_a_free_sibling_stays_local(repo):
    # The mixed carrier: loop-check requires BOTH bot-x (login) and the local
    # composite. Announcing "no App bot" here would assert there is no App gate
    # while one is live.
    root = repo(
        '[review]\npeers = [{provider="codex", identity="bot-x"}, "gemini"]\n'
    )
    line = _done_when_line({}, root)
    assert "reviewed by [bot-x (no --post runner" in line, line
    assert "peer -> /fno:review peer gemini --attest (cross-model only)" in line, line


def test_empty_peer_identity_is_absent_not_configured(repo):
    # loop-check's parser drops an empty peer_identity to None BEFORE the
    # `is_some()` test, so "" leaves the identity-free local gate live. Reading
    # "" as a configured login here would print "none (PR + CI only)" for an
    # armed gate - the wedge this whole file exists to close.
    root = repo('[review]\npeers = ["codex"]\npeer_identity = ""\n')
    line = _done_when_line({}, root)
    assert "peer -> /fno:review peer codex --attest (cross-model only)" in line, line
    assert "none (PR + CI only)" not in line, line


def test_provider_less_peer_arms_nothing(repo):
    # loop-check's `value_as_peers` drops an entry with no provider and no
    # identity, so it holds no gate; printing one would also print a producer
    # command with no provider to run.
    root = repo('[review]\npeers = [""]\n')
    assert _done_when_line({}, root).startswith(
        "PR + CI green + reviewed by [none (PR + CI only)]"
    )


def test_stock_install_announces_the_self_review_floor(repo):
    # A code payload on a lane-less stock install carries the self-review
    # obligation: the done-when line names it and a verb this harness serves.
    root = repo("")
    line = _done_when_line({}, root)
    assert "self-review required for code" in line
    assert "--to-self --raw" in line
    # Opting out restores the plain stock line (no clause).
    root = repo("[review]\nself_review_required = false\n")
    line = _done_when_line({}, root)
    assert "self-review required for code" not in line
    assert line.startswith("PR + CI green + reviewed by [none (PR + CI only)]")
    # A configured local lane already names the reviewer, so no floor clause.
    root = repo('[review]\nreviewers = ["code-review"]\n')
    line = _done_when_line({}, root)
    assert "self-review required for code" not in line


def test_unreadable_review_config_reads_unknown_not_no_gate(repo):
    # A reviewers typo raises out of the Python validator but still declares a
    # gate loop-check will hold. "none (PR + CI only)" would be the same lie.
    # Only the REVIEW half is unknown: PR + CI green holds whatever the config
    # says, so dropping it would understate the gate in the other direction.
    root = repo('[review]\nreviewers = ["sigmaa"]\n')
    assert _done_when_line({}, root) == (
        "PR + CI green + reviewed by [unknown (config.review unreadable) "
        "| resolve: fno config doctor]"
    )


def test_local_peer_producer_names_the_configured_provider(repo):
    # `/review peer` defaults a missing provider to codex and refuses a provider
    # matching the invoking harness, so a bare producer is unrunnable on a
    # codex-authored session whose only peer is gemini.
    root = repo('[review]\npeers = ["gemini"]\n')
    assert "peer -> /fno:review peer gemini --attest (cross-model only)" in _done_when_line({}, root)


def test_multiple_free_peers_offer_the_choice(repo):
    root = repo('[review]\npeers = ["codex", "gemini"]\n')
    line = _done_when_line({}, root)
    assert (
        "peer -> /fno:review peer <provider> --attest (cross-model only, configured: codex, gemini)"
    ) in line, line


def test_undrivable_only_peers_name_the_missing_runner_not_a_producer(repo):
    # loop-check arms the composite `peer` gate on any identity-free entry, but
    # `/review peer` matches its provider by name: `opencode` is discarded and
    # the provider silently falls back to codex. Printing it as a producer is
    # the wedge in its loudest form - a command that runs and reviews on a model
    # nobody configured, or is refused outright on a codex-authored session.
    root = repo('[review]\npeers = ["opencode"]\n')
    line = _done_when_line({}, root)
    assert "no /fno:review peer runner for [opencode]" in line, line
    assert "/fno:review peer opencode --attest" not in line, line
    assert "none (PR + CI only)" not in line, line


def test_registry_reviewer_producer_appends_the_emit_step(repo):
    # A built-in carries its own emit; a project-registered reviewer does not,
    # so following its invocation verbatim would review and leave the gate unmet.
    root = repo(
        "[review]\n"
        'reviewers = ["house-panel"]\n'
        '[review.reviewer_registry.house-panel]\n'
        'kind = "local-attestation"\n'
        'requires = "none"\n'
        'invocation = "/house-panel"\n'
        'asserts = "invocation"\n'
    )
    line = _done_when_line({}, root)
    assert (
        "house-panel -> /house-panel, then "
        "bash skills/review/scripts/emit-attestation.sh house-panel"
    ) in line, line


def test_configured_reviewers_are_announced_with_their_invocation(repo):
    root = repo('[review]\nreviewers = ["sigma"]\n')
    line = _done_when_line({}, root)
    assert "sigma -> /fno:review sigma" in line, line


def test_app_bots_and_local_gates_compose(repo):
    root = repo(
        '[review]\ngithub_apps = ["chatgpt-codex-connector"]\npeers = ["codex"]\n'
    )
    line = _done_when_line({}, root)
    assert "chatgpt-codex-connector" in line, line
    assert "peer -> /fno:review peer codex --attest (cross-model only)" in line, line


def test_advisory_run_is_unchanged(repo):
    root = repo('[review]\npeers = ["codex"]\n')
    assert _done_when_line({"no_ship": "true"}, root) == (
        "advisory: written + eval-green (no PR)"
    )


def test_distinct_logins_are_not_prefix_deduped(repo):
    # Once an entry carries a producer suffix, deduping on the formatted string
    # drops a login whose name is a prefix of one already present. loop-check
    # requires both, so omitting either is the wedge this file closes.
    root = repo(
        "[review]\n"
        'peers = [{provider="codex", identity="bot-extra"}, '
        '{provider="gemini", identity="bot"}]\n'
    )
    line = _done_when_line({}, root)
    assert "bot-extra (" in line, line  # both logins survive dedup
    assert "bot (no --post runner" in line, line


def test_no_external_suppresses_every_login_gate(repo):
    # loop-check skips ALL GitHub-login reads under no_external, so announcing
    # an App bot or an identity-backed peer post sends the session to satisfy a
    # review nothing waits on. Local attestations survive: reviewers_ok is
    # computed independently of that skip.
    root = repo(
        "[review]\n"
        'github_apps = ["chatgpt-codex-connector"]\n'
        'optional_apps = ["some-optional-bot"]\n'
        'peers = ["codex"]\n'
    )
    line = _done_when_line({"no_external": "true"}, root)
    assert "chatgpt-codex-connector" not in line, line
    assert "some-optional-bot" not in line, line
    assert "none (--no-external skips every login gate)" in line, line
    assert "peer -> /fno:review peer codex --attest (cross-model only)" in line, line


def test_identity_backed_login_states_the_cross_model_condition(repo):
    # loop-check swaps a same-model peer login for an unmatchable sentinel, and
    # init does not refuse that case. Deciding WHICH login is affected needs the
    # author harness this file declines, so the line states the condition rather
    # than asserting a clearability it cannot verify.
    root = repo('[review]\npeers = ["codex"]\npeer_identity = "fno-peer-bot"\n')
    assert "cross-model only" in _done_when_line({}, root)


def test_peer_identity_colliding_with_an_app_login_keeps_its_producer(repo):
    # First-seen dedup skipped the peer entirely, so the entry read as an App
    # that posts itself and lost both its producer and its condition.
    root = repo(
        "[review]\n"
        'github_apps = ["shared-bot"]\n'
        'peers = [{provider="codex", identity="shared-bot"}]\n'
    )
    line = _done_when_line({}, root)
    assert "shared-bot (no --post runner" in line, line


def test_shared_identity_names_a_drivable_provider_not_the_first(repo):
    # Under a shared identity only the first provider survived, so a config
    # whose second entry is the only drivable one advertised the undrivable.
    root = repo(
        '[review]\npeers = ["opencode", "codex"]\npeer_identity = "fno-peer-bot"\n'
    )
    line = _done_when_line({}, root)
    assert "peer <pr#> codex --post" in line, line
    assert "opencode" not in line, line


def test_identity_backed_undrivable_provider_names_the_missing_runner(repo):
    # The --post carrier gets the same drivability rule as the local producer.
    root = repo(
        '[review]\npeers = ["opencode"]\npeer_identity = "oc-bot"\n'
    )
    line = _done_when_line({}, root)
    assert "oc-bot (no /fno:review peer runner for [opencode])" in line, line


def test_self_cert_rung_is_marked_as_such(repo):
    # loop-check's block reason marks a self-cert; printing declare like sigma
    # collapses the trust spectrum in the one line meant to show the gate.
    root = repo('[review]\nreviewers = ["declare"]\n')
    assert "[self-cert: asserts no review evidence]" in _done_when_line({}, root)


def test_per_entry_identity_has_no_post_runner(repo):
    # --post posts under the SHARED peer_identity and reads its PAT from
    # peer_token_env; it cannot select a per-entry identity (peer.md
    # preconditions), so advertising --post for one names a command whose
    # helper stops before posting.
    root = repo('[review]\npeers = [{provider="codex", identity="bot-x"}]\n')
    line = _done_when_line({}, root)
    assert "no --post runner" in line, line
    assert "--post;" not in line, line


def test_post_carrier_never_prints_an_alternation(repo):
    # The peer arg parser matches a known provider name, so <a|b> is discarded
    # and the provider falls back to codex - which can refuse a gate the
    # configured gemini would have cleared.
    root = repo(
        '[review]\npeers = ["codex", "gemini"]\npeer_identity = "fno-peer-bot"\n'
    )
    line = _done_when_line({}, root)
    assert "<codex|gemini>" not in line, line
    assert "<provider> --post" in line, line
    assert "configured: codex, gemini" in line, line
