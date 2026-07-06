"""Spawn gate (x-c5cc): global concurrency cap + free-RAM floor + queue loop.

Called at the top of ``cmd_spawn`` before the substrate fan-out. Mirrors
``crates/fno-agents/src/spawn_gate.rs`` — the two gates sit on mutually
exclusive execution paths (the front door execs the binary for bg/headless;
the Rust ``pane`` arm re-execs this CLI), so every spawn passes exactly one.

The gate is READ-ONLY over the fno registry and claude's daemon roster; its
only writes are its own claims (``spawn-gate`` check→dispatch mutex,
``worker:<name>`` headless slot claims, both under the GLOBAL claims root —
the RAM budget is machine-wide). Every guard fails OPEN on read errors: the
gate must never become the thing that bricks spawning.
"""
from __future__ import annotations

import json
import os
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional
from urllib.parse import unquote

# Exit codes, distinct from existing dispatch codes (2, 13, 14, 15, 18, 127)
# and byte-parity with the Rust gate.
EXIT_QUEUE_TIMEOUT = 75
EXIT_NO_WAIT = 76
EXIT_RAM_REFUSED = 77

QUEUE_POLL_S = 2.0
QUEUE_PROGRESS_EVERY_S = 30.0
QUEUE_TIMEOUT_S = 600.0
GATE_CLAIM_TTL_MS = 5 * 60 * 1000
WORKER_CLAIM_TTL_MS = 4 * 60 * 60 * 1000

#: Registry statuses that can hold a live process. `idle` counts when the pid
#: is alive (an unreaped idle process still holds RAM); a reaped pid drops out
#: via the liveness check — the reaper is our slot-release mechanism.
LIVE_STATUSES = frozenset(
    {"spawning", "ready", "idle", "busy", "live", "restarting"}
)


def _warn(msg: str) -> None:
    print(msg, file=sys.stderr)


# ---------------------------------------------------------------------------
# Layer 2: available RAM
# ---------------------------------------------------------------------------

def available_ram_gb() -> Optional[float]:
    """Available system RAM in GB, or None when unreadable (guard skipped)."""
    try:
        import psutil

        return psutil.virtual_memory().available / (1024.0**3)
    except Exception:
        return None


# ---------------------------------------------------------------------------
# Layer 1: the union live-count
# ---------------------------------------------------------------------------

def _pid_alive(pid: Optional[int], recorded_start: Optional[int]) -> bool:
    """Is ``pid`` a live (non-zombie) process?

    The strict pid_start_time equality check lives on the Rust side, which
    minted the recorded value in platform-native units; psutil's epoch-seconds
    basis cannot be compared to it, so Python degrades to existence+status
    (matching ``pid_is_ours``'s own no-recorded-value fallback).
    """
    del recorded_start
    if not pid or pid <= 1:
        return False
    try:
        import psutil

        proc = psutil.Process(pid)
        return proc.is_running() and proc.status() != psutil.STATUS_ZOMBIE
    except Exception:
        return False


def _roster_path() -> Path:
    override = os.environ.get("FNO_CLAUDE_DAEMON_DIR")
    base = Path(override) if override else Path.home() / ".claude" / "daemon"
    return base / "roster.json"


@dataclass
class LiveWorker:
    """One live process row, shared by the gate count and ``fno agents top``."""

    source: str  # "fno" | "claude"
    name: str
    provider: str
    substrate: str
    pid: Optional[int]
    status: str


@dataclass
class LiveCensus:
    workers: list[LiveWorker] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    #: live worker:<name> slot claims (headless one-shots, no process row yet)
    slot_claims: int = 0

    @property
    def count(self) -> int:
        return len(self.workers) + self.slot_claims


