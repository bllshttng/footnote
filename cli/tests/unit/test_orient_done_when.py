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
    assert _done_when_line({}, root) == "PR + CI green + reviewed by [none (PR + CI only)]"


def test_identity_free_peers_announce_the_local_peer_gate_and_its_producer(repo):
    root = repo('[review]\npeers = ["codex", "opencode"]\n')
    line = _done_when_line({}, root)
    assert (
        "peer -> /fno:review peer <provider> --attest (configured: codex, opencode)"
    ) in line, line
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
    assert "reviewed by [fno-peer-bot (post: /fno:review peer <pr#> codex --post)]" in line, line


def test_per_entry_identity_is_a_login_while_a_free_sibling_stays_local(repo):
    # The mixed carrier: loop-check requires BOTH bot-x (login) and the local
    # composite. Announcing "no App bot" here would assert there is no App gate
    # while one is live.
    root = repo(
        '[review]\npeers = [{provider="codex", identity="bot-x"}, "gemini"]\n'
    )
    line = _done_when_line({}, root)
    assert "reviewed by [bot-x (post: /fno:review peer <pr#> codex --post)]" in line, line
    assert "peer -> /fno:review peer gemini --attest" in line, line


def test_empty_peer_identity_is_absent_not_configured(repo):
    # loop-check's parser drops an empty peer_identity to None BEFORE the
    # `is_some()` test, so "" leaves the identity-free local gate live. Reading
    # "" as a configured login here would print "none (PR + CI only)" for an
    # armed gate - the wedge this whole file exists to close.
    root = repo('[review]\npeers = ["codex"]\npeer_identity = ""\n')
    line = _done_when_line({}, root)
    assert "peer -> /fno:review peer codex --attest" in line, line
    assert "none (PR + CI only)" not in line, line


def test_provider_less_peer_arms_nothing(repo):
    # loop-check's `value_as_peers` drops an entry with no provider and no
    # identity, so it holds no gate; printing one would also print a producer
    # command with no provider to run.
    root = repo('[review]\npeers = [""]\n')
    assert _done_when_line({}, root) == (
        "PR + CI green + reviewed by [none (PR + CI only)]"
    )


def test_unreadable_review_config_reads_unknown_not_no_gate(repo):
    # A reviewers typo raises out of the Python validator but still declares a
    # gate loop-check will hold. "none (PR + CI only)" would be the same lie.
    root = repo('[review]\nreviewers = ["sigmaa"]\n')
    assert _done_when_line({}, root) == (
        "unknown (config.review unreadable) | resolve: fno config doctor"
    )


def test_local_peer_producer_names_the_configured_provider(repo):
    # `/review peer` defaults a missing provider to codex and refuses a provider
    # matching the invoking harness, so a bare producer is unrunnable on a
    # codex-authored session whose only peer is gemini.
    root = repo('[review]\npeers = ["gemini"]\n')
    assert "peer -> /fno:review peer gemini --attest" in _done_when_line({}, root)


def test_multiple_free_peers_offer_the_choice(repo):
    root = repo('[review]\npeers = ["codex", "opencode"]\n')
    line = _done_when_line({}, root)
    assert (
        "peer -> /fno:review peer <provider> --attest (configured: codex, opencode)"
    ) in line, line


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
    assert "peer -> /fno:review peer codex --attest" in line, line


def test_advisory_run_is_unchanged(repo):
    root = repo('[review]\npeers = ["codex"]\n')
    assert _done_when_line({"no_ship": "true"}, root) == (
        "advisory: written + eval-green (no PR)"
    )
