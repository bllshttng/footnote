"""Typer CliRunner tests for the fno agents claim CLI surface."""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.claims.cli import cli, _parse_ttl


runner = CliRunner()


@pytest.mark.parametrize(
    ("text", "want"),
    [
        ("60", 60_000),  # bare digits are seconds
        ("5m", 5 * 60_000),
        ("", None),
        ("xyz", "error"),
    ],
)
def test_ttl_parser_table(text, want):
    if want == "error":
        with pytest.raises(Exception):
            _parse_ttl(text)
    else:
        assert _parse_ttl(text) == want


def test_reconcile_pr_reservation_mutex(cwd_tmp):
    """Post-merge ritual reservation: distinct holders race, exactly one wins.

    Pins the mutex the double-fire fix relies on. Two runners (attended +
    dispatched) enter the ritual for the same PR with DISTINCT session-keyed
    holders; the reconcile:pr-<n> claim is the mutex, so exactly one acquires
    (exit 0) and the loser exits 1. A re-acquire with the SAME holder is
    idempotent success - the trap that would silently defeat the mutex if the
    holder were a shared constant, so it is pinned here.

    (`reconcile:` routes to the global claims root; cwd_tmp pins HOME=cwd so the
    global root coincides with the tmp dir and stays isolated.)
    """
    key = "reconcile:pr-286"
    a = runner.invoke(cli, ["acquire", key, "--holder", "postmerge:pr-286:sessA", "--ttl", "15m"])
    assert a.exit_code == 0
    b = runner.invoke(cli, ["acquire", key, "--holder", "postmerge:pr-286:sessB", "--ttl", "15m"])
    assert b.exit_code == 1
    assert "held by" in b.output
    a2 = runner.invoke(cli, ["acquire", key, "--holder", "postmerge:pr-286:sessA", "--ttl", "15m"])
    assert a2.exit_code == 0


# ---------------------------------------------------------------------------
# node: keys auto-resolve the global claims root (ab-fcf9cec5)
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# node: keys cross-check the roster before rendering "free"
#
# The leaf runs inside the bundled fno-agents binary; Python forwards argv and
# re-emits its streams. A monkeypatch cannot cross that subprocess boundary,
# so injection rides the seams that do: a PATH stub of `claude` (the only
# binary the fleet read spawns) prints a canned `claude agents --json`
# payload, CLAUDE_DIR_OVERRIDE pins the transcript root for witness plants,
# FNO_AGENTS_HOME pins the agent registry to an empty dir so the real fleet
# never joins the scan, and FNO_STATE_DIR seeds a graph the overlay join
# reads. Fixtures that lived on the Python reader's warning plumbing (probe
# headroom, unanticipated-warning polarity, a raising registry) have no seam
# across the boundary and are gone; their surviving outcomes are pinned below.
# ---------------------------------------------------------------------------


def _stub_row(name, state, cwd, sid=None):
    """One fleet row in the `claude agents --json` payload dialect."""
    row = {"name": name, "state": state, "cwd": cwd}
    if sid is not None:
        row["sessionId"] = sid
    return row


def _plant_worktree(root, node_id):
    """A linked worktree as the attribution probe reads it: a `.git` file
    (not a dir) plus `.fno/target-state.md` naming the node. Without this a
    row with a session id reads unresolved, not attributed."""

    wt = Path(root) / ("wt-" + node_id)
    (wt / ".fno").mkdir(parents=True)
    (wt / ".git").write_text("gitdir: /nowhere\n")
    (wt / ".fno" / "target-state.md").write_text("graph_node_id: %s\n" % node_id)
    return str(wt)


def _plant_transcript(home, sid, age_s=60):
    """A claude transcript whose last record is a fresh assistant turn: the
    liveness witness the overlay fold consults for an undated row."""
    from datetime import datetime, timedelta, timezone

    proj = home / "projects" / "proj-x"
    proj.mkdir(parents=True, exist_ok=True)
    stamp = (datetime.now(timezone.utc) - timedelta(seconds=age_s)).isoformat()
    record = {
        "type": "assistant",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
        "timestamp": stamp,
    }
    (proj / (sid + ".jsonl")).write_text(json.dumps(record) + "\n")


