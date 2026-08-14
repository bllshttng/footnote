"""Find processes that outlived whatever started them, and say so out loud.

The measurement this exists for: 73 `yes > /dev/null` at PPID 1, 797% CPU,
71 core-hours in one night, plus a `grep -rn` at 64% and a set of background
tasks whose session had exited. The guard in ``hooks/bg-process-guard.py``
prevents the first class at creation time. Nothing can refuse the other two at
creation time, so this sweep is what covers them, and it is the only
path-agnostic net in the design: a test fixture, a non-Claude harness, and a
hook that leaks its own work all land here.

Two rules shape this module.

**Assert a positive marker, never an absence.** A sweep reporting zero orphans
has two explanations, and a clean machine looks exactly like a dead instrument.
So the sweep plants its own orphans first and must find them before it may
report a count. Each arm of the predicate gets its own probe that cannot
satisfy the other arm; a control clearing an easier bar than a real finding
clears is the same lie as the absence assertion it replaced.

**Never `ps ... | awk`.** That pipeline returned count=0 twice during the
investigation, because the harness truncated 1189 lines and the pipeline
counted the summary. ``psutil.process_iter`` returns objects, so there is no
text stream to truncate.
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import tempfile
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Iterator, Optional

#: Name prefix that makes a process ours by construction. `fno agents orphans
#: --reap` kills only these, which is why the guard's refusal text hands back
#: `exec -a fno-load-...` rather than a bare `timeout`.
FNO_PREFIX = "fno-"

#: A named orphan younger than this is left alone. A worker that just launched
#: something is not an orphan yet, it is a worker.
REAP_MIN_AGE_S = 600

PROBE_LIFETIME_S = 30

#: Name a sweep gives its own NAME probe. Excluded from findings so two sweeps
#: running inside one probe lifetime do not report each other's controls, which
#: the shared hourly stamp across worktrees makes likely.
PROBE_PREFIX = FNO_PREFIX + "orphan-probe-"

#: The same marker for the CWD probe, deliberately WITHOUT the `fno-` prefix so
#: the NAME arm still cannot claim it and `--reap` still cannot kill it. An
#: unmarked CWD probe is a plain `sleep` sitting in the repo root at PPID 1,
#: which is precisely what the CWD arm reports, so a concurrent sweep listed the
#: other one's control as an orphan - the leak the NAME marker above was added
#: to close, left open on the arm that needed it more.
CWD_PROBE_PREFIX = "orphan-probe-cwd-"

#: Footnote's own daemons. Each runs detached at PPID 1 by design, so PPID 1
#: says nothing about whether one leaked, and `was_renamed` is already False for
#: them so `--reap` could never touch one. Matched on argv[0] exactly, never by
#: prefix, so a renamed `fno-agents-daemon-load` is still a finding.
OWN_DAEMONS = frozenset({"fno-agents-daemon", "fno-agents-worker"})


def display_name(info: dict) -> str:
    """What an operator sees in `top`: argv[0], not the executable.

    MEASURED 2026-08-13, and it corrects the assumption this module was
    designed on. After `exec -a fno-load-test sleep 20`, `ps -o comm` prints
    `fno-load-test` but ``psutil.name()`` returns `sleep`: psutil reads the
    executable on macOS, and the rename lands in argv[0] alone. Attributing on
    ``name()`` made the NAME arm unfirable, and the probe is what caught it.
    """
    argv = info.get("cmdline") or []
    if argv and argv[0]:
        return argv[0].rsplit("/", 1)[-1]
    return info.get("name") or ""


def was_renamed(info: dict) -> bool:
    """True when argv[0] does not match the executable, i.e. someone relabelled
    this process on purpose.

    Load-bearing for the reap. `fno-agents` and `fno-mux` are real binaries
    whose argv[0] IS their name, and a bare `fno-` prefix test would hand them
    to the killer. A deliberate rename cannot happen by accident.
    """
    name = info.get("name") or ""
    return bool(name) and display_name(info) != name


@dataclass
class Finding:
    pid: int
    name: str
    exe_name: str
    renamed: bool
    cmdline: str
    cwd: Optional[str]
    cpu_seconds: float
    age_seconds: float
    start_time: float
    arm: str  # NAME | CWD

    @property
    def avg_cpu_percent(self) -> float:
        """Mean CPU over the process's whole life.

        Deliberately not ``cpu_percent()``: that needs a sampling interval, and
        a sweep that sleeps to sample is a sweep nobody runs at session start.
        A mean over lifetime is also the honest number for an orphan - the
        question is how much it has cost, not what it is doing this instant.
        """
        if self.age_seconds <= 0:
            return 0.0
        return 100.0 * self.cpu_seconds / self.age_seconds

    @property
    def core_hours(self) -> float:
        return self.cpu_seconds / 3600.0

    def reapable(self) -> bool:
        return (
            self.renamed
            and self.name.startswith(FNO_PREFIX)
            and self.age_seconds > REAP_MIN_AGE_S
        )


@dataclass
class ScanResult:
    total: int = 0
    #: Processes whose ppid is a reaper. Named for the CONDITION, not for pid 1:
    #: the label `ppid=1` was hardcoded inside a design whose whole point is that
    #: the reaper is measured, so on a subreaper host it printed `ppid=1` over a
    #: count of something else.
    orphaned: int = 0
    same_uid: int = 0
    census_excluded: int = 0
    #: Footnote's own long-lived daemons, counted rather than listed. Never
    #: silently dropped: an unnamed exclusion is the absence trap this module
    #: exists to refuse.
    daemons_excluded: int = 0
    findings: list[Finding] = field(default_factory=list)
    reaped: list[Finding] = field(default_factory=list)
    control: dict[str, bool] = field(default_factory=dict)
    broken_reason: Optional[str] = None
    #: Whether `--reap` was asked for. `render` must not print a `reaped:` line
    #: on a plain report run: "reaped: 0" there reads as an attempted kill that
    #: found nothing, when no kill was ever attempted.
    reap_requested: bool = False

    @property
    def broken(self) -> bool:
        return self.broken_reason is not None


# ─── enumeration ────────────────────────────────────────────────────────────


def reaper_set(reaper: int) -> set[int]:
    """The ppids that mean "this process lost its parent".

    A subreaper claims only its OWN descendants. PID 1 stays the reaper of last
    resort for everything else, so a host with `systemd --user` has TWO such
    ppids live at once. Measuring one reaper and filtering `ppid != reaper`
    dropped every orphan under the other, and the sweep still printed a green
    control and a positive count. That is this module's own thesis failing in
    its own terms: a count that is wrong is worse than a withheld one.
    """
    return {reaper, 1}


def _iter_processes(reaper: int = 1) -> Iterator[dict]:
    """Every process this uid can read, as plain dicts.

    Isolated behind one function so the tests can substitute a fabricated table
    and exercise the arms the live probes cannot reach (foreign uid, census
    membership, the reap age gate).
    """
    import psutil

    reapers = reaper_set(reaper)
    attrs = ["pid", "ppid", "name", "cmdline", "uids", "create_time", "cpu_times"]
    for proc in psutil.process_iter(attrs):
        info = dict(proc.info)
        info["cwd"] = None
        # cwd() is a separate syscall and the common failure (AccessDenied on a
        # process this uid cannot inspect) must not drop the row: a process
        # with no readable cwd can still match the NAME arm.
        try:
            if info.get("ppid") in reapers:
                info["cwd"] = proc.cwd()
        except Exception:  # noqa: BLE001 -- unreadable cwd is not a verdict
            pass
        yield info


def _repo_roots() -> list[str]:
    """Paths whose descendants are attributable to this project.

    The canonical checkout covers `.claude/worktrees/*` because those live
    inside it. `git worktree list` adds the rest: an externally-based worktree
    under `config.paths.worktrees_base` is beneath neither the checkout nor a
    literal `.claude/worktrees/` path, so a path test alone reported a clean
    machine on those projects.
    """
    roots: list[str] = []
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            capture_output=True, text=True, timeout=5,
        )
        common = out.stdout.strip()
        if common:
            roots.append(str(Path(common).parent))
    except Exception:  # noqa: BLE001 -- outside a repo is not an error here
        pass
    # Externally-based worktrees are NOT under the canonical root and carry no
    # literal `.claude/worktrees/` in their path. `config.paths.worktrees_base`
    # puts them at <base>/<repo>/<name>, and the harness fallback at
    # ~/.fno/worktrees/... On such a project an orphan sitting in its own
    # worktree matched neither arm, and the sweep reported a clean machine.
    try:
        listing = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            capture_output=True, text=True, timeout=5,
        )
        for line in listing.stdout.splitlines():
            if line.startswith("worktree "):
                path = line.split(" ", 1)[1].strip()
                if path and path not in roots:
                    roots.append(path)
    except Exception:  # noqa: BLE001 -- a listing failure narrows scope, never widens
        pass
    return roots


def _census_pids() -> Optional[set[int]]:
    """PIDs of live workers that are supposed to exist.

    Load-bearing, not defensive. A `claude --bg` worker is detached to PPID 1
    and its cwd IS a worktree, so without this exclusion every legitimate
    worker on the machine reports as an orphan and the first `--reap` takes
    down the fleet.
    """
    try:
        from fno.agents.spawn_gate import census

        return {w.pid for w in census().workers if w.pid}
    except Exception:  # noqa: BLE001
        # None, never an empty set. An empty set reads as "no live workers" and
        # hands every legitimate `claude --bg` process to `--reap`, which
        # SessionStart runs unattended. A census this module cannot read is a
        # broken instrument, and a broken scan reaps nothing.
        return None


def _attribute(info: dict, roots: Iterable[str]) -> Optional[str]:
    """Which arm claims this process, or None.

    Exactly two arms. An earlier draft had a third matching `.claude/worktrees/`
    in argv; CWD already catches everything it would, and a third arm is a
    third thing the positive control has to prove.

    There is no CPU floor here on purpose. CPU is reported on every finding and
    gates none of them: specimen 3 burned no CPU at all, and a floor is also a
    bar a probe cannot clear without burning real CPU to clear it.
    """
    if was_renamed(info) and display_name(info).startswith(FNO_PREFIX):
        return "NAME"
    cwd = info.get("cwd")
    if cwd:
        if "/.claude/worktrees/" in cwd:
            return "CWD"
        for root in roots:
            if cwd == root or cwd.startswith(root.rstrip("/") + "/"):
                return "CWD"
    return None


# ─── the positive control ───────────────────────────────────────────────────


def _spawn_probe(rename_to: Optional[str], cwd: str) -> Optional[int]:
    """Start a `sleep` that is orphaned to PPID 1, and return its pid.

    The parent shell exits immediately after backgrounding the child, so the
    child is reparented exactly the way a real orphan is. `exec -a` is bash
    only; zsh has no such flag, which is also why the guard's refusal text
    spells out `bash -c`.
    """
    bash = shutil.which("bash")
    if not bash:
        return None
    # The child's stdout MUST be redirected, not inherited. An inherited pipe
    # stays open for the child's whole life, so `subprocess.run` blocks until
    # the probe's own `sleep` ends, hits the timeout, and reports a healthy
    # machine as scan-broken.
    tail = f"sleep {PROBE_LIFETIME_S} >/dev/null 2>&1 & echo $!"
    script = f"exec -a {rename_to} {tail}" if rename_to else tail
    try:
        out = subprocess.run(
            [bash, "-c", script], cwd=cwd, capture_output=True, text=True, timeout=10
        )
        return int(out.stdout.strip())
    except Exception:  # noqa: BLE001
        return None


def _await_orphaned(pid: Optional[int], timeout_s: float = 3.0) -> Optional[int]:
    """The pid an orphan reparents to on this host, measured not assumed.

    Returns that reaper's pid, or None when the probe never settled.

    PID 1 is a macOS assumption. A Linux host where `systemd --user` (or any
    ancestor that called PR_SET_CHILD_SUBREAPER, which some containers and
    multiplexers do) is the reaper reparents an orphan to THAT manager. A
    literal `== 1` made both probes fail there, so every hourly sweep printed
    `verdict withheld (scan-broken)` forever with no way to quiet it. Hourly
    noise nobody can act on is how a sweep gets ignored.

    `_spawn_probe` already waits for the parent shell to exit, so the probe is
    reparented by the time we hold its pid. Reparenting is still not
    instantaneous, hence waiting for the same answer twice rather than trusting
    the first read.
    """
    if pid is None:
        return None
    import psutil

    deadline = time.time() + timeout_s
    last = None
    while time.time() < deadline:
        try:
            ppid = psutil.Process(pid).ppid()
        except Exception:  # noqa: BLE001
            return None
        if ppid and ppid != os.getpid() and ppid == last:
            return ppid
        last = ppid
        time.sleep(0.05)
    return None


def _pid_alive(pid: int) -> bool:
    """True when the pid still exists.

    EPERM means the process is there and this uid cannot signal it, which is
    ALIVE. Reading a bare OSError as death makes both the kill control and the
    reap re-check answer "killed" for a process still running.
    """
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return True


def _kill(pid: int) -> bool:
    """SIGTERM, then SIGKILL if it is still there. Shared by the reap and by
    probe cleanup, so every run exercises the kill path that `--reap` uses.

    Returns whether OUR signal reached the process. A pid that exited on its
    own between the scan and the reap raises ProcessLookupError here, and the
    caller's liveness re-check then sees a dead process and credits a kill this
    sweep never made. "reaped: 1" for a process that died by itself is the same
    receipt-that-lies this module exists to refuse, so the caller needs both
    halves: we signalled it, AND it is gone.
    """
    try:
        os.kill(pid, signal.SIGTERM)
    except Exception:  # noqa: BLE001
        return False
    for _ in range(20):
        time.sleep(0.1)
        if not _pid_alive(pid):
            return True
    try:
        os.kill(pid, signal.SIGKILL)
    except Exception:  # noqa: BLE001
        return True  # our SIGTERM landed; only the follow-up failed
    # SIGKILL is not instantaneous. Returning here with no grace made the
    # caller's liveness re-check race it, and a probe that ignored SIGTERM for
    # the full window reported `kill arm FAILED` microseconds before it died.
    for _ in range(20):
        if not _pid_alive(pid):
            return True
        time.sleep(0.05)
    return True


# ─── the scan ───────────────────────────────────────────────────────────────


def scan(reap: bool = False, skip_probe: Optional[str] = None) -> ScanResult:
    """Plant the controls, enumerate, attribute, and optionally reap.

    ``skip_probe`` is `name` or `cwd` and exists so an operator can break one
    arm on purpose and watch the verdict be withheld. Without that, "the
    control passed" is itself an unfalsifiable claim.
    """
    result = ScanResult(reap_requested=reap)
    if skip_probe not in (None, "name", "cwd"):
        # An unrecognised value silently skipped nothing, the scan came back
        # healthy, and an operator read that as "the control works". A
        # falsifier that cannot fail is the shape this module's docstring
        # refuses, and `FNO_ORPHANS_SKIP_PROBE=NAME` is one typo away.
        result.broken_reason = (
            f"FNO_ORPHANS_SKIP_PROBE={skip_probe!r} is not 'name' or 'cwd'"
        )
        return result

    roots = _repo_roots()
    if not roots:
        # No git root means the CWD arm has no territory. The first fix here
        # fell back to os.getcwd(), which made every PPID-1 process under $HOME
        # a finding when the verb ran from a home directory: a wall of noise
        # from the one command whose output has to stay trustworthy. Inventing
        # territory to keep a control green is the absence trap wearing a
        # different hat, so refuse and say which arm has nowhere to look.
        result.broken_reason = "no git root here, so the CWD arm has no territory"
        return result
    repo_root = roots[0]
    # One fixed, empty directory rather than a fresh mkdtemp per run. A unique
    # temp dir has to be removed, and a sweep killed at the SessionStart time
    # bound never reaches its own cleanup, so it would leak one directory an
    # hour. A SIGKILL cannot be handled in-process at all, so the fix is to
    # have nothing to clean up.
    scratch = Path(tempfile.gettempdir()) / "fno-orphan-probe"
    try:
        scratch.mkdir(exist_ok=True)
    except Exception:  # noqa: BLE001 -- fall back to a dir that surely exists
        scratch = Path(tempfile.gettempdir())
    nonce = uuid.uuid4().hex[:4]

    # NAME probe: renamed, and parked OUTSIDE any repo so the CWD arm cannot
    # carry it. Giving it the repo root as cwd (the first draft) would let CWD
    # match first and prove nothing about names.
    name_pid = (
        None if skip_probe == "name"
        else _spawn_probe(f"{PROBE_PREFIX}{nonce}", str(scratch))
    )
    # CWD probe: in the repo, and marked with a prefix that carries no `fno-`,
    # so only the CWD arm can still claim it while a concurrent sweep can tell
    # it apart from a real orphan.
    cwd_pid = (
        None if skip_probe == "cwd"
        else _spawn_probe(f"{CWD_PROBE_PREFIX}{nonce}", repo_root)
    )

    census_pids = _census_pids()
    if census_pids is None:
        # Refuse rather than proceed with no exclusion set. Every legitimate
        # `claude --bg` worker is at PPID 1 with a worktree cwd, so an
        # unreadable census turns the fleet into findings, and SessionStart
        # runs `--reap` unattended.
        result.broken_reason = "census unreadable"
        census_pids = set()
    now = time.time()
    uid = os.getuid()
    found_name_probe = False
    found_cwd_probe = False

    try:
        # Inside the try, with the enumeration. `_await_orphaned` imports psutil
        # and touches live processes, and an exception out here escaped scan()
        # as a traceback: exit 1, empty stdout, stderr swallowed by the hook,
        # stamp already written. Silence for a full window is the one outcome
        # this module exists to make impossible.
        # The reaper is MEASURED from our own probe, never assumed to be 1.
        # Either probe answers it; they reparent the same way.
        reaper = _await_orphaned(name_pid) or _await_orphaned(cwd_pid)
        if reaper is None:
            # No probe settled, so there is no reaper to compare against and
            # every ppid test below would be meaningless. Say so instead of
            # counting against a guess.
            #
            # Fall THROUGH rather than return. The kill block below is what
            # cleans up our own two probes, so returning here left them running
            # for their full lifetime on exactly the host where reparenting is
            # already misbehaving: the one path in this module that leaks the
            # thing the module hunts. Both arms stay False, so the verdict is
            # still withheld, and `control` now names which arms failed instead
            # of reporting nothing at all.
            result.broken_reason = "no probe reparented, so the reaper is unknown"
        reapers = reaper_set(reaper) if reaper is not None else set()
        for info in _iter_processes(reaper) if reaper is not None else ():
            result.total += 1
            if info.get("ppid") not in reapers:
                continue
            result.orphaned += 1
            uids = info.get("uids")
            proc_uid = uids[0] if uids else None
            if proc_uid != uid:
                continue
            result.same_uid += 1
            pid = info.get("pid")
            # A row with no pid is unusable: it cannot be excluded by census,
            # reported by number, or killed. Dropping it beats carrying an
            # Optional through every downstream field.
            if not isinstance(pid, int):
                continue
            if pid in census_pids or pid == os.getpid():
                result.census_excluded += 1
                continue
            arm = _attribute(info, roots)
            if arm is None:
                continue
            if display_name(info).startswith(
                (PROBE_PREFIX, CWD_PROBE_PREFIX)
            ) and pid not in (name_pid, cwd_pid):
                continue  # another sweep's live probe, not an orphan
            if display_name(info) in OWN_DAEMONS:
                # PPID 1 is these daemons' NORMAL state, so their presence
                # carries no signal at all, and every restart mints a fresh
                # seen-key that speaks again through --quiet-unless-new. An
                # hourly report nobody can act on is how a sweep gets ignored.
                # Counted, never dropped in silence.
                result.daemons_excluded += 1
                continue

            create = info.get("create_time") or now
            times = info.get("cpu_times")
            cpu = (times.user + times.system) if times else 0.0
            finding = Finding(
                pid=pid,
                name=display_name(info),
                exe_name=info.get("name") or "",
                renamed=was_renamed(info),
                cmdline=" ".join(info.get("cmdline") or []) or (info.get("name") or ""),
                cwd=info.get("cwd"),
                cpu_seconds=cpu,
                age_seconds=max(0.0, now - create),
                start_time=create,
                arm=arm,
            )
            if pid == name_pid:
                found_name_probe = arm == "NAME"
                continue
            if pid == cwd_pid:
                found_cwd_probe = arm == "CWD"
                continue
            result.findings.append(finding)
    except Exception as exc:  # noqa: BLE001
        # Never overwrite a reason already set. `census unreadable` is set
        # before this and names a fixable cause; replacing it with a generic
        # enumeration error costs the operator the one line that says what to
        # do. First reason wins, because the first one is the root.
        if result.broken_reason is None:
            result.broken_reason = f"enumeration failed: {exc}"

    # The kill control is measured AFTER the kill, never before it. Setting it
    # from "a probe was spawned" asserts an easier thing than the real kill
    # does, which is the exact lie this module exists to refuse: `_kill`
    # swallows every exception, so a broken kill path would report healthy
    # while `--reap` silently no-opped and still printed `reaped: N`.
    # Both halves here too, for the same reason the reap needs both: a probe
    # that exited on its own certifies nothing about the kill path, and reading
    # only "it is gone now" lets a completely broken `_kill` report healthy on
    # any short-lived probe. The control must prove WE ended it.
    killed_ok = True
    for pid in (name_pid, cwd_pid):
        if pid is None:
            continue
        signalled = _kill(pid)
        killed_ok = killed_ok and signalled and not _pid_alive(pid)

    result.control = {
        "NAME": found_name_probe,
        "CWD": found_cwd_probe,
        "kill": killed_ok and (name_pid is not None or cwd_pid is not None),
    }

    if result.broken_reason is None:
        missing = [
            arm for arm, ok in (
                ("NAME", found_name_probe),
                ("CWD", found_cwd_probe),
                ("kill", result.control["kill"]),
            )
            if not ok
        ]
        if missing:
            result.broken_reason = f"{' and '.join(missing)} arm FAILED"

    # A broken scan reaps nothing. An instrument that cannot see its own probe
    # has not earned a kill signal.
    if reap and not result.broken:
        for finding in result.findings:
            if not finding.reapable():
                continue
            signalled = _kill(finding.pid)
            # BOTH halves, never assumed. An unconditional append printed
            # `reaped: N` for a process still running, so the liveness re-check
            # went in. That closed the still-running direction and left the
            # already-dead one: a pid that exited on its own between the scan
            # and the reap reads as dead, and crediting that is a receipt for a
            # signal nobody sent. The kill control proves the PATH works against
            # a probe we own; it says nothing about this particular signal.
            if signalled and not _pid_alive(finding.pid):
                result.reaped.append(finding)

    result.findings.sort(key=lambda f: f.cpu_seconds, reverse=True)
    return result


def render(result: ScanResult) -> str:
    """Findings first, count last, and no count at all when the scan is broken."""
    lines = [
        f"scan: {result.total} processes, {result.orphaned} reparented, "
        f"{result.same_uid} same-uid, {result.census_excluded} census-excluded, "
        f"{result.daemons_excluded} own-daemon"
    ]
    if result.broken:
        lines.append(f"control: {result.broken_reason}")
        lines.append("verdict withheld (scan-broken)")
        return "\n".join(lines)

    lines.append(
        "control: NAME arm ok, CWD arm ok, kill path ok "
        "(uid, census and reap-age arms are unit-tested, not probed)"
    )
    for f in result.findings:
        lines.append(
            f"  pid {f.pid:<7} {f.name[:24]:<24} {f.avg_cpu_percent:6.1f}% cpu "
            f"{f.core_hours:6.2f} core-h  {f.cwd or '-'}"
        )
    lines.append(f"orphans: {len(result.findings)}")
    if result.reaped:
        names = ", ".join(str(f.pid) for f in result.reaped)
        lines.append(f"reaped: {len(result.reaped)}  (fno-named, age > 10m: {names})")
    elif result.reap_requested and result.findings:
        # Every orphan predating the guard is named whatever it was named, and
        # a name-gated reap can never touch it. Say so rather than letting the
        # gap read as a broken reap.
        lines.append("reaped: 0  (nothing both fno-named and older than 10m)")
    return "\n".join(lines)


def _seen_key(f: Finding) -> str:
    """Identify the PROCESS, not the pid slot, using its START TIME.

    `pid:name` alone filters out a genuinely new orphan that lands on a reused
    pid with the same command name. An age BUCKET fixes that and breaks the
    quiet gate instead: age advances every minute, so a long-lived daemon gets
    a fresh key at every hourly sweep, reads as new forever, and reprints at
    every session start. Start time is stable for the life of the process,
    which is the property the key actually needs.
    """
    return f"{f.pid}:{f.name}:{int(f.start_time)}"


def seen_path() -> Path:
    """Where the already-reported set lives, anchored to the repo root.

    Not a bare relative `.fno/...`: a caller with an unexpected cwd would nest
    a second state directory somewhere it does not belong, which is the
    cwd-anchoring bug class `scripts/ci/check-placement-rule.sh` exists to
    catch. Falls back to cwd only outside a repo, where there is no root to
    anchor to.
    """
    roots = _repo_roots()
    base = Path(roots[0]) if roots else Path.cwd()
    return base / ".fno" / ".orphan-sweep-seen"


def filter_new(result: ScanResult, seen_path: Path) -> bool:
    """Record this scan's findings and answer "is any of them new?".

    The verb always reports everything. This exists for the SessionStart
    nudge, which fires on every session and would otherwise reprint the same
    long-lived third-party daemons forever until someone disables the hook. A
    known finding is not news; an unknown one is.

    Returns True when something here has not been reported before, so the
    caller may stay silent on False. Best-effort: an unreadable or unwritable
    file means "everything is new", which is the loud direction.
    """
    if result.broken:
        # A broken scan WITHHELD its findings: `render` prints the reason and no
        # list. Recording them as reported would silence them on the next
        # healthy sweep, so one census failure could hide a real orphan forever.
        # Leave the file alone and answer loud; the caller speaks on broken too.
        return True
    keys = {_seen_key(f) for f in result.findings}
    try:
        # Split on LINES, never on whitespace. The file is newline-joined and a
        # key carries argv[0], which on macOS is routinely `Foo Helper`. Split
        # on whitespace that key never matched itself again, so a long-lived
        # third-party process read as new at every session start forever - the
        # reprint loop `--quiet-unless-new` exists to stop.
        seen = set(seen_path.read_text(encoding="utf-8").splitlines())
    except Exception:  # noqa: BLE001 -- no memory means report it all
        seen = set()
    try:
        seen_path.parent.mkdir(parents=True, exist_ok=True)
        seen_path.write_text("\n".join(sorted(keys)), encoding="utf-8")
    except Exception:  # noqa: BLE001 -- a read-only .fno never silences a finding
        pass
    return bool(keys - seen)


def to_json(result: ScanResult) -> dict:
    return {
        "scan": {
            "total": result.total,
            "orphaned": result.orphaned,
            "same_uid": result.same_uid,
            "census_excluded": result.census_excluded,
            "daemons_excluded": result.daemons_excluded,
        },
        "control": result.control,
        "broken": result.broken,
        "broken_reason": result.broken_reason,
        "orphans": None if result.broken else [
            {
                "pid": f.pid,
                "name": f.name,
                "cmdline": f.cmdline,
                "cwd": f.cwd,
                "arm": f.arm,
                "avg_cpu_percent": round(f.avg_cpu_percent, 1),
                "core_hours": round(f.core_hours, 3),
                "age_seconds": round(f.age_seconds),
            }
            for f in result.findings
        ],
        "reaped": [f.pid for f in result.reaped],
    }
