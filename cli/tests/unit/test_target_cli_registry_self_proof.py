"""The resolve-owned-identity verb resolves through claims.self_identity (x-0992).

History this file used to pin: x-a0cd measured a daemon-hosted codex thread
under seatbelt where only self's ppid is readable, so every walk inside init's
script chain returned None and the verb fell to collision-elimination, which
rejected the worker's OWN spawn-minted row. The launcher stamp
(FNO_SESSION_HARNESS/FNO_SESSION_PID, exported by the `fno do target init` CLI
while ITS ppid read is still permitted) fixed the walk half. x-0992 measured
the other half: a PANE-spawned codex worker carries no FNO_HARNESS_SESSION_ID
at all (the pane row is written after the child starts), so its stamp is
name_only and the verb's former private own_binding construction - gated on a
COMPLETE stamp - was always None. The verb now routes through
resolve_self_identity, the one owned-identity implementation, and these tests
pin the verb-level contract the hook parses."""

from typer.testing import CliRunner

from fno.cli import app

# The conftest's autouse _neutral_host_harness patches resolve_session_harness
# to None; the stamp tests below pin the REAL function, captured at collection
# time before any fixture runs (the same pattern the fno_py_cmd tests use).
from fno.claims import session_pid as _session_pid

_REAL_RESOLVE_HARNESS = _session_pid.resolve_session_harness

runner = CliRunner()


def _fields(result):
    return {
        line.split("=", 1)[0]: line.split("=", 1)[1]
        for line in result.stdout.splitlines()
        if "=" in line
    }


def _silent_walk_and_attester(monkeypatch, attested_id: str):
    """Pin the sandbox shape: no harness ancestor to walk, so the attester's
    witness stays env_only while it still names the env's own marker value -
    exactly what a bare runner (and a seatbelt sandbox) produces for a single
    codex family. A real runner's ancestry may or may not be readable, and the
    verb's answer must not depend on which."""
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: None
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_attester_identity",
        lambda env=None: (attested_id, "env_only"),
    )


def _silent_walk_and_attester_with_codex_proof(monkeypatch, attested_id: str):
    """The lead's shape (x-a409): the process tree DOES prove codex (the pane
    runner), but the attester stays env_only - codex never carries
    CODEX_THREAD_ID in its own env, so ancestry cannot witness the id value.
    This is exactly the gap the rollout witness fills."""
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: "codex"
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_attester_identity",
        lambda env=None: (attested_id, "env_only"),
    )


def test_name_only_pane_stamp_resolves_without_row_or_proof(tmp_path, monkeypatch):
    """The x-0992 repro, pinned: a pane-spawned codex worker's environment
    (name_only stamp, both codex markers carrying the same uuid, no walk, no
    row) resolves to HARNESS=codex with a non-empty SESSION_ID and no
    COLLISION. The pre-fix verb answered ambiguous here, which stamped
    harness=unknown/provider=claude onto a codex worker's manifest."""
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    _silent_walk_and_attester(monkeypatch, mine)
    monkeypatch.setenv("FNO_HARNESS_NAME", "codex")
    monkeypatch.setenv("CODEX_THREAD_ID", mine)
    monkeypatch.setenv("CODEX_SESSION_ID", mine)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "codex"
    assert fields["SESSION_ID"] == mine
    assert fields["DISPOSITION"] == "single"
    assert fields["COLLISION"] == ""


def test_name_only_own_row_resolves_when_the_attester_witnesses(
    tmp_path, monkeypatch
):
    """The real pane worker's resolution path: the spawn stamp names the
    family, the launcher stamps the family proof (or the walk finds it), and
    the attester witnesses the marker value from process ancestry. With that
    independent ground the worker resolves its own identity even though its
    own spawn-minted row already holds the id."""
    from fno.agents.registry import register_existing_session
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    register_existing_session(harness="codex", session_id=mine, cwd="/x")
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: "codex"
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_attester_identity",
        lambda env=None: (mine, "process"),
    )
    monkeypatch.setenv("FNO_HARNESS_NAME", "codex")
    monkeypatch.setenv("CODEX_THREAD_ID", mine)
    monkeypatch.setenv("CODEX_SESSION_ID", mine)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "codex"
    assert fields["SESSION_ID"] == mine
    assert fields["DISPOSITION"] == "canonical"
    assert fields["COLLISION"] == ""