class _StubFleet:
    """Reinstallable canned fleet. Each call rewrites the payload the stub
    `claude` prints; ``home`` is the transcript root for witness plants."""

    def __init__(self, payload, home):
        self._payload = payload
        self.home = home

    def __call__(self, rows):
        self._payload.write_text(json.dumps({"agents": rows}))


@pytest.fixture
def stub_roster(tmp_path, monkeypatch):
    """A fleet the binary can see. Returns a _StubFleet; call it with rows to
    (re)install the payload, mirroring the old ``fake_roster(rows=[...])``."""
    bin_dir = tmp_path / "stub-bin"
    bin_dir.mkdir()
    payload = bin_dir / "agents.json"
    payload.write_text(json.dumps({"agents": []}))
    script = bin_dir / "claude"
    script.write_text("#!/bin/sh\ncat '%s'\n" % payload)
    script.chmod(0o755)

    home = tmp_path / "claude-home"
    home.mkdir()
    registry = tmp_path / "agents-home"
    registry.mkdir()
    monkeypatch.setenv("PATH", str(bin_dir) + os.pathsep + os.environ.get("PATH", ""))
    monkeypatch.setenv("CLAUDE_DIR_OVERRIDE", str(home))
    monkeypatch.setenv("FNO_AGENTS_HOME", str(registry))
    return _StubFleet(payload, home)


def test_the_crosscheck_leaves_stdout_parseable_as_json(cwd_tmp, stub_roster):
    """`handoff.sh` pipes this command into jq without --json. A prose line on
    stdout broke that read exactly when the claim had lapsed, which is the case
    the operator most needs a truthful holder for. The verdict goes to stderr."""
    import json as _json

    stub_roster([
        _stub_row("t-x76d1-rmtruth", "working",
                  _plant_worktree(cwd_tmp, "x-76d1"), sid="sess-w2")
    ])
    r = runner.invoke(cli, ["status", "node:x-76d1"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert _json.loads(r.stdout)["state"] == "free"
    assert "UNCLAIMED but a live worker" in r.output


def test_a_completeness_warning_degrades_to_a_named_skip(cwd_tmp, stub_roster):
    """Rows that carry no session id are rows the scan cannot vouch for: they
    skip with an advisory summary and the reader answers from the rows it can
    vouch for. The refusal posture once blanked the unplanned queue on one
    stuck no-provenance pane, so an unattributable row now degrades the read
    instead of refusing it. The cwds stay plain: a linked-worktree cwd would
    make the row an attributed advisory instead of a skipped count."""
    stub_roster([
        _stub_row("t-nosid-a", "working", str(cwd_tmp / "plain-a")),
        _stub_row("t-nosid-b", "working", str(cwd_tmp / "plain-b")),
        _stub_row("t-nosid-c", "working", str(cwd_tmp / "plain-c")),
    ])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "roster not consulted" not in r.output
    assert r.exit_code == 0, r.output


def test_handover_refuses_transient_cli_pid(monkeypatch, cwd_tmp):
    """A handover moves a live worker's claim; anchoring it to the transient
    CLI pid strands the node STALE the moment the command exits. From a plain
    shell (no resolvable session pid) and no --ttl, the answer is exit 2 with
    the remedy, not a silently dying claim."""
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_pid", lambda *a, **k: None
    )
    result = runner.invoke(
        cli,
        ["acquire", "node:ab-9", "--holder", "h2", "--handover-from", "h1"],
    )
    assert result.exit_code == 2
    assert "--handover-from needs a durable pid" in result.output


def test_reap_names_the_remedy_when_the_binary_is_missing(monkeypatch):
    monkeypatch.setattr("fno.rust_binary.resolve_binary", lambda: None)

    result = runner.invoke(cli, ["reap"])

    assert result.exit_code == 127
    assert "fno-agents binary was not found" in result.stderr
    assert "FNO_AGENTS_BIN" in result.stderr
