"""The manual `fno event gate-escape` verb + the rebase stale-base nudge (x-91b5).

Covers AC1-ERR (fail-closed enum), AC2-HP (manual tag counts), AC2-INV (dedup),
and AC1-UI (the rebase nudge is advisory-only - a note, never an event).
"""
from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.events.cli import cli

runner = CliRunner()


def _escapes(ev: Path, reason: str | None = None) -> list[dict]:
    if not ev.exists():
        return []
    out = []
    for line in ev.read_text().splitlines():
        if not line.strip():
            continue
        e = json.loads(line)
        if e.get("type") != "gate_escape":
            continue
        if reason is None or e["data"]["reason"] == reason:
            out.append(e)
    return out


def test_ac1_err_unknown_reason_fails_loud_no_event(tmp_path):
    """AC1-ERR: an unknown reason exits non-zero, names the valid reasons, and
    emits nothing."""
    ev = tmp_path / "events.jsonl"
    res = runner.invoke(cli, ["gate-escape", "flek", "--events", str(ev)])
    assert res.exit_code != 0
    assert "flek" in res.output and "allowed" in res.output
    assert not ev.exists()


def test_ac2_hp_manual_tag_counts(tmp_path):
    """AC2-HP: a manual wedge tag lands one gate_escape retro can rank."""
    ev = tmp_path / "events.jsonl"
    res = runner.invoke(
        cli,
        ["gate-escape", "wedge", "--node", "x-1234", "--detail", "freed suspect claim",
         "--events", str(ev)],
    )
    assert res.exit_code == 0
    escapes = _escapes(ev, "wedge")
    assert len(escapes) == 1
    assert escapes[0]["data"]["graph_node_id"] == "x-1234"
    assert escapes[0]["data"]["detail"] == "freed suspect claim"


def test_ac2_inv_dedup_same_key(tmp_path):
    """AC2-INV: three tags with the same explicit dedup key count once."""
    ev = tmp_path / "events.jsonl"
    for _ in range(3):
        runner.invoke(
            cli, ["gate-escape", "spawn-cap", "--dedup-key", "k1", "--events", str(ev)]
        )
    assert len(_escapes(ev, "spawn-cap")) == 1


def test_pr_dedups_a_pr_bearing_escape(tmp_path):
    """A --pr escape dedups on (reason, pr); the second call is a no-op."""
    ev = tmp_path / "events.jsonl"
    for _ in range(2):
        runner.invoke(cli, ["gate-escape", "flake", "--pr", "42", "--events", str(ev)])
    escapes = _escapes(ev, "flake")
    assert len(escapes) == 1
    assert escapes[0]["data"]["pr"] == 42


def test_ac1_ui_rebase_success_prints_nudge_no_event(monkeypatch, capsys):
    """AC1-UI: a successful rebase prints ONE stderr note suggesting the tag
    command, and the nudge emits no event by itself."""
    from fno.pr import _rebase

    monkeypatch.setattr(_rebase, "_phase_a", lambda base, cwd: 0)
    rc = _rebase.run_rebase([], cwd="/tmp")
    assert rc == 0
    err = capsys.readouterr().err
    assert "gate-escape stale-base" in err
    assert err.count("note:") == 1


def test_rebase_failure_no_nudge(monkeypatch, capsys):
    """The nudge is scoped to success: a failed rebase prints no tag note."""
    from fno.pr import _rebase

    monkeypatch.setattr(_rebase, "_phase_a", lambda base, cwd: 2)
    rc = _rebase.run_rebase([], cwd="/tmp")
    assert rc == 2
    assert "gate-escape" not in capsys.readouterr().err
