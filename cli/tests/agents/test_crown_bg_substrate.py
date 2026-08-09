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
    """Isolated fno home with a fake claude binary on PATH."""
    from tests.agents._fake_claude import install_fake_claude

    use_tmpdir(monkeypatch, tmp_path)
    bin_dir = tmp_path / "bin"
    install_fake_claude(bin_dir)
    monkeypatch.setenv("PATH", str(bin_dir))
    return tmp_path


def _spawn(*args: str):
    from fno.agents.cli import agents_app

    return CliRunner().invoke(agents_app, list(args), catch_exceptions=False)


def _row(name: str) -> AgentEntry:
    entry = next((e for e in load_registry() if e.name == name), None)
    assert entry is not None, f"no registry row named {name!r}"
    return entry


# --- the crown lands on bg ---------------------------------------------------


def test_bg_spawn_stamps_the_crown(bg_home, monkeypatch) -> None:
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "parent-sess-abc")

    result = _spawn(
        "spawn", "--name", "king-bg", "-H", "claude", "reign",
        "--substrate", "bg", "--crown", "level=1,scope=epic-x",
    )
    assert result.exit_code == 0, result.output

    row = _row("king-bg")
    assert row.crown_level == 1
    assert row.crown_scope == "epic-x"
    # Provenance, not self-declaration: the grantor is the session that spawned it.
    assert row.crown_grantor == "parent-sess-abc"
    assert row.crown_label == "L1 epic-x"


def test_bg_crown_grantor_defaults_to_human(bg_home, monkeypatch) -> None:
    """No parent session env == a human's own shell, same rule as the pane path."""
    result = _spawn(
        "spawn", "--name", "king-bg-human", "-H", "claude", "reign",
        "--substrate", "bg", "--crown", "level=0,scope=proj-a",
    )
    assert result.exit_code == 0, result.output
    assert _row("king-bg-human").crown_grantor == "human"


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
    worker is recoverable via `fno agents crown`. So the spawn SUCCEEDS and the
    crown is declined, matching the pane path rather than refusing the launch."""
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="sitting-king",
                cwd=str(bg_home),
                log_path="",
                harness="claude",
                status="busy",  # active, not merely the literal "live"
                crown_level=1,
                crown_scope="epic-x",
                crown_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "pretender", "-H", "claude", "reign",
        "--substrate", "bg", "--crown", "level=1,scope=epic-x",
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
                status="exited",
                crown_level=1,
                crown_scope="epic-y",
                crown_grantor="human",
            )
        ]
    )

    result = _spawn(
        "spawn", "--name", "successor", "-H", "claude", "reign",
        "--substrate", "bg", "--crown", "level=1,scope=epic-y",
    )
    assert result.exit_code == 0, result.output
    assert _row("successor").crown_level == 1


# --- headless stays refused --------------------------------------------------


@pytest.mark.parametrize("one_shot_args", [["--substrate", "headless"], ["-p"], ["--once"]])
def test_headless_crown_is_refused(bg_home, one_shot_args) -> None:
    """A one-shot exits after one answer, so its crown names a dead ruler before
    the grantor's next turn. This is the ONE substrate the refusal still covers."""
    result = _spawn(
        "spawn", "--name", "one-shot-king", "-H", "claude", "reign",
        *one_shot_args, "--crown", "level=1,scope=epic-z",
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
        "-p", "--crown", "level=1,scope=epic-z",
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


# --- the two terminal-status copies must not drift ---------------------------


def test_crown_terminal_set_parity() -> None:
    """cli.py keeps a literal copy rather than importing registry at module scope
    (~30ms on every `fno agents` invocation for one frozenset). That is only safe
    while the copies agree: a set that forgets a status reads a dead king as
    reigning, or a live one as dead, and mints a second crown over one scope."""
    from fno.agents.cli import _TERMINAL_STATUSES
    from fno.agents.registry import TERMINAL_STATUSES

    assert _TERMINAL_STATUSES == TERMINAL_STATUSES
