"""Spawn gate (x-c5cc): global concurrency cap + free-RAM floor + queue loop.

Called at the top of ``cmd_spawn`` before the substrate fan-out. Mirrors
``crates/fno-agents/src/spawn_gate.rs`` — the two gates sit on mutually
exclusive execution paths (the front door execs the binary for bg/headless;
the Rust ``pane`` arm re-execs this CLI), so every spawn passes exactly one.

The gate is READ-ONLY: the ``max_live`` slot cap counts fno registry rows
(worker provenance) and the RAM floor reads real system RAM. The claude daemon
roster feeds the ``fno agents top`` display and serves as a LIVENESS ORACLE for
fno bg rows that carry no local pid, but is never a population to count toward
the slot cap (x-bdf9 — only a row that is ALSO in the fno registry counts, so
non-work sessions never consume slots). Its only writes are its own claims
(``spawn-gate`` check→dispatch mutex,
``worker:<name>`` headless slot claims, both under the GLOBAL claims root —
the RAM budget is machine-wide). Global guards fail OPEN on read errors. The
per-provider cap is stricter: an unreadable live count refuses, never assumes
zero.
"""
from __future__ import annotations

import json
import math
import os
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, NoReturn, Optional
from urllib.parse import unquote

from fno.harness_identity import claude_transport_short_id

# Exit codes, distinct from existing dispatch codes (2, 13, 14, 15, 18, 127)
# and byte-parity with the Rust gate.
EXIT_QUEUE_TIMEOUT = 75
EXIT_NO_WAIT = 76
EXIT_RAM_REFUSED = 77
EXIT_PROVIDER_CAP = 78
EXIT_LOAD_REFUSED = 79
EXIT_KING_SHARE = 80

QUEUE_POLL_S = 2.0
QUEUE_PROGRESS_EVERY_S = 30.0
QUEUE_TIMEOUT_S = 600.0
GATE_CLAIM_TTL_MS = 5 * 60 * 1000
#: How long to tolerate an UNBROKEN run of failed mutex acquisitions before
#: proceeding unserialized. The mutex is a check->dispatch serializer, not a
#: state owner: a spawner that dies inside the critical section leaves it
#: `suspect` for the full ``GATE_CLAIM_TTL_MS``, and with no bound here EVERY
#: spawner on the machine then queues behind that corpse until its own queue
#: timeout - the gate becoming the very thing that bricks spawning, which the
#: module contract forbids. Failing open can overshoot the cap by the number of
#: racing spawners; wedging the whole mesh is strictly worse. Mirrors
#: ``spawn_gate.rs::MUTEX_WAIT_BUDGET``.
MUTEX_WAIT_BUDGET_S = 60.0
WORKER_CLAIM_TTL_MS = 4 * 60 * 60 * 1000
CLAIM_RELEASE_ATTEMPTS = 3

#: Registry statuses that can hold a live process. `idle` counts when the pid
#: is alive (an unreaped idle process still holds RAM); a reaped pid drops out
#: via the liveness check — the reaper is our slot-release mechanism.
LIVE_STATUSES = frozenset(
    {"spawning", "ready", "idle", "busy", "live", "restarting"}
)


def _warn(msg: str) -> None:
    print(msg, file=sys.stderr)


def _maybe_emit_spawn_cap_escape() -> None:
    """Auto-emit ``gate_escape{reason:spawn-cap}`` when an operator bypasses the
    gate (``FNO_SPAWN_GATE=0``) OUTSIDE a test context (x-91b5, Locked Decision
    2). Fully fail-open: any error is swallowed so a spawn is NEVER blocked by
    telemetry (AC1-FR).

    The test-context guard is load-bearing: ``FNO_SPAWN_GATE=0`` also disables
    the gate for the test suite, so without it every CI run would count as an
    escape and the metric would read pure noise (AC1-EDGE). The Rust gate emits
    the same event on its own bypass path; a shared fixture enforces the two
    guards agree (AC2-FR)."""
    try:
        from fno.events.gate_escape import (
            default_dedup_key,
            emit_gate_escape,
            should_emit_spawn_cap,
        )

        if not should_emit_spawn_cap():
            return
        emit_gate_escape(
            "spawn-cap",
            dedup_key=default_dedup_key("spawn-cap"),
            detail="FNO_SPAWN_GATE=0 operator bypass",
        )
    except Exception:
        pass  # ponytail: telemetry must never block a spawn (AC1-FR)


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

def _process_start_time(pid: int, _psutil=None) -> Optional[int]:
    """Return the process-incarnation token in the Rust registry's units."""
    if sys.platform.startswith("linux"):
        try:
            stat = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
            return int(stat.rsplit(")", 1)[1].split()[19])
        except (OSError, ValueError, IndexError):
            return None
    if sys.platform == "darwin":
        try:
            if _psutil is None:
                import psutil as _psutil
            return int(round(_psutil.Process(pid).create_time() * 1_000_000))
        except Exception:
            return None
    return None


