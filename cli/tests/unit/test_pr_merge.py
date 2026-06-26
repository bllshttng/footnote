"""Characterization tests for the _merge.py port (ab-d4c98550, US1/AC1/AC5).

Mocks gh/git at the _proc.run seam so the guard-rejection, classification,
and worktree-recovery branches are exercised deterministically (they are hard
to reproduce against a live PR). Pins the JSON-line schema, the exit codes,
and the stdout-vs-stderr routing the bash used.
"""
from __future__ import annotations

import json

import pytest

from fno.config import AutoMergeBlock
from fno.pr import _merge
from fno.pr._proc import Result


class FakeRun:
    """Dispatch canned Results by command, recording every call."""

    def __init__(
        self,
        *,
        gh_merge: Result | None = None,
        merged_at: str = "null",
        view_url: str = "https://example/pr",
        api_ok: bool = False,
        toplevel: str | None = None,
    ) -> None:
        self.gh_merge = gh_merge or Result(0, "", "")
        self.merged_at = merged_at
        self.view_url = view_url
        self.api_ok = api_ok
        self.toplevel = toplevel
        self.calls: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        cmd = list(cmd)
        self.calls.append(cmd)
        tool = cmd[0]
        if tool == "git":
            if cmd[1:3] == ["rev-parse", "--show-toplevel"]:
                return Result(0, (self.toplevel or cwd or "") + "\n", "")
            return Result(0, "", "")
        if tool == "gh":
            if cmd[1:3] == ["pr", "merge"]:
                return self.gh_merge
            if cmd[1:3] == ["pr", "view"]:
                if "mergedAt" in cmd:
                    return Result(0, self.merged_at + "\n", "")
                return Result(0, self.view_url + "\n", "")
            if cmd[1] == "api":
                return Result(0, "", "") if self.api_ok else Result(1, "", "api failed")
        if tool == "bash":
            return Result(0, "", "")
        return Result(0, "", "")


@pytest.fixture
def enabled(monkeypatch):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=True))
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: "/usr/bin/gh")


def _last_json(capsys, *, stream="out") -> dict:
    cap = capsys.readouterr()
    text = cap.out if stream == "out" else cap.err
    return json.loads(text.strip().splitlines()[-1])


# ---- arg validation ----


def test_unknown_arg_exits_1(capsys):
    assert _merge.run_merge(["--bogus"]) == 1


def test_missing_invoker_exits_1(capsys):
    assert _merge.run_merge(["42"]) == 1


def test_missing_pr_exits_1(capsys):
    assert _merge.run_merge(["--invoker=target"]) == 1


def test_invalid_invoker_exits_1(capsys):
    assert _merge.run_merge(["--invoker=evil", "42"]) == 1


def test_invalid_pr_number_exits_1_with_failed_json_on_stderr(capsys):
    assert _merge.run_merge(["--invoker=target", "0"]) == 1
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert "invalid pr number" in obj["reason"]


# ---- config + gh gates ----


def test_auto_merge_disabled_skips_exit_2(monkeypatch, capsys):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=False))
    assert _merge.run_merge(["--invoker=target", "42"]) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "skipped"
    assert obj["pr"] == 42


def test_invoker_not_allowed_skips_exit_2(monkeypatch, capsys):
    monkeypatch.setattr(
        _merge,
        "_load_auto_merge",
        lambda: AutoMergeBlock(enabled=True, allowed_invokers=["megawalk"]),
    )
    assert _merge.run_merge(["--invoker=target", "42"]) == 2
    assert _last_json(capsys)["outcome"] == "skipped"


def test_gh_missing_exits_127(monkeypatch, capsys):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=True))
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: None)
    assert _merge.run_merge(["--invoker=target", "42"]) == 127
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert obj["reason"] == "gh CLI not installed"


# ---- classification ----


