"""The removal legs must not believe an asserted merge (x-5a30 task 1.3, Jam D).

Two triggers reach the ritual and only one verifies the merge.

  pr-watch daemon   fires on a gh-backed `state == MERGED`     external truth
  `fno pr merged N` reaches `_resolve_pr`, which returns the    an ARGUMENT
                    caller's number unchecked (`if pr: return int(pr)`)

`leg_archive` used to read only `headRefName` from its `gh pr view` call. On an
OPEN PR it resolved the branch, found the worktree, and ran archive-worktree.sh
-- whose own checks (clean tree, pushed, no live session) describe worktree
CONTENT. A worker who finished, pushed, and now waits on review passes all
three, and its worktree is the one holding the raw attestation its PR needs.

The guard is one word: `state` in the existing `--json` list. These tests pin
that it is read, that it refuses, and that it fails closed.
"""
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.pr import _ritual


class _Recorder:
    """A runner seam that answers gh and records what was asked."""

    def __init__(self, state: str = "MERGED", *, gh_ok: bool = True,
                 bad_json: bool = False) -> None:
        self.state = state
        self.gh_ok = gh_ok
        self.bad_json = bad_json
        self.calls: list[list[str]] = []

    def __call__(self, argv, cwd=None, timeout=None):  # noqa: ANN001
        self.calls.append(list(argv))
        if argv[:3] == ["gh", "pr", "view"]:
            if not self.gh_ok:
                return SimpleNamespace(ok=False, returncode=1, stdout="", stderr="boom")
            body = "{not json" if self.bad_json else json.dumps(
                {"state": self.state, "headRefName": "feature/x-dead"}
            )
            return SimpleNamespace(ok=True, returncode=0, stdout=body, stderr="")
        return SimpleNamespace(ok=True, returncode=0, stdout="", stderr="")


def _ritual_for(runner: _Recorder, tmp_path: Path, pr: int = 999) -> _ritual.Ritual:
    # `canon` is a read-only property over ctx.canon, so it is set there.
    r = _ritual.Ritual.__new__(_ritual.Ritual)
    r.runner = runner
    r.cwd = tmp_path
    r.pr_supplied = True
    r.ctx = SimpleNamespace(
        pr=pr, autonomous=False, canon=tmp_path, settings=None,
        pm=SimpleNamespace(self_reap=True), project="fno", lane_project="fno",
        parking_lot=None, holder="test", node_ids=["x-dead"],
    )
    r.receipts = []
    return r


def _emits(r: _ritual.Ritual) -> list[tuple[str, str, str]]:
    out: list[tuple[str, str, str]] = []
    r._emit = lambda step, status, detail="": out.append((step, status, detail))  # type: ignore[method-assign]
    return out


# -- AC3-ERR: an open PR never reaches a removal -----------------------------


@pytest.mark.parametrize("state", ["OPEN", "CLOSED", "DRAFT"])
def test_archive_refuses_when_not_merged(tmp_path: Path, state: str) -> None:
    runner = _Recorder(state=state)
    r = _ritual_for(runner, tmp_path)
    seen = _emits(r)

    r.leg_archive()

    assert seen == [("archive", "skipped", f"not-merged (state={state})")]
    assert not any("archive-worktree.sh" in " ".join(c) for c in runner.calls)


def test_reap_rows_refuses_when_not_merged(tmp_path: Path) -> None:
    runner = _Recorder(state="OPEN")
    r = _ritual_for(runner, tmp_path)
    seen = _emits(r)

    r.leg_reap_rows()

    assert seen == [("reap-rows", "skipped", "not-merged (state=OPEN)")]
    assert not any(c[:2] == ["agents", "stop"] for c in runner.calls)


def test_the_gh_call_actually_asks_for_state(tmp_path: Path) -> None:
    """Pin the one word. Dropping it restores the whole exposure silently."""
    runner = _Recorder(state="MERGED")
    r = _ritual_for(runner, tmp_path)
    _emits(r)

    r.leg_archive()

    view = next(c for c in runner.calls if c[:3] == ["gh", "pr", "view"])
    assert "state" in view[view.index("--json") + 1]