def census() -> LiveCensus:
    """The union live-count: fno registry ∪ claude roster (deduped by claude
    session short_id) + live ``worker:<name>`` slot claims. Read-only; every
    source failure degrades to zero contribution with one warning."""
    out = LiveCensus()
    counted_short_ids: set[str] = set()

    # claude roster first: RAM ground truth for foreign `claude --bg` sessions.
    roster_workers: dict[str, dict] = {}
    try:
        raw = json.loads(_roster_path().read_text(encoding="utf-8"))
        if isinstance(raw, dict) and isinstance(raw.get("workers"), dict):
            roster_workers = raw["workers"]
    except FileNotFoundError:
        pass  # fresh machine / daemon never ran: claude-side count is zero.
    except Exception as exc:
        out.warnings.append(
            f"spawn-gate: claude roster unreadable ({exc}); counting fno registry only"
        )

    seen_sessions: set[str] = set()
    for w in roster_workers.values():
        if not isinstance(w, dict):
            continue
        session_id = str(w.get("sessionId") or "")
        if not session_id or session_id in seen_sessions:
            continue
        seen_sessions.add(session_id)
        pid = w.get("pid") if isinstance(w.get("pid"), int) else None
        if _pid_alive(pid, None):
            short_id = session_id.split("-")[0]
            counted_short_ids.add(short_id)
            out.workers.append(
                LiveWorker(
                    source="claude",
                    name=short_id,
                    provider="claude",
                    substrate="(foreign)",
                    pid=pid,
                    status="live",
                )
            )

    # fno registry rows, skipping ones already counted via the roster.
    try:
        from fno.agents.registry import load_registry

        rows = load_registry()
    except Exception as exc:
        out.warnings.append(
            f"spawn-gate: fno registry unreadable ({exc}); counting claude roster only"
        )
        rows = []
    for row in rows:
        if row.status not in LIVE_STATUSES:
            continue
        dedup_key = row.claude_short_id or row.short_id or None
        if dedup_key and dedup_key in counted_short_ids:
            continue
        if _pid_alive(row.pid, row.pid_start_time):
            if dedup_key:
                counted_short_ids.add(dedup_key)
            substrate = "pane" if getattr(row, "mux", None) else (
                "bg" if row.claude_short_id else "worker"
            )
            out.workers.append(
                LiveWorker(
                    source="fno",
                    name=row.name,
                    provider=row.provider,
                    substrate=substrate,
                    pid=row.pid,
                    status=str(row.status),
                )
            )

    out.slot_claims = _live_worker_slot_claims(out.warnings)
    return out


def _gate_claims_root() -> Path:
    from fno.claims.io import global_claims_root

    return global_claims_root()


def _live_worker_slot_claims(warnings: list[str]) -> int:
    """Live ``worker:<name>`` slot claims under the GLOBAL claims root."""
    try:
        from fno.claims.core import claim_status
    except Exception:
        return 0
    root = _gate_claims_root()
    claims_dir = root / ".fno" / "claims"
    if not claims_dir.is_dir():
        return 0
    n = 0
    for f in claims_dir.glob("worker%3A*.lock"):
        key = unquote(f.name[: -len(".lock")])
        try:
            state = claim_status(key, root=root).get("state")
        except Exception:
            continue
        if state in ("live", "suspect"):
            n += 1
        elif state == "corrupted":
            warnings.append(f"spawn-gate: corrupted slot claim {key} ignored")
    return n


# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------

@dataclass
class GateGuard:
    """Held gate state. The caller keeps this across dispatch and calls
    ``release()`` when the dispatch result (registry row / roster receipt)
    exists — for headless, the worker slot claim outlives the mutex."""

    _gate_holder: Optional[str] = None
    _worker_key: Optional[str] = None
    _worker_holder: Optional[str] = None

    def release_gate_mutex(self) -> None:
        if self._gate_holder is None:
            return
        holder, self._gate_holder = self._gate_holder, None
        try:
            from fno.claims.core import release_claim

            release_claim("spawn-gate", holder, root=_gate_claims_root())
        except Exception:
            pass

    def release(self) -> None:
        self.release_gate_mutex()
        if self._worker_key is None:
            return
        key, self._worker_key = self._worker_key, None
        try:
            from fno.claims.core import release_claim

            release_claim(key, self._worker_holder or "", root=_gate_claims_root())
        except Exception:
            pass


