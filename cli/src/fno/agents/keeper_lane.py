"""The watchdog's keeper lane: collect an orphaned ``fno-agents-worker`` by
process, never by socket.

Two shipped sweeps each correctly decline the same process. :mod:`orphans`
excludes the keeper binary via ``OWN_DAEMONS`` because keepers run detached at
ppid 1 by design, so ppid 1 says nothing about whether one leaked. The
registry-side sweep (:mod:`crates/fno-agents/src/daemon.rs`) walks only the
thread socket dir, and a ``--pane`` keeper is by definition the other sweep's
ground. The third owner, ``KeeperAdopt`` in ``crates/fno/src/pty.rs``, runs
inside the mux server, and an orphaned keeper exists precisely because its
server died. Nobody was left. Measured 2026-09-01: seven live keepers at
ppid 1, ages up to 22.5h, all ``--pane`` test fixtures, with ZERO socket
directories and ZERO claiming registry rows on disk. A socket walk reports a
clean machine over seven live processes, so discovery here is the process
table joined through argv: every keeper carries ``--sock``/``--session`` on
its own command line.

**Never `ps ... | awk`.** That pipeline returned count=0 twice during the
investigation, because the harness truncated 1189 lines and the pipeline
counted the summary. ``psutil.process_iter`` returns objects, so there is no
text stream to truncate. (Inherited verbatim from :mod:`orphans`, whose
docstring records the measurement.)

The probe vocabulary mirrors ``KeeperProbe`` (daemon.rs) rather than inventing
a second one, and adds ``absent`` for the measured case where the socket file
itself is gone. ``silent`` and ``unreadable`` never reap: silence never proves
death, and that rule is inherited, not relaxed.
"""
from __future__ import annotations

import errno as _errno
import socket
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Iterable, Optional

from fno.agents.orphans import REAP_MIN_AGE_S, iter_processes

#: The keeper binary. argv[0] basename, matched exactly - the same discipline
#: ``orphans.OWN_DAEMONS`` uses, so a renamed lookalike never matches.
KEEPER_BIN_NAME = "fno-agents-worker"

#: The argv flag each lane is launched with (``dispatch.py`` pane spawns write
#: ``--pane``, lane-B thread spawns write ``--keeper``).
LANE_FLAGS = {"--pane": "pane", "--keeper": "thread"}

#: Per-probe reply budget, mirroring ``KEEPER_SWEEP_REPLY_TIMEOUT``
#: (daemon.rs:8804): a wedged keeper must be NAMED inside this bound and never
#: wedge the sweep.
PROBE_BUDGET_S = 0.75

#: Whole-sweep probe budget, mirroring ``KEEPER_SWEEP_BUDGET`` (daemon.rs): a
#: fleet of wedged keepers each burning the per-probe bound is still bounded
#: work. Keepers left unprobed when it lapses read as a distinct never-reap
#: state, never as a death reading.
SWEEP_BUDGET_S = 10.0

LISTENER = "listener"
NO_LISTENER = "no_listener"
ABSENT = "absent"
SILENT = "silent"
UNREADABLE = "unreadable"
UNPROBED = "unprobed"
#: connect() failed with an errno that is not a refusal (EACCES, resource
#: exhaustion, ...). The listener is UNPROVEN, not dead: only a refused or
#: vanished endpoint may reap.
UNREACHABLE = "unreachable"

REAP = "reap"
LEAVE = "leave"

#: Socket states a positive death reading may rest on. ``no_listener``: the
#: file exists and connect() is refused. ``absent``: the file is gone - the
#: measured shape; a socket-walk discovery would never have produced the
#: candidate at all. Everything else (a live listener, silence, no declared
#: socket) is a refusal, and the verdict names which arm refused.
REAPABLE_SOCK_STATES = frozenset({NO_LISTENER, ABSENT})


@dataclass(frozen=True)
class KeeperObs:
    """One live keeper candidate read off the process table."""

    pid: int
    lane: str  # "pane" (--pane) | "thread" (--keeper)
    sock: Optional[Path]  # from argv --sock; None when argv declares none
    session: Optional[str]  # from argv --session
    cwd: Optional[str]
    age_s: float
    child_pids: tuple[int, ...]  # psutil children of this keeper
    sock_state: str  # listener | no_listener | absent | silent | unreadable
    claimed_by: Optional[str]  # registry row name whose pid or keeper_child_pid matches
    #: False when the registry could not be read at all. A failed read leaves
    #: ``claimed_by`` None, which the verdict would otherwise read as
    #: unclaimed - the exact absence-as-proof shape the watchdog exists to
    #: refuse. Unreadable registry, no reap.
    registry_ok: bool = True