def _pid_alive(
    pid: Optional[int], recorded_start: Optional[int], *, _psutil=None
) -> Optional[bool]:
    """Return process liveness, or ``None`` when incarnation proof is unreadable.

    A recorded process-start token makes PID reuse fail closed. Legacy rows
    without a token retain the existence check and rely on their hosting
    substrate for the additional incarnation proof.
    """
    # Reject an out-of-range pid outright. Beyond being absurd, it is the value
    # that turns a signal into a broadcast once it reaches a signed pid_t (the
    # Rust probe's twin guard): 4294967295 wraps to -1, "every process I may
    # signal". Nothing downstream should get the chance.
    if not pid or pid <= 1 or pid > 0x7FFFFFFF:
        return False
    try:
        if _psutil is None:
            import psutil as _psutil

        proc = _psutil.Process(pid)
        if not proc.is_running() or proc.status() == _psutil.STATUS_ZOMBIE:
            return False
        current_start = _process_start_time(pid, _psutil)
        if recorded_start is not None:
            if current_start is None:
                return None
            return current_start == recorded_start
        return True
    except Exception as exc:
        # "Gone" is the ONLY confident death. Anything else - psutil missing,
        # AccessDenied on a process this uid cannot inspect - is unreadable, and
        # returning False there would present an undecidable case to callers as a
        # decided death, which reconcile writes through as `orphaned`.
        if _psutil is not None and isinstance(exc, _psutil.NoSuchProcess):
            return False
        try:
            import psutil

            if isinstance(exc, psutil.NoSuchProcess):
                return False
        except Exception:  # noqa: BLE001 -- no psutil means no verdict, not death
            pass
        return None


def _roster_path() -> Path:
    override = os.environ.get("FNO_CLAUDE_DAEMON_DIR")
    base = Path(override) if override else Path.home() / ".claude" / "daemon"
    return base / "roster.json"


@dataclass
class LiveWorker:
    """One live process row, shared by the gate count and ``fno agents top``."""

    source: Literal["fno", "claude"]
    name: str
    # The CLI the worker runs under, never the model vendor. Named `provider`
    # once, which made a claude-hosted worker on a z.ai route read as running on
    # claude; `fno agents list` had the identical alias and it drove a wrong
    # diagnosis. Only reader is `fno agents top`.
    harness: str
    substrate: str
    pid: Optional[int]
    status: str
    #: The full session uuid, so a display row can be joined back to its registry
    #: handle. Without it the two views name the SAME session differently -- this
    #: row is labelled with the FIRST 8 hex of the uuid while the registry handle
    #: is the LAST 8 -- and an operator comparing them by eye finds no overlap.
    #: That is how a census once got read as "all agents are dead".
    session_id: Optional[str] = None
    #: The pid of the process that IS the session (x-3f84 W2), resolved through
    #: the claude bg rendezvous sockets when the row is a bg session. ``pid``
    #: above keeps the RECORDED pid, which for a bg row names the PTY HOST;
    #: cost readers (``agents top``, the process-cost gate) must use this one.
    session_pid: Optional[int] = None
    #: The session id of the KING that spawned this worker (x-3f84 W4): the
    #: row's ``spawned_by_session``, None for an operator-run or legacy row.
    #: Cost is attributed through this field so a shared ceiling can be
    #: divided without minting a second budget record.
    spawned_by: Optional[str] = None


@dataclass
class LiveCensus:
    workers: list[LiveWorker] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    #: live worker:<name> slot claims (headless one-shots, no process row yet)
    slot_claims: int = 0
    #: live fno registry work rows, counted straight from the registry
    #: (dedup-independent) so the slot cap mirrors the Rust gate exactly — see
    #: :attr:`slot_count`.
    fno_slot_workers: int = 0
    #: live slot-holding rows per king session id (x-3f84 W4). None keys the
    #: rows no king is attributable to (operator-run / pre-lineage rows): they
    #: still consume ``max_live`` but never divide the share, because an
    #: unknown lineage must not shrink everyone else's share to hide in it.
    king_counts: dict[Optional[str], int] = field(default_factory=dict)

    @property
    def count(self) -> int:
        """The full union size (fno rows + roster sessions + slot claims). The
        RAM-ground-truth / ``fno agents top`` display number — NOT the slot cap
        denominator."""
        return len(self.workers) + self.slot_claims

    @property
    def slot_count(self) -> int:
        """Worker SLOTS in use for the ``max_live`` cap (x-bdf9): live fno
        registry rows + headless slot claims. Counted straight from the
        registry, NOT by filtering the display union — a bg/adopted fno agents worker
        is display-deduped into its roster row (``source == "claude"``) but is
        still fno work and must hold a slot, exactly as the Rust gate counts it.
        The claude roster's non-work sessions (claude-mem observers, resident
        idle) never enter this count; their RAM cost stays honored by the
        separate ``min_free_gb`` floor."""
        return self.fno_slot_workers + self.slot_claims