class GateRefused(SystemExit):
    """Raised (as SystemExit subclass) when the gate refuses the spawn."""


def _acquire_gate_mutex(holder: str) -> bool:
    """One attempt at the spawn-gate mutex. True = held. Errors fail open."""
    try:
        from fno.claims.core import ClaimHeldByOther, acquire_claim

        try:
            acquire_claim(
                "spawn-gate",
                holder,
                ttl_ms=GATE_CLAIM_TTL_MS,
                root=_gate_claims_root(),
            )
            return True
        except ClaimHeldByOther:
            return False
    except Exception as exc:
        _warn(f"spawn-gate: mutex unavailable ({exc}); proceeding unserialized")
        return True


def _check_ram_floor(floor_gb: float) -> None:
    """Refuse (never queue) below the floor; <= 0 disables; unreadable skips."""
    if floor_gb <= 0:
        return
    avail = available_ram_gb()
    if avail is None:
        _warn("spawn-gate: could not read available RAM; skipping the floor check")
        return
    if avail < floor_gb:
        _warn(
            f"spawn-gate: available RAM {avail:.1f}GB is below the min_free_gb "
            f"floor {floor_gb:.1f}GB; refusing to spawn (--force to bypass)"
        )
        raise GateRefused(EXIT_RAM_REFUSED)


def _acquire_worker_slot(guard: GateGuard, name: str, holder: str) -> None:
    key = f"worker:{name}"
    try:
        from fno.claims.core import acquire_claim

        acquire_claim(
            key, holder, ttl_ms=WORKER_CLAIM_TTL_MS, root=_gate_claims_root()
        )
        guard._worker_key = key
        guard._worker_holder = holder
    except Exception:
        # Fail open: a slot claim is count VISIBILITY, not a correctness gate.
        _warn(f"spawn-gate: worker slot claim {key} unavailable; proceeding uncounted")


def run_gate(
    name: str,
    substrate: str,
    *,
    force: bool = False,
    no_wait: bool = False,
) -> GateGuard:
    """Run the full gate. Returns a :class:`GateGuard` to hold across dispatch
    on pass; raises :class:`GateRefused` (a SystemExit) on refusal/timeout.
    All output goes to stderr (the stdout receipt shape is reserved)."""
    # FNO_SPAWN_GATE=0 disables the gate entirely (the FNO_THINK_SPAWN=0
    # precedent): test suites exercising spawn plumbing must not queue behind
    # the REAL machine's live workers, and it doubles as an operator escape.
    if os.environ.get("FNO_SPAWN_GATE") == "0":
        return GateGuard()
    try:
        from fno.config import load_settings

        agents_cfg = load_settings().config.agents
        cap = int(agents_cfg.max_live)
        floor_gb = float(agents_cfg.min_free_gb)
    except Exception:
        cap, floor_gb = 3, 4.0

    holder = f"spawn-gate:{os.getpid()}:{name}"
    guard = GateGuard()

    if force:
        _warn("spawn-gate: forced past cap and RAM floor (--force)")
        if substrate == "headless":
            _acquire_worker_slot(guard, name, holder)
        return guard

    started = time.monotonic()
    last_progress = started
    announced = False

    while True:
        if _acquire_gate_mutex(holder):
            guard._gate_holder = holder
            c = census()
            for w in c.warnings:
                _warn(w)
            if c.count < cap:
                try:
                    _check_ram_floor(floor_gb)
                except GateRefused:
                    guard.release()
                    raise
                if substrate == "headless":
                    _acquire_worker_slot(guard, name, holder)
                    guard.release_gate_mutex()
                # pane/bg: keep the mutex until dispatch returns (the row
                # exists by then); the caller releases via guard.release().
                return guard
            guard.release_gate_mutex()

            if no_wait:
                _warn(
                    f"spawn-gate: {c.count} live workers >= max_live {cap}; "
                    f"refusing (--no-wait). See `fno agents top`."
                )
                raise GateRefused(EXIT_NO_WAIT)
            now = time.monotonic()
            if not announced:
                _warn(
                    f"spawn queued: {c.count} live workers >= max_live {cap}; "
                    f"waiting for a free slot (--no-wait to fail fast, "
                    f"--force to bypass)"
                )
                announced = True
                last_progress = now
            elif now - last_progress >= QUEUE_PROGRESS_EVERY_S:
                _warn(
                    f"still queued: {c.count}/{cap} live, "
                    f"waited {int(now - started)}s"
                )
                last_progress = now

        if time.monotonic() - started >= QUEUE_TIMEOUT_S:
            _warn(
                f"spawn-gate: queue timeout after {int(QUEUE_TIMEOUT_S)}s at "
                f"max_live {cap}; inspect live workers with `fno agents top`, "
                f"or retry with --no-wait/--force"
            )
            raise GateRefused(EXIT_QUEUE_TIMEOUT)
        time.sleep(QUEUE_POLL_S)


