"""Typer CliRunner tests for the fno claim CLI surface."""
from __future__ import annotations

import json
import os

import pytest
from typer.testing import CliRunner

from fno.claims.cli import cli, _merge_claims_across_roots, _parse_ttl
from fno.claims.core import ClaimContended, acquire_claim
from fno.claims.io import dedup_claims_roots

from .test_claim_reap import _dead_pid  # noqa: F401


runner = CliRunner()


def test_ttl_parser_seconds_no_unit():
    assert _parse_ttl("60") == 60_000


def test_ttl_parser_seconds():
    assert _parse_ttl("60s") == 60_000


def test_ttl_parser_minutes():
    assert _parse_ttl("5m") == 5 * 60_000


def test_ttl_parser_hours():
    assert _parse_ttl("2h") == 2 * 3_600_000


def test_ttl_parser_empty_string_returns_none():
    assert _parse_ttl("") is None


def test_ttl_parser_invalid_raises():
    with pytest.raises(Exception):
        _parse_ttl("xyz")


def test_help_lists_all_verbs():
    result = runner.invoke(cli, ["--help"])
    assert result.exit_code == 0
    for verb in ("acquire", "release", "refresh", "status", "list"):
        assert verb in result.output


def test_acquire_fresh_key(cwd_tmp):
    result = runner.invoke(cli, ["acquire", "node:ab-1", "--holder", "h1"])
    assert result.exit_code == 0
    assert "acquired" in result.output
    assert (cwd_tmp / ".fno" / "claims" / "node%3Aab-1.lock").exists()


def test_acquire_json_output(cwd_tmp):
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["key"] == "k"
    assert parsed["holder"] == "h"


def test_acquire_conflict_exits_1(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h1"])
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h2"])
    assert result.exit_code == 1
    assert "held by" in result.output


def test_acquire_contention_exhaustion_exits_1_not_a_traceback(cwd_tmp, monkeypatch):
    """acquire_claim's contention-retry-exhaustion ClaimContended must be
    caught and mapped to exit 1 (same "retry later" code as
    ClaimHeldByOther), not escape as an uncaught traceback."""
    import fno.claims.cli as claims_cli

    def _raise(*args, **kwargs):
        raise ClaimContended("acquire_claim gave up after 5 contention retries on 'k'")

    monkeypatch.setattr(claims_cli, "acquire_claim", _raise)
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h1"])
    assert result.exit_code == 1
    assert "contention error" in result.output
    assert result.exception is None or isinstance(result.exception, SystemExit)


def test_refresh_contention_exhaustion_exits_1_not_a_traceback(cwd_tmp, monkeypatch):
    """Same as acquire's: refresh_claim's contention-exhaustion ClaimContended
    must be caught, not escape as an uncaught traceback."""
    import fno.claims.cli as claims_cli

    def _raise(*args, **kwargs):
        raise ClaimContended("refresh_claim gave up after 5 contention retries on 'k'")

    monkeypatch.setattr(claims_cli, "refresh_claim", _raise)
    result = runner.invoke(cli, ["refresh", "k", "--holder", "h1"])
    assert result.exit_code == 1
    assert "contention error" in result.output
    assert result.exception is None or isinstance(result.exception, SystemExit)


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
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--ttl", "1h", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["expires_at"] is not None


def test_acquire_omitted_pid_defaults_to_session_ancestor(cwd_tmp, monkeypatch):
    # ponytail hardening: an omitted --pid anchors to the durable session
    # (nearest agent ancestor), not the transient acquiring process. os.getppid()
    # is a real, live, DISTINCT pid so this proves the wiring (not the old
    # os.getpid() default).
    monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid",
                        lambda from_pid=None: os.getppid())
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["pid"] == os.getppid() and parsed["pid"] != os.getpid()


