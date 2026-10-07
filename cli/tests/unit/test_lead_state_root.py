from __future__ import annotations

import json
import subprocess
from pathlib import Path

from fno.paths import space_dir


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True)


def _cancel(*args: str):
    from fno.lead.cli import agents_lead_app
    from typer.testing import CliRunner

    return CliRunner().invoke(agents_lead_app, ["cancel", *args])


def test_lead_state_root_from_linked_worktree_is_canonical(
    tmp_path: Path, monkeypatch, loop_admission_ready
) -> None:
    main = tmp_path / "main"
    main.mkdir()
    _git(main, "init", "-q")
    _git(main, "config", "user.email", "test@example.com")
    _git(main, "config", "user.name", "Test")
    _git(main, "commit", "--allow-empty", "-qm", "init")
    linked = tmp_path / "linked"
    _git(main, "worktree", "add", "-q", str(linked), "-b", "feature")

    from fno.lead.state import lead_state_root
    from fno.paths import space_dir

    # The coordination root is the repo's SPACE (x-b1ee): canonical-keyed, so
    # both worktree spellings answer one path, and no checkout holds a role.
    expected = space_dir(main)
    assert lead_state_root(linked) == expected
    assert lead_state_root(main) == expected

    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    monkeypatch.chdir(linked)
    import fno.lead.state as state
    from typer.testing import CliRunner

    monkeypatch.setattr(state, "lead_loop_enabled", lambda: True)
    from fno.lead.cli import lead_app

    leads = space_dir(main) / "leads"
    stale_signal = leads / "cli.cancelled"
    stale_signal.parent.mkdir(parents=True, exist_ok=True)
    stale_signal.touch()
    result = CliRunner().invoke(
        lead_app,
        [
            "init",
            "--scope",
            "cli",
            "--harness-session-id",
            "11111111-1111-4111-8111-111111111111",
        ],
    )
    assert result.exit_code == 0, result.output
    assert (leads / "cli.md").is_file()
    assert not stale_signal.exists()
    assert str(leads / "cli.md") in result.output


def test_lead_cancel_sets_signal_and_emits_event(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    from fno.lead.state import lead_manifest_path, write_manifest

    manifest = lead_manifest_path("k")
    write_manifest(manifest, scope="k", harness_session_id="session")

    result = _cancel("--scope", "k")

    assert result.exit_code == 0, result.output
    sentinel = manifest.with_suffix(".cancelled")
    assert sentinel.is_file()
    from tests._event_rows import event_rows

    events = event_rows(space_dir(tmp_path).joinpath("events.jsonl"))
    assert any(
        event["type"] == "cancel_signal_set"
        and event["data"]["lane"] == "lead"
        and event["data"]["path"] == str(sentinel)
        for event in events
    )


def test_lead_cancel_event_failure_does_not_prevent_signal(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    from fno.lead.state import lead_manifest_path, write_manifest

    manifest = lead_manifest_path("k")
    write_manifest(manifest, scope="k", harness_session_id="session")
    import fno.events

    def fail_append(*_args, **_kwargs):
        raise OSError("events unavailable")

    monkeypatch.setattr(fno.events, "append_event", fail_append)

    result = _cancel("--scope", "k")

    assert result.exit_code == 0, result.output
    assert manifest.with_suffix(".cancelled").is_file()


def test_lead_cancel_missing_scope_names_canonical_manifest_path(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))

    result = _cancel("--scope", "missing")

    assert result.exit_code == 1
    assert str(space_dir(tmp_path) / "leads" / "missing.md") in result.output
