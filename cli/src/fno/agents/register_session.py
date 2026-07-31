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
import sys
from typing import Optional, Sequence

from fno.agents import events
from fno.agents.registry import register_existing_session, restamp_harness_session_id


def _restamp(agent_self: str, harness: str, session_id: str) -> int:
    """Re-point a SPAWNED worker's own row at its live session id, then stop.

    Split from registration because the two answer different questions. A
    spawned worker already HAS a row; the only thing that can be wrong is which
    session id it records, and a harness that re-minted the id we passed at
    spawn leaves the row addressing nothing. Registration keys on that same
    re-mintable id, so routing a re-minted worker through it appends a SECOND
    row for one worker instead of fixing the first -- which is why this returns
    rather than falling through.
    """
    try:
        entry = restamp_harness_session_id(
            name=agent_self, harness=harness, session_id=session_id
        )
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
        entry = register_existing_session(
            provider=args.harness,
            session_id=args.session_id,
            cwd=args.cwd,
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
        provider=entry.harness,
        name=entry.name,
        session_id=args.session_id,
        cwd=entry.cwd,
    )
    print(f"register_session: registered {entry.name} ({entry.harness})", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
