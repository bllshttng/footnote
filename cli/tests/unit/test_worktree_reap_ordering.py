"""Reaping must follow the retro, on BOTH paths (x-5a30 task 1.7).

The sequence is: PR merges, retro, reap the session, archive the worktree, drop
the row. Getting it wrong destroys evidence, because a worker's attestation is
session-bound and its raw event store is worktree-bound. PR 834 and PR 835 both
sat green with `reviewed_count` 0 while the only copy of the attestation lived
inside the worktree. Reaping those sessions destroys the only process able to
satisfy its own PR's review gate.

Inside the ritual the order already holds, and this file PINS it rather than
rebuilding it: `run()` calls the legs in a fixed sequence with the retro at
position 2 and both removal legs after it.

The gap is the OTHER path. `fno worktree cleanup --merged` knows nothing about a
retro and neither does the daemon tick. A guard on one of N reachable paths is
decorative, so the sweep's refusals are asserted here too.
"""
import inspect
import re
import subprocess
from pathlib import Path

import pytest

from fno.pr import _ritual

REPO_ROOT = Path(__file__).resolve().parents[3]
LIFECYCLE = REPO_ROOT / "scripts" / "lib" / "worktree-lifecycle.sh"


# -- AC7-ORDER: the ritual's leg order is a contract, not a coincidence -------


def _leg_order() -> list[str]:
    """The leg names `run()` calls, in source order."""
    src = inspect.getsource(_ritual.Ritual.run)
    return re.findall(r"self\.(leg_\w+)\(\)", src)


def test_retro_precedes_every_removal_leg() -> None:
    order = _leg_order()

    assert "leg_harvest" in order, "the retro leg must still be in run()"
    retro = order.index("leg_harvest")
    for removal in ("leg_archive", "leg_reap_rows"):
        assert removal in order, f"{removal} must still be in run()"
        assert order.index(removal) > retro, (
            f"{removal} runs at {order.index(removal)}, retro at {retro}. "
            "Reaping before the retro destroys the evidence the retro reads."
        )


def test_the_retro_leg_actually_runs_the_retro() -> None:
    """Pin that `leg_harvest` is the retro, so a rename cannot hollow out the
    ordering assertion above into a check on two arbitrary names."""
    src = inspect.getsource(_ritual.Ritual.leg_harvest)

    assert '"retro"' in src
    assert '"run"' in src


def test_archive_precedes_row_reap() -> None:
    """Archive the worktree, THEN drop the row. The row is how the worktree is
    found; dropping it first orphans the directory with nothing pointing at it."""
    order = _leg_order()

    assert order.index("leg_archive") < order.index("leg_reap_rows")


# -- AC7-ERR / AC7-EDGE: the sweep path refuses the same cases ---------------


def _lifecycle_source() -> str:
    return LIFECYCLE.read_text()


def test_sweep_requires_merged_into_origin_main() -> None:
    """The sweep keys on "branch tip landed in origin/main", never on a commit
    count. An open PR's branch never satisfies that, which is what keeps the
    834/835 shape safe on this path."""
    src = _lifecycle_source()

    assert "kept (unmerged)" in src, "the unmerged refusal must still exist"


def test_sweep_never_keys_on_zero_commits_ahead() -> None:
    """ZERO-AHEAD IS NOT A REAP KEY, and this is the assertion that keeps it out.

    A session that finished its work and has not pushed reads as zero commits
    ahead of origin/main. So does a session whose branch merged. The first still
    holds the only copy of its PR's attestation.

    `rev-list --count` is legitimate for REPORTING, so the refusal is narrow:
    no comparison of a commit count against zero may decide a removal. The
    detached-HEAD decision routes through wt_unpushed_count, so the scan covers
    the helper where the count is computed; the sound key there is "no commit
    absent from EVERY remote" (a finished-but-unpushed session reads > 0), and
    this scan is what forces a future narrowing back to one default branch to
    show up as a diff here.
    """
    unpushed_lib = REPO_ROOT / "scripts" / "lib" / "worktree-unpushed.sh"
    src = _lifecycle_source() + "\n" + unpushed_lib.read_text()

    # Find any `-eq 0` / `== 0` test in the same line as a rev-list count.
    for line in src.splitlines():
        if "rev-list" not in line or "--count" not in line:
            continue
        assert not re.search(r"(-eq|==)\s*0\b", line), (
            f"zero-ahead used as a decision: {line.strip()!r}. "
            "A finished-but-unpushed session is also zero-ahead."
        )


def test_sweep_reports_before_it_removes() -> None:
    """Dry-run is the default for --merged, and an explicit --dry-run wins even
    when --apply is also passed, so a safety wrapper appending --dry-run is
    never ignored."""
    src = _lifecycle_source()

    assert re.search(r'-z\s+"\$APPLY"\s+\|\|\s+-n\s+"\$DRY_RUN"', src), (
        "the dry-run-by-default guard must survive"
    )


@pytest.mark.parametrize(
    "verdict",
    ["kept (dirty)", "kept (unmerged)", "kept (unpushed)", "kept (live-session)", "kept (permanent)"],
)
def test_sweep_keeps_rather_than_forces(verdict: str) -> None:
    """Every refusal the sweep can reach must KEEP. A sweep that forced past any
    of them would be the unattended path doing what only an operator may do."""
    src = _lifecycle_source()

    assert verdict in src


def test_sweep_reap_is_reachable_at_all() -> None:
    """Positive control. Every assertion above is about a refusal, and a script
    that could never archive anything would satisfy all of them."""
    src = _lifecycle_source()

    assert "would-archive" in src
    assert '"$ARCHIVE"' in src


# -- The sweep is report-only when a timer fires it --------------------------


def test_daemon_sweep_is_report_only() -> None:
    """A merged PR is external proof the work landed. A timer tick is not, so
    the daemon's sweep reports and the merge-triggered path removes."""
    daemon = (REPO_ROOT / "crates" / "fno-agents" / "src" / "daemon.rs").read_text()
    start = daemon.index("pub fn worktree_sweep(")
    body = daemon[start : start + 2500]

    assert "--apply" not in body
    assert '"report-only"' in body
