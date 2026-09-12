"""A merge mints a cleanup request when, and only when, `fno do pr merge` completes `_run_post_merge_followups` and its own `gh pr view` read confirms `state == MERGED` with a non-empty `headRefName`. Every other outcome now emits `merge_cleanup_skipped` with the reason. A merge that never ran the verb emits neither, and is out of scope here.

The invariant these cases pin: `_emit_merge_cleanup_request` writes exactly
one journal row on every call, a request or a skip. Silence is not legal.
"""

import json

import pytest


def _patch_events_log(monkeypatch, tmp_path):
    import fno.agents.events as E

    log = tmp_path / "agents-events.jsonl"
    monkeypatch.setattr(E, "daemon_lifecycle_log", lambda: log)
    return log


def _write_manifest(tmp_path):
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(exist_ok=True)
    manifest = state_dir / "target-state.md"
    manifest.write_text("---\nsession_id: sess-skip\nharness: claude\n---\n")
    return manifest


def _stub_gh(monkeypatch, module, *, ok=True, stdout="", stderr=""):
    class _R:
        pass

    def _gh(args, cwd):
        r = _R()
        r.ok = ok
        r.stdout = stdout
        r.stderr = stderr
        return r

    monkeypatch.setattr(module, "_gh", _gh)


def _stub_git_root(monkeypatch, module, root):
    class _R:
        ok = True
        stderr = ""
        stdout = str(root)

    monkeypatch.setattr(module, "_git", lambda args, cwd: _R())


def _rows(log, kind):
    if not log.exists():
        return []
    return [
        json.loads(line)
        for line in log.read_text().splitlines()
        if json.loads(line).get("type") == kind
    ]


MERGED = json.dumps({"state": "MERGED", "headRefName": "feature/x", "url": ""})

# (case name, gh ok, gh stdout, gh stderr, expected reason, expected detail)
SKIP_CASES = [
    ("gh read failed", False, "", "gh: not found", "gh-unavailable", "gh: not found"),
    ("unparseable json", True, "{not json", "", "unparseable-pr-json", "JSONDecodeError"),
    (
        "still open",
        True,
        json.dumps({"state": "OPEN", "headRefName": "feature/x"}),
        "",
        "not-merged",
        "state=OPEN",
    ),
    (
        "merged with no head ref",
        True,
        json.dumps({"state": "MERGED", "headRefName": ""}),
        "",
        "no-branch",
        "",
    ),
]


@pytest.mark.parametrize(
    "name,ok,stdout,stderr,reason,detail",
    SKIP_CASES,
    ids=[c[0] for c in SKIP_CASES],
)
def test_every_unmet_precondition_speaks_its_reason(
    tmp_path, monkeypatch, name, ok, stdout, stderr, reason, detail
):
    import fno.pr._merge as M

    log = _patch_events_log(monkeypatch, tmp_path)
    _stub_gh(monkeypatch, M, ok=ok, stdout=stdout, stderr=stderr)
    _stub_git_root(monkeypatch, M, tmp_path)
    M._REPO_ROOT_CACHE[str(tmp_path)] = str(tmp_path)
    manifest = _write_manifest(tmp_path)

    M._emit_merge_cleanup_request(11, str(tmp_path), str(manifest), [])

    skipped = _rows(log, "merge_cleanup_skipped")
    assert len(skipped) == 1
    assert _rows(log, "merge_cleanup_requested") == []
    data = skipped[0]["data"]
    assert data["reason"] == reason
    assert data["detail"] == detail
    assert data["pr"] == 11
    assert data["session_id"] == "sess-skip"
    assert data["harness"] == "claude"


def test_a_confirmed_merge_still_mints_its_request(tmp_path, monkeypatch):
    # The positive control for the absence assertions above: the same
    # instrument, the same journal, one request row and no skip row.
    import fno.agents.events as E
    import fno.pr._merge as M
    import fno.worktree_reapable as WR

    log = _patch_events_log(monkeypatch, tmp_path)
    _stub_gh(monkeypatch, M, ok=True, stdout=MERGED)
    _stub_git_root(monkeypatch, M, tmp_path)
    M._REPO_ROOT_CACHE[str(tmp_path)] = str(tmp_path)
    manifest = _write_manifest(tmp_path)
    monkeypatch.setattr(WR, "is_linked_worktree", lambda p: False)
    monkeypatch.setattr(
        E, "rows_for_cleanup", lambda worktree, node_ids, runner=None: []
    )

    M._emit_merge_cleanup_request(11, str(tmp_path), str(manifest), [])

    requested = _rows(log, "merge_cleanup_requested")
    assert len(requested) == 1
    assert requested[0]["data"]["branch"] == "feature/x"
    assert _rows(log, "merge_cleanup_skipped") == []


def test_a_raising_mint_speaks_emit_failed_and_the_merge_stands(tmp_path, monkeypatch):
    import fno.pr._merge as M

    log = _patch_events_log(monkeypatch, tmp_path)
    _stub_git_root(monkeypatch, M, tmp_path)
    M._REPO_ROOT_CACHE[str(tmp_path)] = str(tmp_path)
    _write_manifest(tmp_path)

    def _boom(pr_number, cwd, state_file, bound_node_ids):
        raise RuntimeError("journal write refused")

    monkeypatch.setattr(M, "_emit_merge_cleanup_request", _boom)

    M._run_post_merge_followups(11, "squash", str(tmp_path), bound_node_ids=[])

    skipped = _rows(log, "merge_cleanup_skipped")
    assert len(skipped) == 1
    assert skipped[0]["data"]["reason"] == "emit-failed"
    assert skipped[0]["data"]["detail"] == "RuntimeError"
    assert _rows(log, "merge_cleanup_requested") == []


def test_an_unknown_reason_is_refused_rather_than_written(tmp_path, monkeypatch):
    from fno.agents.events import emit_merge_cleanup_skipped

    log = _patch_events_log(monkeypatch, tmp_path)

    with pytest.raises(ValueError):
        emit_merge_cleanup_skipped(repo="r", project="p", pr=11, reason="because")

    assert _rows(log, "merge_cleanup_skipped") == []
