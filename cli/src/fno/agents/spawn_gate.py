"""Spawn gate : global concurrency cap + free-RAM floor + queue loop.

Called at the top of ``cmd_spawn`` before the substrate fan-out. Mirrors
``crates/fno-agents/src/spawn_gate.rs`` — the two gates sit on mutually
exclusive execution paths (the front door execs the binary for bg/headless;
the Rust ``pane`` arm re-execs this CLI), so every spawn passes exactly one.

The gate is READ-ONLY: the ``max_live`` slot cap counts fno registry rows
(worker provenance) and the RAM floor reads real system RAM. The claude daemon
roster is never a population to count toward the slot cap (: only a row
ALSO in the fno registry counts). Its only writes are its own claims under the
GLOBAL claims root - the RAM budget is machine-wide. Global guards fail OPEN
on read errors; the per-provider cap is stricter: an unreadable live count
refuses, never assumes zero.
"""
from __future__ import annotations

import contextvars
import json
import os
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, NoReturn, Optional, cast
from urllib.parse import unquote

from fno.agents.row_contradiction import project_row
from fno.footprint import Admission
from fno.harness_identity import claude_transport_short_id

# THE exit-code allocation table for the gate band across both trees (this
# file and crates/fno-agents/src/spawn_gate.rs). A value >= 64 claims its
# number once; the same NAME at the same number in both trees is byte-parity,
# and cli/tests/unit/test_exit_code_allocation.py fails any duplicate. The
# convention band stays outside the claim: small ints 0-5 and 13-25 repeat per
# verb by design, and 124/127/137/143 mirror the standard timeout/signal codes.
#   75-77, 79   capacity refusals, both gates (queue, no-wait, RAM, load)
#   78          provider cap; the quota lock and the lane faults keep it so
#               exit-code consumers are unaffected
#   80, 81      king share, registry schema
#   82, 83      fleet incident stop pair, both gates (byte-parity)
#   84          state root ungranted. Permanent until a human grants.
#   85          Python sandbox probe: sandbox unreachable.
#   86          the spawn-gate transport could not get an answer at all (the
#               gate verb missing, failed, or timed out); fail closed, never
#               admit on an unreadable gate.
#   90, 91      Rust fleet-incident check verb (fleet_incident.rs).
EXIT_QUEUE_TIMEOUT = 75
EXIT_NO_WAIT = 76
EXIT_RAM_REFUSED = 77
EXIT_PROVIDER_CAP = 78
EXIT_LOAD_REFUSED = 79
EXIT_KING_SHARE = 80
EXIT_REGISTRY_SCHEMA = 81
EXIT_FLEET_STOP = 82
EXIT_FLEET_STOP_UNAVAILABLE = 83
# Rust gate only (crates/fno-agents/src/spawn_gate.rs): the lane declares
# nothing about how it stands toward the fno state root.
EXIT_STATE_ROOT_UNGRANTED = 84
EXIT_GATE_UNAVAILABLE = 87


#: The refusal reasons a caller may outlast by retrying (spawn --wait). Owned
#: HERE because these tokens are the gate's vocabulary; the CLI imports this
#: set rather than re-spelling it. no_wait/no_wait_mutex_held surface only
#: when an attempt runs no_wait (spawn --wait forces that), so the CLI
#: deadline - not this gate's 600s queue - bounds the wait.
WAITABLE_REFUSAL_REASONS = frozenset(
    {
        "ram_floor", "swap_pressure",
        "cpu_instrument_unreadable",
        "cpu_share_undecidable", "fleet_cpu_share", "provider_cap",
        "max_live", "no_wait", "no_wait_mutex_held",
    }
)

