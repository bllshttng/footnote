"""Typer CliRunner tests for the fno claim CLI surface."""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.claims.cli import cli, _parse_ttl


runner = CliRunner()


@pytest.fixture
def cwd_tmp(tmp_path: Path, monkeypatch):
    """Change cwd to a tmp path so .fno/claims/ does not pollute the worktree.

    Also pin HOME to the same tmp dir (and clear FNO_CLAIMS_ROOT) so the
    global node-claim root (~/.fno/claims) coincides with cwd. node:<id>
    keys now auto-resolve the global root (ab-fcf9cec5); pinning HOME=cwd keeps
    these tests' cwd-relative lock assertions valid for both node and non-node keys.
    """
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FNO_CLAIMS_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    yield tmp_path


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
    for verb in ("acquire", "release", "refresh", "status", "list", "force-release"):
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


def test_force_release_succeeds(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    result = runner.invoke(cli, ["force-release", "k", "--reason", "operator override"])
    assert result.exit_code == 0


def test_force_release_empty_reason_exits_2(cwd_tmp):
    runner.invoke(cli, ["acquire", "k", "--holder", "h"])
    # Typer's BadParameter on empty value goes through option parsing; pass empty string explicitly
    result = runner.invoke(cli, ["force-release", "k", "--reason", ""])
    assert result.exit_code == 2


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
# worktree-guard (x-193d Wave 5)
# ---------------------------------------------------------------------------


def _wt_env(monkeypatch, tmp_path, harness_marker, session, worktree="/w/repo/wt-a"):
    """Isolate the claim in tmp and stamp the ambient harness identity."""
    import fno.claims.worktree_guard as wg

    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    for m in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv(harness_marker, session)
    monkeypatch.setattr(wg, "resolve_worktree_root", lambda cwd=None: Path(worktree))


def test_worktree_guard_acquire_then_foreign_refused(tmp_path, monkeypatch):
    _wt_env(monkeypatch, tmp_path, "CLAUDE_CODE_SESSION_ID", "s1")
    r1 = runner.invoke(cli, ["worktree-guard", "--json"])
    assert r1.exit_code == 0
    assert json.loads(r1.stdout)["verdict"] == "acquired"

    # A codex session entering the same worktree is refused (exit 1).
    _wt_env(monkeypatch, tmp_path, "CODEX_THREAD_ID", "s2")
    r2 = runner.invoke(cli, ["worktree-guard", "--json"])
    assert r2.exit_code == 1
    out = json.loads(r2.stdout)
    assert out["verdict"] == "foreign"
    assert out["owner_harness"] == "claude"


def test_worktree_guard_override_env(tmp_path, monkeypatch):
    _wt_env(monkeypatch, tmp_path, "CLAUDE_CODE_SESSION_ID", "s1")
    runner.invoke(cli, ["worktree-guard", "--json"])
    _wt_env(monkeypatch, tmp_path, "CODEX_THREAD_ID", "s2")
    monkeypatch.setenv("FNO_WORKTREE_OK", "1")
    r = runner.invoke(cli, ["worktree-guard", "--json"])
    assert r.exit_code == 0
    assert json.loads(r.stdout)["verdict"] == "override"