def sock_state_of(sock: Optional[Path]) -> str:
    """Probe one keeper socket with the keeper's own frame codec.

    ``UNREADABLE`` for a keeper whose argv declared no socket. ``ABSENT`` when
    the file is gone. Connect refused is ``NO_LISTENER``. A connection that
    accepts gets one ``Identify`` (tag 4, empty payload) and
    ``PROBE_BUDGET_S`` to answer (tag 5, JSON): answered is ``LISTENER``,
    everything else is ``SILENT``. Frame layout mirrors
    ``dispatch._keeper_identify`` and ``crates/fno-agents/src/pane_keeper.rs``:
    u8 tag | u32 LE payload length | payload.
    """
    if sock is None:
        return UNREADABLE
    if not sock.exists():
        return ABSENT
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(PROBE_BUDGET_S)
    try:
        s.connect(str(sock))
    except OSError as exc:
        # Triage by errno: ECONNREFUSED is the one error that PROVES nothing
        # accepts; ENOENT here means the file vanished after the exists()
        # check, which is the absent shape. Any other errno - permission,
        # resource exhaustion - leaves the listener unproven.
        if exc.errno == _errno.ECONNREFUSED:
            return NO_LISTENER
        if exc.errno == _errno.ENOENT:
            return ABSENT
        return UNREACHABLE
    # Everything AFTER a successful connect is the conversation, and every
    # way it can fail - recv timeout, reset, close, garbage - is SILENT.
    # ``socket.timeout`` is an OSError subclass; reading the whole block as
    # no_listener turned a wedged keeper into a death certificate.
    try:
        s.sendall(b"\x04" + (0).to_bytes(4, "little"))
        buf = b""
        deadline = time.monotonic() + PROBE_BUDGET_S
        while time.monotonic() < deadline:
            chunk = s.recv(4096)
            if not chunk:
                return SILENT  # closed without answering
            buf += chunk
            while len(buf) >= 5:
                tag = buf[0]
                length = int.from_bytes(buf[1:5], "little")
                if len(buf) < 5 + length:
                    break
                if tag == 5:  # IdentifyReply
                    try:
                        import json

                        json.loads(buf[5 : 5 + length])
                        return LISTENER
                    except ValueError:
                        return SILENT
                buf = buf[5 + length :]
        return SILENT  # accepted and stayed quiet past the bound
    except OSError:
        return SILENT
    finally:
        s.close()


def keeper_verdict(obs: KeeperObs, *, grace_s: Optional[float] = None) -> tuple[str, str]:
    """``(verdict, reason)`` - pure over one observation, so tests need no
    live processes.

    REAP requires all three arms, each a POSITIVE reading; anything else is
    LEAVE and the reason names the arm that refused. ``grace_s`` defaults to
    the module constant read at CALL time (a def-time bind would freeze a
    test's monkeypatch out)."""
    grace_s = REAP_MIN_AGE_S if grace_s is None else grace_s
    if obs.sock_state not in REAPABLE_SOCK_STATES:
        if obs.sock_state == SILENT:
            return LEAVE, (
                "socket accepted and stayed silent - silence never proves death"
            )
        if obs.sock_state == UNREADABLE:
            return LEAVE, "argv declares no --sock, so the socket arm cannot read"
        if obs.sock_state == UNPROBED:
            return LEAVE, "sweep probe budget spent before this keeper was probed"
        if obs.sock_state == UNREACHABLE:
            return LEAVE, (
                "connect failed without refusing - the listener is unproven, not dead"
            )
        return LEAVE, f"socket has a live listener ({obs.sock})"
    if not obs.registry_ok:
        return LEAVE, "registry unreadable, so the claim arm cannot read - no reap"
    if obs.claimed_by is not None:
        return LEAVE, f"registry row {obs.claimed_by} claims this keeper"
    if obs.age_s <= grace_s:
        return LEAVE, f"age {obs.age_s:.0f}s <= grace {grace_s:.0f}s - a fresh keeper is a keeper"
    return REAP, (
        f"socket {obs.sock_state}, no registry row claims it, "
        f"age {obs.age_s:.0f}s > grace {grace_s:.0f}s"
    )


@dataclass
class KeeperLaneResult:
    observations: list[KeeperObs] = field(default_factory=list)
    #: verdict per pid, decided by :func:`keeper_verdict`.
    verdicts: dict[int, tuple[str, str]] = field(default_factory=dict)
    broken_reason: Optional[str] = None

    @property
    def broken(self) -> bool:
        return self.broken_reason is not None

    @property
    def reapable(self) -> list[KeeperObs]:
        # A broken lane reaps nothing, so a partial run's REAP verdicts made
        # before the failure never surface as collectable.
        if self.broken:
            return []
        return [o for o in self.observations if self.verdicts[o.pid][0] == REAP]

    def to_json(self) -> dict:
        return {
            "broken": self.broken,
            "broken_reason": self.broken_reason,
            "keepers": [
                {
                    "pid": o.pid,
                    "lane": o.lane,
                    "session": o.session,
                    "sock": str(o.sock) if o.sock else None,
                    "cwd": o.cwd,
                    "age_s": round(o.age_s),
                    "child_pids": list(o.child_pids),
                    "sock_state": o.sock_state,
                    "claimed_by": o.claimed_by,
                    "verdict": self.verdicts[o.pid][0],
                    "reason": self.verdicts[o.pid][1],
                }
                for o in self.observations
            ],
        }


