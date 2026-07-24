"""Incarnation fence (x-eea5 1.3): a losing incarnation refuses outward actions."""
from typer.testing import CliRunner

from fno.claims.cli import cli as claims_cli
from fno.claims.incarnation import incarnation_fence_blocks, resolve_fence_session_uuid

runner = CliRunner()


def _wire(monkeypatch, status, *, own_pid=None):
    monkeypatch.setattr("fno.claims.core.claim_status", lambda key, root=None: status)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: own_pid
    )
    monkeypatch.setattr("fno.claims.incarnation.socket.gethostname", lambda: "h")


def test_no_uuid_is_invisible():
    assert incarnation_fence_blocks(None) == (False, "")
    assert incarnation_fence_blocks("") == (False, "")


def test_free_claim_proceeds(monkeypatch):
    _wire(monkeypatch, {"state": "free"})
    assert incarnation_fence_blocks("uuid1") == (False, "")


def test_ours_proceeds(monkeypatch):
    # AC5-EDGE: the sole incarnation holding its own claim is never fenced.
    _wire(
        monkeypatch,
        {"state": "live", "holder": "me", "pid": 123, "host": "h"},
        own_pid=123,
    )
    assert incarnation_fence_blocks("uuid1") == (False, "")


def test_other_live_blocks(monkeypatch):
    # AC3-ERR: another live incarnation holds the lineage claim -> refuse.
    _wire(
        monkeypatch,
        {"state": "live", "holder": "target-session:other", "pid": 999, "host": "h"},
        own_pid=123,
    )
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "session:uuid1" in reason
    assert "other" in reason


def test_unreadable_claims_fails_closed(monkeypatch):
    # AC4-FR: an unreadable claims dir refuses outward actions.
    def boom(key, root=None):
        raise RuntimeError("unreadable")

    monkeypatch.setattr("fno.claims.core.claim_status", boom)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked and "unreadable" in reason


def test_stale_holder_proceeds(monkeypatch):
    # A dead/stale contender is no contention -> proceed.
    _wire(
        monkeypatch,
        {"state": "stale", "holder": "dead", "pid": 1, "host": "h"},
        own_pid=123,
    )
    assert incarnation_fence_blocks("uuid1") == (False, "")


def test_corrupted_claim_fails_closed(monkeypatch):
    # F3: claim_status returns state="corrupted" (no raise) for a malformed claim
    # file. An unverifiable single-writer state must fail closed, not read clear.
    _wire(monkeypatch, {"state": "corrupted", "error": "bad json"})
    blocked, reason = incarnation_fence_blocks("uuid1")
    assert blocked
    assert "corrupt" in reason.lower()


def test_resolve_uuid_from_env(monkeypatch):
    # F1: the fence keys on the TRANSCRIPT uuid (CLAUDE_CODE_SESSION_ID), not the
    # target run id (TARGET_SESSION_ID); the single-writer claim is held under the
    # transcript uuid, so the run id would read a nonexistent key as clear.
    monkeypatch.setenv("TARGET_SESSION_ID", "run-id")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "transcript-uuid")
    assert resolve_fence_session_uuid() == "transcript-uuid"


def test_target_session_id_is_not_the_fence_key(monkeypatch, tmp_path):
    # F1: TARGET_SESSION_ID is the target run id, not the claim key. With no
    # transcript uuid resolvable the fence is invisible (None), never the run id.
    monkeypatch.setenv("TARGET_SESSION_ID", "run-id")
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    assert resolve_fence_session_uuid(tmp_path) is None


def test_resolve_uuid_from_manifest(tmp_path, monkeypatch):
    # F1: the manifest's transcript uuid (claude_session_id / harness_session_id)
    # is the claim key, not the run-id `session_id` field.
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        'session_id: "run-uuid"\n'
        'claude_session_id: "transcript-uuid"\n'
        'harness_session_id: "transcript-uuid"\n'
    )
    assert resolve_fence_session_uuid(tmp_path) == "transcript-uuid"


def test_resolve_manifest_run_id_only_is_none(tmp_path, monkeypatch):
    # F1: a manifest carrying only the run-id session_id yields None; the run id
    # is never the fence key.
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text('session_id: "run-uuid"\n')
    assert resolve_fence_session_uuid(tmp_path) is None


def test_verb_blocked_exits_nonzero(monkeypatch):
    monkeypatch.setattr("fno.claims.incarnation.resolve_fence_session_uuid", lambda: "uuid1")
    monkeypatch.setattr(
        "fno.claims.incarnation.incarnation_fence_blocks",
        lambda u, **k: (True, "session:uuid1 held by target-session:other"),
    )
    r = runner.invoke(claims_cli, ["incarnation-fence"])
    assert r.exit_code == 2
    assert "incarnation-fence" in r.output


def test_verb_clear_proceeds(monkeypatch):
    monkeypatch.setattr("fno.claims.incarnation.resolve_fence_session_uuid", lambda: "uuid1")
    monkeypatch.setattr(
        "fno.claims.incarnation.incarnation_fence_blocks", lambda u, **k: (False, "")
    )
    r = runner.invoke(claims_cli, ["incarnation-fence"])
    assert r.exit_code == 0


def test_verb_no_identity_proceeds(monkeypatch):
    monkeypatch.setattr("fno.claims.incarnation.resolve_fence_session_uuid", lambda: None)
    r = runner.invoke(claims_cli, ["incarnation-fence"])
    assert r.exit_code == 0


def test_run_merge_blocked_by_fence(monkeypatch, tmp_path):
    # The merge outward action refuses when the fence blocks (before any merge work).
    monkeypatch.setattr(
        "fno.claims.incarnation.resolve_fence_session_uuid", lambda cwd=None: "uuid1"
    )
    monkeypatch.setattr(
        "fno.claims.incarnation.incarnation_fence_blocks",
        lambda u, **k: (True, "session:uuid1 held by other"),
    )
    from fno.pr import _merge

    rc = _merge.run_merge(["123"], cwd=str(tmp_path))
    assert rc == 2
