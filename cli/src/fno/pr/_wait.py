"""`fno do pr wait <n>` - the one sanctioned watcher loop (x-4eac).

Kings and workers hand-rolled `while ...; do fno do pr status <n> |
grep '"settled": true'; sleep 60; done` - one loop per session per PR against
a quota every session on the machine shares, each with its own iteration cap
and none of them able to back off. This verb is that loop with the fleet's
interests built in: every tick goes through the coalescing cache
(``fno.pr._cache.cached_status``), so N waiters on one PR cost one network
read per TTL; a secondary-limit backoff window serves the row degraded to
``settled: false``, which the wait rides out instead of hammering; and the
process's gh-call count prints at exit, so the spend is visible to the
spender.

Exit codes are `fno do pr status`'s alphabet, answering the condition asked:
0 green, 1 red (settled-red under ``--until settled``), 2 pending or
timeout-without-settling, 3 unknown, 4 fetch error, 127 gh-missing. A timeout
exits with the LAST observed code and a note - a caller re-arms, exactly as
it re-armed a bounded hand-rolled loop. A persistent fetch error waits out
the timeout rather than flapping: one blip recovers, and the cache's backoff
is already throttling rate-class errors.
"""

from __future__ import annotations

import json
import re
import sys
import time
from typing import Callable, Optional

_DURATION = re.compile(r"^(\d+(?:\.\d+)?)(s|m|h)?$", re.IGNORECASE)
_MIN_INTERVAL = 5.0


def parse_duration(text: str) -> float:
    """``30m`` / ``90s`` / ``1.5h`` / plain seconds -> seconds. 0 on garbage."""
    m = _DURATION.match(str(text).strip())
    if not m:
        return 0.0
    mult = {"s": 1.0, "m": 60.0, "h": 3600.0}.get((m.group(2) or "s").lower(), 1.0)
    return float(m.group(1)) * mult


def _poll(pr: str, cwd: Optional[str]) -> "tuple[int, dict, str, str]":
    """One cache-coalesced status tick with both streams captured.

    ``cached_status`` prints (the human line to stderr, the JSON to stdout);
    a wait must not re-print that every tick, so both streams are captured and
    only the FINAL tick's output is re-emitted by the caller.
    """
    import io

    from fno.pr._cache import cached_status

    out, err = io.StringIO(), io.StringIO()
    real_out, real_err = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = out, err
    try:
        rc = cached_status(str(pr), cwd)
    finally:
        sys.stdout, sys.stderr = real_out, real_err
    payload: dict = {}
    for line in reversed(out.getvalue().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(parsed, dict):
                payload = parsed
            break
    return rc, payload, out.getvalue(), err.getvalue()


def wait_status(
    pr: str,
    *,
    until: str = "settled",
    timeout: float = 1800.0,
    interval: float = 60.0,
    cwd: Optional[str] = None,
    sleeper: Callable[[float], None] = time.sleep,
    clock: Callable[[], float] = time.monotonic,
) -> int:
    """Poll ``cached_status`` until the condition holds or the deadline fires."""
    if until not in ("settled", "green"):
        sys.stderr.write("fno do pr wait: --until must be settled or green\n")
        return 2
    interval = max(_MIN_INTERVAL, interval)
    deadline = clock() + max(0.0, timeout)
    rc: int = 2
    payload: dict = {}
    out_text, err_text = "", ""
    while True:
        rc, payload, out_text, err_text = _poll(pr, cwd)
        done = payload.get("green") if until == "green" else payload.get("settled")
        if done:
            sys.stdout.write(out_text)
            sys.stderr.write(err_text)
            return rc
        now = clock()
        if now + interval > deadline:
            sys.stdout.write(out_text)
            sys.stderr.write(err_text)
            sys.stderr.write(
                f"wait: still not {until} after {int(timeout)}s; last verdict "
                f"{payload.get('verdict')}. Re-arm the wait or read the PR.\n"
            )
            return rc if rc != 0 else 2
        sleeper(interval)


def main(argv: "list[str]") -> int:
    args = [a for a in argv if not str(a).startswith("-")]
    until = "settled"
    timeout_s, interval_s = "30m", "60"
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--until":
            until = argv[i + 1] if i + 1 < len(argv) else ""
            i += 2
        elif a.startswith("--until="):
            until = a.split("=", 1)[1]
            i += 1
        elif a == "--timeout":
            timeout_s = argv[i + 1] if i + 1 < len(argv) else "30m"
            i += 2
        elif a.startswith("--timeout="):
            timeout_s = a.split("=", 1)[1]
            i += 1
        elif a == "--interval":
            interval_s = argv[i + 1] if i + 1 < len(argv) else "60"
            i += 2
        elif a.startswith("--interval="):
            interval_s = a.split("=", 1)[1]
            i += 1
        else:
            i += 1
    if len(args) != 1 or not str(args[0]).strip().isdigit():
        sys.stderr.write(
            "usage: fno do pr wait <pr-number> [--until settled|green] "
            "[--timeout 30m] [--interval 60]\n"
        )
        return 2
    timeout = parse_duration(timeout_s)
    interval = parse_duration(interval_s)
    if timeout <= 0 or interval <= 0:
        sys.stderr.write("fno do pr wait: --timeout/--interval must parse (30m, 90s, 1h)\n")
        return 2
    try:
        return wait_status(
            str(args[0]),
            until=until,
            timeout=timeout,
            interval=interval,
        )
    except Exception as exc:  # noqa: BLE001 - ToolMissing subclasses included
        from fno.pr._proc import ToolMissing

        if isinstance(exc, ToolMissing):
            sys.stderr.write(f"fno do pr wait: {exc.tool} not found on PATH\n")
            return 127
        raise