def _parse_argv(argv: list[str]) -> tuple[Optional[str], Optional[Path]]:
    """``(session, sock)`` off the keeper's own command line."""
    session: Optional[str] = None
    sock: Optional[Path] = None
    it = iter(argv)
    for arg in it:
        if arg == "--session":
            session = next(it, None)
        elif arg == "--sock":
            value = next(it, None)
            sock = Path(value) if value else None
    return session, sock


def discover(
    *,
    now_s: Optional[float] = None,
    iter_fn: Optional[Callable[..., Iterable[dict]]] = None,
    registry_path: Optional[Path] = None,
    grace_s: Optional[float] = None,
    sock_probe_fn: Optional[Callable[[Optional[Path]], str]] = None,
) -> KeeperLaneResult:
    """Enumerate keepers, probe their sockets, join the registry, and decide.

    Read-only. The kill lives in :func:`reap_keepers`, behind ``--apply-all``
    at the verb."""
    now_s = now_s if now_s is not None else time.time()
    iter_fn = iter_fn or iter_processes
    probe = sock_probe_fn or sock_state_of
    result = KeeperLaneResult()
    # monotonic, never now_s: the sweep budget bounds WORK TIME, and an
    # injected epoch must not arm or spend it.
    probe_deadline = time.monotonic() + SWEEP_BUDGET_S

    try:
        import psutil
    except ImportError as exc:  # pragma: no cover - psutil is a hard dep
        result.broken_reason = f"psutil unavailable: {exc}"
        return result

    # The claim net: any of the keeper's OWN pid or its hosted children
    # appearing in a row's ``pid`` or ``keeper_child_pid`` means a registry row
    # still claims work this keeper hosts. A claimed keeper is never touched,
    # so the net is cast wide - the wide direction is LEAVE.
    claimed: dict[int, str] = {}
    registry_ok = True
    registry_error: Optional[str] = None
    try:
        from fno.agents.registry import load_registry

        loaded = load_registry(registry_path)
        for row in loaded:
            for pid in (getattr(row, "pid", None), getattr(row, "keeper_child_pid", None)):
                if isinstance(pid, int):
                    claimed[pid] = row.name
        # A forward read SKIPS rows it cannot represent and announces them via
        # ``complete=False`` (``LoadedRegistry``). Treating the retained rows
        # as a whole census would read an omitted claiming row as "unclaimed"
        # and hand a live keeper to the kill - the fail-closed arm covers this
        # exactly as it covers a raised read.
        if not getattr(loaded, "complete", True):
            registry_ok = False
            registry_error = (
                "registry read is incomplete: row(s) were skipped this fno "
                "cannot represent, so the claim census is not whole"
            )
    except Exception as exc:  # noqa: BLE001 - a damaged registry must not read as empty
        registry_ok = False
        registry_error = str(exc)

    try:
        for info in iter_fn():
            argv = info.get("cmdline") or []
            if not argv:
                continue
            exe = str(argv[0]).rsplit("/", 1)[-1]
            if exe != KEEPER_BIN_NAME:
                continue
            lane = next((v for flag, v in LANE_FLAGS.items() if flag in argv), None)
            if lane is None:
                continue
            pid = info.get("pid")
            if not isinstance(pid, int):
                continue
            session, sock = _parse_argv([str(a) for a in argv])
            try:
                create = info.get("create_time") or now_s
                age_s = max(0.0, now_s - create)
            except (TypeError, ValueError):
                age_s = 0.0
            try:
                children = tuple(
                    c.pid for c in psutil.Process(pid).children() if c.pid != pid
                )
            except Exception:  # noqa: BLE001 - unreadable children default empty
                children = ()
            if info.get("cwd"):
                cwd = info.get("cwd")
            else:
                try:
                    cwd = psutil.Process(pid).cwd()
                except Exception:  # noqa: BLE001 - unreadable cwd is not a verdict
                    cwd = None
            claimed_by = claimed.get(pid)
            if claimed_by is None and registry_ok:
                claimed_by = next(
                    (name for child in children for name in [claimed.get(child)] if name),
                    None,
                )
            # A cheap local check first: an absent socket needs no probe at
            # all, and spending the sweep budget on it would starve keepers
            # whose state only a probe can read.
            state = ABSENT if sock is not None and not sock.exists() else None
            if state is None:
                if time.monotonic() >= probe_deadline:
                    state = UNPROBED
                else:
                    state = probe(sock)
            obs = KeeperObs(
                pid=pid,
                lane=lane,
                sock=sock,
                session=session,
                cwd=str(cwd) if cwd else None,
                age_s=age_s,
                child_pids=children,
                sock_state=state,
                claimed_by=claimed_by,
                registry_ok=registry_ok,
            )
            result.observations.append(obs)
            result.verdicts[pid] = keeper_verdict(obs, grace_s=grace_s)
    except Exception as exc:  # noqa: BLE001 - enumeration failure withholds the verdict
        result.broken_reason = f"keeper enumeration failed: {exc}"
        return result

    if not registry_ok:
        # Named once at the result level too: a run whose every verdict reads
        # "registry unreadable" must say WHY at the top, not once per row.
        result.broken_reason = f"registry unreadable: {registry_error}"
    return result


