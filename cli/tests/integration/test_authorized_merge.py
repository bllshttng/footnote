"""Integration: the merge verb and its owner, across the process seam.

The decision itself is under test in ``crates/fno-agents/src/authorized_merge.rs``
against injected probes. What this file proves is the seam those unit tests
cannot see: that ``fno do pr merge`` reaches the REAL compiled owner, that the
receipt it reads back is rendered onto this verb's surface intact, and that a
queue-armed merge GitHub lands later still gets its remote ref cleaned up.

Only ``gh`` is faked. The payload, the subprocess, the JSON parse and the
outcome mapping are all real.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.pr import _merge
from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


@requires_rust
def test_the_owner_answers_over_the_real_process_seam(tmp_path, monkeypatch):
    """A live round trip: payload in, receipt out, no faking of the door.

    The `cwd` this payload names has no PR and no gh auth, so the owner's first
    guarded fetch cannot answer. That is the point: the receipt must come back
    as a parsed `unknown`, never as a crash and never as a clear merge.
    """
    # The autouse hermetic fixture stubs the door; this test wants the door.
    monkeypatch.setattr(_merge, "_authorized_merge", _REAL)
    receipt = _merge._authorized_merge(
        424242,
        str(tmp_path),
        effect="merge",
        approved=None,
        source="",
    )
    assert receipt["outcome"] in ("unknown", "refused", "held"), receipt
    assert receipt["detail"], "every receipt names why"


_REAL = _merge._authorized_merge


@requires_rust
def test_a_bad_payload_is_refused_rather_than_answered(tmp_path):
    """The verb refuses an unusable payload instead of inventing a verdict."""
    import subprocess

    binary = find_dev_binary()
    proc = subprocess.run(
        [str(binary), "authorized-merge"],
        input=json.dumps({"cwd": str(tmp_path)}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert proc.returncode == 2
    assert "effect merge|arm" in proc.stderr


def test_an_unreachable_owner_never_reads_as_a_merge(tmp_path, monkeypatch):
    """The fail-closed direction, without needing a binary at all.

    A merge whose authorization could not be read has not been authorized, and
    `unknown` maps onto `held` - retry - never onto a silent success.
    """
    from fno.rust_binary import VerbUnavailable

    def _gone(verb, payload, unavailable=VerbUnavailable):
        raise unavailable("the fno-agents binary was not found")

    monkeypatch.setattr("fno.rust_binary.verb_call", _gone)
    monkeypatch.setattr(_merge, "_authorized_merge", _REAL)
    receipt = _merge._authorized_merge(
        7, str(tmp_path), effect="arm", approved=True, source="config"
    )
    assert receipt["outcome"] == "unknown"
    word, code, _err = _merge._OUTCOME_EMIT[receipt["outcome"]]
    assert (word, code) == ("held", 2)


# ---------------------------------------------------------------------------
# x-033e: the queue lands the merge later, and the remote ref must still go.
# ---------------------------------------------------------------------------


class _FakeGh:
    """Records gh calls; answers the two reads the cleanup owner makes."""

    def __init__(self, *, head_repo="owner/repo", base_repo="owner/repo", branch="feature/x"):
        self.calls: list[list[str]] = []
        self._fields = f"{branch}\t{head_repo}\t{base_repo}"

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        from fno.pr._proc import Result

        cmd = list(cmd)
        self.calls.append(cmd)
        if cmd[:2] == ["gh", "api"] and "--jq" in cmd:
            return Result(0, self._fields, "")
        if cmd[:3] == ["gh", "api", "-X"]:
            return Result(0, "", "")
        raise AssertionError(f"unexpected command: {cmd}")

    @property
    def ref_deletes(self) -> list[list[str]]:
        return [c for c in self.calls if "DELETE" in c]


def _auto_merge(delete_branch: bool):
    from fno.config import AutoMergeBlock

    return AutoMergeBlock(enabled=True, delete_branch_on_merge=delete_branch)


def test_a_queue_merged_pr_gets_its_remote_ref_deleted_once(tmp_path, monkeypatch):
    """The gap x-033e names: when finalize arms GitHub's queue and the queue
    lands the merge later, no fno process runs the post-merge step, so the
    remote ref stayed behind forever. The watcher tick is the detector that
    sees the confirmed merge, so it pays that step through the merge verb's own
    cleanup owner - one implementation, not a second one for this path.
    """
    from fno.pr_watch._dispatch import _finish_queue_merge

    fake = _FakeGh()
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        "fno.config.load_settings_for_repo",
        lambda _p: type("S", (), {"auto_merge": _auto_merge(True)})(),
    )
    emitted: list[tuple] = []
    _finish_queue_merge(Path(tmp_path), 1042, lambda kind, data: emitted.append((kind, data)))

    assert len(fake.ref_deletes) == 1, fake.calls
    assert fake.ref_deletes[0][-1] == "repos/owner/repo/git/refs/heads/feature/x"
    assert emitted == [], "a clean cleanup emits no failure event"


def test_the_cleanup_respects_the_delete_branch_switch(tmp_path, monkeypatch):
    """A repo that keeps its merged branches keeps them on this path too."""
    from fno.pr_watch._dispatch import _finish_queue_merge

    fake = _FakeGh()
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        "fno.config.load_settings_for_repo",
        lambda _p: type("S", (), {"auto_merge": _auto_merge(False)})(),
    )
    _finish_queue_merge(Path(tmp_path), 1042, lambda kind, data: None)
    assert fake.calls == [], "nothing is read or deleted when the switch is off"


def test_a_fork_head_is_never_deleted_from_the_base_repo(tmp_path, monkeypatch):
    """A live fork owns its head branch: deleting a same-named base branch
    would be data loss, so a head repo that is not the base repo is a
    no-delete terminal, never a cleanup failure."""
    from fno.pr_watch._dispatch import _finish_queue_merge

    fake = _FakeGh(head_repo="contributor/repo", base_repo="owner/repo")
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        "fno.config.load_settings_for_repo",
        lambda _p: type("S", (), {"auto_merge": _auto_merge(True)})(),
    )
    emitted: list[tuple] = []
    _finish_queue_merge(Path(tmp_path), 1042, lambda kind, data: emitted.append((kind, data)))
    assert fake.ref_deletes == []
    assert emitted == []


def test_a_cleanup_failure_is_reported_and_never_raises(tmp_path, monkeypatch):
    """Cleanup is warn-only: it follows a merge that already happened, so it
    must never take the tick down with it."""
    from fno.pr_watch._dispatch import _finish_queue_merge

    def _boom(cmd, **kwargs):
        raise RuntimeError("gh vanished")

    monkeypatch.setattr(_merge, "run", _boom)
    monkeypatch.setattr(
        "fno.config.load_settings_for_repo",
        lambda _p: type("S", (), {"auto_merge": _auto_merge(True)})(),
    )
    emitted: list[tuple] = []
    _finish_queue_merge(Path(tmp_path), 1042, lambda kind, data: emitted.append((kind, data)))
    assert emitted == [], "an exception is logged, not emitted as a cleanup verdict"