def test_acquire_omitted_pid_degrades_when_no_session(cwd_tmp, monkeypatch):
    # No agent ancestor (standalone use) -> resolve returns None -> the prior
    # os.getpid() default is preserved byte-for-byte.
    monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid",
                        lambda from_pid=None: None)
    result = runner.invoke(cli, ["acquire", "k", "--holder", "h", "--json"])
    assert result.exit_code == 0
    assert json.loads(result.output)["pid"] == os.getpid()


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
    (ab-6d5afbde: the daemon's stream-claim shelled `fno claim acquire`, whose
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


def test_status_free(cwd_tmp):
    result = runner.invoke(cli, ["status", "nothing", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    assert parsed["state"] == "free"


def test_status_live(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
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
    runner.invoke(cli, ["acquire", "node:ab-1", "--holder", "h"])
    runner.invoke(cli, ["acquire", "fleet:m1", "--holder", "h"])
    result = runner.invoke(cli, ["list", "--prefix", "node:", "--json"])
    assert result.exit_code == 0
    parsed = json.loads(result.output)
    keys = [r["key"] for r in parsed]
    assert keys == ["node:ab-1"]


def test_list_no_prefix_sees_global_claims_from_different_cwd(tmp_path, monkeypatch):
    """A no-prefix `list` must find node: claims under the global root even
    when cwd's local claims dir is a different, empty directory.

    Regression for the bug measured 2026-08-14: 573 claim files on disk under
    ~/.fno/claims, but `fno claim list` printed "no claims" because it only
    scanned cwd's local root - global-id claims (node:/dispatch:/session:)
    never live there, so an unscoped call saw nothing.
    """
    home = tmp_path / "home"
    home.mkdir()
    repo = tmp_path / "repo"
    repo.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.chdir(repo)

    acquired = runner.invoke(cli, ["acquire", "node:ab-1", "--holder", "h"])
    assert acquired.exit_code == 0
    # Sanity: the claim landed under the global root, not the repo-local one.
    assert (home / ".fno" / "claims" / "node%3Aab-1.lock").exists()
    assert not (repo / ".fno" / "claims").exists()

    listed = runner.invoke(cli, ["list", "--json"])
    assert listed.exit_code == 0
    keys = [r["key"] for r in json.loads(listed.output)]
    assert keys == ["node:ab-1"]


def test_list_prefix_node_scans_global_root_once(cwd_tmp):
    """An explicit global --prefix still resolves the global root directly;
    the merge in list_cmd must not duplicate its rows."""
    runner.invoke(cli, ["acquire", "node:ab-1", "--holder", "h"])
    result = runner.invoke(cli, ["list", "--prefix", "node:", "--json"])
    assert result.exit_code == 0
    keys = [r["key"] for r in json.loads(result.output)]
    assert keys == ["node:ab-1"]


def test_merge_across_roots_first_root_wins_row_and_totals_together(tmp_path):
    """A key present in two roots with divergent states (e.g. a stale
    leftover from an older fno that predates FNO_CLAIMS_ROOT-based global
    routing, sitting alongside a live claim the current fno wrote to the
    global root - see the version-skew note on CLAIMS_ROOT_ENV in io.py)
    must be claimed by the SAME root for both the displayed row and the
    totals bucket. A prior implementation deduped all_rows and totals with
    two different predicates, so a key could show as a live row (from the
    first root) while also being counted stale (from the second root) - an
    internal inconsistency invisible today only because the totals hint text
    happens to be gated on all_rows being empty."""
    root_a = tmp_path / "root_a"
    root_b = tmp_path / "root_b"
    acquire_claim("k", "holder-a", pid=os.getpid(), root=root_a)
    acquire_claim("k", "holder-b", pid=_dead_pid(), root=root_b)

    deduped = dedup_claims_roots([root_a, root_b])
    all_rows, row_roots, totals = _merge_claims_across_roots(
        deduped, prefix="", include_stale=False
    )

    assert [r["key"] for r in all_rows] == ["k"]
    assert all_rows[0]["state"] == "live"
    assert row_roots["k"] == str(root_a / ".fno" / "claims")
    assert totals == {"stale": 0, "corrupted": 0, "free": 0}, (
        "root_a won the key for the row (live); root_b's stale sighting of "
        "the SAME key must not also land in totals"
    )


def test_merge_across_roots_live_in_second_root_is_not_hidden_by_first_roots_stale(tmp_path):
    """The opposite ordering from the test above: root_a (scanned first)
    has a STALE leftover for key 'k'; root_b (scanned second) has a
    genuinely LIVE claim for the SAME key - e.g. the old FNO_CLAIMS_ROOT
    left a stale sighting behind while the current root holds the real,
    active claim. Pure first-root-wins would let root_a's stale sighting
    dedup away root_b's live one entirely: no row, and root_a's stale gets
    counted instead - defeating the exact migration scenario this
    function's own docstring names. The live claim must win regardless of
    which root was scanned first."""
    root_a = tmp_path / "root_a"
    root_b = tmp_path / "root_b"
    acquire_claim("k", "holder-a", pid=_dead_pid(), root=root_a)
    acquire_claim("k", "holder-b", pid=os.getpid(), root=root_b)

    deduped = dedup_claims_roots([root_a, root_b])
    all_rows, row_roots, totals = _merge_claims_across_roots(
        deduped, prefix="", include_stale=False
    )

    assert [r["key"] for r in all_rows] == ["k"], (
        "root_b's live claim must surface as a row even though root_a "
        "(scanned first) had a stale sighting of the same key"
    )
    assert all_rows[0]["state"] == "live"
    assert row_roots["k"] == str(root_b / ".fno" / "claims")
    assert totals == {"stale": 0, "corrupted": 0, "free": 0}


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
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])  # PID-liveness
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
    """`fno claim status node:<id>` from a project cwd, with no
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
    r = runner.invoke(cli, ["status", "node:ab-deadbeef", "--json"])
    assert r.exit_code == 0, r.output
    info = json.loads(r.output)
    assert info["state"] == "live", info
    assert info["holder"] == "target-session:s"


def test_list_node_prefix_finds_global_claims_without_env(tmp_path, monkeypatch):
    """`fno claim list --prefix node:` resolves the global root too."""
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
    g.write_text('{"entries": [{"id": "ab-dotest", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)

    acquire_claim(key="node:ab-dotest", holder="target-session:s",
                  ttl_ms=3_600_000, root=home)

    stamped = runner.invoke(
        cli, ["release", "node:ab-dotest", "--holder", "target-session:s", "--stamp-do"]
    )
    assert stamped.exit_code == 0, stamped.output
    rows = json.loads(g.read_text())["entries"][0].get("sessions", [])
    do = [x for x in rows if x.get("phase") == "do"]
    assert len(do) == 1
    assert do[0]["harness"] == "claude"
    # owned (holder) session wins over the ambient CLAUDE_CODE_SESSION_ID
    assert do[0]["session_id"] == "s"
    assert do[0]["started_at"] and do[0]["ended_at"]
    assert do[0]["started_at"] <= do[0]["ended_at"]


def test_handover_acquire_opens_the_do_row_too(tmp_path, monkeypatch):
    """The handover return is a THIRD acquire path, and the stamp below it calls
    itself the one choke point every acquire path reaches. It is also the
    default path for every `fno agents spawn --node` worker, so a worker killed
    mid-phase would leave no do row at all."""
    import fno.paths
    from fno.claims.core import acquire_claim

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-ho-1")
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)

    g = tmp_path / "graph.json"
    g.write_text('{"entries": [{"id": "ab-hotest", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)

    # The spawn side takes the launch-window claim, then the worker names it back.
    acquire_claim(key="node:ab-hotest", holder="spawn-handover:t-worker",
                  ttl_ms=900_000, root=home)
    out = runner.invoke(cli, [
        "acquire", "node:ab-hotest", "--holder", "target-session:w",
        "--handover-from", "spawn-handover:t-worker", "--ttl", "2h",
        "--harness", "claude",
    ])
    assert out.exit_code == 0, out.output
    assert "handover from" in out.output

    rows = json.loads(g.read_text())["entries"][0].get("sessions", [])
    do = [x for x in rows if x.get("phase") == "do"]
    assert len(do) == 1, rows
    assert do[0]["started_at"]
    assert not do[0].get("ended_at")


def test_acquire_opens_do_provenance_row(tmp_path, monkeypatch):
    """A node claim acquire opens the do row with started_at from the claim's
    acquire time and NO ended_at - so a session killed before its release
    terminal still leaves a started row instead of reading unstarted (the
    killed-mid-phase specimen: PR open and green while the node showed only a
    blueprint row)."""
    import fno.paths

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-acq-1")
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)

    g = tmp_path / "graph.json"
    g.write_text('{"entries": [{"id": "ab-acqtest", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)

    acq = runner.invoke(
        cli, ["acquire", "node:ab-acqtest", "--holder", "target-session:s", "--ttl", "1h"]
    )
    assert acq.exit_code == 0, acq.output
    rows = json.loads(g.read_text())["entries"][0].get("sessions", [])
    do = [x for x in rows if x.get("phase") == "do"]
    assert len(do) == 1
    assert do[0]["harness"] == "claude"
    # owned (holder) session wins over the ambient CLAUDE_CODE_SESSION_ID
    assert do[0]["session_id"] == "s"
    assert do[0]["started_at"]
    assert "ended_at" not in do[0]  # opened, not closed


def test_acquire_then_release_closes_do_window(tmp_path, monkeypatch):
    """Acquire opens the do row (started_at, no end); release --stamp-do fills
    ended_at on the SAME row via duplicate-fill - the merge that makes
    acquire-time stamping safe without losing the release window or adding a
    second row."""
    import fno.paths

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-acq-2")
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)

    g = tmp_path / "graph.json"
    g.write_text('{"entries": [{"id": "ab-acqrel", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)

    acq = runner.invoke(
        cli, ["acquire", "node:ab-acqrel", "--holder", "target-session:s", "--ttl", "1h"]
    )
    assert acq.exit_code == 0, acq.output
    rel = runner.invoke(
        cli, ["release", "node:ab-acqrel", "--holder", "target-session:s", "--stamp-do"]
    )
    assert rel.exit_code == 0, rel.output
    rows = json.loads(g.read_text())["entries"][0].get("sessions", [])
    do = [x for x in rows if x.get("phase") == "do"]
    assert len(do) == 1  # one row, not two - release closed the acquire row
    assert do[0]["started_at"] and do[0]["ended_at"]
    assert do[0]["started_at"] <= do[0]["ended_at"]


def _do_graph(tmp_path, monkeypatch, node_id, session_marker):
    """A one-node graph wired as fno.paths.graph_json, with a clean claude
    ambient identity. Returns the graph path."""
    import fno.paths

    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", session_marker)
    for m in ("CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID",
              "OPENCODE_SESSION_ID", "CLAUDE_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)
    g = tmp_path / "graph.json"
    g.write_text('{"entries": [{"id": "%s", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n' % node_id)
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)
    return g


def _do_rows(graph_path):
    return [
        x for x in json.loads(graph_path.read_text())["entries"][0].get("sessions", [])
        if x.get("phase") == "do"
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


def test_rollback_do_spares_an_earlier_open_row_from_the_same_session(
    tmp_path, monkeypatch
):
    """The dangerous case: a session did real work, was killed with its row still
    open, then re-acquired and was refused. An idempotent re-acquire refreshes
    the claim's acquired_at while the row keeps the FIRST started_at (append
    never overwrites), so the rollback's started_at no longer matches and the
    earlier window survives its successor's refusal."""
    import yaml

    from fno.claims.io import claim_path

    g = _do_graph(tmp_path, monkeypatch, "ab-rbearly", "sess-rb-3")
    home = tmp_path / "home"

    runner.invoke(
        cli, ["acquire", "node:ab-rbearly", "--holder", "target-session:s", "--ttl", "1h"]
    )
    first_started = _do_rows(g)[0]["started_at"]

    # Stand in for the re-acquire's refreshed acquired_at without a wall-clock
    # sleep: the row's started_at is second-granular, so a same-second re-acquire
    # would not exercise the divergence this test is about.
    cp = claim_path("node:ab-rbearly", root=home)
    raw = yaml.safe_load(cp.read_text())
    raw["acquired_at"] = raw["acquired_at"] + 60_000
    cp.write_text(yaml.safe_dump(raw, sort_keys=False))

    rel = runner.invoke(
        cli, ["release", "node:ab-rbearly", "--holder", "target-session:s", "--rollback-do"]
    )
    assert rel.exit_code == 0, rel.output
    rows = _do_rows(g)
    assert len(rows) == 1
    assert rows[0]["started_at"] == first_started
    assert "no open do row to roll back" in rel.output


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
    g.write_text('{"entries": [{"id": "ab-dotest2", "title": "t", '
                 '"domain": "code", "project": "p"}]}\n')
    monkeypatch.setattr(fno.paths, "graph_json", lambda: g)

    acquire_claim(key="node:ab-dotest2", holder="target-session:s",
                  ttl_ms=3_600_000, root=home)
    bare = runner.invoke(
        cli, ["release", "node:ab-dotest2", "--holder", "target-session:s"]
    )
    assert bare.exit_code == 0, bare.output
    assert json.loads(g.read_text())["entries"][0].get("sessions", []) == []


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


def _row(name, state, node):
    from fno.agents.watchdog import Row

    return Row(row_id=name, name=name, state=state, node=node, cwd="/tmp/wt")


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

    Carrying it is safe because an unknown state matches no finished state, so
    the alarm reads the worker as engaged - the conservative direction."""
    from fno.agents.watchdog import ADVISORY_WARNING_PREFIX

    fake_roster(
        rows=[_row("t-x76d1-rmtruth", "frobnicating", "x-76d1")],
        warnings=[f"{ADVISORY_WARNING_PREFIX}unmapped row state 'frobnicating'"],
    )
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "UNCLAIMED but a live worker is on this node" in r.output
    assert "roster not consulted" not in r.output


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
    monkeypatch.setattr(
        "fno.claims.cli._transcript_activity", lambda *_a, **_kw: False
    )
    fake_roster(rows=[_row("t-x76d1-rmtruth", "done", "x-76d1")])
    r = runner.invoke(cli, ["status", "node:x-76d1"])
    assert "UNCLAIMED but a live worker is on this node" in r.output


def test_an_aged_out_transcript_leaves_the_row_standing(cwd_tmp, fake_roster, monkeypatch):
    """The other direction, and it is deliberately NOT what the reap probe does.
    A wrong reap archives a live worker's claim; a wrong line here is an alarm
    on an empty node, and one that fires on every finished session whose
    transcript has aged out teaches operators to ignore the alarm."""
    monkeypatch.setattr(
        "fno.claims.cli._transcript_activity", lambda *_a, **_kw: None
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
    acquire_claim(key="node:x-held", holder="target-session:s", ttl_ms=60_000)
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