def census() -> LiveCensus:
    """The full union: fno registry ∪ claude roster (deduped by claude session
    short_id) + live ``worker:<name>`` slot claims. This is the display /
    RAM-ground-truth view (``fno agents top`` renders every row). The spawn
    gate's ``max_live`` decision uses :attr:`LiveCensus.slot_count`, which
    counts fno-sourced rows only — the roster is kept here for visibility but
    does NOT consume worker slots (x-bdf9). Read-only; every source failure
    degrades to zero contribution with one warning."""
    out = LiveCensus()
    counted_short_ids: set[str] = set()
    live_registry_names: set[str] = set()

    # One socket-farm read per census (x-3f84 W2): every claude row below joins
    # through this map so no consumer re-runs lsof, and the recorded pid stays
    # on the row beside the resolved one. An empty map is "unknown", never
    # "no bg sessions" - rows then keep their recorded (host) pid.
    from fno.agents.session_procs import bg_socket_pid_map, resolve_session_pid

    try:
        sock_map = bg_socket_pid_map()
    except Exception:  # noqa: BLE001 - a broken join must not break the census
        sock_map = {}

    # claude roster first: display + dedup key for adopted sessions. Kept in the
    # union for `fno agents top`, but excluded from the slot cap (see slot_count).
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
            short_id = claude_transport_short_id(session_id)
            counted_short_ids.add(short_id)
            out.workers.append(
                LiveWorker(
                    source="claude",
                    name=short_id,
                    harness="claude",
                    substrate="(foreign)",
                    pid=pid,
                    status="live",
                    session_id=session_id,
                    session_pid=resolve_session_pid(
                        harness="claude",
                        short_id=short_id,
                        pid=pid,
                        socket_map=sock_map,
                    ),
                )
            )

    # Snapshot the LIVE roster short_ids before the registry loop mutates
    # counted_short_ids. This is the liveness oracle for fno bg rows that carry
    # no local pid (their process is the claude daemon's), NOT a population to
    # count — only a row that is ALSO in the fno registry is ever counted.
    roster_live_short_ids = set(counted_short_ids)

    # fno registry rows: every live one holds a worker slot; the roster only
    # decides whether to add a DUPLICATE display row for a bg/adopted worker.
    try:
        from fno.agents.registry import load_registry

        rows = load_registry()
    except Exception as exc:
        out.warnings.append(
            f"spawn-gate: fno registry unreadable ({exc}); registry rows omitted from the census"
        )
        rows = []
    for row in rows:
        if row.status not in LIVE_STATUSES:
            continue
        pid_state = _pid_alive(row.pid, row.pid_start_time)
        if pid_state is None:
            out.warnings.append(
                f"spawn-gate: process incarnation unreadable for {row.name}; "
                "counting the live registry row conservatively"
            )
        pid_alive = pid_state is not False
        # A fno `claude --bg` row is minted with a jobId in short_id but no local
        # pid (liveness lives in the claude daemon roster). Resolve it via the
        # roster so real fno bg workers hold slots — a pid-only filter would drop
        # them and let the cap admit unbounded bg workers (Codex P1, PR #235).
        # Still no non-fno session counted: a claude-mem observer has no
        # registry row and never reaches here. v9 unified the jobId into short_id;
        # a bg row is discriminated from a daemon PTY worker by pid==None +
        # provider claude + roster membership (a worker's name-derived short is
        # never in the claude roster, so the guard is self-limiting).
        bg_alive = (
            pid_state is False
            and row.pid is None
            and row.harness == "claude"
            and bool(row.short_id)
            and row.short_id in roster_live_short_ids
        )
        if not (pid_alive or bg_alive):
            continue
        # A live fno row is fno work: it holds a slot regardless of the display
        # dedup below (x-bdf9 — a bg/adopted worker also appears in the roster,
        # but its registry row is the slot, matching the registry-only Rust gate).
        out.fno_slot_workers += 1
        out.king_counts[row.spawned_by_session] = (
            out.king_counts.get(row.spawned_by_session, 0) + 1
        )
        live_registry_names.add(row.name)
        dedup_key = row.short_id or None
        if dedup_key and dedup_key in counted_short_ids:
            # Already shown as its roster row in the display union. That roster
            # row carries no lineage of its own, so the KING the fno row
            # attributes this cost to rides onto it here - without the backfill
            # the one view built to show ownership names '-' for exactly the
            # rows the king-share gate counts (review finding, x-3f84).
            for shown in out.workers:
                if shown.source == "claude" and shown.name == dedup_key:
                    shown.spawned_by = row.spawned_by_session
                    break
            continue
        if dedup_key:
            counted_short_ids.add(dedup_key)
        substrate = "pane" if getattr(row, "mux", None) else (
            "bg" if bg_alive else "worker"
        )
        out.workers.append(
            LiveWorker(
                source="fno",
                name=row.name,
                harness=row.harness,
                substrate=substrate,
                pid=row.pid,
                status=str(row.status),
                session_id=row.harness_session_id,
                spawned_by=row.spawned_by_session,
                session_pid=resolve_session_pid(
                    harness=row.harness,
                    short_id=row.short_id,
                    session_id=row.harness_session_id,
                    pid=row.pid,
                    socket_map=sock_map,
                ),
            )
        )

    out.slot_claims = _live_worker_slot_claims(out.warnings, live_registry_names)
    return out


class ProviderCountUnavailable(RuntimeError):
    """A provider count cannot be proved from registry/liveness evidence."""


_KNOWN_UNROUTED_PROVIDER = "__uncapped__"
_PROVIDER_ADMISSION_TOKEN = object()


def _provider_roster_live_short_ids(short_ids: set[str]) -> set[str]:
    """Return requested bg ids with a positive live roster marker."""
    try:
        raw = json.loads(_roster_path().read_text(encoding="utf-8"))
    except Exception as exc:
        raise ProviderCountUnavailable(f"claude roster unreadable: {exc}") from exc
    workers = raw.get("workers") if isinstance(raw, dict) else None
    if not isinstance(workers, dict):
        raise ProviderCountUnavailable("claude roster has no workers object")

    live: set[str] = set()
    undecidable: set[str] = set()
    for worker in workers.values():
        if not isinstance(worker, dict):
            continue
        session_id = worker.get("sessionId")
        if not isinstance(session_id, str) or not session_id:
            continue
        short_id = claude_transport_short_id(session_id)
        if short_id not in short_ids:
            continue
        pid = worker.get("pid") if isinstance(worker.get("pid"), int) else None
        state = _pid_alive(pid, None)
        if state is True:
            live.add(short_id)
        elif state is None:
            undecidable.add(short_id)
    unknown = undecidable - live
    if unknown:
        raise ProviderCountUnavailable(
            f"process incarnation unreadable for bg worker(s) {sorted(unknown)}"
        )
    return live


