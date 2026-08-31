from __future__ import annotations

from pathlib import Path

from fno import config


def test_candidate_chain_accepts_a_seeded_worktree_root(monkeypatch, tmp_path: Path):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    worktree.mkdir()
    (worktree / ".git").touch()
    global_config = tmp_path / "global.toml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(global_config))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_NO_CANONICAL_CONFIG", raising=False)
    monkeypatch.setattr(
        "fno.paths.resolve_canonical_worktree",
        lambda root=None, timeout=None: canonical,
    )

    locations = config._settings_yaml_locations(worktree)
    candidates = config._candidate_paths(worktree)

    assert locations[:2] == [
        worktree / ".fno" / "settings.yaml",
        canonical / ".fno" / "settings.yaml",
    ]
    assert candidates[:4] == [
        worktree / ".fno" / "config.toml",
        worktree / ".fno" / "settings.yaml",
        canonical / ".fno" / "config.toml",
        canonical / ".fno" / "settings.yaml",
    ]


def test_repo_loader_reads_the_canonical_tier_for_a_linked_worktree(
    monkeypatch, tmp_path: Path
):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    worktree.mkdir()
    (worktree / ".git").touch()
    (canonical / ".fno").mkdir(parents=True)
    (canonical / ".fno" / "config.toml").write_text(
        "[review]\nmax_rounds = 5\ngithub_approval_satisfies = false\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.toml"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setattr(
        "fno.paths.resolve_canonical_worktree",
        lambda root=None, timeout=None: canonical,
    )

    settings = config.load_settings_for_repo(worktree)

    assert settings.review.max_rounds == 5
    assert settings.review.github_approval_satisfies is False


def test_repo_loader_keeps_local_override_allowlist_and_canonical_fallback(
    monkeypatch, tmp_path: Path, caplog
):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    worktree.mkdir()
    (worktree / ".git").touch()
    (worktree / ".fno").mkdir(parents=True)
    (canonical / ".fno").mkdir(parents=True)
    (canonical / ".fno" / "config.toml").write_text(
        "[project]\nid = 'canonical'\n[review]\nmax_rounds = 5\n",
        encoding="utf-8",
    )
    (worktree / ".fno" / "config.local.toml").write_text(
        "[project]\nid = 'local'\n[review]\nmax_rounds = 99\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.toml"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setattr(
        "fno.paths.resolve_canonical_worktree",
        lambda root=None, timeout=None: canonical,
    )

    settings = config.load_settings_for_repo(worktree)

    assert settings.project.id == "local"
    assert settings.review.max_rounds == 5
    assert "ignoring non-worktree-local key(s): review.max_rounds" in caplog.text


def test_repo_loader_preserves_a_real_worktree_config_file(monkeypatch, tmp_path: Path):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    worktree.mkdir()
    (worktree / ".git").touch()
    (worktree / ".fno").mkdir(parents=True)
    (canonical / ".fno").mkdir(parents=True)
    (worktree / ".fno" / "config.toml").write_text(
        "[review]\nmax_rounds = 7\n",
        encoding="utf-8",
    )
    (canonical / ".fno" / "config.toml").write_text(
        "[review]\ngithub_approval_satisfies = false\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.toml"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setattr(
        "fno.paths.resolve_canonical_worktree",
        lambda root=None, timeout=None: canonical,
    )

    settings = config.load_settings_for_repo(worktree)

    assert settings.review.max_rounds == 7
    assert settings.review.github_approval_satisfies is False
