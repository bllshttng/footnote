from __future__ import annotations

from pathlib import Path

from fno.config_cli import get_cmd


def _patch_roots(monkeypatch, worktree: Path, canonical: Path, tmp_path: Path):
    worktree.mkdir(parents=True, exist_ok=True)
    (worktree / ".git").touch()
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.toml"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_NO_CANONICAL_CONFIG", raising=False)
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: worktree)
    monkeypatch.setattr(
        "fno.paths.resolve_canonical_worktree",
        lambda root=None, timeout=None: canonical,
    )
    monkeypatch.setattr("fno.paths.resolve_canonical_repo_root", lambda: canonical)


def test_config_get_receipt_names_root_and_deciding_file(monkeypatch, tmp_path: Path, capsys):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    (canonical / ".fno").mkdir(parents=True)
    deciding_file = canonical / ".fno" / "config.toml"
    deciding_file.write_text("[review]\nmax_rounds = 5\n", encoding="utf-8")
    _patch_roots(monkeypatch, worktree, canonical, tmp_path)

    get_cmd("review.max_rounds", False)

    captured = capsys.readouterr()
    assert captured.out == "5\n"
    assert f"source: {deciding_file}" in captured.err
    assert f"root: {worktree}" in captured.err
    assert f"searched: {worktree / '.fno' / 'config.toml'}" in captured.err
    assert f"{canonical / '.fno' / 'config.toml'}" in captured.err


def test_config_get_receipt_follows_a_pinned_fno_config(monkeypatch, tmp_path: Path, capsys):
    pinned = tmp_path / "pinned" / "config.toml"
    pinned.parent.mkdir(parents=True)
    pinned.write_text("[review]\nmax_rounds = 7\n", encoding="utf-8")
    _patch_roots(monkeypatch, tmp_path / "worktree", tmp_path / "canonical", tmp_path)
    monkeypatch.setenv("FNO_CONFIG", str(pinned))

    get_cmd("review.max_rounds", False)

    captured = capsys.readouterr()
    assert captured.out == "7\n"
    assert f"source: {pinned}" in captured.err
    assert f"root: {pinned.parent}" in captured.err
    assert f"searched: {pinned}" in captured.err
