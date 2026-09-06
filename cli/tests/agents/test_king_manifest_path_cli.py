"""`fno agents king manifest-path` resolves a live crown to its manifest.

The stop hook calls the deprecated `fno king manifest-path` spelling, which
verb_moves forwards onto the agents app. The verb missed the agents fold, so
the resolver exited 2 and every stop on an active kings dir burned its
unavailable-retries before allowing exit. These tests pin the verb onto the
agents app and pin the deprecated spelling onto the same command.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.registry import AgentEntry, update_registry
from fno.king.state import king_manifest_path, write_manifest
from fno.paths_testing import use_tmpdir

CALLER_SESSION = "0c1f2f9a-2222-4000-8000-000000000002"
SCOPE = "epic-y"


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
def court(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.chdir(tmp_path)
    return tmp_path


def _seat_crown():
    update_registry(
        lambda rows: rows
        + [
            AgentEntry(
                name="crowned-king",
                cwd="/tmp",
                log_path="",
                harness="claude",
                harness_session_id=CALLER_SESSION,
                status="busy",
                crown_level=2,
                crown_scope=SCOPE,
                crown_grantor="human",
            )
        ]
    )
    return king_manifest_path(SCOPE, state_root=Path(".fno"))


def _manifest_path(*args: str):
    from fno.king.cli import agents_king_app

    return CliRunner().invoke(
        agents_king_app, ["manifest-path", *args]
    )


def test_manifest_path_resolves_a_live_crown(court) -> None:
    manifest = _seat_crown()
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)

    result = _manifest_path(
        "--harness-session-id", CALLER_SESSION,
        "--state-root", str(court / ".fno"),
    )

    assert result.exit_code == 0, result.output
    assert str(manifest) in result.output


def test_manifest_path_frees_a_stranger(court) -> None:
    """No registry row names this session: exit 1, the hook's "stranger goes
    free" contract, never the exit-2 parse failure the missing verb produced."""
    result = _manifest_path(
        "--harness-session-id", CALLER_SESSION,
        "--state-root", str(court / ".fno"),
    )

    assert result.exit_code == 1, result.output


def test_deprecated_king_spelling_forwards_onto_the_agents_app(court) -> None:
    """The stop hook's literal argv: `fno king manifest-path ...`. The banner
    must stay on stderr so the hook's stdout capture reads a clean path."""
    from fno.cli import app

    manifest = _seat_crown()
    write_manifest(manifest, scope=SCOPE, harness_session_id=CALLER_SESSION)

    result = CliRunner().invoke(
        app,
        [
            "king", "manifest-path",
            "--harness-session-id", CALLER_SESSION,
            "--state-root", str(court / ".fno"),
        ],
    )

    assert result.exit_code == 0, result.output
    assert str(manifest) in result.output
    assert "is now" in (result.stderr or ""), "the rename banner names the new spelling"


def test_king_init_canonicalizes_a_set_scope_into_one_manifest(
    court, monkeypatch
) -> None:
    """`king init --scope e-2,e-1,e-2` is one crown over {e-1,e-2}: the manifest
    lands at the canonical joined name, the one every spelling of the set and
    every scope-keyed reader resolves."""
    import fno.king.state as king_state
    from fno.cli import app

    monkeypatch.setattr(king_state, "king_loop_enabled", lambda: True)
    monkeypatch.setattr(king_state, "king_state_root", lambda: court / ".fno")

    result = CliRunner().invoke(
        app,
        [
            "king", "init",
            "--scope", "e-2,e-1,e-2",
            "--harness-session-id", CALLER_SESSION,
        ],
    )

    assert result.exit_code == 0, result.output
    canonical = king_manifest_path("e-1,e-2", state_root=court / ".fno")
    assert canonical.exists()
    assert str(canonical) in result.output
    stray = king_manifest_path("e-2,e-1,e-2", state_root=court / ".fno")
    assert not stray.exists(), "a non-canonical spelling armed a second manifest"


def test_king_init_resolves_a_project_alias_into_the_canonical_manifest(
    court, monkeypatch
) -> None:
    """`king init --scope a` must arm kings/alpha.md. canonical_scope sorted and
    deduped but never resolved the alias, so the manifest landed at a path no
    row-keyed reader resolves (they build from row.crown_scope) while the
    uncrowned-row warning stayed silent: it compares through alias
    normalization, so 'a' and 'alpha' read as one crown."""
    import fno.king.state as king_state
    from fno.cli import app
    from fno.projects import resolve as proj_resolve

    cfg = court / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\nprojects = [{ name = "alpha", short_name = "a" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()

    monkeypatch.setattr(king_state, "king_loop_enabled", lambda: True)
    monkeypatch.setattr(king_state, "king_state_root", lambda: court / ".fno")

    result = CliRunner().invoke(
        app,
        ["king", "init", "--scope", "a", "--harness-session-id", CALLER_SESSION],
    )

    assert result.exit_code == 0, result.output
    canonical = king_manifest_path("alpha", state_root=court / ".fno")
    assert canonical.exists()
    assert not (court / ".fno" / "kings" / "a.md").exists()


def test_king_init_refuses_a_path_unsafe_scope_without_a_traceback(
    court, monkeypatch
) -> None:
    """The path-safety refusal in king_manifest_path is an operator typo
    ('a/b'), so it must surface as a named refusal at exit 2, not as a
    ValueError traceback."""
    import fno.king.state as king_state
    from fno.cli import app

    monkeypatch.setattr(king_state, "king_loop_enabled", lambda: True)
    monkeypatch.setattr(king_state, "king_state_root", lambda: court / ".fno")

    result = CliRunner().invoke(
        app,
        ["king", "init", "--scope", "a/b", "--harness-session-id", CALLER_SESSION],
    )

    assert result.exit_code == 2, result.output
    assert "unsafe king scope" in result.output
    assert not isinstance(result.exception, ValueError)