def provider_live_count(provider: str) -> int:
    """Count provider rows only when status and positive liveness agree."""
    try:
        from fno.agents.registry import load_registry

        loaded = load_registry()
        if getattr(loaded, "complete", True) is not True:
            raise ProviderCountUnavailable(
                "registry forward read skipped rows; provider count is incomplete"
            )
        live_rows = [row for row in loaded if row.status in LIVE_STATUSES]
        unattributed: dict[tuple[str, str], int] = {}
        for row in live_rows:
            if row.provider:
                continue
            shape = (row.harness or "unknown", row.origin or "unknown")
            unattributed[shape] = unattributed.get(shape, 0) + 1
        for (harness, origin), count in sorted(unattributed.items()):
            _warn(
                f"{count} live row(s) were minted without a provider stamp "
                f"(harness={harness}, origin={origin})"
            )
        candidates = [row for row in live_rows if row.provider == provider]
    except Exception as exc:
        raise ProviderCountUnavailable(f"fno registry unreadable: {exc}") from exc

    bg_short_ids = {
        row.short_id
        for row in candidates
        if row.pid is None
        and row.harness == "claude"
        and bool(row.short_id)
    }
    bg_live = (
        _provider_roster_live_short_ids(bg_short_ids) if bg_short_ids else set()
    )

    count = 0
    counted_names: set[str] = set()
    for row in candidates:
        if row.pid is not None:
            if row.pid_start_time is None:
                state = _pid_alive(row.pid, None)
                if state is False:
                    continue
                if isinstance(row.mux, dict):
                    try:
                        from fno.agents.mux_spawn import _mux_pane_alive

                        pane_state = _mux_pane_alive(row.mux)
                    except Exception as exc:
                        raise ProviderCountUnavailable(
                            f"pane liveness unreadable for {row.name}: {exc}"
                        ) from exc
                    if pane_state is True:
                        count += 1
                        counted_names.add(row.name)
                        continue
                    if pane_state is False:
                        continue
                raise ProviderCountUnavailable(
                    f"process incarnation token missing for {row.name}"
                )
            state = _pid_alive(row.pid, row.pid_start_time)
            if state is None:
                raise ProviderCountUnavailable(
                    f"process incarnation unreadable for {row.name}"
                )
            if state is True:
                count += 1
                counted_names.add(row.name)
            continue
        if isinstance(row.mux, dict):
            try:
                from fno.agents.mux_spawn import _mux_pane_alive

                pane_state = _mux_pane_alive(row.mux)
            except Exception as exc:
                raise ProviderCountUnavailable(
                    f"pane liveness unreadable for {row.name}: {exc}"
                ) from exc
            if pane_state is True:
                count += 1
                counted_names.add(row.name)
                continue
            if pane_state is False:
                continue
            raise ProviderCountUnavailable(
                f"pane liveness unreadable for {row.name}"
            )
        if row.short_id and row.short_id in bg_live:
            count += 1
            counted_names.add(row.name)
    return count + _provider_live_slot_claims(provider, counted_names)


def _provider_live_slot_claims(provider: str, counted_names: set[str]) -> int:
    """Count provider-tagged headless reservations not represented by rows."""
    try:
        from fno.claims.core import claim_status

        root = _gate_claims_root()
        claims_dir = root / ".fno" / "claims"
        if not claims_dir.exists():
            return 0
        paths = list(claims_dir.glob("worker%3A*.lock"))
    except Exception as exc:
        raise ProviderCountUnavailable(
            f"worker reservations unreadable: {exc}"
        ) from exc

    count = 0
    for path in paths:
        key = unquote(path.name[: -len(".lock")])
        name = key.removeprefix("worker:")
        if name in counted_names:
            continue
        try:
            status = claim_status(key, root=root)
        except Exception as exc:
            raise ProviderCountUnavailable(
                f"worker reservation {key} unreadable: {exc}"
            ) from exc
        state = status.get("state")
        if state in ("free", "stale"):
            continue
        if state == "corrupted":
            raise ProviderCountUnavailable(
                f"worker reservation {key} is corrupted"
            )
        metadata = status.get("metadata")
        model_provider = (
            metadata.get("model_provider") if isinstance(metadata, dict) else None
        )
        if not isinstance(model_provider, str) or not model_provider:
            _warn(
                f"live worker reservation {key} was minted without "
                "model_provider; skipping"
            )
            continue
        if model_provider == _KNOWN_UNROUTED_PROVIDER:
            continue
        if model_provider != provider:
            continue
        if state == "suspect":
            raise ProviderCountUnavailable(
                f"worker reservation {key} liveness is suspect"
            )
        if state == "live":
            count += 1
    return count


def _gate_claims_root() -> Path:
    from fno.claims.io import global_claims_root

    return global_claims_root()


