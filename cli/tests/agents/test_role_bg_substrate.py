"""`fno agents spawn --promote --substrate bg`: the role rides the bg substrate.

A role is three registry fields (`role_level` / `role_scope` / `role_grantor`)
and nothing in it needs a PTY. The substrate axis it actually cares about is
TERM LENGTH: a lead must outlive the grant. `bg` qualifies - a bg worker is a
full persistent conversation in claude's agent view, attachable and resumable,
differing from a pane only in who draws it. `headless` does not: it answers once
and exits, so its role is orphaned at birth.

These tests exercise the END-TO-END CLI path (`spawn --promote --substrate bg`),
not `_claude_create_path` in isolation. That is deliberate: the original defect
was a refusal at the CLI seam sitting in front of unplumbed params, so a test
that called the helper directly would have passed against the broken build.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.registry import AgentEntry, load_registry, update_registry
from fno.paths_testing import use_tmpdir


@pytest.fixture(autouse=True)
def _clear_parent_markers(monkeypatch):
    for marker in (
        "FNO_SESSION",
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
    ):
        monkeypatch.delenv(marker, raising=False)


@pytest.fixture
def bg_home(tmp_path, monkeypatch, native_backlog_door):
    """Isolated fno home with a fake claude, a graph holding two epics, and one
    configured project. The territory has to exist because the rung is DERIVED
    from it: a scope naming nothing is refused, so a fixture without a graph
    would test the refusal path in every case. Pins the role-settle occupancy
    call to this checkout's dev build (role-settle runs through Rust now)."""
    import json

    from tests.agents._fake_claude import install_fake_claude
    from fno import paths
    from fno.projects import resolve as proj_resolve

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))

    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    seed_graph(graph_path, json.dumps(
            {
                "entries": [
                    {"id": "epic-x", "type": "epic", "project": "alpha"},
                    {"id": "epic-y", "type": "epic", "project": "alpha"},
                    {"id": "epic-z", "type": "epic", "project": "alpha"},
                ]
            }
        ))
    cfg = tmp_path / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\nprojects = [{ name = "alpha" }]\n', encoding="utf-8"
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()
    yield tmp_path
    proj_resolve._clear_cache()


def _spawn(*args: str):
    from fno.agents.cli import agents_app

    return CliRunner().invoke(agents_app, list(args), catch_exceptions=False)


def _row(name: str) -> AgentEntry:
    entry = next((e for e in load_registry() if e.name == name), None)
    assert entry is not None, f"no registry row named {name!r}"
    return entry


# --- the role lands on bg ---------------------------------------------------


def test_bg_spawn_stamps_the_role(bg_home, monkeypatch) -> None:
    import fno.lead.state as lead_state

    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-sess-abc")
    # Seat the grantor as a registered lead over epic-x; the spawn is then a
    # succession (it hands its own scope to the successor). An agent identity with no
    # registry row is now refused at the grantor check, so the agent must be in
    # the registry - the corrected opposite of the fail-open this test rode.
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="parent",
                cwd="/tmp",
                log_path="",
                harness="claude",
                harness_session_id="parent-sess-abc",
                short_id="parent",
                status="busy",
                role_level=2,
                role_scope="epic-x",
                role_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "Rowan", "-H", "claude", "term",
        "--substrate", "thread", "--cwd", str(bg_home), "--promote", "epic-x", "--succeed",
    )
    assert result.exit_code == 0, result.output

    row = _row("Rowan")
    assert row.role_level == 2, "an epic is a Director"
    assert row.role_scope == "epic-x"
    # Provenance, not self-declaration: the grantor is the session that spawned it.
    assert row.role_grantor == "parent-sess-abc"
    assert row.role_label == "L2 epic-x"
    # The bg receipt names the session as an 8-hex prefix, which no transcript
    # basename ever matches, so arming at spawn would write a gate that arms
    # dead. The role is stamped, the manifest is refused, and the spawn says
    # so; re-arming belongs to the later role once the session self-identifies.
    manifest = Path(row.cwd) / ".fno" / "leads" / "epic-x.md"
    assert not manifest.exists(), "armed a manifest no transcript can ever match"
    assert "was NOT armed" in result.output


def test_bg_role_grantor_defaults_to_human(bg_home, monkeypatch) -> None:
    """No parent session env == a human's own shell, same rule as the pane path."""
    result = _spawn(
        "spawn", "--name", "lead-bg-human", "-H", "claude", "term",
        "--substrate", "thread", "--promote", "alpha",
    )
    assert result.exit_code == 0, result.output
    assert _row("lead-bg-human").role_grantor == "human"
    assert _row("lead-bg-human").role_level == 1, "a project is a project lead"
    assert "lead loop disabled" in result.output


