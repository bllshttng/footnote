"""The spawn's thread-portal door and the substrate gates the CLI applies.

One module answers three questions: which substrate a spawn takes when the
caller left it open, which substrate/portal combinations refuse, and how the
portal view is placed after the receipt. Placement runs the documented
two-call seam (``fno mux thread <name> --portal N``), so binary selection and
geometry stay owned by that verb; a placement failure never recolors the
worker receipt - the receipt is the truth and the worker is already live.
"""
from __future__ import annotations

import os
import subprocess
import sys
from typing import Optional, Tuple

import typer


def resolve_body_substrate(
    harness: str,
    *,
    passthrough: bool,
    split: Optional[str],
    at: Optional[str],
    tab: Optional[str],
    bounded_placement: bool,
    squad: Optional[str],
    monitor: Optional[str],
) -> Tuple[str, Optional[int]]:
    """The substrate for a spawn whose ``--substrate`` option arrived empty.

    The seam at the front door injects the same answer as an explicit token;
    this body path re-derives it only when a spawn reaches Python dispatch
    without one. A pane-only capability (the fence, placement, a monitor)
    implies the pane. Otherwise the harness decides: thread where it seats
    one, else the closable pane. A thread resolved this way from INSIDE a mux
    also requests the default view (portal 0); outside a mux there is no
    session to host a portal and the thread starts paneless.
    """
    from fno.agents.harness_map import thread_seatable

    pane_implied = bool(
        passthrough or split or at or tab or bounded_placement or squad
        or monitor is not None
    )
    if pane_implied or not thread_seatable(harness):
        return "pane", None
    return "thread", (0 if os.environ.get("FNO_PANE") else None)


def resolve_substrate_or_exit(substrate: str) -> str:
    """Validate the substrate value and canonicalize ``thread`` to ``bg``.

    Keep the lower-level spawn branches on the historical lane name while the
    public vocabulary migrates. Exit 2 on a value outside the closed set; the
    deprecated ``bg`` spelling warns and still works.
    """
    if substrate not in ("pane", "thread", "bg", "headless"):
        print(
            f"--substrate must be one of: pane, thread, headless (bg is a deprecated alias; got {substrate})",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if substrate == "bg":
        print(
            "warning: substrate value 'bg' is deprecated; use 'thread' instead; "
            "the alias will be removed after one release",
            file=sys.stderr,
        )
    if substrate == "thread":
        substrate = "bg"
    return substrate


def validate_portal_or_exit(portal: Optional[int], substrate: str) -> None:
    """Refuse a portal that cannot open, before anything spawns."""
    if portal is None:
        return
    if not 0 <= portal <= 255:
        print(f"--portal takes an index 0-255 (got {portal})", file=sys.stderr)
        raise typer.Exit(code=2)
    if substrate != "bg":
        # A portal is the pane a thread hosts. A pane hosts its own view
        # (nothing to place); a one-shot exits before it can be viewed.
        print(
            "--portal applies only to the thread substrate; a pane hosts "
            "its own view and a one-shot exits before it can be viewed",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)


def validate_monitor_or_exit(
    monitor: Optional[str], substrate: str, *, once: bool, harness: str
) -> None:
    """The monitor gate: initial support is exactly claude+zai on a pane."""
    if monitor is not None and monitor != "happy":
        print(f"--monitor must be 'happy' (got {monitor!r})", file=sys.stderr)
        raise typer.Exit(code=2)
    if monitor == "happy" and (substrate != "pane" or once):
        print(
            "--monitor happy is pane-only; bg and headless workers do not pass "
            "the happy launcher seam",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if monitor == "happy" and harness != "claude":
        print(
            f"--monitor happy requires the claude harness; got harness {harness!r}",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)


def place_thread_portal(name: str, portal: int) -> None:
    """Open a portal on a spawned thread (the two-call seam's second call).

    Best-effort by contract: the worker receipt is already the truth, so a
    failure is a named stderr line pointing at the manual reach, never a
    failed spawn - a retrying caller must not create a duplicate worker.
    """
    detail = ""
    try:
        fno_bin = os.environ.get("FNO_BIN") or "fno"
        proc = subprocess.run(
            [fno_bin, "mux", "thread", name, "--portal", str(portal)],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if proc.returncode == 0:
            return
        lines = (proc.stderr or proc.stdout or "").strip().splitlines()
        detail = lines[-1] if lines else f"exit {proc.returncode}"
    except (OSError, subprocess.SubprocessError) as exc:
        detail = str(exc)
    print(
        f"portal placement failed: {detail}; the worker is live: reach it "
        f"with `fno mux thread {name} --portal {portal}`",
        file=sys.stderr,
    )
