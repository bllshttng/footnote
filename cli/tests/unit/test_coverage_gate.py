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


def test_attestation_chain_reads_a_row_that_rotated_out(monkeypatch, tmp_path: Path):
    """The Python half of the Rust rotated-fixture test: a review round that
    left the live journal still counts, read from the store."""
    import json
    import subprocess

    import pytest

    from tests.conftest import checkout_fno_agents_binary

    binary = checkout_fno_agents_binary()
    if binary is None:
        pytest.skip("fno-agents binary not built (cargo build -p fno-agents); set FNO_AGENTS_BIN")

    # A '#' in the path would end a hand-built file: URI early.
    live = tmp_path / "space#1" / "events.jsonl"
    live.parent.mkdir()
    row = {
        "ts": "2026-09-15T08:26:07Z",
        "type": "review_attestation",
        "source": "test",
        "data": {"reviewer": "code-review", "branch": "feature/x", "head_sha": "abc1234", "verdict": "pass"},
    }
    live.write_text(json.dumps(row) + "\n", encoding="utf-8")
    # Rotation's own order: the Rust ingest into the store, then the rename.
    subprocess.run(
        [str(binary), "king-history", "--scope", "x-aaaa", "--events-path", str(live), "--json"],
        check=True,
        capture_output=True,
    )
    live.rename(live.parent / "events.jsonl.1")
    live.write_text("", encoding="utf-8")
    assert "abc1234" not in live.read_text(encoding="utf-8")
    assert "abc1234" in (live.parent / "events.jsonl.1").read_text(encoding="utf-8")

    from fno.pr import _coverage_gate, _reviews

    monkeypatch.setattr(_reviews, "_coverage_logs", lambda cwd, project_events: (live, None, None))
    chain = _coverage_gate.attestation_chain(None, "feature/x", "abc1234")
    assert [e["head_sha"] for e in chain] == ["abc1234"]


def test_journal_lines_without_a_store_is_the_live_file(tmp_path: Path):
    from fno.pr._reviews import journal_lines

    live = tmp_path / "events.jsonl"
    live.write_text('{"type":"review_coverage"}\nnot json\n', encoding="utf-8")
    assert list(journal_lines(live, ("review_coverage",))) == ['{"type":"review_coverage"}\n', "not json\n"]