def test_bg_spawn_without_role_leaves_the_fields_none(bg_home, monkeypatch) -> None:
    """The stamp is opt-in: an ordinary bg spawn is not accidentally promoted."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-sess-abc")

    result = _spawn(
        "spawn", "--name", "plain-bg", "-H", "claude", "work", "--substrate", "thread"
    )
    assert result.exit_code == 0, result.output

    row = _row("plain-bg")
    assert (row.role_level, row.role_scope, row.role_grantor) == (None, None, None)


# --- one live role per scope, enforced on bg too ----------------------------


def test_bg_spawn_refuses_a_duplicate_role_before_launch(bg_home, monkeypatch) -> None:
    """A second role over one scope would launch a successor with no role, so the
    spawn refuses BEFORE launch rather than succeeding unpromoted: nothing
    should exist that never held authority to. --succeed transfers instead."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="sitting-lead",
                cwd=str(bg_home),
                log_path="",
                harness="claude",
                harness_session_id="sess-sitting-lead",  # x-7bcd: resolvable handle
                status="busy",  # active, not merely the literal "live"
                role_level=2,
                role_scope="epic-x",
                role_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "pretender", "-H", "claude", "term",
        "--substrate", "thread", "--promote", "epic-x",
    )
    assert result.exit_code == 2

    assert not [e for e in load_registry() if e.name == "pretender"], (
        "a refused role must launch nothing"
    )
    assert "--hand-off" in result.output


def test_bg_spawn_refuses_a_role_over_one_member_of_a_live_set(bg_home, monkeypatch) -> None:
    """A live epic-set role rules each member, so a spawn promoted over ONE
    member refuses before launch the way `fno agents role` does: the rivalry
    rule is ladder-aware, not an exact scope string."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="sitting-lead",
                cwd=str(bg_home),
                log_path="",
                harness="claude",
                harness_session_id="sess-sitting-lead",
                status="busy",
                role_level=2,
                role_scope="epic-x,epic-y",
                role_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "pretender", "-H", "claude", "term",
        "--substrate", "thread", "--promote", "epic-x",
    )
    assert result.exit_code == 2

    assert not [e for e in load_registry() if e.name == "pretender"], (
        "a refused role must launch nothing"
    )
    assert "sitting-lead" in result.output
    assert "epic-x,epic-y" in result.output


def test_bg_spawn_roles_over_a_scope_whose_lead_is_terminal(bg_home, monkeypatch) -> None:
    """A dead lead does not block succession - that is the orphaned scope the
    role exists to let someone reclaim."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="dead-lead",
                cwd=str(bg_home),
                log_path="",
                harness="claude",
                harness_session_id="sess-dead-lead",  # x-7bcd: resolvable handle
                status="exited",
                role_level=2,
                role_scope="epic-y",
                role_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "successor", "-H", "claude", "term",
        "--substrate", "thread", "--promote", "epic-y",
    )
    assert result.exit_code == 0, result.output
    assert _row("successor").role_level == 2
    dead = _row("dead-lead")
    assert (dead.role_level, dead.role_scope, dead.role_grantor) == (
        None,
        None,
        None,
    )
    assert [row.name for row in load_registry() if row.role_scope == "epic-y"] == [
        "successor"
    ]


# --- headless stays refused --------------------------------------------------


@pytest.mark.parametrize("one_shot_args", [["--substrate", "headless"], ["-p"], ["--once"]])
def test_headless_role_is_refused(bg_home, one_shot_args) -> None:
    """A one-shot exits after one answer, so its role names a dead ruler before
    the grantor's next turn. This is the ONE substrate the refusal still covers."""
    result = _spawn(
        "spawn", "--name", "one-shot-lead", "-H", "claude", "term",
        *one_shot_args, "--promote", "epic-z",
    )
    assert result.exit_code == 2
    assert "outlives the grant" in result.output
    assert "not yet supported" not in result.output
    assert "--substrate pane" in result.output and "--substrate thread" in result.output
    assert not [e for e in load_registry() if e.name == "one-shot-lead"], (
        "a refused role must launch nothing"
    )


# --- in-process callers get the same guards ----------------------------------