# ---------------------------------------------------------------------------
# Layer 3: background QoS
# ---------------------------------------------------------------------------

def _qos_enabled() -> bool:
    try:
        from fno.config import load_settings

        return load_settings().config.agents.worker_qos != "off"
    except Exception:
        return True


def qos_wrap(argv: list[str]) -> list[str]:
    """Exec-wrap a child command at background priority when
    ``config.agents.worker_qos`` is ``utility``. Identity on ``off``.

    Absolute wrapper paths + existence check: a missing wrapper degrades to
    an unwrapped exec (fail open), never a spawn failure.
    """
    if not argv or not _qos_enabled():
        return argv
    # Don't wrap a command that won't resolve: a missing provider CLI must
    # surface as its own NotFound, not the wrapper's error.
    import shutil

    target = argv[0]
    if ("/" in target and not os.path.exists(target)) or (
        "/" not in target and shutil.which(target) is None
    ):
        return argv
    if sys.platform == "darwin" and os.path.exists("/usr/sbin/taskpolicy"):
        return ["/usr/sbin/taskpolicy", "-c", "utility", "--"] + argv
    if sys.platform.startswith("linux") and os.path.exists("/usr/bin/nice"):
        return ["/usr/bin/nice", "-n", "10"] + argv
    return argv


def qos_demote_pid(pid: int) -> None:
    """Best-effort post-hoc demotion of an already-running pid. Non-fatal."""
    if not _qos_enabled():
        return
    import subprocess

    if sys.platform == "darwin":
        cmd = ["/usr/sbin/taskpolicy", "-b", "-p", str(pid)]
    elif sys.platform.startswith("linux"):
        cmd = ["/usr/bin/renice", "10", "-p", str(pid)]
    else:
        return
    try:
        rc = subprocess.run(
            cmd, capture_output=True, timeout=10, check=False
        ).returncode
        if rc != 0:
            raise RuntimeError(f"exit {rc}")
    except Exception:
        _warn(f"spawn-gate: QoS demotion of pid {pid} failed (non-fatal)")


def qos_demote_bg_worker(claude_short_id: str, *, poll_s: float = 10.0) -> None:
    """After a ``--substrate bg`` dispatch, poll the roster briefly for the
    new worker's pid and demote it post-hoc. Bounded; one warning on miss."""
    if not claude_short_id or not _qos_enabled():
        return
    deadline = time.monotonic() + poll_s
    while True:
        try:
            raw = json.loads(_roster_path().read_text(encoding="utf-8"))
            workers = raw.get("workers", {}) if isinstance(raw, dict) else {}
            for w in workers.values():
                if not isinstance(w, dict):
                    continue
                sid = str(w.get("sessionId") or "")
                if sid.split("-")[0] == claude_short_id and isinstance(
                    w.get("pid"), int
                ):
                    qos_demote_pid(w["pid"])
                    return
        except Exception:
            pass
        if time.monotonic() >= deadline:
            _warn(
                f"spawn-gate: bg worker {claude_short_id} pid not in roster "
                f"within {int(poll_s)}s; QoS demotion skipped (non-fatal)"
            )
            return
        time.sleep(0.5)