def _live_worker_slot_claims(
    warnings: list[str], counted_names: Optional[set[str]] = None
) -> int:
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
    counted_names = counted_names or set()
    for f in claims_dir.glob("worker%3A*.lock"):
        key = unquote(f.name[: -len(".lock")])
        if key.removeprefix("worker:") in counted_names:
            continue
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
    _route_provider: Optional[str] = None
    _spawn_name: Optional[str] = None
    _substrate: Optional[str] = None
    _admission_token: object | None = None
    _consumed: bool = False
    _released: bool = False

    def _consume_provider(self, provider: str, name: str, substrate: str) -> bool:
        authorized = (
            self._admission_token is _PROVIDER_ADMISSION_TOKEN
            and self._route_provider == provider
            and self._spawn_name == name
            and self._substrate == substrate
            and not self._consumed
            and not self._released
        )
        if authorized:
            self._consumed = True
        return authorized

    def retain_revived_worker(
        self,
        short_id: str,
        *,
        worker_name: Optional[str] = None,
        worker_pid: Optional[int] = None,
        positive_marker: str = "claude-respawn-ok",
    ) -> None:
        """Convert a BG admission into durable fail-closed worker evidence."""
        if self._route_provider is None or self._spawn_name is None:
            raise ProviderCountUnavailable("provider admission identity unavailable")
        holder = self._gate_holder or f"spawn-gate:{os.getpid()}:{self._spawn_name}"
        _acquire_worker_slot(
            self,
            worker_name or self._spawn_name,
            holder,
            self._route_provider,
            fail_closed=True,
            worker_pid=worker_pid,
            metadata={
                "session_short_id": short_id,
                "positive_marker": positive_marker,
            },
        )

    def release_worker_reservation(self) -> None:
        if self._worker_key is None:
            return
        key = self._worker_key
        if not _release_claim_bounded(key, self._worker_holder or ""):
            return
        self._worker_key = None
        self._worker_holder = None

    def release_gate_mutex(self) -> None:
        if self._gate_holder is None:
            return
        holder = self._gate_holder
        if not _release_claim_bounded("spawn-gate", holder):
            return
        self._gate_holder = None

    def release(self) -> None:
        self._released = True
        self.release_gate_mutex()
        self.release_worker_reservation()


def consume_provider_admission(
    guard: object, provider: str, name: str, substrate: str
) -> bool:
    """Consume one genuine opaque admission; duck-typed substitutes never pass."""
    return isinstance(guard, GateGuard) and guard._consume_provider(
        provider, name, substrate
    )


def _release_claim_bounded(key: str, holder: str) -> bool:
    """Release a gate claim with a short retry budget for transient store faults."""
    from fno.claims.core import release_claim

    last_error: Exception | None = None
    for attempt in range(CLAIM_RELEASE_ATTEMPTS):
        try:
            release_claim(key, holder, root=_gate_claims_root())
            return True
        except Exception as exc:
            last_error = exc
            if attempt + 1 < CLAIM_RELEASE_ATTEMPTS:
                time.sleep(0.01)
    label = "gate mutex" if key == "spawn-gate" else f"worker reservation {key}"
    _warn(f"spawn-gate: could not release {label}: {last_error}")
    return False


class GateRefused(SystemExit):
    """Raised (as SystemExit subclass) when the gate refuses the spawn."""

    def __init__(self, code: int, receipt: Optional[dict[str, object]] = None) -> None:
        super().__init__(code)
        self.receipt = receipt


def _acquire_gate_mutex(holder: str, *, fail_closed: bool = False) -> bool:
    """One attempt at the spawn-gate mutex. True = held. Errors fail open."""
    try:
        from fno.claims.core import CLAIM_UNAVAILABLE, acquire_claim

        try:
            acquire_claim(
                "spawn-gate",
                holder,
                ttl_ms=GATE_CLAIM_TTL_MS,
                root=_gate_claims_root(),
            )
            return True
        except CLAIM_UNAVAILABLE:
            # Must not fall to the outer except below, which proceeds
            # UNSERIALIZED on a claims-layer fault. Contention is the
            # opposite: someone else has it, so this attempt fails closed.
            return False
    except Exception as exc:
        if fail_closed:
            raise ProviderCountUnavailable(f"spawn mutex unavailable: {exc}") from exc
        _warn(f"spawn-gate: mutex unavailable ({exc}); proceeding unserialized")
        return True


def provider_lanes_cap(budget: object) -> Optional[int]:
    """The `lanes` dimension of one provider budget, whichever spelling arrived.

    `config.agents.provider_limits.<provider>` is a :class:`~fno.config.ProviderBudget`
    record since x-c703, and was a bare integer before it. Both reach this seam:
    the configured table carries the record, and the fail-safe fallback below
    carries the integer. Reading them through one function is what keeps the two
    paths from disagreeing about a cap.

    Returns None for "no lane cap", which is what an unlisted provider and an
    unreadable budget both mean here.
    """
    if isinstance(budget, bool) or budget is None:
        return None
    if isinstance(budget, int):
        return budget if budget >= 1 else None
    lanes = getattr(budget, "lanes", None)
    return lanes if isinstance(lanes, int) and lanes >= 1 else None


def _refuse_provider_cap(
    provider: str,
    cap: int,
    *,
    current: Optional[int] = None,
    error: Optional[BaseException] = None,
) -> NoReturn:
    current_text = str(current) if current is not None else "unavailable"
    detail = f" ({error})" if error is not None else ""
    _warn(
        f"spawn-gate: provider {provider}, cap {cap}, current count "
        f"{current_text}{detail}; refusing immediately; no worker launched"
    )
    receipt = {
        "status": "refused",
        "reason": "provider_cap",
        "provider": provider,
        "cap": cap,
        "count": current,
        "current_count": current,
    }
    raise GateRefused(EXIT_PROVIDER_CAP, receipt)


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
        receipt = {
            "status": "refused",
            "reason": "ram_floor",
            "available_gb": avail,
            "min_free_gb": floor_gb,
        }
        raise GateRefused(EXIT_RAM_REFUSED, receipt)


