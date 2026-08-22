"""SessionStart entry point: bind the current session to its registry row.

Invoked by ``hooks/register-session-start.sh`` as
``python3 -m fno.agents.register_session --harness claude ...``. Two modes,
selected by ``--agent-self``:

- without it, REGISTER an operator-started session (it has no row yet);
- with it, RESTAMP a footnote-spawned worker's existing row, named by
  ``FNO_AGENT_SELF``, onto the session id its harness is actually using. The
  id footnote passed at spawn is not durable, and registration keys its upsert
  on that same id, so a re-minted worker routed through registration would
  gain a second row rather than have its first corrected.

Fail-soft by contract (US7 AC7-ERR): any failure emits a
``session_register_failed`` / ``session_restamp_failed`` warning event and
still exits 0, so the hook never blocks session start even when the registry
is locked or unwritable. On success it emits ``session_registered`` /
``session_id_restamped`` and prints a one-line stderr note (hook stdout is
reserved for the session preamble).
"""
from __future__ import annotations

import argparse
import os
import sys
import time
from typing import Optional, Sequence

from fno.agents import events
from fno.agents.registry import register_existing_session, restamp_harness_session_id
from fno.agents.spawn_defaults import resolve_lane_vendor

#: How long a spawned worker waits for its own row to appear before giving up.
#: Covers the spawner's post-`mux pane run` tail with room to spare: child-pid
#: lookup, the <=1s readiness probe, the seed submission, the registry lock.
#:
#: The seed can cost a second round on EITHER path, and the budget must cover
#: both. The pane-send path (agy) pays one `_SEED_RETRY_DELAY_S` sleep plus a
#: second `_submit_spawn_seed` and its mux RPCs. The argv path no longer reaches
#: that retry, but it pays the same sleep plus one read when a frame comes back
#: unreadable, which is the re-probe that keeps a healthy pane from being
#: condemned on one timed-out read. Same order of cost, so do not read the argv
#: path as free. Both fire exactly when panes paint slowest, so the delays
#: arrive together rather than independently. A miss costs
#: addressability, never the session: the hook exits 0 regardless. It leaves the
#: row on the spawn-time session id, which the watchdog ghost lane then reports
#: as `no transcript for <id>`, so widen this before trimming it.
_RESTAMP_ROW_WAIT_S = 10.0
_RESTAMP_ROW_POLL_S = 0.25


def _row_exists(name: str, harness: str) -> bool:
    """True when this worker's row is already in the registry.

    Fails to ``True`` (do not wait) on any read error: a registry we cannot read
    is not evidence that the row is missing, and sleeping on that guess would
    delay session start for a reason we cannot even state.
    """
    from fno.agents.registry import load_registry

    try:
        return any(e.name == name and e.harness == harness for e in load_registry())
    except Exception:  # noqa: BLE001 -- unreadable registry is not "row absent"
        return True


