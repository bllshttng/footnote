from __future__ import annotations

import logging
from pathlib import Path

from fno import config
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
    config.load_settings.cache_clear()


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


def test_config_get_default_receipt_names_searched_roots(
    monkeypatch, tmp_path: Path, capsys
):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    _patch_roots(monkeypatch, worktree, canonical, tmp_path)

    get_cmd("review.max_rounds", False)

    captured = capsys.readouterr()
    assert captured.out == "2\n"
    assert "source: default (no config file sets this key)" in captured.err
    assert f"root: {worktree}" in captured.err
    assert f"{canonical / '.fno' / 'config.toml'}" in captured.err


def test_describe_settings_for_repo_returns_the_seeded_candidate_chain(
    monkeypatch, tmp_path: Path
):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    _patch_roots(monkeypatch, worktree, canonical, tmp_path)

    candidates = config.describe_settings_for_repo(worktree)

    assert candidates[:4] == [
        worktree / ".fno" / "config.toml",
        worktree / ".fno" / "settings.yaml",
        canonical / ".fno" / "config.toml",
        canonical / ".fno" / "settings.yaml",
    ]


def test_coverage_gate_consumers_emit_verbose_root_receipts(
    monkeypatch, tmp_path: Path, caplog
):
    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    (canonical / ".fno").mkdir(parents=True)
    (canonical / ".fno" / "config.toml").write_text(
        "[review]\nmax_rounds = 5\nrequire_corroboration = false\n",
        encoding="utf-8",
    )
    _patch_roots(monkeypatch, worktree, canonical, tmp_path)

    from fno.pr import _coverage_gate

    monkeypatch.setattr(_coverage_gate, "_repo_root", lambda cwd: worktree)
    caplog.set_level(logging.DEBUG, logger="fno.pr._coverage_gate")

    _coverage_gate.resolved_max_rounds(str(worktree))
    _coverage_gate._resolved_categories(str(worktree))

    messages = [record.getMessage() for record in caplog.records]
    for key in (
        "config.review.max_rounds",
        "config.review.nonblocking_categories",
    ):
        assert any(f"key={key}" in message for message in messages)
    assert all(f"root={worktree}" in message for message in messages)


def test_config_get_receipt_follows_a_pinned_fno_config(monkeypatch, tmp_path: Path, capsys):
    pinned = tmp_path / "pinned" / "config.toml"
    pinned.parent.mkdir(parents=True)
    pinned.write_text("[review]\nmax_rounds = 7\n", encoding="utf-8")
    _patch_roots(monkeypatch, tmp_path / "worktree", tmp_path / "canonical", tmp_path)
    monkeypatch.setenv("FNO_CONFIG", str(pinned))
    config.load_settings.cache_clear()

    get_cmd("review.max_rounds", False)

    captured = capsys.readouterr()
    assert captured.out == "7\n"
    assert f"source: {pinned}" in captured.err
    assert f"root: {pinned.parent}" in captured.err
    assert f"searched: {pinned}" in captured.err
