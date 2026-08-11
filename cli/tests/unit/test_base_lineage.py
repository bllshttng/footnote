"""The stacked-base guard: refusing a merge into a base that no longer leads to main.

Mocks gh/git at the ``_proc.run`` seam, like ``test_pr_merge.py``, so the two
probes and their three-way verdict are exercised without a live PR.

The load-bearing cases are the blindness pair and the reuse case. (i) and (ii)
exist to cover each other: under ``merge_strategy = "squash"`` a landed base is
not an ancestor of main, so (ii) sees nothing; a base that landed leaving no
merged PR blinds (i). A suite that only ever satisfies both at once would pass
with either check deleted.
"""
from __future__ import annotations

import pytest

from fno.pr import _base_lineage
from fno.pr._proc import Result, ToolMissing

LANDED = "85b90e485aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
MOVED = "ca9c86308bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"


class FakeRun:
    """Canned gh/git answers for the lineage probes."""

    def __init__(
        self,
        *,
        default: str = "main",
        base: str = "feature/bg-crown",
        merged_pr: str = "",
        merged_head: str = LANDED,
        base_tip: str = LANDED,
        contained: bool = False,
        default_fails: bool = False,
        base_fails: bool = False,
        list_fails: bool = False,
        fetch_fails: bool = False,
        base_fetch_fails: bool = False,
        base_still_on_remote: bool = False,
        git_missing: bool = False,
    ) -> None:
        self.default = default
        self.base = base
        self.merged_pr = merged_pr
        self.merged_head = merged_head
        self.base_tip = base_tip
        self.contained = contained
        self.default_fails = default_fails
        self.base_fails = base_fails
        self.list_fails = list_fails
        self.fetch_fails = fetch_fails
        self.base_fetch_fails = base_fetch_fails
        self.base_still_on_remote = base_still_on_remote
        self.git_missing = git_missing
        self.calls: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        cmd = list(cmd)
        self.calls.append(cmd)
        # Every probe must be bounded: `_merge.py` calls this inside the global
        # merge lock, so an unbounded git fetch would wedge every other lane.
        assert timeout, f"probe ran without a timeout: {cmd}"
        if cmd[0] == "git" and self.git_missing:
            raise ToolMissing("git")
        if cmd[0] == "gh":
            if cmd[1:3] == ["repo", "view"]:
                if self.default_fails:
                    return Result(1, "", "gh: api error")
                return Result(0, self.default + "\n", "")
            if cmd[1:3] == ["pr", "view"]:
                if self.base_fails:
                    return Result(1, "", "gh: api error")
                return Result(0, self.base + "\n", "")
            if cmd[1:3] == ["pr", "list"]:
                if self.list_fails:
                    return Result(1, "", "gh: api error")
                if not self.merged_pr:
                    return Result(0, "null null\n", "")
                return Result(0, f"{self.merged_pr} {self.merged_head}\n", "")
        if cmd[0] == "git":
            if cmd[1] == "fetch":
                gone = self.base_fetch_fails and f"refs/heads/{self.base}:" in cmd[-1]
                return Result(1 if (self.fetch_fails or gone) else 0, "", "")
            if cmd[1] == "ls-remote":
                # `fetch_fails` models git/network being down wholesale, so the
                # remote query fails too. Answering "empty, therefore deleted"
                # there would let a dead network forge a deletion; only
                # `base_fetch_fails` (one ref, others fine) reaches a verdict.
                if self.fetch_fails:
                    return Result(128, "", "could not read from remote")
                # Empty stdout = the branch is really gone; a line = still there.
                if self.base_still_on_remote:
                    return Result(0, f"{MOVED}\trefs/heads/{self.base}\n", "")
                return Result(0, "", "")
            if cmd[1] == "rev-parse":
                # An empty base_tip models a ref this checkout does not have -
                # what a fresh clone sees once the base was deleted on merge.
                # git exits 128 there; answering LANDED regardless was how the
                # deleted-base test passed without exercising the deletion.
                if not self.base_tip:
                    return Result(128, "", "unknown revision")
                return Result(0, self.base_tip + "\n", "")
            if cmd[1] == "merge-base":
                if not self.base_tip:
                    return Result(128, "", "Not a valid object name")
                return Result(0 if self.contained else 1, "", "")
        return Result(0, "", "")


@pytest.fixture
def patch_run(monkeypatch):
    def _apply(fake):
        monkeypatch.setattr(_base_lineage, "run", fake)
        return fake

    return _apply


