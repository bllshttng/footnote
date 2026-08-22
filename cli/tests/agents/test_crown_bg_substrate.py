"""`fno agents spawn --crown --substrate bg`: the crown rides the bg substrate.

A crown is three registry fields (`crown_level` / `crown_scope` / `crown_grantor`)
and nothing in it needs a PTY. The substrate axis it actually cares about is
REIGN LENGTH: a king must outlive the grant. `bg` qualifies - a bg worker is a
full persistent conversation in claude's agent view, attachable and resumable,
differing from a pane only in who draws it. `headless` does not: it answers once
and exits, so its crown is orphaned at birth.

These tests exercise the END-TO-END CLI path (`spawn --crown --substrate bg`),
not `_claude_create_path` in isolation. That is deliberate: the original defect
was a refusal at the CLI seam sitting in front of unplumbed params, so a test
that called the helper directly would have passed against the broken build.
"""
from __future__ import annotations

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
def bg_home(tmp_path, monkeypatch):
    """Isolated fno home with a fake claude, a graph holding two epics, and one
    configured project. The territory has to exist because the rung is DERIVED
    from it: a scope naming nothing is refused, so a fixture without a graph
    would test the refusal path in every case."""
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
    graph_path.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "epic-x", "type": "epic", "project": "alpha"},
                    {"id": "epic-y", "type": "epic", "project": "alpha"},
                    {"id": "epic-z", "type": "epic", "project": "alpha"},
                ]
            }
        ),
        encoding="utf-8",
    )
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


# --- the crown lands on bg ---------------------------------------------------


def test_bg_spawn_stamps_the_crown(bg_home, monkeypatch) -> None:
    import fno.king.state as king_state

    monkeypatch.setattr(king_state, "king_loop_enabled", lambda: True)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-sess-abc")
    # Seat the grantor as a registered king over epic-x; the spawn is then a
    # succession (it hands its own scope to the heir). An agent identity with no
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
                crown_level=2,
                crown_scope="epic-x",
                crown_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "king-bg", "-H", "claude", "reign",
        "--substrate", "bg", "--cwd", str(bg_home), "--crown", "epic-x",
    )
    assert result.exit_code == 0, result.output

    row = _row("king-bg")
    assert row.crown_level == 2, "an epic is a Director"
    assert row.crown_scope == "epic-x"
    # Provenance, not self-declaration: the grantor is the session that spawned it.
    assert row.crown_grantor == "parent-sess-abc"
    assert row.crown_label == "L2 epic-x"
    manifest = Path(row.cwd) / ".fno" / "kings" / "epic-x.md"
    assert king_state.parse_manifest(manifest)["harness_session_id"] == (
        row.harness_session_id or row.short_id
    )


def test_bg_crown_grantor_defaults_to_human(bg_home, monkeypatch) -> None:
    """No parent session env == a human's own shell, same rule as the pane path."""
    result = _spawn(
        "spawn", "--name", "king-bg-human", "-H", "claude", "reign",
        "--substrate", "bg", "--crown", "alpha",
    )
    assert result.exit_code == 0, result.output
    assert _row("king-bg-human").crown_grantor == "human"
    assert _row("king-bg-human").crown_level == 1, "a project is a project king"
    assert "king loop disabled" in result.output


def test_bg_spawn_without_crown_leaves_the_fields_none(bg_home, monkeypatch) -> None:
    """The stamp is opt-in: an ordinary bg spawn is not accidentally crowned."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-sess-abc")

    result = _spawn(
        "spawn", "--name", "plain-bg", "-H", "claude", "work", "--substrate", "bg"
    )
    assert result.exit_code == 0, result.output

    row = _row("plain-bg")
    assert (row.crown_level, row.crown_scope, row.crown_grantor) == (None, None, None)


# --- one live crown per scope, enforced on bg too ----------------------------


def test_bg_spawn_declines_a_duplicate_crown_and_launches_uncrowned(
    bg_home, monkeypatch
) -> None:
    """A second crown over one scope is the unrecoverable failure; an uncrowned
    worker can still be crowned later by spawning again. So the spawn SUCCEEDS and the
    crown is declined, matching the pane path rather than refusing the launch."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="sitting-king",
                cwd=str(bg_home),
                log_path="",
                harness="claude",
                harness_session_id="sess-sitting-king",  # x-7bcd: resolvable handle
                status="busy",  # active, not merely the literal "live"
                crown_level=2,
                crown_scope="epic-x",
                crown_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "pretender", "-H", "claude", "reign",
        "--substrate", "bg", "--crown", "epic-x",
    )
    assert result.exit_code == 0, result.output

    row = _row("pretender")
    assert row.crown_level is None, "a duplicate crown must not be stamped"
    assert row.crown_scope is None
    assert row.crown_grantor is None
    assert "crown declined" in result.output