def test_name_only_own_row_resolves_by_rollout_witness(tmp_path, monkeypatch):
    """AC1 (x-a409): the lead shape. A name_only codex stamp, a live registry
    row holding the marker id, a codex-proofed tree, and an attester that only
    saw env - yet the rollout fd witnesses the id, and that fd cannot be
    forged by a leaked marker. The resolver completes its own pair and answers
    canonically instead of rejecting its own row as a stranger."""
    from fno.agents.registry import register_existing_session
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    register_existing_session(harness="codex", session_id=mine, cwd="/x")
    _silent_walk_and_attester_with_codex_proof(monkeypatch, mine)
    monkeypatch.setattr(
        "fno.agents.codex_rollout.codex_rollout_witness",
        lambda harness, env=None: frozenset({mine}),
    )
    monkeypatch.setenv("FNO_HARNESS_NAME", "codex")
    monkeypatch.setenv("CODEX_THREAD_ID", mine)
    monkeypatch.setenv("CODEX_SESSION_ID", mine)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "codex"
    assert fields["SESSION_ID"] == mine
    assert fields["DISPOSITION"] == "canonical"
    assert fields["COLLISION"] == ""


def test_name_only_foreign_row_with_other_witness_fails_closed(tmp_path, monkeypatch):
    """AC2 (x-a409): the marker names another live row's id and the rollout
    witness sees a DIFFERENT session - no ground to claim the marker, so the
    refusal stands and names the owner."""
    from fno.agents.registry import register_existing_session
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    theirs = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    other = "019cc082-1111-7283-97cc-751c46742a08"
    owner = register_existing_session(harness="codex", session_id=theirs, cwd="/x").name
    _silent_walk_and_attester_with_codex_proof(monkeypatch, theirs)
    monkeypatch.setattr(
        "fno.agents.codex_rollout.codex_rollout_witness",
        lambda harness, env=None: frozenset({other}),
    )
    monkeypatch.setenv("FNO_HARNESS_NAME", "codex")
    monkeypatch.setenv("CODEX_THREAD_ID", theirs)
    monkeypatch.setenv("CODEX_SESSION_ID", theirs)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["DISPOSITION"] == "ambiguous"
    assert fields["COLLISION"] == owner
    assert fields["COLLISION_ID"] == theirs


def test_name_only_own_row_daemon_unavailable_fails_closed(tmp_path, monkeypatch):
    """AC6-ERR (x-a409): no tree rollout and no daemon answer means no witness.
    A name_only worker whose id a live row holds still refuses - the marker
    under test never completes its own pair (that would be circular), so the
    live row reads as contention and the verb answers empty, naming the
    owner and the id. The daemon being down never widens into a guess."""
    from fno.agents.registry import register_existing_session
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    owner = register_existing_session(harness="codex", session_id=mine, cwd="/x").name
    _silent_walk_and_attester_with_codex_proof(monkeypatch, mine)
    monkeypatch.setattr(
        "fno.agents.codex_rollout.codex_rollout_witness", lambda harness, env=None: frozenset()
    )
    monkeypatch.setenv("FNO_HARNESS_NAME", "codex")
    monkeypatch.setenv("CODEX_THREAD_ID", mine)
    monkeypatch.setenv("CODEX_SESSION_ID", mine)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == ""
    assert fields["SESSION_ID"] == ""
    assert fields["DISPOSITION"] == "ambiguous"
    assert fields["COLLISION"] == owner
    assert fields["COLLISION_ID"] == mine


# The session-harness stamp rules (honored while the pid is alive, ignored
# when the pid is dead, ignored for an unknown harness) moved with the walk
# into the native resolver; they are pinned Rust-side by
# fno-agents' session_identity_ambient stamp tests. The Python module is a
# shim over that verb and carries no stamp logic to test here.


def _silent_walk_claude_proof(monkeypatch, attested_id: str):
    """The claude spawned worker's shape: the tree proves the claude family,
    but the attester stays env_only - supervisor births poison the session id
    out of the child env and darwin no longer reads harness env through ps,
    so ancestry can never witness the id value."""
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: "claude"
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_attester_identity",
        lambda env=None: (attested_id, "env_only"),
    )


