"""Typer CliRunner tests for the fno agents claim CLI surface."""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
import os

import pytest
from typer.testing import CliRunner

from fno.claims.cli import cli, _parse_ttl
from fno.claims.core import acquire_claim

from .test_claim_reap import _dead_pid  # noqa: F401
from fno.graph.store import read_graph_strict


runner = CliRunner()


@pytest.mark.parametrize(
    ("text", "want"),
    [
        ("60", 60_000),  # bare digits are seconds
        ("60s", 60_000),
        ("5m", 5 * 60_000),
        ("2h", 2 * 3_600_000),
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


def test_acquire_json_output(cwd_tmp):
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["key"] == "k"
    assert parsed["holder"] == "h"


def test_acquire_conflict_exits_1(cwd_tmp):
    pid = str(os.getpid())
    runner.invoke(cli, ["acquire", "k", "--holder", "h1", "--pid", pid])
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h2", "--pid", pid])
    assert result.exit_code == 1
    assert "held by" in result.output


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


def test_acquire_validation_exits_2(cwd_tmp):
    """key too long -> exit 2."""
    result = runner.invoke(cli, ["acquire", "x" * 300, "--holder", "h"])
    assert result.exit_code == 2


def test_acquire_with_ttl(cwd_tmp):
    result = runner.invoke(
        cli,
        ["acquire", "k", "--holder", "h", "--ttl", "1h", "--pid-unavailable", "--json"],
    )
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["expires_at"] is not None
    assert parsed["pid"] is None
    assert parsed["pid_unavailable"] is True


def test_acquire_omitted_pid_records_an_anchor(cwd_tmp):
    # An omitted --pid records a process anchor: the durable session walk's
    # answer when a harness ancestor exists, the calling process otherwise
    # (the documented degrade). The walk-proven case is characterized in
    # crates/fno-agents/tests/claim_acquire_parity.rs; the suite runs
    # harness-neutral, so here the anchor is simply present and the claim
    # never reads pid-unavailable without the flag.
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["pid"] is not None
    # to_yaml_dict omits the flag when False; a pid-anchored record must not
    # carry it at all.
    assert "pid_unavailable" not in parsed


def test_acquire_omitted_pid_with_ttl_is_explicitly_unavailable(cwd_tmp, monkeypatch):
    # No agent ancestor (standalone use) -> a TTL claim records positive PID
    # absence instead of naming the transient CLI process.
    monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid",
                        lambda from_pid=None: None)
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--ttl", "1h", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["pid"] is None
    assert parsed["pid_unavailable"] is True


def test_acquire_explicit_pid_overrides_session_default(cwd_tmp, monkeypatch):
    # An explicit --pid always wins; resolve_session_pid is never consulted.
    called = {"n": 0}

    def _should_not_run(from_pid=None):
        called["n"] += 1
        return 4242

    monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid", _should_not_run)
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h",
                                 "--pid", str(os.getppid()), "--json"])
    assert result.exit_code == 0
    assert json.loads(result.output)["pid"] == os.getppid()
    assert called["n"] == 0


def test_acquire_invalid_ttl_format(cwd_tmp):
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--ttl", "garbage"])
    assert result.exit_code != 0


def test_acquire_pid_flag_anchors_liveness_to_given_pid(cwd_tmp):
    """--pid pins PID-liveness to a long-lived owner instead of this process
    (ab-6d5afbde: the daemon's stream-claim shelled `fno agents claim acquire`, whose
    ephemeral PID died at once and read the claim stale on write)."""
    result = runner.invoke(
        cli, ["acquire", "session:uuid-x", "--holder", "stream:sw7", "--pid", "99999", "--json"]
    )
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["pid"] == 99999, "the claim must record the explicit --pid, not os.getpid()"


