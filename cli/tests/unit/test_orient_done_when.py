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
    assert "peer -> /fno:review peer --attest" in line, line
    # The old string is the lie: it told the session there was nothing to run.
    assert "none (PR + CI only)" not in line, line


def test_peer_identity_keeps_peers_on_the_github_login_carrier(repo):
    # A shared identity makes every peer a posted-review login, not a local
    # attestation -- loop-check resolves no `peer` reviewer, so neither do we.
    root = repo('[review]\npeers = ["codex"]\npeer_identity = "fno-peer-bot"\n')
    assert "peer ->" not in _done_when_line({}, root)


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
    assert "peer -> /fno:review peer --attest" in line, line


def test_advisory_run_is_unchanged(repo):
    root = repo('[review]\npeers = ["codex"]\n')
    assert _done_when_line({"no_ship": "true"}, root) == (
        "advisory: written + eval-green (no PR)"
    )