def _is_dead(pid: int) -> bool:
    """True when the pid is gone OR a zombie.

    ``os.kill(pid, 0)`` cannot tell a zombie from a live process: both answer,
    because the pid survives until the PARENT waits. A group-killed keeper
    whose spawning server is still up (the test harness, or a mux server mid-
    teardown) sits zombie until that parent reaps it, and reading it as alive
    made the receipt say "reaped 0 keeper(s)" over a keeper the group signal
    did end - measured in the planted control. A zombie holds no pty and no
    socket; it is dead for every purpose this lane has. An unreadable status
    errs ALIVE: the receipt only claims what it knows."""
    import psutil

    try:
        return psutil.Process(pid).status() == psutil.STATUS_ZOMBIE
    except psutil.NoSuchProcess:
        return True
    except Exception:  # noqa: BLE001 - unreadable status is not a death proof
        return False


def reap_keepers(
    result: KeeperLaneResult, *, kill_fn: Optional[Callable[[int], bool]] = None
) -> tuple[list[int], list[int]]:
    """Kill exactly the REAP verdicts and their hosted children. Returns
    ``(keeper_pids, child_pids)`` actually reaped.

    The keeper calls ``setsid`` (``pane_keeper.rs:330``): it is its own
    session AND process-group leader, so one group signal ends the keeper and
    everything it hosts together. Measured in the planted control: killing
    the children first makes the keeper observe the child's death and exit on
    its own before its own signal lands, and the receipt then reads
    "reaped 0 keeper(s)" over a pid we did end - the died-on-its-own
    miscredit ``orphans._kill`` exists to refuse. Any survivor of the group
    signal falls back to the per-pid term-then-kill escalation. A broken lane
    reaps nothing: an instrument that cannot see its own ground has not
    earned a kill signal."""
    import os
    import signal

    from fno.agents.orphans import _kill

    kill = kill_fn or _kill
    if result.broken:
        return [], []
    keepers: list[int] = []
    children: list[int] = []
    for obs in result.reapable:
        try:
            os.killpg(obs.pid, signal.SIGKILL)
        except OSError:
            pass  # gone already, or not a group leader - the per-pid arm decides
        for child in obs.child_pids:
            if _is_dead(child) or (kill(child) and _is_dead(child)):
                children.append(child)
        if _is_dead(obs.pid) or (kill(obs.pid) and _is_dead(obs.pid)):
            keepers.append(obs.pid)
    return keepers, children


def render(result: KeeperLaneResult) -> str:
    """Every finding names its verdict, its reason, and (for a reapable one)
    the command that collects it. Refusals cite their own evidence."""
    lines = [f"keeper lane: {len(result.observations)} candidate(s)"]
    if result.broken:
        # The reason rides ABOVE the table: the rows are named with their LEAVE
        # verdicts, and nothing is reapable while the lane is broken.
        lines.append(f"verdict withheld ({result.broken_reason})")
    for obs in result.observations:
        verdict, reason = result.verdicts[obs.pid]
        sock = str(obs.sock) if obs.sock else "-"
        lines.append(
            f"  pid {obs.pid:<7} {obs.lane:6} age {obs.age_s / 3600:5.1f}h "
            f"{obs.sock_state:11} {verdict:5} {reason}  sock={sock}"
        )
    reapable = result.reapable
    if reapable:
        pids = ", ".join(str(o.pid) for o in reapable)
        lines.append(
            f"reapable: {len(reapable)}  (dry run; "
            f"`fno agents watchdog --only keeper --apply-all` collects: {pids})"
        )
    else:
        lines.append("reapable: 0")
    return "\n".join(lines)