def test_base_is_default_branch_is_ok_without_probing(patch_run):
    """The common case short-circuits: no PR list, no fetch, no merge-base."""
    fake = patch_run(FakeRun(base="main"))
    verdict, _ = _base_lineage.lineage_verdict(805, "/repo")
    assert verdict == "ok"
    assert not any(c[1:3] == ["pr", "list"] for c in fake.calls)
    assert not any(c[0] == "git" for c in fake.calls)


def test_merged_pr_on_unmoved_base_refuses(patch_run):
    """The specimen: PR #789 merged feature/bg-crown, whose tip had not moved."""
    patch_run(FakeRun(merged_pr="789", merged_head=LANDED, base_tip=LANDED, contained=True))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "stale"
    assert "#789" in why
    # The refusal has to name the fix, not just the fault.
    assert "gh pr edit 800 --base main" in why


def test_recreated_branch_with_same_name_is_not_refused(patch_run):
    """A merged PR is a fact about a NAME, not about the branch as it stands.

    Delete `feature/x` after merging, recreate it for new work, and the old
    merged PR answers this query forever. Refusing on that alone would be a
    permanent false refusal with no remedy but the bypass - the shape that gets
    a guard switched off.
    """
    patch_run(FakeRun(merged_pr="789", merged_head=LANDED, base_tip=MOVED, contained=False))
    verdict, _ = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "ok"


def test_ancestry_alone_refuses_when_no_merged_pr_exists(patch_run):
    """(ii) covers (i)'s blind spot: a base that landed leaving no merged PR."""
    patch_run(FakeRun(merged_pr="", contained=True))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "stale"
    assert "fully contained" in why
    # The one known false positive (a base whose own work is not pushed yet) is
    # named in the refusal, because only the reader can tell the two apart.
    assert "not pushed yet" in why


def test_merged_pr_alone_refuses_when_base_is_not_an_ancestor(patch_run):
    """(i) covers (ii)'s blind spot: under squash a landed base is no ancestor."""
    patch_run(FakeRun(merged_pr="789", merged_head=LANDED, base_tip=LANDED, contained=False))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "stale"
    assert "#789" in why


def test_healthy_stack_passes(patch_run):
    """A base with commits main lacks and no merged PR is legitimate stacking.

    This is the false-refusal case that would get the guard switched off: the
    base exists, carries work, and nobody has opened its PR yet.
    """
    patch_run(FakeRun(merged_pr="", contained=False))
    verdict, _ = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "ok"


def test_deleted_base_branch_still_refuses(patch_run):
    """`delete_branch_on_merge` removes the base ref the instant it lands.

    Fetching both refspecs in ONE `git fetch` failed wholesale on the missing
    base ref, left `origin/main` unrefreshed too, and produced `unknown` -
    which every in-process caller treats as proceed. Fetched separately, the
    stale `origin/<base>` still pins the commit that landed and both checks
    answer.
    """
    patch_run(
        FakeRun(base_fetch_fails=True, merged_pr="789", merged_head=LANDED,
                base_tip=LANDED, contained=True)
    )
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "stale"
    assert "#789" in why


def test_deleted_base_with_no_local_ref_still_refuses(patch_run):
    """The state a FRESH clone sees, which is every CI runner.

    `delete_branch_on_merge` removes the base as it lands, and a checkout that
    never fetched the branch has no `origin/<base>` to pin - so (i) has no tip
    to compare and (ii) cannot resolve ancestry. That read `unknown`, and every
    in-process caller treats `unknown` as proceed, so the guard failed OPEN in
    exactly its headline scenario. The confirmed deletion is verdict enough.
    """
    patch_run(
        FakeRun(base_fetch_fails=True, merged_pr="789", merged_head=LANDED, base_tip="")
    )
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "stale"
    assert "no longer exists on the remote" in why
    assert "gh pr edit 800 --base main" in why


def test_deleted_base_with_no_local_ref_and_no_merged_pr_still_refuses(patch_run):
    """(iii) does not lean on the gh probe: a deleted branch carries nothing
    onward whatever killed it, so it refuses with the merged-PR probe empty."""
    patch_run(FakeRun(base_fetch_fails=True, merged_pr="", base_tip=""))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "stale"
    assert "no longer exists on the remote" in why


def test_missing_local_ref_without_a_confirmed_deletion_stays_unknown(patch_run):
    """(iii) fires only on a CONFIRMED deletion. A base still on the remote that
    this checkout simply cannot resolve is an unanswered question, not a
    refusal - forging one here would block healthy stacked PRs on a git hiccup.
    """
    patch_run(FakeRun(base_still_on_remote=True, merged_pr="789", base_tip=""))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "unknown"
    assert "ancestry probe" in why