def test_merge_immediate_exit_0(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj["strategy"] == "merge"
    assert obj["invoker"] == "target"


def test_merge_queued_exit_0(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(0, "Pull request #42 will be automatically merged", ""),
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "queued"


def test_merge_failed_protected_exit_1(enabled, monkeypatch, capsys, tmp_path):
    fake = FakeRun(gh_merge=Result(1, "", "branch is protected"))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 1
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert obj["reason"] == "branch protected"


def test_worktree_recovery_already_merged_serverside(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(1, "", "fatal: 'main' is already used by worktree at /x"),
        merged_at="2026-06-13T00:00:00Z",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert "server-side" in obj["reason"]


def test_worktree_recovery_api_fallback(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(1, "", "is already used by worktree"),
        merged_at="null",
        api_ok=True,
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert "worktree fallback" in obj["reason"]
    # The API path uses a literal that does NOT contain "gh pr merge".
    api_calls = [c for c in fake.calls if c[:2] == ["gh", "api"]]
    assert api_calls and "PUT" in api_calls[0]


# ---- post-merge followups ----


def test_post_merge_sentinels_written(enabled, monkeypatch, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path))
    mem = tmp_path / ".fno" / ".memory-pass-pending"
    triage = tmp_path / ".fno" / ".triage-pending"
    assert mem.read_text().strip() == "42"
    sentinel = json.loads(triage.read_text())
    assert sentinel["pr_number"] == 42
    assert sentinel["mode"] == "interactive"
    assert sentinel["pr_url"] == "https://example/pr"


def test_post_merge_mode_autonomous_with_megawalk_state(enabled, monkeypatch, tmp_path):
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir()
    (fno_dir / "megawalk-state.md").write_text("x\n")
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["--invoker=megawalk", "7"], cwd=str(tmp_path))
    sentinel = json.loads((fno_dir / ".triage-pending").read_text())
    assert sentinel["mode"] == "autonomous"


def test_session_satisfied_emitted_when_state_present(enabled, monkeypatch, tmp_path):
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir()
    (fno_dir / "target-state.md").write_text(
        '---\nsession_id: "20260613T000000Z-1-abc"\n---\n'
    )
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path))
    events = fno_dir / "events.jsonl"
    assert events.exists()
    line = json.loads(events.read_text().strip().splitlines()[-1])
    assert line["type"] == "session_satisfied"
    assert line["data"]["source"] == "pr_merge"
    assert line["data"]["session_id"] == "20260613T000000Z-1-abc"


# ---- stub-manifest draft-held guard (G3, x-24b7) ----


def _held(*_a, **_k):
    return {"_node": "x-9", "stubs": [{"stub_id": "a"}]}


def test_unreconciled_stub_manifest_holds_merge_exit_2(enabled, monkeypatch, capsys, tmp_path):
    # AC3-ERR / AC7-EDGE: auto_merge ENABLED, but a contract dependent's
    # unreconciled manifest still refuses the merge, and the merge subcommand is
    # never invoked (no mocks ship).
    import fno.stub_manifest as sm
    monkeypatch.setattr(sm, "unreconciled_manifest_for_pr", _held)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "held"
    assert "x-9" in obj["reason"]
    assert not any(c[1:3] == ["pr", "merge"] for c in fake.calls)


def test_hard_node_merges_unaffected_by_guard(enabled, monkeypatch, capsys, tmp_path):
    # AC6-EDGE: guard returns None for a non-contract PR -> normal merge.
    (tmp_path / ".fno").mkdir()
    import fno.stub_manifest as sm
    monkeypatch.setattr(sm, "unreconciled_manifest_for_pr", lambda *a, **k: None)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_guard_own_failure_does_not_block_merge(enabled, monkeypatch, capsys, tmp_path):
    # The guard is best-effort: if its own lookup raises, a normal merge proceeds.
    (tmp_path / ".fno").mkdir()
    import fno.stub_manifest as sm

    def _boom(*_a, **_k):
        raise RuntimeError("graph wedged")

    monkeypatch.setattr(sm, "unreconciled_manifest_for_pr", _boom)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["--invoker=target", "42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"
