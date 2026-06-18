"""Unit tests for paths.inbox_root_for(project_name).

Used by cross-project inbox routing: when sender in project A writes to
recipient in project B, the recipient's inbox path needs `{project}` to
resolve to "B", not the sender's current `config.project.id`.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno import paths


def _write_settings(tmp_path, body: str) -> None:
    settings_yaml = tmp_path / ".fno" / "settings.yaml"
    settings_yaml.parent.mkdir(parents=True, exist_ok=True)
    settings_yaml.write_text(body)


def test_inbox_root_for_vault_derived_bare_name(monkeypatch, tmp_path):
    """A bare obsidian.vault name maps to ~/<name>, preserving the historical
    ``~/<vault>/internal/agents/<project>/inbox`` cross-project default."""
    _write_settings(
        tmp_path,
        "config:\n  obsidian:\n    enabled: true\n    vault: myvault\n",
    )
    # Pin HOME so the bare name resolves under the tempdir, not the real home.
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    paths._settings.cache_clear()  # type: ignore[attr-defined]
    result = paths.inbox_root_for("acme-web")
    assert result == (
        tmp_path / "myvault" / "internal" / "agents" / "acme-web" / "inbox"
    ).resolve()


def test_inbox_root_for_vault_derived_absolute(monkeypatch, tmp_path):
    """An absolute obsidian.vault path is honored as-is."""
    vault = tmp_path / "abs-vault"
    _write_settings(
        tmp_path,
        f"config:\n  obsidian:\n    enabled: true\n    vault: {vault}\n",
    )
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    paths._settings.cache_clear()  # type: ignore[attr-defined]
    result = paths.inbox_root_for("acme-web")
    assert result == (
        vault / "internal" / "agents" / "acme-web" / "inbox"
    ).resolve()


def test_inbox_root_for_neutral_default_when_obsidian_disabled(monkeypatch, tmp_path):
    """With Obsidian disabled and no override, defaults to a neutral
    ``state_dir()/inbox/agents/<project>/inbox`` - never a stranger's ~/your-vault."""
    state = tmp_path / "state"
    _write_settings(
        tmp_path,
        f"config:\n  obsidian:\n    enabled: false\n  state_dir: {state}\n",
    )
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    paths._settings.cache_clear()  # type: ignore[attr-defined]
    result = paths.inbox_root_for("acme-web")
    assert result == (
        state / "inbox" / "agents" / "acme-web" / "inbox"
    ).resolve()


def test_inbox_root_for_substitutes_target_project_not_current(monkeypatch, tmp_path):
    """The {project} template resolves to the passed name, not config.project.id."""
    # Build a fake settings.yaml that uses {project} template
    settings_yaml = tmp_path / ".fno" / "settings.yaml"
    settings_yaml.parent.mkdir(parents=True, exist_ok=True)
    settings_yaml.write_text(
        "config:\n"
        "  obsidian:\n"
        "    enabled: true\n"
        f"    vault: {tmp_path}/vault\n"
        '  paths:\n'
        '    inbox_dir: "{vault}/agents/{project}/inbox"\n'
        '  project:\n'
        '    id: "sender-project"\n'
    )
    (tmp_path / "vault").mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    paths._settings.cache_clear()  # type: ignore[attr-defined]

    # Sender's project id is "sender-project" but we route to "recipient-project".
    result = paths.inbox_root_for("recipient-project")
    expected = (tmp_path / "vault" / "agents" / "recipient-project" / "inbox").resolve()
    assert result == expected, f"got {result}, want {expected}"


def test_inbox_dir_for_uses_inbox_root_for(monkeypatch, tmp_path):
    """inbox_dir_for(project) routes through paths.inbox_root_for when no env override."""
    _write_settings(
        tmp_path,
        "config:\n  obsidian:\n    enabled: true\n    vault: myvault\n",
    )
    # Ensure no env override is set.
    monkeypatch.delenv("FNO_INBOX_ROOT", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    paths._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.inbox.store import inbox_dir_for
    result = inbox_dir_for("foo-project")
    assert result == (
        tmp_path / "myvault" / "internal" / "agents" / "foo-project" / "inbox"
    ).resolve()


@pytest.mark.parametrize(
    "bad_name",
    [
        "..",
        "../etc",
        "../../etc/passwd",
        "name/sub",
        "name\\sub",
        ".hidden",
        "",
        "with space",
    ],
)
def test_inbox_root_for_rejects_path_traversal(monkeypatch, tmp_path, bad_name):
    """paths.inbox_root_for must reject path-traversal attempts at the entry point.

    The validator lives in paths.py (not only in inbox.store.inbox_dir_for)
    because paths is a shared utility module - any future caller reaching
    inbox_root_for directly with an unvalidated name would otherwise inject
    `..` into the {project} substitution.
    """
    monkeypatch.setenv("HOME", str(tmp_path))
    paths._settings.cache_clear()  # type: ignore[attr-defined]

    with pytest.raises(ValueError):
        paths.inbox_root_for(bad_name)
