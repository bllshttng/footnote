"""SessionStart entry point: register the current operator-started session.

Invoked by ``hooks/register-session-start.sh`` as
``python3 -m fno.agents.register_session --provider claude ...``.

Fail-soft by contract (US7 AC7-ERR): any failure emits a
``session_register_failed`` warning event and still exits 0, so the hook
never blocks session start even when the registry is locked or unwritable.
On success it emits ``session_registered`` and prints a one-line stderr
note (hook stdout is reserved for the session preamble).
"""
from __future__ import annotations

import argparse
import sys
from typing import Optional, Sequence

from fno.agents import events
from fno.agents.registry import register_existing_session


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="register_session")
    parser.add_argument("--provider", required=True)
    parser.add_argument("--session-id", required=True)
    parser.add_argument("--cwd", required=True)
    parser.add_argument("--name", default=None)
    parser.add_argument("--log-path", default="")
    args = parser.parse_args(argv)

    # An empty session id reaches here when the hook's CLI env var is unset
    # (non-claude harness, or claude not exporting it). Treat as a silent
    # no-op rather than a noisy failure event: there is nothing to register.
    if not args.session_id:
        return 0

    try:
        entry = register_existing_session(
            provider=args.provider,
            session_id=args.session_id,
            cwd=args.cwd,
            name=args.name or None,
            log_path=args.log_path,
        )
    except Exception as exc:  # fail-open: never block session start (AC7-ERR)
        events.emit(
            "session_register_failed",
            provider=args.provider,
            session_id=args.session_id,
            error=str(exc),
        )
        print(f"register_session: warning: {exc}", file=sys.stderr)
        return 0

    events.emit(
        "session_registered",
        provider=entry.provider,
        name=entry.name,
        session_id=args.session_id,
        cwd=entry.cwd,
    )
    print(f"register_session: registered {entry.name} ({entry.provider})", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
