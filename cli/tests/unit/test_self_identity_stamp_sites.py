"""Durable stamp sites resolve identity through the owned path, not precedence.

`resolve_harness_identity` returns the first ambient harness marker in
precedence order. That is right when exactly one harness family is present and
wrong the moment a process inherits a foreign one, so a site that WRITES the
resolved value onto a durable record must use `resolve_self_identity`, which
proves ownership from the process tree and refuses rather than guessing.

`scripts/ci/check-identity-stamp-sites.sh` keeps the remainder from growing.
These tests pin the behavior the gate cannot see.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.claims.self_identity import resolve_self_identity

ROOT = Path(__file__).resolve().parents[3]

# Every site whose resolved harness or session id lands on a record that
# outlives the process: a claim, an event, an agent-state row, a spawn lineage
# edge, a question event, a claim holder string.
STAMP_SITES = (
    "cli/src/fno/claims/core.py",
    "cli/src/fno/events/cli.py",
    "cli/src/fno/agent/state.py",
    "cli/src/fno/agents/dispatch.py",
    "cli/src/fno/outstanding/cli.py",
    "cli/src/fno/target_cli.py",
    "cli/src/fno/mail/cli.py",
)


@pytest.mark.parametrize("rel", STAMP_SITES)
def test_stamp_site_uses_the_owned_resolver(rel):
    # A resolver nothing calls would pass every behavior test while the stamp
    # sites kept the precedence primitive.
    assert "self_identity" in (ROOT / rel).read_text(encoding="utf-8"), rel


def test_a_single_marker_resolves_exactly_as_precedence_would(monkeypatch):
    # The dominant case, and the one that must not change for anyone who never
    # had a leak: one harness family present resolves to it, byte-identical to
    # the raw primitive.
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda *a, **k: "claude"
    )
    env = {"CLAUDE_CODE_SESSION_ID": "abc123"}

    ident = resolve_self_identity(env)

    from fno.harness_identity import resolve_harness_identity

    precedence = resolve_harness_identity(env)
    assert (ident.session_id, ident.harness) == (
        precedence.session_id,
        precedence.harness,
    )
    assert ident.disposition == "single"


def test_a_mixed_env_the_tree_can_decide_resolves_rather_than_refusing(monkeypatch):
    # The incident: a claude session carrying a CODEX_THREAD_ID it inherited
    # from its spawner. The raw primitive no longer launders that into the
    # stranger's id - it refuses the whole mixed env - so nothing is stamped
    # wrong either way. What the owned path adds is the RIGHT answer: the
    # process tree proves which marker this session minted, so a real worker
    # stamps its own identity instead of going unstamped.
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda *a, **k: "claude"
    )
    env = {"CODEX_THREAD_ID": "foreign-thread", "CLAUDE_CODE_SESSION_ID": "mine"}

    from fno.harness_identity import resolve_harness_identity

    assert resolve_harness_identity(env).session_id is None

    ident = resolve_self_identity(env)
    assert (ident.session_id, ident.harness) == ("mine", "claude")
    assert ident.disposition == "proven"


def test_an_unprovable_disagreement_stamps_nothing(monkeypatch):
    # No harness ancestor (CI, cron, a bare shell) and two families: genuinely
    # undecidable, so both fields come back None and every caller's existing
    # "no ambient identity" branch becomes the refusal.
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda *a, **k: None
    )

    ident = resolve_self_identity(
        {"CODEX_THREAD_ID": "theirs", "CLAUDE_CODE_SESSION_ID": "mine"}
    )

    assert (ident.session_id, ident.harness) == (None, None)
    assert ident.disposition == "ambiguous"
    # The markers travel with the refusal, so a caller can name both in its
    # message rather than reporting a bare absence.
    assert {marker for marker, _h, _v in ident.markers_present} == {
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
    }