def _footprint_cause_evidence() -> Optional[str]:
    """Read one fail-open fleet footprint for an over-load refusal."""
    try:
        from fno.doctor_footprint import _cpu_capacity_cores, cause_reading

        reading, _error = cause_reading()
        if reading is None:
            return None
        capacity = float(_cpu_capacity_cores())
        measured_share = (
            reading.fleet_cpu_cores / reading.measured_cpu_cores * 100
            if reading.measured_cpu_cores > 0
            else 0.0
        )
        capacity_share = reading.fleet_cpu_cores / capacity * 100
        values = (
            reading.fleet_cpu_cores,
            capacity,
            capacity_share,
            measured_share,
        )
        if capacity <= 0 or any(not math.isfinite(value) or value < 0 for value in values):
            return None
        return (
            "spawn-gate: footprint attributes "
            f"{reading.fleet_cpu_cores:.2f}/{capacity:.2f} cores "
            f"({capacity_share:.1f}% capacity, {measured_share:.1f}% of measured CPU) "
            "to the fleet"
        )
    except Exception:
        return None


def _check_load_ceiling(max_load_per_cpu: float) -> None:
    """Refuse (never queue) above the CPU ceiling (x-3f84 W3).

    The ceiling is `max_load_per_cpu x cpu count` on the 1-min loadavg, so one
    number ports across machines without an edit. Measured motivation: load 309
    on 12 CPUs while the RAM floor held ten times its margin - the one machine
    guard was reading the one resource that was never scarce. Same contract as
    :func:`_check_ram_floor`: <= 0 disables, unreadable skips, refuse never
    queues.
    """
    if max_load_per_cpu <= 0:
        return
    try:
        load1 = os.getloadavg()[0]
    except (OSError, AttributeError):
        # OSError: unreadable. AttributeError: the platform has no getloadavg
        # at all (the Rust gate cfg-guards the same case).
        _warn("spawn-gate: could not read load average; skipping the load check")
        return
    # Affinity/cgroup-aware where available (mirrors the Rust gate's
    # available_parallelism, so the two gates compute the same ceiling on a
    # constrained host instead of a 4x disagreement).
    cpus = getattr(os, "process_cpu_count", os.cpu_count)() or 1
    ceiling = max_load_per_cpu * cpus
    if load1 > ceiling:
        _warn(
            f"spawn-gate: 1-min load {load1:.1f} exceeds max_load_per_cpu "
            f"{max_load_per_cpu:g} x {cpus} cpus = {ceiling:.1f}; refusing to "
            f"spawn (--force to bypass)"
        )
        # No evidence probe here: it costs seconds of ps/lsof, and this check
        # runs inside the held gate mutex. The caller releases first, then
        # gathers (see run_gate).
        raise GateRefused(EXIT_LOAD_REFUSED)