def test_name_only_claude_spawn_row_witness(tmp_path, monkeypatch):
    """A claude spawned worker's own spawn row - bound by its spawn-minted
    name - witnesses the id the spawn flow wrote: the identity resolves
    canonically when the named row holds the marker id, a stale name export
    (the row under that name holds a different live id) fails closed and
    names the true holder, and the reader keys on FNO_AGENT_SELF with a
    FNO_WORKER_NAME fallback, empty on any harness mismatch. The name ground
    also answers with the walk silent: a thread worker whose
    ancestry hides the harness process keeps its spawn-row proof."""
    from fno.agents.registry import register_existing_session, spawn_row_session_ids
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    stale_row_id = "019cc082-1111-7283-97cc-751c46742a08"
    _silent_walk_claude_proof(monkeypatch, mine)
    monkeypatch.setenv("FNO_HARNESS_NAME", "claude")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", mine)

    row = register_existing_session(harness="claude", session_id=mine, cwd="/x")
    monkeypatch.setenv("FNO_AGENT_SELF", row.name)
    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "claude"
    assert fields["SESSION_ID"] == mine
    assert fields["DISPOSITION"] == "canonical"
    assert fields["COLLISION"] == ""

    # The same name ground with the walk silent: resolution must not depend
    # on an ancestry read a thread worker cannot supply.
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: None
    )
    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "claude"
    assert fields["SESSION_ID"] == mine
    assert fields["DISPOSITION"] == "single"
    assert fields["COLLISION"] == ""

    # The reader seam behind the witness, keyed by the spawn-minted name.
    assert spawn_row_session_ids("claude") == frozenset({mine})
    monkeypatch.delenv("FNO_AGENT_SELF")
    monkeypatch.setenv("FNO_WORKER_NAME", row.name)
    assert spawn_row_session_ids("claude") == frozenset({mine})
    assert spawn_row_session_ids("codex") == frozenset()

    # A stale name export cannot witness: the row under that name holds a
    # different live id, so the marker stays refused and names the holder.
    stale = register_existing_session(harness="claude", session_id=stale_row_id, cwd="/x")
    monkeypatch.delenv("FNO_WORKER_NAME")
    monkeypatch.setenv("FNO_AGENT_SELF", stale.name)
    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["DISPOSITION"] == "ambiguous"
    assert fields["COLLISION"] == row.name
    assert fields["COLLISION_ID"] == mine


def test_redispatched_worker_cwd_record_proves_self(tmp_path, monkeypatch):
    """A re-dispatched spawn reaches init with a name_only stamp and
    NO spawn-minted name in its env, so the name ground is empty. The
    cwd-keyed spawn record - the one live thread row the spawn flow wrote at
    the worker's cwd - is the remaining non-circular ground, and a marker
    naming its id resolves instead of reading the worker's own fresh row as
    contention. A second live thread row on the same cwd restores the
    refusal: the exactly-one contract is the bystander guard."""
    import os

    from fno.harness_identity import HARNESS_SESSION_MARKERS
    from fno.paths import agents_registry_path
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.chdir(tmp_path)
    mine = "01a06d40-5f68-7da0-96cb-f57006ca2d2c"
    sibling = "019cc082-1111-7283-97cc-751c46742a08"
    row = {
        "name": "w-redispatch",
        "status": "live",
        "substrate": "thread",
        "harness": "claude",
        "harness_session_id": mine,
        "cwd": os.path.realpath(str(tmp_path)),
        "log_path": str(tmp_path / "w.log"),
    }
    path = agents_registry_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    from tests._table_seed import seed_registry

    seed_registry([row], path=path)
    for marker, _harness in HARNESS_SESSION_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda from_pid=None: None
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_attester_identity",
        lambda env=None: (mine, "env_only"),
    )
    monkeypatch.setenv("FNO_HARNESS_NAME", "claude")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", mine)

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["HARNESS"] == "claude"
    assert fields["SESSION_ID"] == mine
    assert fields["COLLISION"] == ""

    # A pruned cwd must degrade, not crash: the registry reader answers None
    # and the resolver keeps its zero-ground answer instead of raising.
    gone = tmp_path / "gone"
    gone.mkdir()
    os.chdir(gone)
    gone.rmdir()
    try:
        result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
        assert result.exit_code == 0, result.output
    finally:
        monkeypatch.chdir(tmp_path)

    # Bystander guard: two live thread rows on one cwd answer nothing, so the
    # marker is refused and names the holder again.
    sibling_row = dict(row, name="w-x1a5a-sibling", harness_session_id=sibling)
    seed_registry([row, sibling_row], path=path)
    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = _fields(result)
    assert fields["DISPOSITION"] == "ambiguous"
    assert fields["COLLISION"] == "w-redispatch"
    assert fields["COLLISION_ID"] == mine
