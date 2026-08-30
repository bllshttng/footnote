from __future__ import annotations

from pathlib import Path


def test_resolved_max_rounds_uses_each_worktree_root(monkeypatch, tmp_path: Path):
    first = tmp_path / "first"
    second = tmp_path / "second"
    for root, rounds in ((first, 5), (second, 7)):
        config_dir = root / ".fno"
        config_dir.mkdir(parents=True)
        (config_dir / "config.toml").write_text(
            f"[review]\nmax_rounds = {rounds}\n", encoding="utf-8"
        )

    from fno.pr import _coverage_gate

    monkeypatch.setattr(_coverage_gate, "_repo_root", lambda cwd: Path(cwd))
    monkeypatch.setattr(
        "fno.paths.resolve_canonical_worktree",
        lambda root=None, timeout=None: Path(root) if root else None,
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.toml"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)

    assert _coverage_gate.resolved_max_rounds(str(first)) == 5
    assert _coverage_gate.resolved_max_rounds(str(second)) == 7


def test_repo_state_root_cache_is_keyed_by_cwd(monkeypatch):
    from fno.pr import _merge
    from fno.pr._proc import Result

    _merge._REPO_ROOT_CACHE.clear()
    calls: list[str] = []

    def fake_git(args, cwd):
        calls.append(cwd)
        return Result(0, f"{cwd}/repo\n", "")

    monkeypatch.setattr(_merge, "_git", fake_git)

    assert _merge._repo_state_dir("/one") == "/one/repo/.fno"
    assert _merge._repo_state_dir("/two") == "/two/repo/.fno"
    assert _merge._repo_state_dir("/one") == "/one/repo/.fno"
    assert calls == ["/one", "/two"]
