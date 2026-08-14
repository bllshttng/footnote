"""A cross-checkout event type must reach the global log, not just the project one.

The project log is written wherever the emitter runs, which for a review is a
worktree. The global log is the one file every checkout of a repo stands in, so
it is where a cross-checkout reader looks.

These tests drive the REAL path resolution through ``FNO_CONFIG``. An earlier
version monkeypatched ``fno.paths.state_dir`` and passed against either
implementation, which pinned nothing: the whole defect it was written for is
that the writer used ``state_dir()`` while the reader in ``fno/pr/_reviews.py``
uses ``global_events_json()``, and those two diverge on exactly the configs a
monkeypatch erases. ``test_relative_state_dir_still_mirrors_where_the_reader_looks``
is the one that fails against the old code.
"""

from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from fno.events.cli import GLOBAL_MIRROR_TYPES, cli as events_app

runner = CliRunner()

ATTESTATION = (
    '{"reviewer": "code-review", "head_sha": "deadbeef", "verdict": "pass",'
    ' "session_id": "s1"}'
)


def _configure(tmp_path: Path, monkeypatch, state_dir: str) -> Path:
    """Point HOME and config.state_dir at tmp_path, and return the global log.

    Returns the path the READER computes (``global_events_json()``), never the
    one the writer happens to use, so a writer that drifts fails the assert.
    """
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))

    cfg = tmp_path / "config.toml"
    cfg.write_text(f'state_dir = "{state_dir}"\n')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    # A RELATIVE state_dir anchors on the repo root, and without this the run
    # creates that directory inside the real checkout. Anchor it under tmp_path.
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path / "anchor"))

    from fno import config as config_mod
    from fno import paths as paths_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    return paths_mod.global_events_json()


def _emit(project: Path, type_: str, data: str = "{}"):
    project.parent.mkdir(parents=True, exist_ok=True)
    return runner.invoke(
        events_app,
        ["emit", "--type", type_, "--data", data,
         "--events", str(project), "--source", "test"],
    )


def _rows(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def _types(path: Path) -> list[str]:
    return [r.get("type") for r in _rows(path)]


def test_review_attestation_reaches_both_logs(tmp_path, monkeypatch):
    global_log = _configure(tmp_path, monkeypatch, str(tmp_path / "state"))
    project = tmp_path / "project" / ".fno" / "events.jsonl"
    result = _emit(project, "review_attestation", ATTESTATION)
    assert result.exit_code == 0, result.output
    assert _types(project) == ["review_attestation"]
    # The whole point: a reader in another checkout can see it.
    assert _types(global_log) == ["review_attestation"], (
        "the attestation stayed worktree-local, so a merge from canonical "
        "cannot see it"
    )


def test_relative_state_dir_still_mirrors_where_the_reader_looks(tmp_path, monkeypatch):
    """The regression test. A relative state_dir splits writer from reader.

    ``ledger_json()`` refuses to follow a relative ``state_dir`` into a repo
    checkout and falls back to ``~/.fno``, so ``global_events_json()`` does too.
    A writer using ``state_dir() / "events.jsonl"`` lands the row in
    ``<cwd>/.fno-relative/events.jsonl``, which no reader ever opens - the
    mirror silently does nothing on exactly this config.
    """
    global_log = _configure(tmp_path, monkeypatch, ".fno-relative/")
    assert global_log == Path(tmp_path / "home" / ".fno" / "events.jsonl"), (
        "the reader's global path should fall back to ~/.fno on a relative "
        f"state_dir, got {global_log}"
    )
    project = tmp_path / "project" / ".fno" / "events.jsonl"
    result = _emit(project, "review_attestation", ATTESTATION)
    assert result.exit_code == 0, result.output
    assert _types(project) == ["review_attestation"]
    assert _types(global_log) == ["review_attestation"], (
        "mirrored to a path the reader does not read"
    )


def test_an_ordinary_event_stays_project_local(tmp_path, monkeypatch):
    """The positive control for the tests above.

    Without it, "attestation reaches global" would pass just as happily against
    an implementation that mirrored every event, which would double the global
    log for no reason and prove nothing about the mirror set.
    """
    global_log = _configure(tmp_path, monkeypatch, str(tmp_path / "state"))
    project = tmp_path / "project" / ".fno" / "events.jsonl"
    result = _emit(project, "daemon_started")
    assert result.exit_code == 0, result.output
    assert _types(project) == ["daemon_started"]
    assert _types(global_log) == []


def test_the_mirrored_copy_is_repo_scoped_and_the_project_copy_is_not(
    tmp_path, monkeypatch
):
    """The global log is cross-project, so its rows need an identity.

    `pr` and `head_sha` are not enough on their own: a fork shares head SHAs.
    loopcheck stamps `repo` on every coverage row it writes there for exactly
    this reason, and an unscoped attestation beside them is the cross-repo
    false positive that scoping exists to prevent.
    """
    import subprocess

    global_log = _configure(tmp_path, monkeypatch, str(tmp_path / "state"))
    checkout = tmp_path / "checkout"
    checkout.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=checkout, check=True)
    subprocess.run(
        ["git", "remote", "add", "origin", "git@github.com:Owner/Repo.git"],
        cwd=checkout, check=True,
    )
    # The identity comes from the resolved REPO ROOT, not from the CWD, so that
    # an `--events` aimed at another checkout cannot stamp this one's name on it.
    # Setting only the CWD here would leave the row unscoped, which is the assert
    # below failing rather than passing by accident.
    monkeypatch.setenv("FNO_REPO_ROOT", str(checkout))
    monkeypatch.chdir(tmp_path)
    from fno import paths as paths_mod
    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]

    project = checkout / ".fno" / "events.jsonl"
    result = _emit(project, "review_attestation", ATTESTATION)
    assert result.exit_code == 0, result.output
    assert _rows(global_log)[0]["data"]["repo"] == "github.com/owner/repo"
    # Not on the project copy: it needs no scoping, and the row already written
    # must not change under the reader that is about to read it.
    assert "repo" not in _rows(project)[0]["data"]


def test_the_mirror_set_is_small_and_named():
    # Membership is earned by having a cross-checkout reader. review_coverage
    # is absent on purpose: loopcheck.rs emits it to both logs itself, and a
    # second writer here would double every row the merge gate counts.
    assert "review_attestation" in GLOBAL_MIRROR_TYPES
    assert "review_coverage" not in GLOBAL_MIRROR_TYPES
