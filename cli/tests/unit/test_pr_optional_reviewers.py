from pathlib import Path

from fno.pr._reviews import optional_reviewer_names


def test_identity_free_peers_are_not_github_optional_logins(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    repo = tmp_path / "repo"
    config_dir = repo / ".fno"
    config_dir.mkdir(parents=True)
    (config_dir / "config.toml").write_text(
        '[review]\npeers = ["codex", '
        '{provider = "gemini", identity = "fno-gemini-bot"}]\n'
    )

    names = optional_reviewer_names(str(repo))

    assert "codex" not in names
    assert "gemini" not in names
    assert "fno-gemini-bot" in names
