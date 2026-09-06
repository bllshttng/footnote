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

import contextvars
import json
import math
import os
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, NoReturn, Optional, cast
from urllib.parse import unquote

from fno.agents.row_contradiction import project_row
from fno.harness_identity import claude_transport_short_id

# Exit codes, distinct from existing dispatch codes (2, 13, 14, 15, 18, 127)
# and byte-parity with the Rust gate.
EXIT_QUEUE_TIMEOUT = 75
EXIT_NO_WAIT = 76
EXIT_RAM_REFUSED = 77
EXIT_PROVIDER_CAP = 78
EXIT_LOAD_REFUSED = 79
EXIT_KING_SHARE = 80
EXIT_REGISTRY_SCHEMA = 81

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
    #: (x-d401) Why ``status`` is not the registry's stored token, when it is
    #: not: a row the contradiction rules rewrote (e.g. a `spawning` token a
    #: live pid outlived renders `live` + basis `stale-spawning-live-pid`).
    #: None means the stored token passed through untouched.
    status_basis: Optional[str] = None


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
    #: False when the registry read failed: every share count is then unknown
    #: (None), never zero (x-5283 AC9, the gather_court rule).
    registry_readable: bool = True
    #: Crowned sessions, read through court.crowned_sessions - the same
    #: field the court reads (x-5283 LD1). This set divides ``max_live``.
    crowned_sessions: set[str] = field(default_factory=set)
    #: Worker rows per ``spawned_by_session``; None keys the unattributed
    #: bucket, which consumes ``max_live`` but divides nothing (x-5283 LD4).
    worker_rows: dict[Optional[str], list[str]] = field(default_factory=dict)

    @property
    def worker_counts(self) -> dict[Optional[str], int]:
        """Live worker rows per spawner, derived from :attr:`worker_rows`."""
        return {k: len(v) for k, v in self.worker_rows.items()}

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
        The claude roster's non-work sessions (memory-plugin observers, resident
        idle) never enter this count; their RAM cost stays honored by the
        separate ``min_free_gb`` floor."""
        return self.fno_slot_workers + self.slot_claims


@dataclass(frozen=True)
class LoadSnapshot:
    load_1m: float | None
    max_load_per_cpu: float
    load_cpu_count: int
    load_ceiling: float
    spawn_load_status: Literal["disabled", "unavailable", "within", "exceeded"]
    # The 5m/15m averages ride along for the court panel: a climb and a spike
    # look identical through the 1m figure alone. Nothing gates on them.
    load_5m: float | None = None
    load_15m: float | None = None


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
        out.registry_readable = False
        rows = []
    claim_live_cache: dict[str, bool] = {}
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
        # Still no non-fno session counted: a memory-plugin observer has no
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
            # The pid gate is process-shaped; a codex thread lane has no local
            # process (the codex app-server hosts it) and no claude roster row,
            # so both arms above are blind to it. Its live worker:<name> slot
            # claim is the liveness oracle - the same evidence the provider
            # count adds at its tail. Admitting the row here moves it from the
            # slot-claim bucket into the row table WITHOUT changing
            # slot_count: the claims walk at the end skips live_registry_names.
            # Without this arm the LANES block reported codex lanes above a
            # table that showed none (measured 2026-09-01: top 23 rows, list
            # 25, every missing id a codex session).
            if not _worker_claim_live(row.name, claim_live_cache):
                continue
            claim_alive = True
        else:
            claim_alive = False
        # A live fno row is fno work: it holds a slot regardless of the display
        # dedup below (x-bdf9 — a bg/adopted worker also appears in the roster,
        # but its registry row is the slot, matching the registry-only Rust gate).
        out.fno_slot_workers += 1
        # x-5283: a crowned row divides the cap and pays no per-king tax.
        if row.crown_level is None:
            out.worker_rows.setdefault(row.spawned_by_session, []).append(row.name)
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
        session_pid = resolve_session_pid(
            harness=row.harness,
            short_id=row.short_id,
            session_id=row.harness_session_id,
            pid=row.pid,
            socket_map=sock_map,
        )
        # (x-d401) The stored token goes through the contradiction rules with
        # the liveness this census ALREADY measured: a `spawning` token a
        # live pid outlived renders the movement-derived state with a basis
        # naming the contradiction, never a bare `spawning` for a working
        # row. Fires only on positive liveness (measured-live pid, or a
        # session pid the rendezvous actually resolved); unknown keeps the
        # token.
        projected = project_row(
            {
                "status": str(row.status),
                "created_at": row.created_at,
                # `row.pid is None` guards the second disjunct, and the guard is
                # load-bearing. `resolve_session_pid` FALLS BACK to the recorded
                # pid: every non-claude harness returns it unchanged, and so does
                # a claude row that misses the socket map. So a bare
                # `session_pid is not None` is true whenever `row.pid` is set,
                # including for a row whose liveness this census just failed to
                # read and warned about as "process incarnation unreadable"
                # above. That handed an UNMEASURED pid to the rule as positive
                # liveness, and the rule then rewrote a parked `spawning` row to
                # `live` under a basis naming a measurement nobody took. The
                # rendezvous case this disjunct exists for is the one where no
                # pid was recorded and the socket map supplied it.
                "pid_alive": pid_state is True
                or (row.pid is None and session_pid is not None)
                or claim_alive,
            }
        )
        status_basis = projected.get("basis") if projected.get(
            "status"
        ) != str(row.status) else None
        out.workers.append(
            LiveWorker(
                source="fno",
                name=row.name,
                harness=row.harness,
                substrate=substrate,
                pid=row.pid,
                status=str(projected.get("status", row.status)),
                status_basis=status_basis,
                session_id=row.harness_session_id,
                spawned_by=row.spawned_by_session,
                session_pid=session_pid,
            )
        )

    out.slot_claims = _live_worker_slot_claims(out.warnings, live_registry_names)

    # The divisor reads crowns through the court's own primitive (x-5283
    # LD1/AC3); share_reading keys unknown off registry_readable.
    if out.registry_readable:
        from fno.agents.court import crowned_sessions

        out.crowned_sessions = crowned_sessions(rows)
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


#: Registry shapes already warned about in THIS process. The unattributed-row
#: warning describes the registry as a whole, not one provider, so a caller
#: asking about several providers - or a spawn queueing round the gate loop -
#: repeated the identical line once per call. Deduped per process rather than
#: silenced: the fact still reaches stderr exactly once.
_UNATTRIBUTED_WARNED: set[tuple[str, str]] = set()


def provider_live_count(provider: str, counted: Optional[set[str]] = None) -> int:
    """Count provider rows only when status and positive liveness agree.

    ``counted``, when given, is filled with the names of the rows this count
    actually included. That is deliberately an out-parameter on the COUNTER
    rather than a second walk in the caller: a display that recounted would
    disagree with the refusal the first time either changed, and a lane display
    that disagrees with the gate is worse than no display. Measured while
    building `fno agents top`'s lane block: a naive registry walk listed five
    openai rows beside the gate's count of 0, because status and positive
    liveness are not the same population.
    """
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
            if (harness, origin) in _UNATTRIBUTED_WARNED:
                continue
            _UNATTRIBUTED_WARNED.add((harness, origin))
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

    def _pane_state(row) -> "bool | None":
        """Pane liveness via the mux probe. Raises ProviderCountUnavailable
        when the probe crashes; None when the row carries no pane ref."""
        if not isinstance(row.mux, dict):
            return None
        try:
            from fno.agents.mux_spawn import _mux_pane_alive

            return _mux_pane_alive(row.mux)
        except Exception as exc:
            raise ProviderCountUnavailable(
                f"pane liveness unreadable for {row.name}: {exc}"
            ) from exc

    for row in candidates:
        if row.pid is not None:
            if row.pid_start_time is None:
                state = _pid_alive(row.pid, None)
                if state is False:
                    continue
                pane = _pane_state(row)
                if pane is True:
                    count += 1
                    counted_names.add(row.name)
                    continue
                if pane is False:
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
        pane = _pane_state(row)
        if pane is True:
            count += 1
            counted_names.add(row.name)
            continue
        if pane is False:
            continue
        if isinstance(row.mux, dict):
            # Unreadable is not absent: the cap refuses before the bg fallback.
            raise ProviderCountUnavailable(
                f"pane liveness unreadable for {row.name}"
            )
        if row.short_id and row.short_id in bg_live:
            count += 1
            counted_names.add(row.name)
    if counted is not None:
        counted.update(counted_names)
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


def _worker_claim_live(name: str, cache: dict[str, bool]) -> bool:
    """Is the ``worker:<name>`` slot claim live? One claim read per name per call.

    The display union's pid gate is process-shaped, and a codex thread lane has
    no local process at all - the codex app-server hosts the session, so the
    row records no pid and the claude-roster arm cannot see it either. The
    live slot claim is the liveness oracle for such rows, the same evidence the
    provider count adds at its tail. ``cache`` dedups the claim reads across
    one census walk; callers seed it with ``{}``.
    """
    if name in cache:
        return cache[name]
    try:
        from fno.claims.core import claim_status

        state = claim_status(f"worker:{name}", root=_gate_claims_root()).get("state")
        live = state in ("live", "suspect")
    except Exception:  # noqa: BLE001 - an unreadable claim proves nothing
        live = False
    cache[name] = live
    return live


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


#: The spawn `run_gate` is currently deciding, as ``(name, substrate)``.
#: Set once at gate entry and read by :func:`_refuse`, so a refusal event can
#: name the spawn it refused without threading `name` through four helper
#: signatures that have no other use for it. A leftover value is harmless: the
#: next `run_gate` overwrites it, and only a refusal inside a gate run reads it.
_CURRENT_SPAWN: "contextvars.ContextVar[tuple[Optional[str], Optional[str]]]" = (
    contextvars.ContextVar("fno_spawn_gate_current", default=(None, None))
)


def _refuse(
    exit_code: int,
    receipt: Optional[dict[str, object]] = None,
    *,
    cause_stated: bool = False,
    **event: Any,
) -> NoReturn:
    """The one seam every gate refusal exits through: emit, then raise.

    Before this existed a refusal lived only in the stderr of a process that
    had already exited, so nobody could ask afterward why a node did not
    launch. Measured 2026-09-01: the global journal carried 4815
    ``claim_acquired`` rows (the positive control that the file is read) and
    zero rows of any kind naming a gate refusal.

    ``receipt`` is the caller-facing shape ``fno agents spawn`` prints on
    stdout and is passed through untouched. Every event carries
    ``gate: "python"`` and the spawn's ``substrate``, so a reader can tell
    this journal's population (the pane substrate, the sole leg this gate
    covers) from the Rust gate's refusals, which x-ab75 owns.

    ``event`` is extra telemetry for
    the refusals that deliberately carry no receipt (the load ceiling and the
    king share), so a refusal can name its measured value against its
    threshold in the log without changing what stdout has always printed.

    The emit is best-effort - ``_emit_gate_event`` swallows everything - so
    telemetry can never change a gate outcome.
    """
    spawn_name, substrate = _CURRENT_SPAWN.get()
    event_data = {**(receipt or {}), **event}
    # The seam-owned fields win on collision: a future receipt carrying `name`
    # or `gate` must not silently rewrite the identity this journal entry is
    # attributed by.
    event_data.update(
        exit_code=exit_code, name=spawn_name, substrate=substrate, gate="python"
    )
    _emit_gate_event("spawn_gate_refused", **event_data)
    refusal = GateRefused(exit_code, receipt)
    if cause_stated:
        refusal.cause_stated = True  # type: ignore[attr-defined]
    raise refusal


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
    _refuse(EXIT_PROVIDER_CAP, receipt)


def _emit_gate_event(kind: str, **data: Any) -> None:
    """Best-effort agents-log event. Never raises, never blocks a spawn."""
    try:
        from fno.agents import events

        events.emit(kind, **data)
    except Exception:  # noqa: BLE001 - telemetry never changes a gate outcome
        pass


def _check_registry_schema() -> None:
    """Refuse a spawn into a fleet whose shared registry this fno cannot write.

    Claiming a node and stamping mail are both WRITES, so while the shared
    registry sits ahead of this fno the whole spawn path is refused. On
    2026-08-28 that produced a worker that reported "Claim store is not writable
    for this Codex session", attributed it to a sandbox permission profile, and
    was believed. Nothing in that chain named the registry.

    So this refuses rather than warns, and the message carries both integers,
    the file, and the repair verb. Same contract as :func:`_check_ram_floor` and
    :func:`_check_load_ceiling` on the edges: an unreadable file skips, because
    a spawn is not the place to adjudicate a torn registry and refusing there
    would make the repair verb itself unspawnable.

    The event is the other half. Every degraded READ prints a banner, but a
    refused WRITE returns an error to one caller who reports it in its own words
    to a king who is not watching; nothing collects those into "the fleet cannot
    write". Emission is best-effort and never blocks the refusal.
    """
    from fno.agents.registry import (
        SCHEMA_VERSION,
        _read_raw_registry,
        _registry_path,
    )

    try:
        target = _registry_path(None)
        raw = _read_raw_registry(target)
    except Exception as exc:  # noqa: BLE001 - resolving the path needs settings,
        # and an unreadable config is not a fleet condition: no spawn is blocked
        # because a path could not be resolved (the module contract that global
        # guards fail OPEN on read errors).
        #
        # The skip leaves a trace, but NOT on stderr. `_check_ram_floor` and
        # `_check_load_ceiling` warn on their equivalent skips, and this one
        # cannot: the gate's own `test_under_cap_passes_silently` pins an empty
        # stderr on the pass path, and this branch fires there. A silent skip is
        # still unobservable, so it emits instead - the same aggregation argument
        # the refusal below already makes, applied to the absence of a check.
        _emit_gate_event("registry_schema_check_skipped", reason=repr(exc))
        return
    if raw is None:
        return
    on_disk = raw.get("schema_version")
    if not isinstance(on_disk, int) or on_disk <= SCHEMA_VERSION:
        return
    _warn(
        f"spawn-gate: the shared agent registry at {target} is "
        f"schema_version={on_disk}, ahead of the schema_version={SCHEMA_VERSION} "
        "this fno understands, so this worker could neither claim its node nor "
        "stamp its mail; refusing to spawn. Upgrade this fno (fno doctor "
        f"update), or repair the file (fno agents registry-repair --to "
        f"{SCHEMA_VERSION} --apply)."
    )
    # This branch used to emit its own `registry_schema_ahead` beside the
    # refusal: a bespoke answer to the general problem `_refuse` now solves for
    # every branch. It had one producer and no consumer, and two events for one
    # moment drift apart. The general event carries strictly more (the spawn
    # name and the exit code), and `reason == "registry_schema"` still isolates
    # this case for anyone querying only for it.
    _refuse(
        EXIT_REGISTRY_SCHEMA,
        {
            "status": "refused",
            "reason": "registry_schema",
            "registry_path": str(target),
            "on_disk": on_disk,
            "understood": SCHEMA_VERSION,
        },
    )


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
        _refuse(EXIT_RAM_REFUSED, receipt)


def _fleet_cpu_reading() -> Optional[tuple[float, float]]:
    """Footprint's attribution as numbers: ``(fleet_cores, capacity_cores)``.

    The governor and the refusal text must read ONE instrument. Before
    x-7c0f the numbers existed only inside the explanation string, which is
    how a gate came to print `0.79/12.00 cores` in the same breath as a
    refusal decided on something else.

    ``None`` means unreadable, which is never headroom (see x-e040: this
    sensor goes blind under exactly the load it exists to measure).
    """
    try:
        from fno.doctor_footprint import _cpu_capacity_cores, cause_reading

        reading, _error = cause_reading()
        if reading is None:
            return None
        if getattr(reading, "attribution_gap", None) is not None:
            # A gapped fleet share is an undercount: unknown, never headroom
            # (x-e040). None routes into the same refuse-above-trigger branch
            # as an unreadable instrument.
            return None
        capacity = float(_cpu_capacity_cores())
        fleet = float(reading.fleet_cpu_cores)
        if capacity <= 0 or not all(
            math.isfinite(v) and v >= 0 for v in (fleet, capacity)
        ):
            return None
        return fleet, capacity
    except Exception:
        return None


def _footprint_cause_evidence() -> Optional[str]:
    """Read one fail-open fleet footprint for an over-load refusal."""
    try:
        from fno.doctor_footprint import _cpu_capacity_cores, cause_reading

        reading, _error = cause_reading()
        if reading is None:
            return None
        if getattr(reading, "attribution_gap", None) is not None:
            # Same rule as _fleet_cpu_reading: an undercount is not evidence
            # about the fleet (x-e040).
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


def _refuse_load_cause_stated(**event: Any) -> NoReturn:
    """A load refusal that already printed the attribution it decided on.

    :func:`run_gate` appends a footprint cause line to a load refusal, taken
    from a SECOND, independent sample. That is honest only when the refusal
    itself could not say whose CPU this is (the backstop). Marking the two
    attribution-aware branches keeps one refusal reading one sample.

    ``event`` carries that one sample into the log, so the emitted refusal
    reports the same numbers the operator was shown rather than a third
    reading taken later.
    """
    _refuse(EXIT_LOAD_REFUSED, cause_stated=True, **event)


#: `_check_load_ceiling` takes its own attribution reading when the caller
#: has not already taken one. `None` is a real reading ("unreadable"), so the
#: "not supplied" case needs a value that cannot be confused with it.
_NOT_PREFETCHED: object = object()


def _load_cpus() -> int:
    """The CPU denominator for the trigger and the backstop.

    Footprint's capacity reading, which is the minimum of the affinity count,
    the host count and the cgroup quota. Two reasons it is worth the import
    over a bare `process_cpu_count`:

    the Rust gate uses `available_parallelism`, which IS quota-aware, so an
    affinity-only count here made the two runtimes compute different triggers
    from one config on a quota-constrained container (2-of-32 cores gives 16
    against 256);

    and the share comparison already divides by this exact number, so a
    different denominator for the trigger meant one check answering a
    question the other was not asking.

    It reads affinity and a cgroup file, never `ps`, so the cheap path stays
    cheap. Falls back rather than raising: a guard must not brick the spawn
    primitive because an import moved.
    """
    try:
        from fno.doctor_footprint import _cpu_capacity_cores

        return int(_cpu_capacity_cores()) or 1
    except Exception:
        return getattr(os, "process_cpu_count", os.cpu_count)() or 1


def _load_snapshot(max_load_per_cpu: float) -> LoadSnapshot:
    cpus = _load_cpus()
    ceiling = max_load_per_cpu * cpus
    if max_load_per_cpu <= 0:
        return LoadSnapshot(
            load_1m=None,
            max_load_per_cpu=max_load_per_cpu,
            load_cpu_count=cpus,
            load_ceiling=ceiling,
            spawn_load_status="disabled",
        )
    try:
        load1, load5, load15 = os.getloadavg()
    except (OSError, AttributeError):
        return LoadSnapshot(
            load_1m=None,
            max_load_per_cpu=max_load_per_cpu,
            load_cpu_count=cpus,
            load_ceiling=ceiling,
            spawn_load_status="unavailable",
        )
    return LoadSnapshot(
        load_1m=load1,
        max_load_per_cpu=max_load_per_cpu,
        load_cpu_count=cpus,
        load_ceiling=ceiling,
        spawn_load_status="within" if load1 <= ceiling else "exceeded",
        load_5m=load5,
        load_15m=load15,
    )


def _needs_attribution(
    load1: float, cpus: int, max_load_per_cpu: float, hard_max_load_per_cpu: float
) -> bool:
    """True only in the band where the verdict depends on WHOSE CPU it is.

    Below the trigger the gate admits without asking, and above the backstop
    it refuses without asking. Only between them does attribution decide, and
    only there is the expensive read worth taking.
    """
    if max_load_per_cpu <= 0:
        return False
    if load1 <= max_load_per_cpu * cpus:
        return False
    if hard_max_load_per_cpu > 0 and load1 > hard_max_load_per_cpu * cpus:
        return False
    return True


def _prefetch_fleet_reading(
    max_load_per_cpu: float, hard_max_load_per_cpu: float
) -> object:
    """Take the attribution reading OUTSIDE the gate mutex, when needed at all.

    The reading is a `ps` snapshot behind a multi-second deadline, and the gate
    mutex serializes every spawner on the machine. Taking it inside the lock
    made a loaded box hold the mutex for seconds, which is exactly when
    contention is worst: concurrent `--no-wait` spawners then refuse with
    `no_wait_mutex_held` for no reason of their own. The measurement is
    identical outside the lock, and it is a SAMPLE either way; the RAM floor
    re-reads on dequeue for the same reason.

    Returns `_NOT_PREFETCHED` when the band does not need attribution, so the
    caller stays free to decide from load alone.
    """
    snapshot = _load_snapshot(max_load_per_cpu)
    if snapshot.load_1m is None:
        return _NOT_PREFETCHED
    if not _needs_attribution(
        snapshot.load_1m,
        snapshot.load_cpu_count,
        max_load_per_cpu,
        hard_max_load_per_cpu,
    ):
        return _NOT_PREFETCHED
    return _fleet_cpu_reading()


def _check_load_ceiling(
    max_load_per_cpu: float,
    max_fleet_cpu_share: float = 0.5,
    hard_max_load_per_cpu: float = 40.0,
    prefetched: object = _NOT_PREFETCHED,
) -> None:
    """Refuse (never queue) when the FLEET is the reason the box is loaded.

    Three thresholds, because the honest question needs two instruments:

    1. ``max_load_per_cpu x cpus`` is a TRIGGER. Below it the gate admits
       without probing, so the common path costs no subprocess.
    2. Above the trigger the gate asks footprint whose CPU this is and
       refuses only when the fleet holds more than ``max_fleet_cpu_share``
       of capacity.
    3. ``hard_max_load_per_cpu x cpus`` refuses regardless of attribution.

    WHY (x-7c0f, measured twice). This check refused on the 1-min load
    average while printing footprint's contradicting attribution in the same
    refusal: `load 127.6 exceeds ... 96.0` beside `attributes 0.79/12.00
    cores (6.6% capacity)`. Load average counts runnable PLUS blocked
    processes, so it is not a CPU measure and belongs to nobody. On
    2026-08-29 the three largest consumers on the refusing box were desktop
    applications, and killing one unscoped ripgrep moved the 1-min load from
    374 to 179 with no agent stopped. A gate that refuses beside its own
    contradicting measurement teaches an operator to reach for --force,
    which is how a guard becomes a formality.

    Step 3 exists because a pure fleet-share governor would admit onto a box
    already thrashing from foreign work. Keep the backstop well above the
    trigger; :func:`AgentsBlock` defaults are 8 and 40.

    Same contract as :func:`_check_ram_floor` otherwise: ``max_load_per_cpu
    <= 0`` disables, unreadable LOAD skips (fail open, the platform may have
    no getloadavg at all). Unreadable ATTRIBUTION refuses (fail closed): an
    unknown share is not evidence of headroom.
    """
    snapshot = _load_snapshot(max_load_per_cpu)
    if snapshot.spawn_load_status == "disabled":
        return
    if snapshot.spawn_load_status == "unavailable":
        # OSError: unreadable. AttributeError: the platform has no getloadavg
        # at all (the Rust gate cfg-guards the same case).
        _warn("spawn-gate: could not read load average; skipping the load check")
        return
    load1 = snapshot.load_1m
    cpus = snapshot.load_cpu_count
    trigger = snapshot.load_ceiling
    assert load1 is not None  # "within"/"exceeded" always carry a reading
    if load1 <= trigger:
        return

    if hard_max_load_per_cpu > 0 and load1 > hard_max_load_per_cpu * cpus:
        _warn(
            f"spawn-gate: 1-min load {load1:.1f} exceeds the absolute machine "
            f"backstop hard_max_load_per_cpu {hard_max_load_per_cpu:g} x {cpus} "
            f"cpus = {hard_max_load_per_cpu * cpus:.1f}; refusing to spawn "
            f"whoever caused it (--force to bypass)"
        )
        _refuse(
            EXIT_LOAD_REFUSED,
            reason="load_backstop",
            load_1m=load1,
            cpus=cpus,
            hard_max_load_per_cpu=hard_max_load_per_cpu,
            threshold=hard_max_load_per_cpu * cpus,
        )

    # run_gate prefetches this outside the gate mutex; a direct caller (and
    # every unit test) still gets the read on demand.
    reading = (
        _fleet_cpu_reading()
        if prefetched is _NOT_PREFETCHED
        else cast("Optional[tuple[float, float]]", prefetched)
    )
    if reading is None:
        _warn(
            f"spawn-gate: 1-min load {load1:.1f} is over the "
            f"max_load_per_cpu trigger {max_load_per_cpu:g} x {cpus} cpus = "
            f"{trigger:.1f} and fleet CPU attribution unavailable; refusing to "
            f"spawn (--force to bypass)"
        )
        # The attribution read just failed, so run_gate's evidence probe would
        # fail the same way one sample later. Nothing to add.
        _refuse_load_cause_stated(
            reason="load_attribution_unavailable",
            load_1m=load1,
            cpus=cpus,
            max_load_per_cpu=max_load_per_cpu,
            threshold=trigger,
        )

    fleet, capacity = reading
    share = fleet / capacity
    if share > max_fleet_cpu_share:
        _warn(
            f"spawn-gate: the fleet holds {fleet:.2f}/{capacity:.2f} cores "
            f"({share * 100:.1f}% of capacity), over the "
            f"max_fleet_cpu_share ceiling {max_fleet_cpu_share * 100:.1f}%; "
            f"refusing to spawn (--force to bypass)"
        )
        # This refusal already names the sample it decided on, so run_gate must
        # not append a SECOND, independently taken attribution beside it: two
        # samples seconds apart disagree, and a refusal printing numbers it did
        # not decide on is the whole defect x-7c0f removed.
        _refuse_load_cause_stated(
            reason="fleet_cpu_share",
            fleet_cores=fleet,
            capacity_cores=capacity,
            share=share,
            max_fleet_cpu_share=max_fleet_cpu_share,
        )

    _warn(
        f"spawn-gate: 1-min load {load1:.1f} is high but only "
        f"{fleet:.2f}/{capacity:.2f} cores ({share * 100:.1f}%) are attributed "
        f"to the fleet, so the load is not attributed to the fleet; admitting "
        f"the spawn"
    )


def _king_share(cap: int, crowned: set[str], caller: str) -> int:
    """One king's fair share of the ceiling: ``cap // crowns`` (x-5283 LD1).

    The divisor counts CROWNS from the court's own ``crown_level`` field; the
    caller folds in only when itself crowned (LD2: still share-checked, never
    in the divisor). The floor of 1 keeps a crowded fleet able to start one
    worker per king. A fleet with NO crowns divides by nothing, so every
    uncrowned caller's share floors at 1: a session holds one worker until
    someone is crowned. That is the crownless fleet refusing to be
    ungoverned, not a malfunction, and the refusal's "across 0 kings" names
    it.
    """
    divisor = len(crowned | ({caller} if caller in crowned else set()))
    return max(1, cap // divisor) if divisor else 1


def share_reading(census_obj: "LiveCensus", cap: int, caller: Optional[str]) -> dict:
    """One share reading, printed by every surface that answers the question.

    ``kings``/``king_sessions``/``share``/``held``/``held_rows`` (the
    caller's worker rows, by name) and ``unattributed`` (the LD4 bucket:
    live rows that name nobody, count plus names). An unreadable registry
    returns None for every count, never zero (x-5283 AC9).
    """
    if not census_obj.registry_readable:
        return {
            "kings": None,
            "king_sessions": None,
            "share": None,
            "held": None,
            "held_rows": None,
            "unattributed": None,
        }
    king_sessions = set(census_obj.crowned_sessions)
    unattributed_rows = census_obj.worker_rows.get(None, [])
    held_rows = list(census_obj.worker_rows.get(caller, [])) if caller else []
    return {
        "kings": len(king_sessions),
        "king_sessions": sorted(king_sessions),
        "share": _king_share(cap, king_sessions, caller or ""),
        "held": len(held_rows),
        "held_rows": held_rows,
        "unattributed": {
            "count": len(unattributed_rows),
            "rows": list(unattributed_rows),
        },
    }


def _take_headless_slot(
    guard, name, holder, route_provider, provider_cap
) -> None:
    """The headless arm: bind the worker slot now. pane/bg keep the gate mutex
    until dispatch returns (the caller releases via guard.release())."""
    try:
        _acquire_worker_slot(
            guard, name, holder, route_provider,
            fail_closed=provider_cap is not None,
        )
    except ProviderCountUnavailable as exc:
        guard.release()
        _refuse_provider_cap(route_provider or "unknown", provider_cap or 0, error=exc)
    guard.release_gate_mutex()


def _check_king_share(
    census_obj: "LiveCensus", cap: int, *, caller_session: Optional[str]
) -> None:
    """Refuse (never queue) when the calling king holds its full share (x-3f84 W4).

    The share divides ``max_live`` by CROWNS (x-5283 LD1) and ``held`` counts
    the caller's worker rows only. Only a caller whose session identity
    resolved is checked: an operator terminal or cron job is not competing
    for the commons. Waiting cannot help - only the caller's own workers
    dying frees its share - so this refuses like the provider cap. Every
    number comes from :func:`share_reading`: the count the gate refuses on
    and the count any readout prints are one value.
    """
    if not caller_session:
        return
    reading = share_reading(census_obj, cap, caller_session)
    held, share, kings = reading["held"], reading["share"], reading["kings"]
    if held is None or share is None or kings is None:
        # An unreadable registry leaves every count unknown; there is nothing
        # to enforce and no zero to fail open on.
        return
    if held >= share:
        msg = (
            f"spawn-gate: king {caller_session[:8]} holds {held} of max_live {cap} "
            f"across {kings} kings (share {share}); refusing to spawn -- waiting "
            f"cannot help while your own workers hold the share (--force to bypass)"
        )
        unattributed = reading["unattributed"] or {}
        if unattributed.get("count"):
            shown = ", ".join(unattributed["rows"][:5])
            extra = "..." if unattributed["count"] > 5 else ""
            msg += (
                f"; {unattributed['count']} live row(s) name nobody and sit in "
                f"the unattributed bucket ({shown}{extra})"
            )
        _warn(msg)
        _refuse(
            EXIT_KING_SHARE,
            reason="king_share",
            king=caller_session,
            held=held,
            share=share,
            max_live=cap,
            kings=kings,
        )


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
    # Set before the first branch that can refuse, so every refusal event in
    # this run names the spawn it refused (see _CURRENT_SPAWN).
    _CURRENT_SPAWN.set((name, substrate))
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
        # These two read through getattr, and the distinction from the line
        # below is deliberate. A missing CAP must fail loudly, because falling
        # back would silently uncap a provider. A missing machine THRESHOLD has
        # a safe default and no such consequence. Reading them strictly put
        # them in the same failure class as `provider_limits`: any caller
        # holding a settings object built before these fields existed dropped
        # the WHOLE block into the fail-safe branch below, which silently
        # discarded that caller's `max_live` too. That turned one new field
        # into a cap bug three test modules away from it.
        max_fleet_cpu_share = float(getattr(agents_cfg, "max_fleet_cpu_share", 0.5))
        hard_max_load_per_cpu = float(getattr(agents_cfg, "hard_max_load_per_cpu", 40.0))
        # A real attribute read, not a getattr fallback: a missing field must
        # fail loudly here rather than silently uncapping every provider.
        limits = dict(agents_cfg.provider_limits)
    except Exception:
        cap, floor_gb, max_load_per_cpu = 3, 4.0, 8.0
        max_fleet_cpu_share, hard_max_load_per_cpu = 0.5, 40.0
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

    # Ahead of the force branch, deliberately. `--force` means "I know the
    # machine is busy", and a schema mismatch is not resource pressure: it is a
    # worker that can neither claim its node nor stamp its mail, which is the
    # failure this check exists to name. Forcing past it would reproduce that
    # failure with the diagnosis suppressed. Nothing is held yet, so a refusal
    # here needs no `guard.release()`. The dequeue path re-checks, the way the
    # RAM floor does, because the queue window is long enough for the shared
    # schema to move underneath a waiting spawn.
    _check_registry_schema()

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
        # Before the mutex, never inside it: this can cost seconds and the
        # mutex serializes every spawner on the machine. Re-taken each pass so
        # a spawn that queued does not decide on a reading from minutes ago.
        prefetched_fleet = _prefetch_fleet_reading(
            max_load_per_cpu, hard_max_load_per_cpu
        )
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
                _refuse(EXIT_NO_WAIT, receipt)
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
                    _take_headless_slot(guard, name, holder, route_provider, provider_cap)
                return guard
            c = census()
            for w in c.warnings:
                _warn(w)
            slots = c.slot_count
            if slots < cap:
                try:
                    # Re-checked on dequeue for the same reason the RAM floor is
                    # (test_dequeue_ram_recheck_refuses): a spawn can sit here for
                    # up to QUEUE_TIMEOUT_S, and another process can raise the
                    # shared schema inside that window. The entry check above owns
                    # the force path; this one owns the queue window.
                    _check_registry_schema()
                except GateRefused:
                    guard.release()
                    raise
                try:
                    _check_ram_floor(floor_gb)
                except GateRefused:
                    guard.release()
                    raise
                try:
                    _check_load_ceiling(
                        max_load_per_cpu,
                        max_fleet_cpu_share,
                        hard_max_load_per_cpu,
                        prefetched=prefetched_fleet,
                    )
                except GateRefused as refusal:
                    # The refusal is decided; release the mutex BEFORE the
                    # cause probe so queued spawners (and --no-wait callers)
                    # never sit behind seconds of evidence gathering.
                    guard.release()
                    # Only the backstop refuses without reading attribution, so
                    # it is the only branch this line can inform. Adding it to
                    # a refusal that already named its own sample would print
                    # two disagreeing measurements in one refusal.
                    if not getattr(refusal, "cause_stated", False):
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
                    _take_headless_slot(guard, name, holder, route_provider, provider_cap)
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
                _refuse(EXIT_NO_WAIT, receipt)
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
            _refuse(EXIT_QUEUE_TIMEOUT, receipt)
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