@pytest.mark.parametrize("harness", ["codex", "opencode", "pi"])
def test_thread_spawn_stamps_the_promotion(bg_home, monkeypatch, harness) -> None:
    """All persistent carriers promote their row and arm the lead loop.
    Codex carries the stamp at mint; keeper promotion precedes seed submission.
    """
    import json as _json
    import subprocess as _subprocess
    import uuid as _uuid

    import fno.lead.state as lead_state

    monkeypatch.setattr(lead_state, "lead_loop_enabled", lambda: True)
    monkeypatch.setattr(
        "fno.rust_binary.resolve_binary", lambda: Path("/fake/fno-agents")
    )
    session_id = "ses_thread_test" if harness == "opencode" else str(_uuid.uuid4())
    seen = {}

    real_run = _subprocess.run  # captured before the patch replaces the attribute

    def fake_run(*args, **kwargs):
        # The Rust lane's registry write, simulated: the row exists BEFORE the
        # Python settlement runs, with all three role fields unset. The fake
        # answers ONLY the lane-spawn argv: this patch rides the shared
        # subprocess module, and every other caller (the event-store writer
        # among them) must reach the real run or a retry loop spins.
        argv = args[0]
        tokens = [a for a in argv if isinstance(a, str)] if argv else []
        if "spawn" not in tokens or "--substrate" not in tokens:
            return real_run(*args, **kwargs)
        if "--" in argv:
            seen["seed"] = argv[argv.index("--") + 1]
        if harness == "codex":
            assert "--role-level=2" in tokens
            assert "--role-scope=epic-x" in tokens
        update_registry(
            lambda rows: rows
            + [
                AgentEntry(
                    name="lead-codex",
                    cwd=str(bg_home),
                    log_path="",
                    harness=harness,
                    harness_session_id=session_id,
                    short_id="codexk1",
                    status="busy",
                    role_level=2 if harness == "codex" else None,
                    role_scope="epic-x" if harness == "codex" else None,
                )
            ]
        )
        return _subprocess.CompletedProcess(
            argv, 0, stdout=_json.dumps({"harness_session_id": session_id, "session_id": session_id}) + "\n",
            stderr="",
        )

    import fno.agents.dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod.subprocess, "run", fake_run)

    # The spawn gate is not this test's contract, and its codex/thread lane
    # consults fleet state a dev-build CI run does not carry: admit ungated,
    # the same posture the test env gives every other spawn axis.
    from fno.agents.spawn_gate import GateGuard

    import fno.agents.spawn_gate as spawn_gate_mod

    monkeypatch.setattr(spawn_gate_mod, "run_gate", lambda *a, **k: GateGuard())

    if harness == "pi":
        def keeper_mint(**kwargs):
            fake_run(["spawn", "--substrate", "thread"])
            return {"session_id": session_id, "keeper_socket": "/fake/keeper.sock"}
        def seed_submit(**kwargs):
            assert _row("lead-codex").role_scope == "epic-x"
            seen["seed"] = kwargs["message"]
        monkeypatch.setattr(dispatch_mod, "_lane_b_thread_spawn", keeper_mint)
        monkeypatch.setattr(dispatch_mod, "_keeper_seed_submit", seed_submit)

    result = _spawn(
        "spawn", "--name", "lead-codex", "-H", harness, "lead",
        "--substrate", "thread", "--cwd", str(bg_home), "--promote", "epic-x",
    )
    assert result.exit_code == 0, result.output
    # Codex spells the plugin verb with $; a /fno:lead seed would hand the
    # lead's first turn a command its harness cannot invoke.
    verb = {"codex": "$fno:lead", "pi": "/skill:lead"}.get(harness, "/fno:lead")
    assert seen["seed"].splitlines()[0] == f"{verb} epic-x"

    row = _row("lead-codex")
    assert row.role_level == 2, "an epic is a Director"
    assert row.role_scope == "epic-x"
    assert row.role_grantor == "human"
    # Resolve the manifest path through the same state-root resolver the arm
    # uses: a dev-build env can pin the state root away from <cwd>/.fno.
    from fno.lead.state import lead_state_root

    manifest = lead_state_root(Path(row.cwd)) / "leads" / "epic-x.md"
    if harness == "opencode":
        assert not manifest.exists()
        assert "was NOT armed" in result.output
    else:
        assert manifest.exists(), "the lead loop manifest armed"


def test_dispatch_spawn_refuses_a_one_shot_promotion(tmp_path: Path, monkeypatch, native_backlog_door) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="one-shot-lead",
            message="term",
            harness="claude",
            cwd=tmp_path,
            headless=True,
            role_level=1,
            role_scope="epic-x",
        )
    assert exc.value.exit_code == 2
    assert "outlives the grant" in str(exc.value)