def test_release_after_acquire(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    result = runner.invoke(cli, ["release", "k", "--holder", "h"])
    assert result.exit_code == 0
    assert "released" in result.output


def test_release_strict_mismatch_exits_4(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h1"])
    result = runner.invoke(cli, ["release", "k", "--holder", "h2", "--strict"])
    assert result.exit_code == 4


def test_release_no_claim_reports_no_op(cwd_tmp):
    """No file for the key means release_claim returns None: nothing was
    unlinked. The old receipt printed 'released: <key>' anyway; that false
    positive is the exact gap x-2146 traces 386 unclosed do rows to. The
    verb stays idempotent (exit 0), only the words change."""
    result = runner.invoke(cli, ["release", "node:never-acquired", "--holder", "h"])
    assert result.exit_code == 0
    assert "no-op" in result.output
    assert "released:" not in result.output


def test_release_no_claim_json_reports_released_false(cwd_tmp):
    result = runner.invoke(cli, ["release", "node:never-acquired", "--holder", "h", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed == {"key": "node:never-acquired", "released": False}


def test_release_stamp_do_no_op_on_non_node_key_is_silent(cwd_tmp):
    """A do row was never in play for a non-node: key (only node: keys stamp),
    so the no-op skip message must not fire either - it would falsely imply a
    do row existed for this key. The node-keyed skip lines themselves are the
    parity goldens' contract (claim_release_parity stamp_do_noop_release)."""
    result = runner.invoke(
        cli, ["release", "dispatch:never-acquired", "--holder", "h", "--stamp-do"]
    )
    assert result.exit_code == 0
    assert "do stamp skipped" not in result.output


def test_status_free(cwd_tmp):
    result = runner.invoke(cli, ["status", "session:nothing", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["state"] == "free"


def test_status_colonless_garbage_refuses(cwd_tmp):
    result = runner.invoke(cli, ["status", "nothing", "--json"])
    assert result.exit_code == 2
    assert "claim key must include a recognized prefix" in result.output


def test_status_live(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h", "--pid", str(os.getpid())])
    result = runner.invoke(cli, ["status", "k", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["state"] == "live"
    assert parsed["holder"] == "h"


def test_list_empty(cwd_tmp):
    result = runner.invoke(cli, ["list", "--json"])
    assert result.exit_code == 0
    assert json.loads(result.output) == []


def test_list_with_prefix(cwd_tmp):
    pid = str(os.getpid())
    runner.invoke(cli, ["acquire", "node:ab-1", "--holder", "h", "--pid", pid])
    runner.invoke(cli, ["acquire", "fleet:m1", "--holder", "h", "--pid", pid])
    result = runner.invoke(cli, ["list", "--prefix", "node:", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    keys = [r["key"] for r in parsed]
    assert keys == ["node:ab-1"]


def test_list_prefix_node_scans_global_root_once(cwd_tmp):
    """An explicit global --prefix still resolves the global root directly;
    the merge in list_cmd must not duplicate its rows."""
    runner.invoke(cli, ["acquire", "node:ab-1", "--holder", "h", "--pid", str(os.getpid())])
    result = runner.invoke(cli, ["list", "--prefix", "node:", "--json"])
    assert result.exit_code == 0
    keys = [r["key"] for r in json.loads(result.output)]
    assert keys == ["node:ab-1"]


def test_force_release_succeeds(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    result = runner.invoke(cli, ["release", "k", "--force", "--reason", "operator override"])
    assert result.exit_code == 0


def test_force_release_empty_reason_exits_2(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    result = runner.invoke(cli, ["release", "k", "--force", "--reason", ""])
    assert result.exit_code == 2


def test_force_release_rejects_a_holder(cwd_tmp):
    """--force drops the claim regardless of owner, so --holder is meaningless.

    Accepting both silently would read as "release it if I hold it, else force",
    which is two different operations behind one invocation.
    """
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    result = runner.invoke(
        cli, ["release", "k", "--force", "--reason", "why", "--holder", "h"]
    )
    assert result.exit_code == 2


def test_a_flag_from_another_mode_is_refused_not_ignored(cwd_tmp):
    """Each collapsed mode refuses the flags that belong to a sibling mode.

    Silently ignoring them is the failure the collapse can introduce: exit 0
    saying it worked while the lane cap was never applied, the override reason
    never recorded, or the do row never stamped.
    """
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    for argv in (
        ["acquire", "k", "--holder", "h", "--max-lanes", "3"],  # cap without a lane
        ["acquire", "--lane", "L", "--max-lanes", "3", "--holder", "h"],
        ["release", "k", "--holder", "h", "--reason", "why"],  # reason without --force
        ["release", "k", "--force", "--reason", "why", "--stamp-do"],
        ["release", "--lane", "L", "--strict"],
    ):
        result = runner.invoke(cli, argv)
        assert result.exit_code == 2, f"{argv} was accepted: {result.output}"


def test_refresh_pid_liveness_is_noop(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h", "--pid", str(os.getpid())])  # PID-liveness
    result = runner.invoke(cli, ["refresh", "k", "--holder", "h"])
    assert result.exit_code == 0
    assert "no-op" in result.output or "PID-liveness" in result.output


def test_refresh_ttl_extends(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h", "--ttl", "1m"])
    result = runner.invoke(cli, ["refresh", "k", "--holder", "h", "--ttl", "5m", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["expires_at"] is not None


def test_refresh_missing_exits_3(cwd_tmp):
    result = runner.invoke(cli, ["refresh", "missing", "--holder", "h"])
    assert result.exit_code == 3


# ---------------------------------------------------------------------------
# node: keys auto-resolve the global claims root (ab-fcf9cec5)
# ---------------------------------------------------------------------------

def test_status_node_key_finds_global_claim_without_env(tmp_path, monkeypatch):
    """`fno agents claim status node:<id>` from a project cwd, with no
    FNO_CLAIMS_ROOT exported, must find a node claim written to the
    global root (~/.fno/claims) - the operator runbook path."""
    from fno.claims.core import acquire_claim

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    # Acquire a live node claim at the GLOBAL root (root=home -> home/.fno/claims).
    acquire_claim(key="node:ab-deadbeef", holder="target-session:s", ttl_ms=3_600_000, root=home)

    # Run the CLI from a DIFFERENT cwd (a project checkout) with no env override.
    proj = tmp_path / "proj"
    proj.mkdir()
    monkeypatch.chdir(proj)
    r = runner.invoke(cli, ["status", "ab-deadbeef", "--json"])
    assert r.exit_code == 0, r.output
    info = json.loads(r.output)
    assert info["key"] == "node:ab-deadbeef"
    assert info["state"] == "live", info
    assert info["holder"] == "target-session:s"


def test_list_node_prefix_finds_global_claims_without_env(tmp_path, monkeypatch):
    """`fno agents claim list --prefix node:` resolves the global root too."""
    from fno.claims.core import acquire_claim

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    acquire_claim(key="node:ab-deadbeef", holder="h", ttl_ms=3_600_000, root=home)

    proj = tmp_path / "proj"
    proj.mkdir()
    monkeypatch.chdir(proj)
    r = runner.invoke(cli, ["list", "--prefix", "node:", "--json"])
    assert r.exit_code == 0, r.output
    keys = [c["key"] for c in json.loads(r.output)]
    assert "node:ab-deadbeef" in keys


def test_release_stamp_do_writes_the_do_window(tmp_path, monkeypatch):
    """--stamp-do on a node claim release writes the do row: started_at from the
    claim's acquire time, ended_at at the release instant - the third choke point.
    Gated to the session's own release (the flag), so a handoff release that does
    not pass it records nothing."""
    import fno.paths
    from fno.claims.core import acquire_claim

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-do-1")
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)

    g = tmp_path / "graph.json"
    seed_graph(g, '{"entries": [{"id": "ab-dotest", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)
    # The wave-2 leaf stamps inside the fno-agents binary, so the store is
    # reached through the state dir, not the in-process graph_json patch (an
    # env var crosses the subprocess boundary; a monkeypatch cannot).
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path))

    acquire_claim(key="node:ab-dotest", holder="target-session:s",
                  ttl_ms=3_600_000, root=home)

    stamped = runner.invoke(
        cli, ["release", "node:ab-dotest", "--holder", "target-session:s", "--stamp-do"]
    )
    assert stamped.exit_code == 0, stamped.output
    rows = read_graph_strict(g)[0].get("sessions", [])
    do = [x for x in rows if x.get("phase") == "execute"]
    assert len(do) == 1
    assert do[0]["harness"] == "claude"
    # owned (holder) session wins over the ambient CLAUDE_CODE_SESSION_ID
    assert do[0]["session_id"] == "s"
    assert do[0]["started_at"] and do[0]["ended_at"]
    assert do[0]["started_at"] <= do[0]["ended_at"]


def test_declined_handover_names_its_reason_before_falling_through(tmp_path, monkeypatch):
    """AC5-EDGE: a declined handover fell through to the ordinary
    acquire with a bare pass, so the caller saw only "held by <holder>" and
    could not tell its own handover claim from a foreign one. The reason now
    reaches stderr; the fall-through behavior (refuse, non-zero) is unchanged."""
    from fno.claims.core import acquire_claim

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-dh-1")
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)

    acquire_claim(key="node:ab-decline", holder="spawn-handover:t-worker",
                  ttl_ms=900_000, root=home)
    out = runner.invoke(cli, [
        "acquire", "node:ab-decline", "--holder", "target-session:w",
        "--handover-from", "spawn-handover:OTHER", "--ttl", "2h",
    ])
    assert out.exit_code != 0, out.output
    assert "handover declined:" in out.output, out.output
    assert "holder mismatch" in out.output, out.output
    assert "held by spawn-handover:t-worker" in out.output, out.output


def test_acquire_opens_do_provenance_row(tmp_path, monkeypatch):
    """A node claim acquire opens the do row with started_at from the claim's
    acquire time and NO ended_at - so a session killed before its release
    terminal still leaves a started row instead of reading unstarted (the
    killed-mid-phase specimen: PR open and green while the node showed only a
    blueprint row)."""

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-acq-1")
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)

    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / "state"))
    g = tmp_path / "state" / "db" / "graph.json"
    seed_graph(g, '{"entries": [{"id": "ab-acqtest", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')

    acq = runner.invoke(
        cli, ["acquire", "node:ab-acqtest", "--holder", "target-session:s", "--ttl", "1h"]
    )
    assert acq.exit_code == 0, acq.output
    rows = read_graph_strict(g)[0].get("sessions", [])
    do = [x for x in rows if x.get("phase") == "execute"]
    assert len(do) == 1
    assert do[0]["harness"] == "claude"
    # owned (holder) session wins over the ambient CLAUDE_CODE_SESSION_ID
    assert do[0]["session_id"] == "s"
    assert do[0]["started_at"]
    # The typed store emits the full envelope: an open row's end fields are null.
    assert do[0].get("ended_at") is None  # opened, not closed


def test_acquire_then_release_closes_do_window(tmp_path, monkeypatch):
    """Acquire opens the do row (started_at, no end); release --stamp-do fills
    ended_at on the SAME row via duplicate-fill - the merge that makes
    acquire-time stamping safe without losing the release window or adding a
    second row."""

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-acq-2")
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)

    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / "state"))
    g = tmp_path / "state" / "db" / "graph.json"
    seed_graph(g, '{"entries": [{"id": "ab-acqrel", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')

    acq = runner.invoke(
        cli, ["acquire", "node:ab-acqrel", "--holder", "target-session:s", "--ttl", "1h"]
    )
    assert acq.exit_code == 0, acq.output
    rel = runner.invoke(
        cli, ["release", "node:ab-acqrel", "--holder", "target-session:s", "--stamp-do"]
    )
    assert rel.exit_code == 0, rel.output
    rows = read_graph_strict(g)[0].get("sessions", [])
    do = [x for x in rows if x.get("phase") == "execute"]
    assert len(do) == 1  # one row, not two - release closed the acquire row
    assert do[0]["started_at"] and do[0]["ended_at"]
    assert do[0]["started_at"] <= do[0]["ended_at"]


def _do_graph(tmp_path, monkeypatch, node_id, session_marker):
    """A one-node graph wired as fno.paths.graph_json, with a clean claude
    ambient identity. Returns the graph path."""

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", session_marker)
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("FNO_STATE_DIR", str(tmp_path / "state"))
    g = tmp_path / "state" / "db" / "graph.json"
    seed_graph(
        g,
        [{"id": node_id, "title": "t", "domain": "code", "project": "p"}],
    )
    return g


def _do_rows(graph_path):
    return [
        x for x in read_graph_strict(graph_path)[0].get("sessions", [])
        if x.get("phase") == "execute"
    ]


def test_release_rollback_do_removes_the_open_acquire_row(tmp_path, monkeypatch):
    """A worker whose POST-acquire validation refuses it did no work, so the row
    its acquire opened must not survive the refusal - otherwise the node reads as
    permanently in progress for a phase that never ran. This is the init
    acquire-then-check-contained rollback path."""
    g = _do_graph(tmp_path, monkeypatch, "ab-rollback", "sess-rb-1")

    acq = runner.invoke(
        cli, ["acquire", "node:ab-rollback", "--holder", "target-session:s", "--ttl", "1h"]
    )
    assert acq.exit_code == 0, acq.output
    assert len(_do_rows(g)) == 1  # the row the refusal must undo

    rel = runner.invoke(
        cli, ["release", "node:ab-rollback", "--holder", "target-session:s", "--rollback-do"]
    )
    assert rel.exit_code == 0, rel.output
    assert _do_rows(g) == []


def test_rollback_do_never_removes_a_closed_row(tmp_path, monkeypatch):
    """A row carrying ended_at recorded a finished window. A later acquire +
    rollback (the same session refused on a second run) must leave it intact -
    the rollback undoes an open row, never real provenance."""
    g = _do_graph(tmp_path, monkeypatch, "ab-rbclosed", "sess-rb-2")

    runner.invoke(
        cli, ["acquire", "node:ab-rbclosed", "--holder", "target-session:s", "--ttl", "1h"]
    )
    closed = runner.invoke(
        cli, ["release", "node:ab-rbclosed", "--holder", "target-session:s", "--stamp-do"]
    )
    assert closed.exit_code == 0, closed.output
    assert _do_rows(g)[0]["ended_at"]

    runner.invoke(
        cli, ["acquire", "node:ab-rbclosed", "--holder", "target-session:s", "--ttl", "1h"]
    )
    rel = runner.invoke(
        cli, ["release", "node:ab-rbclosed", "--holder", "target-session:s", "--rollback-do"]
    )
    assert rel.exit_code == 0, rel.output
    rows = _do_rows(g)
    assert len(rows) == 1
    assert rows[0]["ended_at"]  # untouched


def test_stamp_do_and_rollback_do_are_mutually_exclusive(tmp_path, monkeypatch):
    """One records a finished window, the other removes a row for work that never
    ran. Passing both is a caller bug, refused before the claim is touched."""
    _do_graph(tmp_path, monkeypatch, "ab-rbboth", "sess-rb-4")

    r = runner.invoke(
        cli, ["release", "node:ab-rbboth", "--holder", "target-session:s",
              "--stamp-do", "--rollback-do"]
    )
    assert r.exit_code == 2, r.output
    assert "mutually exclusive" in r.output


def test_release_without_stamp_do_writes_no_provenance(tmp_path, monkeypatch):
    """A bare release (the handoff path) records nothing - the do window would
    mis-attribute the predecessor under the successor's identity."""
    import fno.paths
    from fno.claims.core import acquire_claim

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-do-2")
    g = tmp_path / "graph.json"
    seed_graph(g, '{"entries": [{"id": "ab-dotest2", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)

    acquire_claim(key="node:ab-dotest2", holder="target-session:s",
                  ttl_ms=3_600_000, root=home)
    bare = runner.invoke(
        cli, ["release", "node:ab-dotest2", "--holder", "target-session:s"]
    )
    assert bare.exit_code == 0, bare.output
    assert read_graph_strict(g)[0].get("sessions", []) == []


def test_non_node_key_uses_cwd_not_global(tmp_path, monkeypatch):
    """A non-node key keeps the cwd default - a node claim at the global root
    must NOT leak into a cwd-scoped lookup of a different key."""
    from fno.claims.core import acquire_claim

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    acquire_claim(key="node:ab-deadbeef", holder="h", ttl_ms=3_600_000, root=home)

    proj = tmp_path / "proj"
    proj.mkdir()
    monkeypatch.chdir(proj)
    # walker: key resolves to cwd; nothing acquired there -> free.
    r = runner.invoke(cli, ["status", "walker:/some/root", "--json"])
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["state"] == "free"


# ---------------------------------------------------------------------------
# node: keys cross-check the roster before rendering "free" (x-cd1e)
# ---------------------------------------------------------------------------


def _row(name, state, node, cwd="/tmp/wt"):
    from fno.agents.watchdog import Row

    return Row(row_id=name, name=name, state=state, node=node, cwd=cwd)


@pytest.fixture
def fake_roster(monkeypatch):
    """Install a roster reading. ``rows``/``warnings`` mimic fleet_rows."""

    def _install(rows=(), warnings=(), raises=None):
        def _fake(*_a, **_kw):
            if raises is not None:
                raise raises
            return list(rows), list(warnings)

        monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _fake)

    return _install


def test_free_node_with_a_live_worker_says_so(cwd_tmp, fake_roster):
    """The state that produced tonight's duplicate PR must not print the same
    word as an idle node. Asserts the positive string, never the absence of
    the word free."""
    fake_roster(rows=[_row("t-x76d1-rmtruth", "working", "x-76d1")])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert r.exit_code == 0, r.output
    assert "UNCLAIMED but a live worker is on this node: t-x76d1-rmtruth" in r.output


def test_free_node_with_nobody_names_the_scan_it_consulted(cwd_tmp, fake_roster):
    """The row count is the positive marker: a scan of 40 rows finding nothing
    is a different answer from a scan that never ran, and both used to print
    the identical word."""
    fake_roster(rows=[_row("t-other", "working", "x-other")])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "free, no live worker found (roster scanned: 1 rows)" in r.output


def test_free_node_reports_unresolved_roster_rows(cwd_tmp, fake_roster):
    fake_roster(rows=[_row("t-x76d1-near-miss", "working", None, "/tmp/x-76d1")])
    r = runner.invoke(cli, ["status", "node:x-76d1", "--json"])
    assert r.exit_code == 0
    info = json.loads(r.output)
    assert info["state"] == "unknown"
    assert info["basis"] == "unresolved-roster-row"
    assert info["roster_rows_scanned"] == 1
    assert info["roster_rows_unresolved"] == 1
    assert info["roster_unresolved_candidates"] == ["t-x76d1-near-miss"]


def test_free_node_names_worktree_near_miss_in_human_line(cwd_tmp, fake_roster):
    fake_roster(rows=[_row("t-x76d1-near-miss", "working", None, "/tmp/x-76d1")])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "no row resolved to this node" in r.output
    assert "t-x76d1-near-miss" in r.output
    assert "fno agents peek t-x76d1-near-miss" in r.output


def test_the_crosscheck_leaves_stdout_parseable_as_json(cwd_tmp, fake_roster):
    """`handoff.sh` pipes this command into jq without --json. A prose line on
    stdout broke that read exactly when the claim had lapsed, which is the case
    the operator most needs a truthful holder for. The verdict goes to stderr."""
    import json as _json

    fake_roster(rows=[_row("t-x76d1-rmtruth", "working", "x-76d1")])
    r = runner.invoke(cli, ["status", "node:x-76d1"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert _json.loads(r.stdout)["state"] == "free"
    assert "UNCLAIMED but a live worker" in r.output


def test_a_latency_notice_does_not_discard_a_complete_roster(cwd_tmp, fake_roster):
    """The headroom notice fires at half the budget on a probe that RETURNED
    every row, and read_roster asks for 10s, so it trips at 5.0s. Treating it as
    a failed instrument threw the full listing away: `claim status` printed
    "roster not consulted" forever and the abandonment probe answered None for
    every SUSPECT claim, so nothing was reaped again."""
    from fno.agents.watchdog import HEADROOM_WARNING_PREFIX

    fake_roster(
        rows=[_row("t-x76d1-rmtruth", "working", "x-76d1")],
        warnings=[f"{HEADROOM_WARNING_PREFIX}took 5.4s of its 10s budget"],
    )
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert r.exit_code == 0, r.output
    assert "UNCLAIMED but a live worker is on this node" in r.output
    assert "roster not consulted" not in r.output


def test_a_completeness_warning_still_degrades(cwd_tmp, fake_roster):
    """The other half of the pair. A dropped-row warning IS a partial list, and
    a truncated scan must never read as authoritative. It carries no advisory
    marker, which is what makes it block."""
    fake_roster(rows=[_row("t-other", "working", "x-other")],
                warnings=["3 row(s) carried no session id, unmeasurable, skipped"])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "roster not consulted" in r.output
    assert "carried no session id" in r.output


def test_an_unmapped_row_state_does_not_degrade_the_reading(cwd_tmp, fake_roster):
    """A status spelling claude has not shipped before is a fidelity note on a
    row that IS in the listing. Blocking on it printed "roster not consulted"
    forever and answered None for every SUSPECT claim, so nothing was reaped.

    An unmapped word is no liveness evidence either: the row reads unmeasured,
    never engaged-by-default - the old conservative alarm fired on a label
    nobody had mapped, which is a verdict from a word, not a measurement."""
    from fno.agents.watchdog import ADVISORY_WARNING_PREFIX

    fake_roster(
        rows=[_row("t-x76d1-rmtruth", "frobnicating", "x-76d1")],
        warnings=[f"{ADVISORY_WARNING_PREFIX}unmapped row state 'frobnicating'"],
    )
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "unmeasured, never live" in r.output
    assert "t-x76d1-rmtruth" in r.output
    assert "roster not consulted" not in r.output
    assert "UNCLAIMED but a live worker" not in r.output


def test_an_unanticipated_warning_degrades_by_default(cwd_tmp, fake_roster):
    """The polarity, pinned. A warning nobody has thought about yet is exactly
    the one that must not be waved through, so the marker is on the harmless
    ones and everything else blocks."""
    fake_roster(rows=[_row("t-x76d1-rmtruth", "working", "x-76d1")],
                warnings=["something nobody has written a branch for yet"])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "roster not consulted" in r.output


def test_a_lying_done_row_still_raises_the_alarm(cwd_tmp, fake_roster, monkeypatch):
    """The roster called a WORKING session done on 2026-08-15, which is the
    incident `_TERMINAL_STATES` carries a warning about. A transcript that is
    positively still moving overrules the row, so an operator deciding whether
    to staff this node is told a worker is on it."""
    import time as _t

    from fno.agents.watchdog import TailFacts

    monkeypatch.setattr(
        "fno.agents.watchdog.tail_facts",
        lambda *_a, **_kw: TailFacts(
            records=None, last_event_epoch=_t.time() - 60,
            tail_text="", last_role="assistant", last_text="working the task",
            pr_polls=None,
        ),
    )
    fake_roster(rows=[_row("t-x76d1-rmtruth", "done", "x-76d1")])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "UNCLAIMED but a live worker is on this node" in r.output


def test_an_aged_out_transcript_leaves_the_row_standing(cwd_tmp, fake_roster, monkeypatch):
    """The other direction, and it is deliberately NOT what the reap probe does.
    A wrong reap archives a live worker's claim; a wrong line here is an alarm
    on an empty node, and one that fires on every finished session whose
    transcript has aged out teaches operators to ignore the alarm."""
    import time as _t

    from fno.agents.watchdog import TailFacts

    monkeypatch.setattr(
        "fno.agents.watchdog.tail_facts",
        lambda *_a, **_kw: TailFacts(
            records=None, last_event_epoch=_t.time() - 11 * 3600,
            tail_text="", last_role="assistant", last_text="...", pr_polls=None,
        ),
    )
    fake_roster(rows=[_row("t-x76d1-rmtruth", "done", "x-76d1")])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "no live worker found" in r.output
    assert "UNCLAIMED but a live worker" not in r.output


def test_roster_read_failure_never_renders_a_clean_free(cwd_tmp, fake_roster):
    """An instrument that did not run must not render as an answer."""
    fake_roster(rows=[], warnings=["claude binary not found on PATH"])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "free, roster not consulted (claude binary not found on PATH)" in r.output
    assert "no live worker found" not in r.output


def test_roster_raising_degrades_loudly(cwd_tmp, fake_roster):
    fake_roster(raises=RuntimeError("registry exploded"))
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert r.exit_code == 0, r.output
    assert "roster not consulted (RuntimeError: registry exploded)" in r.output


def test_an_honestly_empty_fleet_is_not_a_failed_read(cwd_tmp, fake_roster):
    """No rows AND no warning is a real zero. Reporting it as 'not consulted'
    would rebuild the ambiguity one layer up."""
    fake_roster(rows=[], warnings=[])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "free, no live worker found (roster scanned: 0 rows)" in r.output


def test_only_finished_sessions_do_not_raise_the_live_worker_alarm(cwd_tmp, fake_roster):
    """A `done` row is resumable, not driving. Printing the alarm for it would
    train every reader to ignore the alarm."""
    fake_roster(rows=[_row("t-xb0dd-outage", "done", "x-b0dd")])
    r = runner.invoke(cli, ["status", "node:x-b0dd"])
    assert "UNCLAIMED but a live worker" not in r.output
    assert "1 finished session(s) resolved to it: t-xb0dd-outage" in r.output


def test_a_held_node_never_pays_for_the_crosscheck(cwd_tmp, monkeypatch):
    """A live claim already answers the question; the roster fields must not
    appear and the harness must not be shelled out to."""
    def _boom(*_a, **_kw):
        raise AssertionError("roster consulted for a held node")

    monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _boom)
    # status routes a node: key to the GLOBAL root; acquire with the same
    # explicit root so both legs read one dir (cwd_tmp collapses them).
    acquire_claim(
        key="node:x-held", holder="target-session:s", ttl_ms=60_000, root=cwd_tmp
    )
    r = runner.invoke(cli, ["status", "node:x-held", "--json"])
    info = json.loads(r.output)
    assert info["state"] == "live"
    assert "roster_consulted" not in info


def test_a_non_node_key_is_rendered_exactly_as_before(cwd_tmp, monkeypatch):
    def _boom(*_a, **_kw):
        raise AssertionError("roster consulted for a non-node key")

    monkeypatch.setattr("fno.agents.watchdog.fleet_rows", _boom)
    r = runner.invoke(cli, ["status", "dispatch:x-76d1", "--json"])
    assert json.loads(r.output) == {"key": "dispatch:x-76d1", "state": "free"}


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


def _write_claim_file(key, *, expires_at):
    """A claim whose TTL lapsed under a provably live pid (this process).

    pid_provenance="session-prover" plus a pid-dies-with-session harness is
    the arm the native verdict keeps Live past the TTL; a legacy-shaped
    record reads Stale on the clock alone.
    """
    import os
    import socket

    import psutil

    from fno.claims.hostid import machine_id
    from fno.claims.io import claim_path, serialize_claim
    from fno.claims.types import Claim

    # The verdict reads PID reuse when the pid's create time EXCEEDS
    # acquired_at, so pin acquired_at a beat after THIS process's birth: the
    # freshest provably-live pid a test can own.
    pid = os.getpid()
    acquired_at = int(psutil.Process(pid).create_time() * 1000) + 2_000
    claim = Claim(
        key=key,
        holder="target-session:sid-e",
        acquired_at=acquired_at,
        pid=pid,
        host=socket.gethostname(),
        machine_id=machine_id(),
        harness="claude",
        pid_provenance="session-prover",
        expires_at=expires_at,
    )
    p = claim_path(key)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(serialize_claim(claim))
    return p


def test_status_live_expired_ttl_names_the_expiry(cwd_tmp):
    """AC3-HP (x-74aa): a live claim past its TTL renders the expiry clause
    with the age and the evidence keeping the holder live, not a bare live."""
    from fno.claims.types import now_ms

    _write_claim_file("k", expires_at=now_ms() - 60_000)
    r = runner.invoke(cli, ["status", "k"])
    assert r.exit_code == 0, r.output
    assert "ttl expired" in r.output
    assert "holder live by" in r.output
    assert json.loads(r.stdout)["state"] == "live"


def test_status_live_fresh_lease_has_no_expiry_clause(cwd_tmp):
    """AC3-EDGE: a lease inside its TTL renders no expiry clause."""
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    r = runner.invoke(cli, ["status", "k"])
    assert r.exit_code == 0, r.output
    assert "ttl expired" not in r.output


def test_unresolved_roster_row_naming_another_node_stays_free(cwd_tmp, fake_roster):
    """AC4-HP (x-74aa): an unresolved row whose worktree names some OTHER node
    is not evidence about this key; the state stays the claim's own free and
    only coverage reads degraded."""
    fake_roster(rows=[_row("t-unrelated", "working", None, "/tmp/x-other-node")])
    r = runner.invoke(cli, ["status", "node:x-76d1", "--json"])
    assert r.exit_code == 0
    info = json.loads(r.output)
    assert info["state"] == "free"
    assert info.get("basis") != "unresolved-roster-row"
    assert info["roster_coverage"] == "degraded"