def _king_share(cap: int, king_counts: dict[Optional[str], int], caller: str) -> int:
    """One king's fair share of the ceiling: a DIVISOR, never a second record.

    ``max_live`` stays the one ceiling; the share is ``cap // kings`` where
    kings are the distinct attributed spawners among live rows, plus the
    caller (a king spawning its FIRST worker still counts, or N kings with
    live rows would admit an unbounded N+1th). Unattributed rows (None) never
    divide the share - an unknown lineage must not shrink everyone else's
    share to hide inside it. The floor of 1 keeps a crowded fleet able to
    start one worker per king.
    """
    kings = {k for k in king_counts if k is not None}
    kings.add(caller)
    return max(1, cap // len(kings))


def _check_king_share(
    census_obj: "LiveCensus", cap: int, *, caller_session: Optional[str]
) -> None:
    """Refuse (never queue) when the calling king holds its full share (x-3f84 W4).

    Six kings dispatching into one undivided ``max_live`` converge on the cap
    by construction, however reasonable each king is alone. The share divides
    THAT ceiling, and only for a caller whose session identity resolved: an
    operator terminal or cron job has no lineage and is not competing for the
    commons, so an unattributed caller skips the check. Waiting cannot help -
    only the caller's own workers dying frees its share - so this refuses like
    the provider cap rather than queueing.
    """
    if not caller_session:
        return
    share = _king_share(cap, census_obj.king_counts, caller_session)
    held = census_obj.king_counts.get(caller_session, 0)
    if held >= share:
        kings = len({k for k in census_obj.king_counts if k is not None} | {caller_session})
        _warn(
            f"spawn-gate: king {caller_session[:8]} holds {held} of max_live {cap} "
            f"across {kings} kings (share {share}); refusing to spawn -- waiting "
            f"cannot help while your own workers hold the share (--force to bypass)"
        )
        raise GateRefused(EXIT_KING_SHARE)


def _acquire_worker_slot(
    guard: GateGuard,
    name: str,
    holder: str,
    route_provider: Optional[str] = None,
    *,
    fail_closed: bool = False,
    worker_pid: Optional[int] = None,
    metadata: Optional[dict[str, object]] = None,
) -> None:
    key = f"worker:{name}"
    try:
        from fno.claims.core import acquire_claim

        claim_metadata: dict[str, object] = {
            "model_provider": route_provider or _KNOWN_UNROUTED_PROVIDER
        }
        if metadata:
            claim_metadata.update(metadata)
        acquire_claim(
            key,
            holder,
            ttl_ms=WORKER_CLAIM_TTL_MS,
            metadata=claim_metadata,
            pid=worker_pid,
            root=_gate_claims_root(),
        )
        guard._worker_key = key
        guard._worker_holder = holder
    except Exception as exc:
        if fail_closed:
            raise ProviderCountUnavailable(
                f"worker reservation {key} unavailable: {exc}"
            ) from exc
        # Fail open: a slot claim is count VISIBILITY, not a correctness gate.
        _warn(f"spawn-gate: worker slot claim {key} unavailable; proceeding uncounted")


def run_gate(
    name: str,
    substrate: str,
    *,
    force: bool = False,
    no_wait: bool = False,
    route_provider: Optional[str] = None,
) -> GateGuard:
    """Run the full gate. Returns a :class:`GateGuard` to hold across dispatch
    on pass; raises :class:`GateRefused` (a SystemExit) on refusal/timeout.
    All output goes to stderr (the stdout receipt shape is reserved)."""
    # FNO_SPAWN_GATE=0 disables the gate entirely (the FNO_THINK_SPAWN=0
    # precedent): test suites exercising spawn plumbing must not queue behind
    # the REAL machine's live workers, and it doubles as an operator escape.
    if os.environ.get("FNO_SPAWN_GATE") == "0":
        _maybe_emit_spawn_cap_escape()
        return GateGuard(
            _route_provider=route_provider,
            _spawn_name=name,
            _substrate=substrate,
            _admission_token=_PROVIDER_ADMISSION_TOKEN,
        )
    try:
        from fno.config import load_settings

        agents_cfg = load_settings().agents
        cap = int(agents_cfg.max_live)
        floor_gb = float(agents_cfg.min_free_gb)
        max_load_per_cpu = float(agents_cfg.max_load_per_cpu)
        # A real attribute read, not a getattr fallback: a missing field must
        # fail loudly here rather than silently uncapping every provider.
        limits = dict(agents_cfg.provider_limits)
    except Exception:
        cap, floor_gb, max_load_per_cpu = 3, 4.0, 8.0
        # The same budget the built-in table carries, coerced through the same
        # model so this fail-safe path cannot disagree with the configured one
        # about zai's caps.
        from fno.config import ProviderBudget, _BUILTIN_PROVIDER_BUDGETS

        limits = {
            k: ProviderBudget(**v) for k, v in _BUILTIN_PROVIDER_BUDGETS.items()
        }

    provider_cap = (
        provider_lanes_cap(limits.get(route_provider))
        if route_provider is not None
        else None
    )

    # The calling king's session id (x-3f84 W4), resolved through the same
    # self-identity source that stamps `spawned_by_session` onto the spawned
    # row, so the gate attributes a spawn exactly the way the row will.
    try:
        from fno.claims.self_identity import resolve_self_identity

        caller_session = resolve_self_identity().session_id
    except Exception:  # noqa: BLE001 - no identity, no share check (an
        # operator-run spawn is not competing for the commons)
        caller_session = None

    holder = f"spawn-gate:{os.getpid()}:{name}"
    guard = GateGuard(
        _route_provider=route_provider,
        _spawn_name=name,
        _substrate=substrate,
        _admission_token=_PROVIDER_ADMISSION_TOKEN,
    )

    if force and provider_cap is None:
        # Byte-twin with the Rust gate (check-reachable-paths); force also
        # bypasses the king share here, which _check_king_share's own refusal
        # names where it matters.
        _warn("spawn-gate: forced past cap, RAM floor, and load ceiling (--force)")
        if substrate == "headless":
            _acquire_worker_slot(guard, name, holder, route_provider)
        return guard

    started = time.monotonic()
    last_progress = started
    announced = False
    slots: int = 0
    #: start of the current UNBROKEN run of failed acquisitions (None = holding
    #: or not yet contended). Reset on every success so a long legitimate queue
    #: never accumulates into a spurious fail-open.
    mutex_blocked_since: Optional[float] = None

    while True:
        try:
            acquired = (
                _acquire_gate_mutex(holder, fail_closed=True)
                if provider_cap is not None
                else _acquire_gate_mutex(holder)
            )
        except ProviderCountUnavailable as exc:
            _refuse_provider_cap(route_provider or "unknown", provider_cap or 0, error=exc)
        if acquired:
            mutex_blocked_since = None
        else:
            if provider_cap is not None:
                _refuse_provider_cap(
                    route_provider or "unknown",
                    provider_cap,
                    error=ProviderCountUnavailable(
                        "spawn mutex is busy; current count cannot be serialized"
                    ),
                )
            now = time.monotonic()
            if mutex_blocked_since is None:
                mutex_blocked_since = now
            # --no-wait means "do not queue", and a busy mutex is queueing.
            # Refusing here (rather than falling through to the sleep) is what
            # keeps the promise: without it the caller waits the full
            # QUEUE_TIMEOUT_S and then gets EXIT_QUEUE_TIMEOUT, so it cannot
            # even tell "cap is full" from "the gate is wedged".
            if no_wait:
                _warn(
                    "spawn-gate: another spawner holds the gate mutex; refusing "
                    "(--no-wait). See `fno agents top`."
                )
                receipt = {
                    "status": "refused",
                    "reason": "no_wait_mutex_held",
                    "max_live": cap,
                }
                raise GateRefused(EXIT_NO_WAIT, receipt)
            if now - mutex_blocked_since >= MUTEX_WAIT_BUDGET_S:
                _warn(
                    f"spawn-gate: gate mutex still held after "
                    f"{int(MUTEX_WAIT_BUDGET_S)}s (holder likely died mid-gate); "
                    f"proceeding unserialized"
                )
                acquired = True
        if acquired:
            guard._gate_holder = holder
            if provider_cap is not None:
                try:
                    provider_slots = provider_live_count(route_provider or "")
                except ProviderCountUnavailable as exc:
                    guard.release_gate_mutex()
                    _refuse_provider_cap(
                        route_provider or "unknown", provider_cap, error=exc
                    )
                if provider_slots >= provider_cap:
                    guard.release_gate_mutex()
                    _refuse_provider_cap(
                        route_provider or "unknown",
                        provider_cap,
                        current=provider_slots,
                    )
            if force:
                _warn(
                    "spawn-gate: forced past cap, RAM floor, and load ceiling "
                    "(--force); provider cap remains enforced"
                )
                if substrate == "headless":
                    try:
                        _acquire_worker_slot(
                            guard,
                            name,
                            holder,
                            route_provider,
                            fail_closed=provider_cap is not None,
                        )
                    except ProviderCountUnavailable as exc:
                        guard.release()
                        _refuse_provider_cap(
                            route_provider or "unknown", provider_cap or 0, error=exc
                        )
                    guard.release_gate_mutex()
                return guard
            c = census()
            for w in c.warnings:
                _warn(w)
            slots = c.slot_count
            if slots < cap:
                try:
                    _check_ram_floor(floor_gb)
                except GateRefused:
                    guard.release()
                    raise
                try:
                    _check_load_ceiling(max_load_per_cpu)
                except GateRefused:
                    # The refusal is decided; release the mutex BEFORE the
                    # cause probe so queued spawners (and --no-wait callers)
                    # never sit behind seconds of evidence gathering.
                    guard.release()
                    _warn(
                        _footprint_cause_evidence()
                        or "spawn-gate: footprint cause unavailable; load refusal unchanged"
                    )
                    raise
                try:
                    _check_king_share(c, cap, caller_session=caller_session)
                except GateRefused:
                    guard.release()
                    raise
                if substrate == "headless":
                    try:
                        _acquire_worker_slot(
                            guard,
                            name,
                            holder,
                            route_provider,
                            fail_closed=provider_cap is not None,
                        )
                    except ProviderCountUnavailable as exc:
                        guard.release()
                        _refuse_provider_cap(
                            route_provider or "unknown", provider_cap or 0, error=exc
                        )
                    guard.release_gate_mutex()
                # pane/bg: keep the mutex until dispatch returns (the row
                # exists by then); the caller releases via guard.release().
                return guard
            guard.release_gate_mutex()

            if no_wait:
                _warn(
                    f"spawn-gate: {slots} live worker slots >= max_live {cap}; "
                    f"refusing (--no-wait). See `fno agents top`."
                )
                receipt = {
                    "status": "refused",
                    "reason": "no_wait",
                    "max_live": cap,
                    "count": slots,
                    "current_count": slots,
                }
                raise GateRefused(EXIT_NO_WAIT, receipt)
            now = time.monotonic()
            if not announced:
                _warn(
                    f"spawn queued: {slots} live worker slots >= max_live {cap}; "
                    f"waiting for a free slot (--no-wait to fail fast, "
                    f"--force to bypass)"
                )
                announced = True
                last_progress = now
            elif now - last_progress >= QUEUE_PROGRESS_EVERY_S:
                _warn(
                    f"still queued: {slots}/{cap} live worker slots, "
                    f"waited {int(now - started)}s"
                )
                last_progress = now

        if time.monotonic() - started >= QUEUE_TIMEOUT_S:
            _warn(
                f"spawn-gate: queue timeout after {int(QUEUE_TIMEOUT_S)}s at "
                f"max_live {cap}; inspect live workers with `fno agents top`, "
                f"or retry with --no-wait/--force"
            )
            receipt = {
                "status": "refused",
                "reason": "queue_timeout",
                "max_live": cap,
                "count": slots,
                "current_count": slots,
            }
            raise GateRefused(EXIT_QUEUE_TIMEOUT, receipt)
        time.sleep(QUEUE_POLL_S)


# ---------------------------------------------------------------------------
# Layer 3: background QoS
# ---------------------------------------------------------------------------

def _qos_enabled() -> bool:
    try:
        from fno.config import load_settings

        return load_settings().agents.worker_qos != "off"
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


def qos_demote_bg_worker(job_id: str, *, poll_s: float = 10.0) -> None:
    """After a ``--substrate bg`` dispatch, poll the roster briefly for the
    new worker's pid and demote it post-hoc. ``job_id`` is the claude bg jobId
    (the registry ``short_id``). Bounded; one warning on miss."""
    if not job_id or not _qos_enabled():
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
                if sid.split("-")[0] == job_id and isinstance(
                    w.get("pid"), int
                ):
                    qos_demote_pid(w["pid"])
                    return
        except Exception:
            pass
        if time.monotonic() >= deadline:
            _warn(
                f"spawn-gate: bg worker {job_id} pid not in roster "
                f"within {int(poll_s)}s; QoS demotion skipped (non-fatal)"
            )
            return
        time.sleep(0.5)