# --- role values are validated on every writer, not just the CLI ------------
#
# The CLI parses `--promote` through _parse_role, but both spawn dispatchers take
# (level, scope) directly from in-process callers. A value that skips validation
# is written to the SHARED registry: Rust's role_level is Option<u32>
# (crates/fno-agents/src/state.rs), so a negative or boolean level breaks
# registry reads for every reader, not just the caller that wrote it.


@pytest.mark.parametrize(
    "level,scope",
    [
        (-1, "epic-x"),        # negative: cannot deserialize into u32
        (3, "epic-x"),         # over the 0..2 ladder ceiling
        (10**20, "epic-x"),    # arbitrary-precision int, overflows u32
        (True, "epic-x"),      # bool is an int subclass; serializes as JSON true
        ("1", "epic-x"),       # str that looks like a level
        (1, ""),               # blank scope
        (1, "   "),            # whitespace-only scope
        (1, None),             # level with no scope: rules nothing, unguardable
        (None, "epic-x"),      # scope with no level
    ],
)
def test_dispatch_spawn_refuses_invalid_role_values(
    tmp_path: Path, monkeypatch, native_backlog_door, level, scope
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="bad-lead",
            message="term",
            harness="claude",
            cwd=tmp_path,
            role_level=level,
            role_scope=scope,
        )
    assert exc.value.exit_code == 2
    assert not load_registry(), "a refused role must write no registry row"


def test_dispatch_spawn_pane_refuses_invalid_role_values(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """The pane path takes the same pair from the same in-process callers, so it
    needs the same guard - the CLI seam is not the only door to either."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane

    def _explode(*a, **k):
        raise AssertionError("a refused role must not reach the pane runner")

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn_pane(
            name="bad-lead",
            message="term",
            provider="claude",
            cwd=tmp_path,
            runner=_explode,
            role_level=-1,
            role_scope="epic-x",
        )
    assert exc.value.exit_code == 2


def test_dispatch_spawn_pane_refuses_a_duplicate_role_before_launch(
    tmp_path: Path, monkeypatch, native_backlog_door
) -> None:
    """Same guard as the bg door: a live holder over the requested scope means
    the runner must never be reached, whether the launch was refused for a bad
    value or for a scope another live row already holds."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane

    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="sitting-lead",
                cwd=str(tmp_path),
                log_path="",
                harness="claude",
                harness_session_id="sess-sitting-lead",
                status="busy",
                role_level=2,
                role_scope="epic-x",
                role_grantor="human",
            )
        ]
    )

    def _explode(*a, **k):
        raise AssertionError("a refused role must not reach the pane runner")

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn_pane(
            name="pretender",
            message="term",
            provider="claude",
            cwd=tmp_path,
            runner=_explode,
            role_level=2,
            role_scope="epic-x",
        )
    assert exc.value.exit_code == 2
    assert "--hand-off" in str(exc.value)


# --- the literal copies in cli.py must not drift from registry ---------------


@pytest.mark.parametrize("flag", ["--promote"])
def test_both_role_spellings_stay_on_the_python_path(flag: str) -> None:
    """A role-bearing bg spawn must NOT exec the Rust client, which parses
    neither spelling and would die on an unknown flag.

    The short form is the one that matters here and the one a detector is most
    likely to miss: the docs teach `-k etl -k web` for a portfolio, so knowing
    only `--promote` would route exactly the multi-scope case into the binary. The
    pane substrate is excluded from the assertion on purpose - it diverts on its
    own, so it would pass with or without this guard and prove nothing."""
    from fno.agents.rust_runtime import (
        _is_promotion_bearing_spawn,
        _is_pane_substrate_spawn,
    )

    args = ["spawn", "w", "--substrate", "thread", flag, "etl", flag, "web"]
    assert _is_promotion_bearing_spawn("spawn", args) is True
    assert _is_pane_substrate_spawn("spawn", args) is False


def test_a_role_after_the_argv_break_belongs_to_the_payload() -> None:
    """`-k` past `--argv` is the spawned command's flag, not fno's, so it must not
    drag an otherwise-Rustable spawn onto the Python path."""
    from fno.agents.rust_runtime import _is_promotion_bearing_spawn

    assert not _is_promotion_bearing_spawn("spawn", ["spawn", "w", "--argv", "-k", "etl"])
