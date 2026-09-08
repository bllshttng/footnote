"""Bounded pane placement verification, split out of mux_spawn.py (file
budget: that file is shrink-only). One listing read must not condemn a pane:
the causes get distinct names, and a missing row is re-listed once before the
judge fires. Fail-closed contract stays in the caller: every raise here is
reaped-and-no-row upstream."""

import time
from typing import Callable, Optional

from fno.agents.dispatch import DispatchAskError


def verify_bounded_placement(
    pane_id: int,
    expected_tab_id: Optional[int],
    placement_receipt: Optional[dict],
    list_panes: Callable[[], list[dict]],
) -> None:
    """Verify a spawned pane sits in its expected tab, or raise naming why.

    ``expected_tab_id`` of None (no explicit tab: the caller chose the tab and
    wants it read back) resolves from the pane's own listing row. The listing
    is read through ``list_panes`` so the transport stays injectable; a missing
    row re-lists once - a fresh pane can be absent from a listing
    (read-after-write), and an EMPTY listing is also what a refused/absent mux
    socket prints (mux_cli.rs ``is_ls && no_server`` prints ``[]`` exit 0),
    so absence alone is evidence of nothing.
    """
    spawned_row: Optional[dict] = None
    listed: list[dict] = []
    for attempt in range(2):
        listed = list_panes()
        spawned_row = next(
            (item for item in listed if item.get("pane_id") == pane_id), None
        )
        if spawned_row is not None or attempt == 1:
            break
        time.sleep(0.25)
    if spawned_row is None:
        if listed:
            raise DispatchAskError(
                f"bounded placement verification failed: pane {pane_id} "
                f"absent from a {len(listed)}-pane listing",
                exit_code=1,
            )
        raise DispatchAskError(
            "bounded placement verification failed: pane listing was "
            "empty, which a refused or absent mux socket also prints; "
            f"placement of pane {pane_id} unverified",
            exit_code=1,
        )
    landed_tab_id = spawned_row.get("tab_id")
    if landed_tab_id is None:
        raise DispatchAskError(
            f"bounded placement verification failed: pane {pane_id} "
            "row carries no tab_id",
            exit_code=1,
        )
    if expected_tab_id is None:
        expected_tab_id = landed_tab_id
    elif landed_tab_id != expected_tab_id:
        landed_name = spawned_row.get("tab_name")
        landed_label = (
            f"{landed_tab_id} ({landed_name})" if landed_name else str(landed_tab_id)
        )
        redirected = ""
        if (
            isinstance(placement_receipt, dict)
            and placement_receipt.get("tab") is not None
        ):
            redirected = (
                "; the server receipt reports it landed in tab "
                f"{placement_receipt.get('tab')}"
            )
        raise DispatchAskError(
            f"bounded placement verification failed: pane {pane_id} "
            f"landed in tab {landed_label}, expected tab "
            f"{expected_tab_id}{redirected}",
            exit_code=1,
        )
    in_tab = [
        item for item in listed
        if isinstance(item, dict) and item.get("tab_id") == expected_tab_id
    ]
    if len(in_tab) > 4:
        raise DispatchAskError(
            "bounded placement verification failed: fifth pane",
            exit_code=1,
        )