def test_bg_spawn_crowns_over_a_scope_whose_king_is_terminal(bg_home, monkeypatch) -> None:
    """A dead king does not block succession - that is the orphaned scope the
    crown exists to let someone reclaim."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="dead-king",
                cwd=str(bg_home),
                log_path="",
                harness="claude",
                harness_session_id="sess-dead-king",  # x-7bcd: resolvable handle
                status="exited",
                crown_level=2,
                crown_scope="epic-y",
                crown_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "successor", "-H", "claude", "reign",
        "--substrate", "bg", "--crown", "epic-y",
    )
    assert result.exit_code == 0, result.output
    assert _row("successor").crown_level == 2
    dead = _row("dead-king")
    assert (dead.crown_level, dead.crown_scope, dead.crown_grantor) == (
        None,
        None,
        None,
    )
    assert [row.name for row in load_registry() if row.crown_scope == "epic-y"] == [
        "successor"
    ]


# --- headless stays refused --------------------------------------------------


@pytest.mark.parametrize("one_shot_args", [["--substrate", "headless"], ["-p"], ["--once"]])
def test_headless_crown_is_refused(bg_home, one_shot_args) -> None:
    """A one-shot exits after one answer, so its crown names a dead ruler before
    the grantor's next turn. This is the ONE substrate the refusal still covers."""
    result = _spawn(
        "spawn", "--name", "one-shot-king", "-H", "claude", "reign",
        *one_shot_args, "--crown", "epic-z",
    )
    assert result.exit_code == 2
    assert "outlives the grant" in result.output
    assert not [e for e in load_registry() if e.name == "one-shot-king"], (
        "a refused crown must launch nothing"
    )


def test_refusal_does_not_claim_bg_is_unsupported(bg_home) -> None:
    """The old message said bg crowns were 'not yet supported', which read as a
    capability claim about the substrate when it was really a plumbing gap - a
    reader took it at face value and filed a design question against it. The
    replacement must name what DOES work and must not resurrect that phrasing."""
    result = _spawn(
        "spawn", "--name", "one-shot-king", "-H", "claude", "reign",
        "-p", "--crown", "epic-z",
    )
    assert "not yet supported" not in result.output
    assert "--substrate pane" in result.output and "--substrate bg" in result.output


# --- in-process callers get the same guards ----------------------------------


def test_dispatch_spawn_refuses_a_crown_it_cannot_stamp(tmp_path: Path, monkeypatch) -> None:
    """The guard lives in dispatch_spawn, not only at the CLI seam: only the
    claude bg branch reaches the stamping helper, so any other provider would
    drop the crown while reporting success. A guard on one of N reachable paths
    is decorative."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="codex-king",
            message="reign",
            provider="codex",
            cwd=tmp_path,
            crown_level=1,
            crown_scope="epic-x",
        )
    assert exc.value.exit_code == 2
    assert "claude-only" in str(exc.value)


def test_dispatch_spawn_refuses_a_one_shot_crown(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="one-shot-king",
            message="reign",
            provider="claude",
            cwd=tmp_path,
            headless=True,
            crown_level=1,
            crown_scope="epic-x",
        )
    assert exc.value.exit_code == 2
    assert "outlives the grant" in str(exc.value)


# --- crown values are validated on every writer, not just the CLI ------------
#
# The CLI parses `--crown` through _parse_crown, but both spawn dispatchers take
# (level, scope) directly from in-process callers. A value that skips validation
# is written to the SHARED registry: Rust's crown_level is Option<u32>
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
def test_dispatch_spawn_refuses_invalid_crown_values(
    tmp_path: Path, monkeypatch, level, scope
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError, dispatch_spawn

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn(
            name="bad-king",
            message="reign",
            provider="claude",
            cwd=tmp_path,
            crown_level=level,
            crown_scope=scope,
        )
    assert exc.value.exit_code == 2
    assert not load_registry(), "a refused crown must write no registry row"


def test_dispatch_spawn_pane_refuses_invalid_crown_values(
    tmp_path: Path, monkeypatch
) -> None:
    """The pane path takes the same pair from the same in-process callers, so it
    needs the same guard - the CLI seam is not the only door to either."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import dispatch_spawn_pane

    def _explode(*a, **k):
        raise AssertionError("a refused crown must not reach the pane runner")

    with pytest.raises(DispatchAskError) as exc:
        dispatch_spawn_pane(
            name="bad-king",
            message="reign",
            provider="claude",
            cwd=tmp_path,
            runner=_explode,
            crown_level=-1,
            crown_scope="epic-x",
        )
    assert exc.value.exit_code == 2


def test_valid_crown_pairs_and_the_uncrowned_pair_pass() -> None:
    """The validator must not reject the two shapes that are legal: a real crown
    at each ladder rung, and both-None (an ordinary uncrowned spawn)."""
    from fno.agents.crown import crown_validation_error

    assert crown_validation_error(None, None) is None
    for lvl in (0, 1, 2):
        assert crown_validation_error(lvl, "epic-x") is None


# --- the literal copies in cli.py must not drift from registry ---------------


@pytest.mark.parametrize("flag", ["--crown", "-k"])
def test_both_crown_spellings_stay_on_the_python_path(flag: str) -> None:
    """A crown-bearing bg spawn must NOT exec the Rust client, which parses
    neither spelling and would die on an unknown flag.

    The short form is the one that matters here and the one a detector is most
    likely to miss: the docs teach `-k etl -k web` for a portfolio, so knowing
    only `--crown` would route exactly the multi-scope case into the binary. The
    pane substrate is excluded from the assertion on purpose - it diverts on its
    own, so it would pass with or without this guard and prove nothing."""
    from fno.agents.rust_runtime import (
        _is_crown_bearing_spawn,
        _is_pane_substrate_spawn,
    )

    args = ["spawn", "w", "--substrate", "bg", flag, "etl", flag, "web"]
    assert _is_crown_bearing_spawn("spawn", args) is True
    assert _is_pane_substrate_spawn("spawn", args) is False


def test_a_crown_after_the_argv_break_belongs_to_the_payload() -> None:
    """`-k` past `--argv` is the spawned command's flag, not fno's, so it must not
    drag an otherwise-Rustable spawn onto the Python path."""
    from fno.agents.rust_runtime import _is_crown_bearing_spawn

    assert not _is_crown_bearing_spawn("spawn", ["spawn", "w", "--argv", "-k", "etl"])
