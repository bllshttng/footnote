"""x-8bfb wave 2, task 2.2: the `state`/`status` alias-conflict WARN stops
firing on a disagreement the parser's own documented precedence already
resolves. Measured on a real fleet: 13-16 of the ~18-21 total WARN lines were
this shape, on rows where BOTH sides were individually understood values
(e.g. state='working'/status='idle') - the exact shape `_STATUS_KEYS`'s own
comment calls expected on claude 2.1.220's live-pid rows.

`state` must keep winning (never `status`); this task changes only whether
the resolver SPEAKS about a disagreement it already knows how to resolve.
"""
from __future__ import annotations

import json
from types import SimpleNamespace

from fno.agents.harnesses import claude as claude_mod


def _fake_completed(stdout: str = "", stderr: str = "", returncode: int = 0):
    return SimpleNamespace(stdout=stdout, stderr=stderr, returncode=returncode)


def test_recognized_disagreement_is_suppressed(monkeypatch):
    # AC6-HP + AC9-FR: state='working'/status='idle' both individually
    # resolve to a KNOWN live status. `state` still wins (unchanged
    # resolution) but the disagreement no longer warns.
    payload = [{"id": "aaaaaaa1", "state": "working", "status": "idle"}]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {"aaaaaaa1": {"live_status": "Working"}}
    assert warnings == [], warnings


def test_agreement_after_normalization_stays_silent(monkeypatch):
    # Regression: two spellings of the SAME meaning (busy/working) already
    # normalized equal before this task; must stay silent.
    payload = [{"id": "aaaaaaa2", "state": "working", "status": "busy"}]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {"aaaaaaa2": {"live_status": "Working"}}
    assert warnings == [], warnings


def test_unrecognized_side_still_warns(monkeypatch):
    # AC8-EDGE: one alias holds a recognized value, the other an unknown one
    # - suppression covers agreement-after-precedence only, never a real
    # unknown, so the conflict warning still fires.
    payload = [{"id": "aaaaaaa3", "state": "flibberty", "status": "idle"}]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    # state still wins (unchanged resolution order) - the value is now
    # unrecognized so it passes through raw rather than normalizing.
    assert result == {"aaaaaaa3": {"live_status": "flibberty"}}
    assert any("conflicting values across aliases" in w for w in warnings), warnings


def test_short_id_alias_conflict_unaffected(monkeypatch):
    # AC9-FR non-goal: the short-id call site never passes `recognized`, so
    # its always-warn-on-difference behavior is untouched by this task.
    payload = {"agents": [{"short_id": "aaaaaaaa", "id": "bbbbbbbb", "state": "working"}]}

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    result, warnings = claude_mod.claude_agents_json()

    assert result == {"aaaaaaaa": {"live_status": "Working"}}
    assert any("conflicting values across aliases" in w for w in warnings), warnings


def test_conflict_warning_dedupes_per_distinct_disagreement(monkeypatch):
    # Same shape the unrecognized-status warning already dedupes to: many
    # rows sharing one disagreement (with a genuinely unknown side, so it
    # still fires) collapse to one line, not one per row.
    payload = [
        {"id": f"row{i:04d}", "state": "flibberty", "status": "idle"} for i in range(4)
    ]

    def _fake(argv, **kwargs):  # noqa: ARG001
        return _fake_completed(stdout=json.dumps(payload))

    monkeypatch.setattr(claude_mod, "_subprocess_run", _fake)
    _, warnings = claude_mod.claude_agents_json()

    matches = [w for w in warnings if "conflicting values across aliases" in w]
    assert len(matches) == 1, warnings