def test_state_costs_no_extra_api_call(tmp_path: Path) -> None:
    """`state` rides the call that already ran; it must not add a second."""
    runner = _Recorder(state="MERGED")
    r = _ritual_for(runner, tmp_path)
    _emits(r)

    r.leg_archive()

    views = [c for c in runner.calls if c[:3] == ["gh", "pr", "view"]]
    assert len(views) == 1


# -- Fails closed ------------------------------------------------------------


def test_unreadable_state_refuses(tmp_path: Path) -> None:
    """"I could not check" must not spend the same as "I checked"."""
    runner = _Recorder(gh_ok=False)
    r = _ritual_for(runner, tmp_path)
    seen = _emits(r)

    r.leg_archive()

    assert seen[0][1] == "skipped"
    assert "gh-unavailable" in seen[0][2]


def test_malformed_json_refuses(tmp_path: Path) -> None:
    runner = _Recorder(bad_json=True)
    r = _ritual_for(runner, tmp_path)
    seen = _emits(r)

    r.leg_reap_rows()

    assert seen[0][1] == "skipped"
    assert "unreadable" in seen[0][2]


# -- AC3-EDGE: inside its own worktree, the work is DEFERRED, not skipped ----


def test_inside_own_worktree_defers_rather_than_advising(tmp_path: Path) -> None:
    """The dominant path: the worker that merged is standing in its worktree.

    The old emit was `skipped` with a command in the detail line. It read as
    "nothing to do" and the command was never run, which is why the freshest
    merged worktrees were the ones that survived.
    """
    runner = _Recorder(state="MERGED")
    r = _ritual_for(runner, tmp_path)
    r._find_worktree = lambda branch: str(tmp_path)  # type: ignore[method-assign]
    seen = _emits(r)

    r.leg_archive()

    assert seen == [("archive", "deferred", "sweep-will-reap")]


def test_deferred_is_a_distinct_status_from_skipped() -> None:
    assert _ritual._DEFERRED == "deferred"
    assert _ritual._DEFERRED != _ritual._SKIPPED


# -- AC3-ORDER: the resolve receipt names the PR and where it came from ------
#
# These go through the REAL constructor. Every test above builds the object with
# __new__ to keep the leg seams cheap, and `AGENTS.md` names that exact move as
# a specimen: `_bare()` bypassing `__init__` shipped a guard the real path never
# ran. So at least one test must prove the real construction produces the state
# the guards read.


def test_real_constructor_marks_a_caller_supplied_pr(tmp_path: Path) -> None:
    runner = _Recorder(state="MERGED")

    r = _ritual.Ritual(pr=834, autonomous=False, cwd=tmp_path, runner=runner)

    assert r.pr_supplied is True
    assert r.ctx.pr == 834
    # A supplied number short-circuits _resolve_pr, so no gh list runs. That is
    # precisely why the merge is an argument on this path.
    assert not any(c[:3] == ["gh", "pr", "list"] for c in runner.calls)


def test_real_constructor_marks_an_inferred_pr(tmp_path: Path) -> None:
    class _ListRunner(_Recorder):
        def __call__(self, argv, cwd=None, timeout=None):  # noqa: ANN001
            self.calls.append(list(argv))
            if argv[:3] == ["gh", "pr", "list"]:
                body = json.dumps([{"number": 835, "mergedAt": "2026-08-13T00:00:00Z"}])
                return SimpleNamespace(ok=True, returncode=0, stdout=body, stderr="")
            return SimpleNamespace(ok=True, returncode=0, stdout="", stderr="")

    runner = _ListRunner()

    r = _ritual.Ritual(pr=None, autonomous=False, cwd=tmp_path, runner=runner)

    assert r.pr_supplied is False
    assert r.ctx.pr == 835