def test_transient_base_fetch_failure_does_not_forge_a_refusal(patch_run):
    """A failed base fetch is not the same as a deleted branch.

    `origin/<base>` is then whatever the last fetch left, so a base that has
    since moved on still reads as "landed and unmoved" and would REFUSE a
    healthy stacked PR. `git ls-remote` separates the two; a branch still on
    the remote leaves the git side blind, which is `unknown`, not `stale`.
    """
    patch_run(
        FakeRun(base_fetch_fails=True, base_still_on_remote=True, merged_pr="789",
                merged_head=LANDED, base_tip=LANDED, contained=True)
    )
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "unknown"
    assert "ancestry probe" in why


def test_merged_pr_without_a_head_oid_is_unknown_not_ok(patch_run):
    """(i) cannot be evaluated without the oid to compare to the live tip, and
    an unevaluated check must never fall through to a pass."""
    patch_run(FakeRun(merged_pr="789", merged_head="null", contained=False))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "unknown"
    assert "merged-PR probe" in why


def test_failed_probe_does_not_mask_a_stale_verdict(patch_run):
    """A dead gh on one probe must not hide the other probe's refusal."""
    patch_run(FakeRun(list_fails=True, contained=True))
    verdict, _ = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "stale"


@pytest.mark.parametrize(
    "kwargs, fragment",
    [
        ({"default_fails": True}, "default branch"),
        ({"base_fails": True}, "base ref"),
        ({"list_fails": True}, "merged-PR probe"),
        ({"fetch_fails": True}, "ancestry probe"),
        ({"list_fails": True, "fetch_fails": True}, "both lineage probes"),
    ],
)
def test_probe_failures_report_unknown_not_ok(patch_run, kwargs, fragment):
    """An unevaluated gate is `unknown`, never a pass - and it names which probe."""
    patch_run(FakeRun(**kwargs))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "unknown"
    assert fragment in why


def test_missing_git_degrades_to_unknown_not_a_traceback(patch_run):
    """`_proc.run` raises ToolMissing when a binary is absent.

    The in-process callers promise that an unevaluated probe degrades to a
    breadcrumb, so an uncaught raise here would surface as a traceback out of
    `fno pr verify` instead - the opposite of the stated contract.
    """
    patch_run(FakeRun(git_missing=True))
    verdict, why = _base_lineage.lineage_verdict(800, "/repo")
    assert verdict == "unknown"
    assert "ancestry probe" in why


def test_cli_exit_codes(patch_run, monkeypatch):
    monkeypatch.delenv(_base_lineage.BYPASS_ENV, raising=False)
    patch_run(FakeRun(merged_pr="789", contained=True))
    assert _base_lineage.run_base_lineage_check(800, "/repo") == _base_lineage.REFUSED_STALE

    patch_run(FakeRun(merged_pr="", contained=False))
    assert _base_lineage.run_base_lineage_check(800, "/repo") == _base_lineage.OK

    patch_run(FakeRun(list_fails=True, fetch_fails=True))
    assert _base_lineage.run_base_lineage_check(800, "/repo") == _base_lineage.UNKNOWN


def test_bypass_escape_survives_a_string_pr_number(monkeypatch):
    """`fno pr verify` carries its PR as a free-form str, and `emit_gate_escape`
    types `pr` as an int (`pr <= 0`). Passing the str through raised TypeError
    into this function's fail-open swallow, so the bypass recorded nothing."""
    import fno.events.gate_escape as ge

    seen: list[dict] = []
    monkeypatch.setattr(ge, "emit_gate_escape", lambda reason, **kw: seen.append(kw))
    _base_lineage.emit_bypass_escape("805", "/repo", "base landed")
    assert seen and seen[0]["pr"] == 805


def test_bypass_passes_and_records_an_escape(patch_run, monkeypatch):
    """The escape hatch exists so nobody deletes the guard, and it leaves a trail."""
    patch_run(FakeRun(merged_pr="789", contained=True))
    monkeypatch.setenv(_base_lineage.BYPASS_ENV, _base_lineage.BYPASS_VALUE)
    seen: list[str] = []
    monkeypatch.setattr(
        _base_lineage, "emit_bypass_escape", lambda pr, cwd, reason: seen.append(reason)
    )
    assert _base_lineage.run_base_lineage_check(800, "/repo") == _base_lineage.OK
    assert seen and "#789" in seen[0]