QUEUE_POLL_S = 2.0
QUEUE_PROGRESS_EVERY_S = 30.0
QUEUE_TIMEOUT_S = 600.0
#: LD4: hold re-sample gap and admission debounce (the macOS `ps`
#: CPU column is a decaying average; two reads 2s apart are one sample).
CPU_HOLD_POLL_S = 15.0
CPU_ADMIT_SAMPLES = 2
#: : a slow bg-socket census names its own wait instead of silence.
SLOW_SCAN_WARN_S = 5.0
#: The mutex claim key. Prefixed so `claims_root_for` routes it to the global
#: root the gate writes; the old colon-less `spawn-gate` key unrouted, so
#: `claim status`/`release --force` read `<space>/claims/spawn-gate.lock`
#: while the gate held `~/.fno/claims/spawn-gate.lock` and both lied.
GATE_CLAIM_KEY = "gate:spawn"
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
    #: The pid of the process that IS the session (W2), resolved through
    #: the claude bg rendezvous sockets when the row is a bg session. ``pid``
    #: above keeps the RECORDED pid, which for a bg row names the PTY HOST;
    #: cost readers (``agents top``, the process-cost gate) must use this one.
    session_pid: Optional[int] = None
    #: The session id of the KING that spawned this worker (W4): the
    #: row's ``spawned_by_session``, None for an operator-run or legacy row.
    #: Cost is attributed through this field so a shared ceiling can be
    #: divided without minting a second budget record.
    spawned_by: Optional[str] = None
    #: Why ``status`` is not the registry's stored token, when it is
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
    #: False when the registry read failed: share counts unknown, never zero.
    registry_readable: bool = True
    #: Crowned sessions via court.crowned_sessions (LD1); the divisor.
    crowned_sessions: set[str] = field(default_factory=set)
    #: Worker rows per ``spawned_by_session``; None = the LD4 bucket.
    worker_rows: dict[Optional[str], list[str]] = field(default_factory=dict)

    @property
    def count(self) -> int:
        """The full union size (fno rows + roster sessions + slot claims). The
        RAM-ground-truth / ``fno agents top`` display number — NOT the slot cap
        denominator."""
        return len(self.workers) + self.slot_claims

    @property
    def slot_count(self) -> int:
        """Worker SLOTS in use for the ``max_live`` cap : live fno
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
    # display/trend only; nothing gates on these.
    load_1m: float | None
    load_cpu_count: int
    load_5m: float | None = None
    load_15m: float | None = None


def census(socket_map: Optional[dict[str, int]] = None) -> LiveCensus:
    """The full union: fno registry ∪ claude roster (deduped by claude session
    short_id) + live ``worker:<name>`` slot claims. This is the display /
    RAM-ground-truth view (``fno agents top`` renders every row). The spawn
    gate's ``max_live`` decision uses :attr:`LiveCensus.slot_count`, which
    counts fno-sourced rows only — the roster is kept here for visibility but
    does NOT consume worker slots. Read-only; every source failure
    degrades to zero contribution with one warning.

    ``socket_map`` injects the bg-socket pid join : the lsof scan
    costs seconds under load, so a caller that censuses repeatedly across one
    decision (the spawn gate's queue loop) scans ONCE and passes the map back
    in. None (the default) scans, one read per census."""
    out = LiveCensus()
    counted_short_ids: set[str] = set()
    live_registry_names: set[str] = set()

    # One socket-farm read per census (W2): every claude row below joins
    # through this map so no consumer re-runs lsof, and the recorded pid stays
    # on the row beside the resolved one. An empty map is "unknown", never
    # "no bg sessions" - rows then keep their recorded (host) pid.
    from fno.agents.session_procs import bg_socket_pid_map, resolve_session_pid

    if socket_map is None:
        try:
            socket_map = bg_socket_pid_map()
        except Exception:  # noqa: BLE001 - a broken join must not break the census
            socket_map = {}
    sock_map = socket_map

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
        # The row's own name dedups its slot claim below (a revived row does
        # not pay twice for the claim that spawned it).
        live_registry_names.add(row.name)
        # A live fno row is fno work: it holds a slot regardless of the display
        # dedup below (— a bg/adopted worker also appears in the roster,
        # but its registry row is the slot, matching the registry-only Rust gate).
        out.fno_slot_workers += 1
        # a crowned row divides the cap and pays no per-king tax.
        if row.crown_level is None:
            out.worker_rows.setdefault(row.spawned_by_session, []).append(row.name)
        dedup_key = row.short_id or None
        if dedup_key and dedup_key in counted_short_ids:
            # Already shown as its roster row in the display union. That roster
            # row carries no lineage of its own, so the KING the fno row
            # attributes this cost to rides onto it here - without the backfill
            # the one view built to show ownership names '-' for exactly the
            # rows the king-share gate counts (review finding).
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
        # The stored token goes through the contradiction rules with
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

    # The divisor reads crowns through the court's own primitive (LD1/AC3).
    if out.registry_readable:
        from fno.agents.court import crowned_sessions

        out.crowned_sessions = crowned_sessions(rows)
    return out


class ProviderCountUnavailable(RuntimeError):
    """A provider count cannot be proved from registry/liveness evidence."""


_KNOWN_UNROUTED_PROVIDER = "__uncapped__"
_PROVIDER_ADMISSION_TOKEN = object()


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
        if not _release_claim_bounded(GATE_CLAIM_KEY, holder):
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
    label = "gate mutex" if key == GATE_CLAIM_KEY else f"worker reservation {key}"
    _warn(f"spawn-gate: could not release {label}: {last_error}")
    return False


class GateRefused(SystemExit):
    """Raised (as SystemExit subclass) when the gate refuses the spawn."""

    def __init__(self, code: int, receipt: Optional[dict[str, object]] = None) -> None:
        super().__init__(code)
        self.receipt = receipt


def provider_lanes_cap(budget: object) -> Optional[int]:
    """The `lanes` dimension of one provider budget, whichever spelling arrived.

    `config.agents.provider_limits.<provider>` is a :class:`~fno.config.ProviderBudget`
    record since, and was a bare integer before it. Both reach this seam:
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

#: AC13: axes read so far plus the axis being decided, stamped onto
#: refusals that carry no explicit axis fields.
_CURRENT_AXES_READ: "contextvars.ContextVar[dict[str, str]]" = (
    contextvars.ContextVar("fno_spawn_gate_axes_read", default={})
)
_CURRENT_AXIS: "contextvars.ContextVar[Optional[str]]" = (
    contextvars.ContextVar("fno_spawn_gate_axis", default=None)
)


def _refuse(
    exit_code: int,
    receipt: Optional[dict[str, object]] = None,
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
    covers) from the Rust gate's refusals, which owns.

    ``event`` is extra telemetry for
    the refusals that deliberately carry no receipt, so a refusal can name
    its measured value against its threshold in the log without changing
    what stdout has always printed.

    The emit is best-effort - ``_emit_gate_event`` swallows everything - so
    telemetry can never change a gate outcome.
    """
    spawn_name, substrate = _CURRENT_SPAWN.get()
    event_data = {**(receipt or {}), **event}
    # AC13: an explicit axis field wins; the contextvar is the best
    # effort a non-CPU refusal site can supply.
    axes_read = _CURRENT_AXES_READ.get()
    if axes_read and "axes_read" not in event_data:
        event_data["axes_read"] = dict(axes_read)
    axis = _CURRENT_AXIS.get()
    if axis and "axis" not in event_data:
        event_data["axis"] = axis
    # The seam-owned fields win on collision: a future receipt carrying `name`
    # or `gate` must not silently rewrite the identity this journal entry is
    # attributed by.
    event_data.update(
        exit_code=exit_code, name=spawn_name, substrate=substrate, gate="python"
    )
    _emit_gate_event("spawn_gate_refused", **event_data)
    refusal = GateRefused(exit_code, receipt)
    raise refusal


def _emit_gate_event(kind: str, **data: Any) -> None:
    """Best-effort agents-log event. Never raises, never blocks a spawn."""
    try:
        from fno.agents import events

        events.emit(kind, **data)
    except Exception:  # noqa: BLE001 - telemetry never changes a gate outcome
        pass


#: `(None, "error")` is a real reading ("unreadable"), so the "not supplied"
#: case needs a value that cannot be confused with it.
_NOT_PREFETCHED: object = object()


def _prefetch_fleet_reading() -> tuple[Optional[Any], Optional[str]]:
    """Take the footprint reading OUTSIDE the gate mutex, ALWAYS.

 LD2: every spawn takes the reading. A trigger that fires on a
    number the node proved does not track the work is not a cost
    optimisation, it is a second decider. The read is one `ps` snapshot
    behind a deadline, and the gate mutex serializes every spawner on the
    machine, so it is taken before the lock exactly as before - the band
    check that sometimes skipped it is what died.

    Returns ``(reading, error)``; exactly one side is usable. A ``None``
    reading is a REFUSAL on ``cpu_instrument``, never a skip (LD3).
    """
    try:
        from fno.doctor_footprint import cause_reading
    except Exception as exc:  # noqa: BLE001 - an import fault is an unreadable instrument
        return None, f"footprint unavailable: {exc}"
    try:
        return cause_reading()
    except Exception as exc:  # noqa: BLE001
        return None, f"footprint unavailable: {exc}"


def _cpu_axis(prefetched: object = _NOT_PREFETCHED) -> Admission:
    """The CPU axis's verdict for THIS spawn: one decider, no second opinion.

    Maps an unreadable instrument to ``refuse`` on ``cpu_instrument`` (LD3:
    the sensor blinds under exactly the load it measures, and an unreadable
    process table is itself a symptom) and otherwise hands the reading to
    :func:`cpu_admission`, whose fleet CPU share decides alone.

    Shared with the ``--explain`` preview, so a dry run answers the question
    the real spawn will.
    """
    reading, error = (
        _prefetch_fleet_reading()
        if prefetched is _NOT_PREFETCHED
        else cast("tuple[Optional[Any], Optional[str]]", prefetched)
    )
    if reading is None:
        why = (error or "the reading failed").strip()
        return Admission(
            verdict="refuse",
            axis="cpu_instrument",
            reason=(
                f"spawn-gate: the CPU instrument is unreadable ({why}); "
                "refusing to spawn (--force to bypass)"
            ),
            share_low=0.0,
            share_high=0.0,
            bound="exact",
            fleet_cores=0.0,
            machine_cores=0.0,
            capacity_cores=0.0,
            ceiling=0.0,
            gap=None,
        )
    from fno.doctor_footprint import _admission_config, cpu_admission

    share_ceiling = _admission_config()
    capacity = float(_load_cpus())
    return cpu_admission(
        reading,
        capacity_cores=capacity,
        share_ceiling=share_ceiling,
    )


def _load_cpus() -> int:
    """The CPU denominator for the CPU axis.

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
    """The display/trend load reading. The retired per-cpu argument stays so
    footprint's degraded-snapshot path keeps its shape; nothing gates here."""
    del max_load_per_cpu
    cpus = _load_cpus()
    try:
        load1, load5, load15 = os.getloadavg()
    except (OSError, AttributeError):
        return LoadSnapshot(load_1m=None, load_cpu_count=cpus)
    return LoadSnapshot(
        load_1m=load1,
        load_cpu_count=cpus,
        load_5m=load5,
        load_15m=load15,
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


def _call_gate_verb(payload: dict) -> dict:
    """One round trip to the Rust gate (``fno-agents spawn-gate``, mode
    ``gate``). The child's stderr stays attached to this process, so a gate
    that queues streams its ``spawn queued: ...`` prose live. The timeout
    bounds the verb's own 600s queue plus prose and startup.
    """
    from fno.rust_binary import verb_call

    return verb_call(
        "spawn-gate",
        payload,
        timeout=QUEUE_TIMEOUT_S + 100.0,
        passthrough_stderr=True,
    )


def run_gate(
    name: str,
    substrate: str,
    *,
    force: bool = False,
    no_wait: bool = False,
    route_provider: Optional[str] = None,
    account: Optional[str] = None,
    succession_scope: Optional[str] = None,
) -> GateGuard:
    """Run the full gate - by asking the ONE gate in the binary. Returns a
    :class:`GateGuard` to hold across dispatch on pass; raises
    :class:`GateRefused` (a SystemExit) on refusal/timeout.

    This is a TRANSPORT, not a second gate: the axes (fleet incident, schema,
    quota lock, provider cap, CPU, slots, RAM, king share) are decided inside
    ``crates/fno-agents/src/spawn_gate.rs`` and this side only carries the
    caller's identity and the refusal out. The refusal event still emits from
    here (locked decision 5), so the journal population is unchanged for
    spawns that enter Python.
    """
    # Set before the first branch that can refuse, so every refusal event in
    # this run names the spawn it refused (see _CURRENT_SPAWN).
    _CURRENT_SPAWN.set((name, substrate))
    # The calling king's session id (W4), resolved through the same
    # self-identity source that stamps `spawned_by_session` onto the spawned
    # row, so the gate attributes a spawn exactly the way the row will.
    try:
        from fno.claims.self_identity import resolve_self_identity

        caller_session = resolve_self_identity().session_id
    except Exception:  # noqa: BLE001 - no identity, no share check (an
        # operator-run spawn is not competing for the commons)
        caller_session = None
    payload = {
        "mode": "gate",
        "name": name,
        "substrate": substrate,
        "force": force,
        "no_wait": no_wait,
        "route_provider": route_provider,
        "account": account,
        "succession_scope": succession_scope,
        "caller_session": caller_session,
        "holder_pid": os.getpid(),
    }
    try:
        answer = _call_gate_verb(payload)
    except Exception as exc:  # noqa: BLE001 - an unanswered gate never admits
        _refuse(
            EXIT_GATE_UNAVAILABLE,
            {
                "status": "refused",
                "reason": "gate_unavailable",
                "error": str(exc),
            },
        )
    if answer.get("status") == "admitted":
        return GateGuard(
            _gate_holder=answer.get("gate_holder"),
            _worker_key=answer.get("worker_key"),
            _worker_holder=answer.get("worker_holder"),
            _route_provider=route_provider,
            _spawn_name=name,
            _substrate=substrate,
            _admission_token=_PROVIDER_ADMISSION_TOKEN,
        )
    # The verb produced an answer, and the answer is a refusal: the exit code
    # and the receipt travelled inside the answer (a refusal is data).
    _refuse(
        int(answer.get("exit_code", EXIT_GATE_UNAVAILABLE)),
        answer.get("receipt"),
        **(answer.get("event") or {}),
    )


def probe_capacity(only: Optional[list[str]] = None) -> dict:
    """Answer "would a dispatch be admitted right now" - by asking the ONE
    gate's read-only probe mode. No mutex, no reservations, no refusal events.
    Never raises: an unanswered gate returns ``verdict: unknown``, never
    saturation, and the measurement blocks (``lanes``/``share``/``rows``)
    carry whatever the probe managed to read. ``only=["lanes"]`` skips the
    CPU and RAM reads for callers already on the spawn path (route
    resolution), where a footprint probe costs seconds under load.
    """
    try:
        from fno.claims.self_identity import resolve_self_identity

        caller = resolve_self_identity().session_id
    except Exception:  # noqa: BLE001 - no identity, no share check
        caller = None
    from fno.rust_binary import verb_call

    try:
        return verb_call(
            "spawn-gate", {"mode": "probe", "caller_session": caller, "only": only}
        )
    except Exception as exc:  # noqa: BLE001 - an unanswered gate is unknown
        return {"verdict": "unknown", "reason": "gate_unavailable", "error": str(exc)}



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