def _restamp(agent_self: str, harness: str, session_id: str) -> int:
    """Re-point a SPAWNED worker's own row at its live session id, then stop.

    Split from registration because the two answer different questions. A
    spawned worker already HAS a row; the only thing that can be wrong is which
    session id it records, and a harness that re-minted the id we passed at
    spawn leaves the row addressing nothing. Registration keys on that same
    re-mintable id, so routing a re-minted worker through it appends a SECOND
    row for one worker instead of fixing the first -- which is why this returns
    rather than falling through.

    RETRIES on a missing row, briefly. The spawner creates the row AFTER
    ``mux pane run`` returns (it needs the pane id, and a half-created row is
    worse than a late one), so a worker whose harness boots fast enough can run
    this hook before its own row exists. `restamp_harness_session_id` cannot
    distinguish "no such row" from "not yet" and returns None either way, so
    without a wait the handoff silently loses the race. That is unrecoverable on
    any route where this restamp is the ONLY path to an id - a happy-hosted
    claude pane cannot be given one at spawn, because happy discards it - and the
    worker would stay id-less and `spawning` for its whole life while being
    perfectly healthy. The wait is bounded and still fail-soft: exhausting it
    returns 0 exactly as a genuine no-row miss always did.
    """
    # Only the pane substrate writes its row after the child starts, and it says
    # so by name. Every other spawn either has its row already or never gets one -
    # a headless one-shot sets FNO_AGENT_SELF and deliberately has no row, so
    # waiting on that signal alone would delay every one-shot's first prompt by
    # the full deadline for a row that is never coming.
    #
    # Comparing to THIS worker's name, not just testing presence: a pane worker
    # passes its whole environment to any one-shot it launches, so the marker is
    # inherited by children that are not the pane. It names the pane, the child
    # overwrites FNO_AGENT_SELF with its own name, and the mismatch cancels the
    # wait without any spawn path having to remember to clear it.
    deadline = time.monotonic() + (
        _RESTAMP_ROW_WAIT_S
        if os.environ.get("FNO_AGENT_ROW_PENDING") == agent_self
        else 0.0
    )
    try:
        while True:
            # Observe the row BEFORE restamping, not after. None from the restamp
            # is ambiguous - no such row YET, or a row that already matches - and
            # only the first is worth waiting on. Reading existence AFTERWARDS
            # answers the wrong instant: the spawner can append the row in between,
            # and we would then break on "a row exists" having never restamped
            # THAT row, stranding the worker id-less for life. Checked first, a
            # True can only mean the row predates this restamp, so a None beside
            # it really is "already current".
            existed = _row_exists(agent_self, harness)
            entry = restamp_harness_session_id(
                name=agent_self, harness=harness, session_id=session_id
            )
            if entry is not None or existed:
                break
            if time.monotonic() >= deadline:
                break
            time.sleep(_RESTAMP_ROW_POLL_S)
    except Exception as exc:  # fail-open: never block session start (AC7-ERR)
        events.emit(
            "session_restamp_failed",
            provider=harness,
            name=agent_self,
            session_id=session_id,
            error=str(exc),
        )
        print(f"register_session: warning: {exc}", file=sys.stderr)
        return 0

    # None means nothing needed doing (id already current, or no such row) --
    # the overwhelmingly common case, so it stays silent on both channels.
    if entry is not None:
        events.emit(
            "session_id_restamped",
            provider=harness,
            name=entry.name,
            session_id=session_id,
        )
        print(
            f"register_session: restamped {entry.name} -> {session_id}",
            file=sys.stderr,
        )
    return 0


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="register_session")
    # --harness is canonical; --provider is the axis-rename alias (x-bab1), kept
    # so the fail-soft SessionStart hook keeps working across the cutover.
    parser.add_argument("--harness", dest="harness",
                        help="Harness/CLI identity to register (claude | codex | gemini).")
    parser.add_argument("--provider", dest="harness", help=argparse.SUPPRESS)
    parser.add_argument("--session-id", required=True)
    parser.add_argument("--cwd", required=True)
    parser.add_argument("--name", default=None)
    parser.add_argument("--log-path", default="")
    parser.add_argument(
        "--agent-self",
        default=None,
        help="This worker's own registry row name (FNO_AGENT_SELF). Set only for "
        "a footnote-SPAWNED worker; switches this call from register to restamp.",
    )
    args = parser.parse_args(argv)

    if not args.harness:
        parser.error("--harness is required")

    # An empty session id reaches here when the hook's CLI env var is unset
    # (non-claude harness, or claude not exporting it). Treat as a silent
    # no-op rather than a noisy failure event: there is nothing to register.
    if not args.session_id:
        return 0

    if args.agent_self:
        return _restamp(args.agent_self, args.harness, args.session_id)

    try:
        vendor = resolve_lane_vendor([args.harness], env=os.environ, harness=args.harness)
        entry = register_existing_session(
            session_id=args.session_id,
            cwd=args.cwd,
            harness=args.harness,
            provider=vendor,
            name=args.name or None,
            log_path=args.log_path,
            origin="operator",
        )
    except Exception as exc:  # fail-open: never block session start (AC7-ERR)
        events.emit(
            "session_register_failed",
            provider=args.harness,
            session_id=args.session_id,
            error=str(exc),
        )
        print(f"register_session: warning: {exc}", file=sys.stderr)
        return 0

    events.emit(
        "session_registered",
        provider=entry.provider,
        harness=entry.harness,
        name=entry.name,
        session_id=args.session_id,
        cwd=entry.cwd,
    )
    print(f"register_session: registered {entry.name} ({entry.harness})", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
