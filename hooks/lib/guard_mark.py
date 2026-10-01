"""guard_mark.py - shared by the Python PreToolUse guards; provides
guard_mark(), the positive liveness signal that a guard actually ran and
what it decided. One guard_decision event row per invocation, committed to
the journal's store through the native ``fno doctor event emit-envelope``
verb: the same write boundary the bash transport (guard-mark.sh via
events.sh) and the Rust dispatcher use, so every guard's row lands in the
one store regardless of language. Best-effort by contract: any failure is
swallowed and can never change a guard's decision."""

import json
import os
import shutil
import subprocess
import time


def _resolve_bin():
    """FNO_BIN, then the checkout build, then PATH - the same policy
    scripts/lib/events.sh applies, so a test tree never answers through a
    stale installed binary."""
    bin_path = os.environ.get("FNO_BIN")
    if bin_path:
        return bin_path
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for profile in ("debug", "release"):
        candidate = os.path.join(root, "crates", "fno", "target", profile, "fno")
        if os.access(candidate, os.X_OK):
            return candidate
    return shutil.which("fno")


def _resolve_agents_bin():
    """FNO_AGENTS_BIN, then the checkout build, then PATH - the same policy
    git-protection.py applies to fno-agents probes."""
    bin_path = os.environ.get("FNO_AGENTS_BIN")
    if bin_path:
        return bin_path
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for profile in ("debug", "release"):
        candidate = os.path.join(root, "crates", "fno-agents", "target", profile, "fno-agents")
        if os.access(candidate, os.X_OK):
            return candidate
    return shutil.which("fno-agents")


def _resolve_events_path():
    """FNO_EVENTS_PATH, then the journal the space layer resolves (the pin a
    harness sets, or the space journal the store reads), then the rev-parse
    degrade. A cwd guess is how guard rows landed in the checkout journal
    the store ignores. The fact comes from `fno-agents state path events`,
    whose resolver mirrors the retired fno.paths import; a missing binary or
    a failing verb degrades rather than blocks."""
    pin = os.environ.get("FNO_EVENTS_PATH")
    if pin:
        return pin
    agents_bin = _resolve_agents_bin()
    if agents_bin:
        try:
            proc = subprocess.run(
                [agents_bin, "state", "path", "events"],
                capture_output=True, text=True, timeout=5,
            )
            out = (proc.stdout or "").strip()
            if proc.returncode == 0 and out:
                return out.splitlines()[0]
        except Exception:
            pass
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, timeout=5,
    ).stdout.strip()
    return os.path.join(root or os.getcwd(), ".fno", "events.jsonl")


def guard_mark(guard, decision, tool):
    if decision == "deny":
        # One vocabulary across every guard: bash guards say block, and the
        # audit row is shared surface, so a refusal is "block" whichever
        # language recorded it.
        decision = "block"
    try:
        row = json.dumps({
            "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "type": "guard_decision",
            "data": {"guard": guard, "decision": decision, "tool": tool},
            "source": "hook",
        }, separators=(",", ":"))
        bin_path = _resolve_bin()
        if not bin_path:
            return
        subprocess.run(
            [bin_path, "doctor", "event", "emit-envelope",
             "--events", _resolve_events_path()],
            input=row, capture_output=True, text=True, timeout=10,
        )
    except Exception:
        pass
