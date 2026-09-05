"""The migration primitive must never move a live checkout journal to a foreign space.

x-d2e9: ``migrate_from_checkout`` guarded only on ``new.exists()``, so any
process whose spaces root resolves elsewhere (a pytest sandbox via
``FNO_SPACES_DIR``, a worktree setup run, a spawned worker) moved the
developer's real ``<repo>/.fno/events.jsonl`` into that root. The MOVED-TO
pointer exists so a stale reader fails loud; a move into a root the repo's
own processes never read again is the opposite: the gate reads a dead file
and reports every PR unreviewed.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

import fno.paths as paths_mod


@pytest.fixture(autouse=True)
def _isolated_roots(monkeypatch: pytest.MonkeyPatch):
    """Reset the memoized resolvers so env pins set per test take effect."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_EVENTS_PATH", raising=False)
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]


def _git_repo(path: Path) -> Path:
    """A real checkout (``git init``), so repo-root discovery has work to do."""
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q", str(path)], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.email", "t@t"], check=True)
    subprocess.run(["git", "-C", str(path), "config", "user.name", "t"], check=True)
    return path


# Scenario table shared with crates/fno-agents/src/paths.rs (AC3-EDGE): the
# two parity legs must refuse the same destinations. Keep the rows aligned
# with the Rust module's ``migrate_destination_parity`` test.
PARITY_SCENARIOS = (
    # (name, spaces-root pin, expect move)
    ("foreign-env-root-refuses", "env", False),
    ("durable-config-root-migrates", "config", True),
)


def test_ac1_foreign_spaces_root_leaves_checkout_journal_alone(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A per-process FNO_SPACES_DIR is not the repo's home: no move."""
    repo = _git_repo(tmp_path / "repo")
    journal = repo / ".fno" / "events.jsonl"
    journal.parent.mkdir(parents=True)
    journal.write_text("ts=2026-09-05T12:00:00Z type=review_attestation\n", encoding="utf-8")
    monkeypatch.setenv("FNO_SPACES_DIR", str(tmp_path / "sandbox-spaces"))
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    monkeypatch.chdir(repo)
    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]

    paths_mod.project_events_json()

    assert journal.read_text(encoding="utf-8") == (
        "ts=2026-09-05T12:00:00Z type=review_attestation\n"
    )


def test_ac2_durable_space_still_migrates_once_and_writes_marker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A genuine legacy checkout journal still moves, once, to the durable root."""
    repo = _git_repo(tmp_path / "repo")
    journal = repo / ".fno" / "events.jsonl"
    journal.parent.mkdir(parents=True)
    journal.write_text("legacy rows\n", encoding="utf-8")
    spaces = tmp_path / "declared-spaces"
    (tmp_path / "settings.yaml").write_text(
        f"paths:\n  spaces_dir: {spaces}\n", encoding="utf-8"
    )
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "settings.yaml"))
    monkeypatch.delenv("FNO_SPACES_DIR", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    monkeypatch.chdir(repo)
    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]

    resolved = paths_mod.project_events_json()

    slug = paths_mod.space_slug(repo.resolve())
    assert not journal.exists(), "the legacy file must move exactly once"
    assert resolved == spaces / slug / "events.jsonl"
    assert (spaces / slug / "events.jsonl").read_text(encoding="utf-8") == "legacy rows\n"
    marker = repo / ".fno" / "MOVED-TO"
    assert marker.exists(), "MOVED-TO must name the space so a stale reader fails loud"
    assert str(spaces / slug) in marker.read_text(encoding="utf-8")


@pytest.mark.parametrize(("name", "root_kind", "expect_move"), PARITY_SCENARIOS)
def test_ac3_parity_scenarios(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, name: str, root_kind: str, expect_move: bool
) -> None:
    """Both legs answer identically for the same scenario (mirror: paths.rs)."""
    repo = _git_repo(tmp_path / "repo")
    journal = repo / ".fno" / "events.jsonl"
    journal.parent.mkdir(parents=True)
    journal.write_text("rows\n", encoding="utf-8")
    spaces = tmp_path / "declared-spaces"
    if root_kind == "env":
        monkeypatch.setenv("FNO_SPACES_DIR", str(tmp_path / "sandbox-spaces"))
    else:
        (tmp_path / "settings.yaml").write_text(
            f"paths:\n  spaces_dir: {spaces}\n", encoding="utf-8"
        )
        monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "settings.yaml"))
        monkeypatch.delenv("FNO_SPACES_DIR", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    monkeypatch.chdir(repo)
    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]

    paths_mod.project_events_json()

    moved = not journal.exists()
    assert moved is expect_move, f"scenario {name}: moved={moved}, expected {expect_move}"
