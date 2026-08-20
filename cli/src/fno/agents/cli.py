"""`fno agents` Typer subapp.

US1 wires ``ask`` to ``dispatch_ask``. US3 (this revision) replaces the
``list`` stub with a real implementation and adds the new ``logs``
verb. ``ping`` remains a Phase 1 stub until its own user story lands.
"""

from __future__ import annotations

import enum
import json
import os
import sys
from pathlib import Path
from typing import Optional

import typer

from fno.agents.rust_runtime import make_agents_group_cls

agents_app = typer.Typer(
    name="agents",
    help=(
        "Cross-CLI agent lifecycle (claude / codex / gemini): "
        "spawn / watch / list / logs / stop. "
        "To message a peer, use `fno mail send <name>` (or the `/mail` skill)."
    ),
    no_args_is_help=True,
    # Default Rust runtime (Phase 6 W6 / cv-d28b266a): by default this group
    # execs the installed `fno-agents` binary for the verbs it implements, and
    # falls back to the Python dispatch below otherwise. FNO_AGENTS_RUNTIME=rust
    # forces the binary; =python forces this Python path. See rust_runtime.py.
    cls=make_agents_group_cls(),
)

# `fno agents mcp` was the re-homed twin of the top-level `fno mcp` (x-71b6):
# one Typer app registered under two paths, so every leaf was counted and
# maintained twice. The Rust daemon's deliver_envelope shells the top-level
# `fno mcp send`, which is the surviving path; the twin had no caller at all.


class AgentStatusFilter(str, enum.Enum):
    """Rendered family-1 liveness values accepted by ``list --status``."""

    live = "live"
    orphaned = "orphaned"
    unknown = "unknown"


class AgentProgressFilter(str, enum.Enum):
    """Progress-axis values accepted by ``list --progress``.

    A SECOND axis beside ``--status``, not a finer version of it: reachability
    answers "can I reach this process"; progress answers "is it advancing,
    awaiting the operator, parked, or refused" (fno.agents.reachability). The
    two filter independently -- a row filtered `--progress parked` still
    counts toward `--status live`.
    """

    advancing = "advancing"
    awaiting_operator = "awaiting-operator"
    parked = "parked"
    refused = "refused"
    unknown = "unknown"


def _remedy_for(key: str) -> str:
    """The two commands that clear KEY, safest first.

    A refusal that names only its blocker leaves the operator doing archaeology:
    read the lockfile for a pid, run ps, force-release. That was three manual
    steps to undo one crash (x-05be). After the self-clearing recovery above,
    this text is for the case the probe could NOT run - which is exactly when
    nobody is coming to help.
    """
    if key.startswith("dispatch:"):
        # A reservation is NOT reapable inside its TTL, by design: that window
        # is the boot window and `classify_for_sweep` deliberately has no
        # one-shot arm for this key family. Naming reap here sent an operator to
        # a command that provably cannot clear what they are looking at.
        return f"  Clear it:  fno claim release {key} --force --reason '<why>'"
    return (
        f"  Clear it:  fno claim reap --apply      "
        f"(takes it only if no live worker is on the node)\n"
        f"  Override:  fno claim release {key} --force --reason '<why>'"
    )


#: Holder prefix of a reservation THIS verb writes. The targeted clear below is
#: scoped to it: every other producer of a `dispatch:` key has its own launch
#: contract, and `fno backlog advance` in particular relies on that reservation
#: outliving its own exit because it takes no node claim to replace it.
_SPAWN_CLI_HOLDER_PREFIX = "spawn-cli:"

#: `_reclaim_if_provably_dead` bucket meaning "a holder we PROVED is alive".
#: The discriminator between benign dedup and a wedge: somebody is genuinely
#: working, so the refusal is the system behaving correctly and there is nothing
#: for an operator to clear. Every other unrecovered bucket is a wedge.
_HOLDER_ALIVE = "live"


def _reclaim_if_provably_dead(key: str, *, probe=None) -> tuple[str | None, str]:
    """Force-release KEY when its holder is PROVABLY dead.

    Returns ``(prior_holder, bucket)``. ``prior_holder`` is None whenever the
    claim was not cleared, and ``bucket`` says why, so the caller can tell a
    live holder (benign dedup) from one it merely could not measure (a wedge).
    Those deserve opposite refusals: pointing an operator at a force-release for
    a reservation whose spawner is mid-launch is worse advice than none.

    Nothing is cleared on a read failure or a probe failure. An instrument that
    could not run is not a finding, and clearing a claim on one hands a live
    worker's node to a second worker.

    The proof itself is :func:`fno.claims.core.sweep_verdict`, the same single
    authority the reaper uses, called on exactly the one key we were asked
    about. This never sweeps: a dispatch deciding to prune the whole store as a
    side effect is a blast radius nobody asked for.
    """
    from fno.claims.core import (
        RECOVERY_LOCK_SUFFIX,
        force_release_claim,
        sweep_verdict,
    )
    from fno.claims.io import claim_path, claims_root_for, read_claim_file
    from fno.claims.staleness import is_live
    from fno.mutex import acquire_dir_mutex, release_dir_mutex

    path = claim_path(key, root=claims_root_for(key))
    # Take the SAME per-key recovery mutex the reaper holds while it re-verifies
    # and archives, and re-read INSIDE it. Reading, deciding, and releasing
    # outside the lock is a TOCTOU window: force_release_claim drops a claim
    # whatever its holder, so a worker that respawns and re-acquires between the
    # read and the release loses a claim it legitimately owns. timeout_s=0
    # because a dispatch must not block on a peer mid-recovery; losing the race
    # just leaves the refusal standing, which is the safe direction.
    lock = path.with_name(path.name + RECOVERY_LOCK_SUFFIX)
    token = acquire_dir_mutex(lock, 0)
    if token is None:
        return None, "contended"
    try:
        try:
            claim = read_claim_file(path)
        except Exception:  # noqa: BLE001 - unreadable is unproven
            return None, "unreadable"
        if key.startswith("dispatch:"):
            # The reservation's own predicate, deliberately NOT in the shared
            # sweep classifier. `spawn-cli:<pid>` launches a worker and exits, so
            # a dead pid means no launch is in flight from that process. A
            # background sweep must not act on that (the TTL is the boot window,
            # see staleness.classify_for_sweep), but THIS caller is the next
            # dispatcher, standing at the moment of launch, and it takes the node
            # claim itself, which covers the window the reservation protected.
            #
            # ONLY this dispatcher's own holder shape. `fno backlog advance`
            # reserves the same key as `advance:<pid>` and spawns WITHOUT
            # --node, so no node claim is taken and that reservation is the only
            # barrier its booting worker has. Its pid is dead by design too, so
            # a predicate reading dead-pid-and-same-host alone cleared it and
            # launched a second worker onto the node advance had just staffed.
            from fno.claims.hostid import is_same_machine

            # LIVENESS FIRST. A live holder is benign dedup whoever wrote it,
            # and answering `foreign-reservation` there would lose the one
            # discriminator callers use to tell dedup from a wedge - they would
            # print force-release advice against a reservation somebody is
            # actively launching under.
            if not is_same_machine(claim.host, claim.machine_id) or is_live(claim):
                return None, _HOLDER_ALIVE if is_live(claim) else "offhost"
            if not claim.holder.startswith(_SPAWN_CLI_HOLDER_PREFIX):
                return None, "foreign-reservation"
            provably_dead, bucket = True, ""
        else:
            try:
                provably_dead, bucket = sweep_verdict(claim, abandonment_probe=probe)
            except Exception:  # noqa: BLE001 - a probe blowing up clears nothing
                return None, "unprobed"
        if not provably_dead:
            return None, bucket
        try:
            force_release_claim(
                key=key,
                reason=f"holder {claim.holder} (pid {claim.pid}) proven dead at dispatch",
                root=claims_root_for(key),
                holding_recovery_lock=True,
            )
        except Exception:  # noqa: BLE001 - a failed release just leaves the refusal
            return None, "release-failed"
        return claim.holder, ""
    finally:
        release_dir_mutex(lock, token)


def _spawn_guard_decision(
    node_id: str,
    holder: str,
    *,
    ttl: str = "3m",
    no_reserve: bool = False,
    cwd: str | None = None,
    handover_holder: str | None = None,
) -> tuple[dict[str, str], int]:
    """Return the shared family-2 pre-birth verdict without rendering it.

    ``handover_holder``, when given, also takes the ``node:<id>`` claim under
    that holder for the launch window, so the node reads as worked from the
    moment it is dispatched rather than from whenever the worker reaches its
    own ``fno target init``.

    ``no_reserve`` makes this a pure PROBE: it takes no reservation, no node
    claim, and performs no recovery. Every mutation in this function is gated on
    it, so a batch sweep that probes each node one at a time changes nothing
    until it actually launches.
    """
    from fno.claims.cli import _parse_ttl
    from fno.claims.core import CLAIM_UNAVAILABLE, acquire_claim, claim_status
    from fno.claims.io import claims_root_for

    node_key = f"node:{node_id}"
    res_key = f"dispatch:{node_id}"

    try:
        info = claim_status(node_key, root=claims_root_for(node_key))
    except Exception as exc:  # pragma: no cover - claim_status never raises today
        return {
            "verdict": "error",
            "detail": f"claim probe failed ({exc}); not dispatching to avoid a double-launch",
        }, 3
    state = info.get("state")
    if not state:
        return {
            "verdict": "error",
            "detail": "claim status returned no parseable state; not dispatching",
        }, 3
    if state == "corrupted":
        return {
            "verdict": "corrupted",
            "detail": (
                f"node:{node_id} claim is corrupted; force-release or repair before dispatching"
            ),
        }, 0

    from fno.backlog.advance import _observe_node_claim

    observation = _observe_node_claim(
        node_id,
        cwd,
        enforce_failure_limit=not no_reserve,
        emit=not no_reserve,
    )
    common = {
        "holder": observation.holder,
        "truth_status": observation.truth_status,
    }
    if observation.action in ("auto-deferred", "defer-failed"):
        return {
            "verdict": "refused",
            "reason": observation.action,
            **common,
        }, 0
    if observation.blocks_dispatch:
        # A LIVE claim is benign dedup: somebody is genuinely building this and
        # a batch sweep must keep going. A SUSPECT one is a wedge - dead pid,
        # unexpired TTL - and nobody will build the node until it clears. Only
        # the wedge is worth trying to recover, and only on a positive finding.
        if state == "suspect" and not no_reserve:
            # A --no-reserve call is a PROBE. It takes no reservation, so it
            # must take no recovery either: dispatch-node.sh probes once per
            # node across a whole batch, and a probe that archives claims and
            # emits events is a side effect nobody reading "probe" expects.
            #
            # Nothing is lost by waiting. Both shell callers probe and then
            # invoke the real `fno agents spawn`, which runs this guard again
            # WITH a reservation, and the recovery happens there - at the moment
            # of launch, by the caller that is about to launch.
            from fno.claims.cli import _abandonment_probe, read_roster

            # The roster is READ HERE, outside the recovery mutex. Reading it
            # under the lock shells out to the harness while holding a mutex
            # `compare_and_rebind` waits only five seconds for, so a peer's
            # probe could make the worker's own `fno target init` handover fail
            # as claim-held-by-other. Handing the reading in leaves nothing
            # under the lock but a dictionary lookup.
            try:
                reading = read_roster()
            except Exception:  # noqa: BLE001 - an unread roster proves nothing
                reading = None
            prior, _bucket = _reclaim_if_provably_dead(
                node_key, probe=_abandonment_probe(reading)
            )
            if prior is not None:
                _emit_reaped_abandoned(node_id, prior, observation.truth_status)
                observation = _observe_node_claim(
                    node_id,
                    cwd,
                    enforce_failure_limit=not no_reserve,
                    emit=False,
                )
                common = {
                    "holder": observation.holder,
                    "truth_status": observation.truth_status,
                }
                # The cleared claim makes this the first reading with the node
                # free, so the failure-limit arm can fire here for the first
                # time. Report what it decided. Falling through would label an
                # auto-deferred node `already-running` and hand back a
                # force-release remedy that does nothing for it.
                if observation.action in ("auto-deferred", "defer-failed"):
                    return {
                        "verdict": "refused",
                        "reason": observation.action,
                        **common,
                    }, 0
        if observation.blocks_dispatch:
            # A launch-window holder is NOT a wedge. Its claim carries the pid of
            # the `fno agents spawn` process, which exits the moment it has
            # forked the worker, so the claim reads SUSPECT for its whole TTL by
            # construction. The abandonment probe already exempts this holder;
            # rendering it as a wedge here would put the exemption on one of two
            # paths and hand an operator force-release advice for a launch that
            # is proceeding normally.
            from fno.claims.cli import HANDOVER_HOLDER_PREFIX

            in_launch_window = str(observation.holder or "").startswith(
                HANDOVER_HOLDER_PREFIX
            )
            # The remedy is force-release advice, and it is only honest once
            # recovery has been TRIED and failed. A probe takes no recovery, so
            # it says the wedge is recoverable-untried instead and the caller
            # goes on to the real spawn, which recovers or refuses for real.
            # `state` is the PRE-recovery reading, so re-read it here: a node
            # re-claimed by a live worker while the reclaim ran would otherwise
            # render as a wedge with force-release advice against a claim that
            # is now genuinely held.
            try:
                current = claim_status(node_key, root=claims_root_for(node_key)).get("state")
            except Exception:  # noqa: BLE001 - an unreadable probe keeps the first reading
                current = state
            wedged = current == "suspect" and not in_launch_window
            recovery = "not-attempted" if wedged and no_reserve else None
            return {
                "verdict": "already-running",
                "reason": "suspect-claim" if wedged else "live-claim",
                **({"recovery": recovery} if recovery else {}),
                **({"remedy": _remedy_for(node_key)}
                   if wedged and not no_reserve else {}),
                **common,
            }, 0
    if no_reserve:
        return {"verdict": "dispatchable"}, 0

    #: True once this call has cleared a dead spawner's reservation. The node
    #: claim below is the barrier that replaced it, so a failure to take it
    #: means something different on this path than on the ordinary one.
    reservation_recovered = False
    try:
        acquire_claim(
            res_key,
            holder,
            reason=f"bg-dispatch reservation for {node_id}",
            ttl_ms=_parse_ttl(ttl),
            root=claims_root_for(res_key),
        )
    except CLAIM_UNAVAILABLE:
        # A dead spawner's reservation blocks nothing. `spawn-cli:<pid>` is one
        # process that launches and exits, so it cannot come back under a new
        # pid and its TTL protects an empty slot. A queued spawn that never got
        # a slot wedged its node this way for the full three minutes (x-05be).
        #
        # Exactly ONE retry, never a loop: a genuine racing dispatcher still
        # wins the second acquire, and losing twice means the contention is real.
        # The recovery touches the filesystem (a mkdir mutex, an archive move),
        # so it can raise for reasons that have nothing to do with the claim -
        # an unwritable claims dir, for one. Raised inside this handler, that
        # escapes past the sibling `except Exception` below as a traceback where
        # the honest answer is the refusal we already have.
        # ONLY when this caller will replace the barrier it removes. Clearing a
        # dead spawner's reservation is justified by the node claim covering the
        # window instead, and that claim is taken further down only when a
        # `handover_holder` was passed. `fno agents spawn-guard` never passes
        # one, so in its reserving mode this cleared a booting worker's
        # boot-window reservation and held nothing but a reservation of its own.
        if handover_holder:
            try:
                cleared, bucket = _reclaim_if_provably_dead(res_key)
            except Exception:  # noqa: BLE001 - a failed recovery clears nothing
                cleared, bucket = None, "unrecoverable"
        else:
            cleared, bucket = None, "no-replacement-barrier"
        reservation_recovered = cleared is not None
        if cleared is None:
            # A live spawner is mid-launch: benign dedup, and naming a
            # force-release here would be worse advice than none.
            return {
                "verdict": "already-running",
                "reason": "reservation-held",
                **({} if bucket == _HOLDER_ALIVE
                   else {"remedy": _remedy_for(res_key)}),
            }, 0
        try:
            acquire_claim(
                res_key,
                holder,
                reason=f"bg-dispatch reservation for {node_id}",
                ttl_ms=_parse_ttl(ttl),
                root=claims_root_for(res_key),
            )
        except CLAIM_UNAVAILABLE:
            return {
                "verdict": "already-running",
                "reason": "reservation-held",
                "remedy": _remedy_for(res_key),
            }, 0
        except Exception as exc:
            return {
                "verdict": "error",
                "detail": f"could not acquire dispatch reservation {res_key} ({exc})",
            }, 3
    except Exception as exc:
        return {
            "verdict": "error",
            "detail": f"could not acquire dispatch reservation {res_key} ({exc})",
        }, 3
    # x-a7ab visibility barrier: the acquisition is not authoritative until the
    # exact holder is observable on disk. A peer that won a visibility-lagged
    # race launches; this caller returns the durable duplicate receipt.
    try:
        post = claim_status(res_key, root=claims_root_for(res_key))
    except Exception:  # pragma: no cover - claim_status never raises today
        post = {}
    if post.get("holder") != holder:
        return {
            "verdict": "already-running",
            "reason": "duplicate-claim",
            "holder": post.get("holder") or "unknown",
        }, 0
    out = {
        "verdict": "dispatchable",
        "reservation_key": res_key,
        "reservation_holder": holder,
    }
    if handover_holder:
        # THE node claim, not another reservation. dispatch:<id> is a launch-
        # window mutex on a key nobody reads: five workers were spawned with an
        # explicit --node tonight and not one of them was visible to `fno claim
        # status node:<id>`, so four kings read those nodes as free (x-cd1e).
        #
        # --node is the only dispatch path holding the node id as a TYPED
        # argument rather than as prose to be re-derived, which is why the claim
        # belongs here and why there are exactly two producers of this key, not
        # more. The other is `fno target init`, and the worker inherits this
        # claim from it rather than taking a second one.
        #
        # A failure to claim is NOT a refusal to launch. The reservation above
        # already prevents the double dispatch this would also prevent, so
        # turning a claim hiccup into a dead launch would trade a visibility bug
        # for an availability one.
        try:
            acquire_claim(
                node_key,
                handover_holder,
                reason=f"spawn handover window for {node_id}",
                ttl_ms=_parse_ttl(HANDOVER_TTL),
                root=claims_root_for(node_key),
            )
        except CLAIM_UNAVAILABLE as exc:
            # SOMEBODY ELSE HOLDS THE NODE, and that is not a hiccup. The
            # reservation above only dedups other DISPATCHERS, so a session that
            # already claimed this node through its own `fno target init` is
            # invisible to it. Swallowing this as best-effort put a second
            # worker on a node a live session was building, which is the whole
            # failure this PR exists to close.
            return {
                "verdict": "already-running",
                "reason": "live-claim",
                "holder": getattr(exc, "holder", "") or "unknown",
                "detail": f"node:{node_id} is held ({exc}); no worker launched",
            }, 0
        except Exception as exc:  # noqa: BLE001 - visibility is best-effort here
            if reservation_recovered:
                # The ONE combination where nothing is protecting the launch
                # window. Clearing a dead spawner's reservation is safe because
                # this claim covers the window instead - and on this path it
                # did not land. A spawner that forked a worker and exited
                # normally is indistinguishable from one that died, so
                # proceeding here re-opens the double dispatch both barriers
                # exist to close. The reservation carries a 3m TTL and an
                # expired claim is provably dead, so the node self-heals.
                return {
                    "verdict": "error",
                    "detail": (
                        f"recovered a dead spawner's {res_key} but could not take "
                        f"node:{node_id} ({exc}); refusing rather than launching "
                        "with neither barrier held"
                    ),
                }, 3
            out["node_claim_error"] = str(exc)
        else:
            out["node_claim_key"] = node_key
            out["node_claim_holder"] = handover_holder
    return out, 0


#: Lease on the spawn-side node claim. It has to outlive the launch-to-init gap
#: or the node reads free again mid-launch, which is the exact hole this closes;
#: the reservation's 3m is the window for ONE process to fork, not for a harness
#: to boot and reach its first `fno target init`. It stays short because a spawn
#: that dies inside it strands the node until expiry, and an expired claim is
#: provably dead on its own so the wedge self-clears.
HANDOVER_TTL = "15m"


def _release_dispatch_claims(*claims) -> None:
    """Release every claim a failed dispatch took, best-effort.

    One helper for both failure paths. They used to release only the
    reservation, in two copies, and adding the node claim to one of them is how
    a guard ends up on one of N paths.
    """
    from fno.claims.core import release_claim
    from fno.claims.io import claims_root_for

    for pair in claims:
        if pair is None:
            continue
        key, holder = pair
        try:
            release_claim(key, holder, root=claims_root_for(key))
        except Exception:  # noqa: BLE001 - a stuck release must not mask the real error
            pass


def _emit_reaped_abandoned(node_id: str, prior_holder: str, truth_status: str) -> None:
    """Record a self-clearing recovery on the claim-observed stream.

    Without it the event log shows a gap where a refusal used to be, and the
    next operator reading back through a wedge cannot tell "it recovered itself"
    from "nothing ever tried".
    """
    try:
        from fno.agents import events as agent_events
        from fno.backlog.advance import EVENT_CLAIM_OBSERVED

        agent_events.emit(
            EVENT_CLAIM_OBSERVED,
            node_id=node_id,
            claim_verdict="dead_predecessor",
            claim_state="suspect",
            holder=prior_holder,
            truth_status=truth_status,
            action="reaped-abandoned",
        )
    except Exception:  # noqa: BLE001 - telemetry must never block a dispatch
        pass


def _resolve_dispatch_workdir(cwd: str | None, fresh: bool, here: bool) -> Path:
    """Worker launch dir honoring --cwd > --here (caller) > default canonical.

    Mirrors the Rust client's ``effective_worker_cwd`` precedence. x-85fe
    inverted the default (was ab-77b691dc's caller-cwd): a spawn with NO explicit
    cwd source now resolves to the canonical (main) checkout, so the identical
    command behaves the same regardless of where the launcher happens to stand.
    ``--here``/``--in-place`` is the explicit opt-in to keep the caller's cwd.
    ``--fresh`` survives as an accepted no-op alias (the default already resolves
    canonical). A canonical that lands on the caller's own dir is a no-op (no
    redirect note). Only the Python fallback runtime reaches this -- when an
    installed binary auto-routes the verb, the Rust client owns the identical
    precedence.
    """
    del fresh  # accepted no-op alias: the default already resolves canonical.
    if cwd:
        return Path(cwd).resolve()
    caller = Path(os.getcwd()).resolve()
    if here:
        return caller
    from fno.paths import resolve_canonical_repo_root

    # Best-effort: any resolution error (missing git, odd environment) falls
    # back to the caller cwd, the safe side, rather than crashing the dispatch.
    try:
        canonical = resolve_canonical_repo_root().resolve()
    except Exception:
        return caller
    if canonical != caller:
        # Never silent: the redirect note fires on every actual move, default
        # path included (x-85fe Locked Decision 5).
        print(
            f"fno agents: dispatching from canonical main (default) ({canonical}); "
            "pass --here to stay in this worktree",
            file=sys.stderr,
        )
    return canonical


# ---------------------------------------------------------------------------
# Group 2, Task 4.3: `fno agents watch` — observe a held stream-json thread
# ---------------------------------------------------------------------------


def _agents_home_dir() -> Path:
    """The agents home (mirrors dispatch._daemon_rpc resolution)."""
    env = os.environ.get("FNO_AGENTS_HOME")
    if env:
        return Path(env)
    return Path(os.path.expanduser("~")) / ".fno" / "agents"


def _worker_rpc(
    sock_path: Path,
    method: str,
    params: dict,
    *,
    connect_timeout: float = 3.0,
    read_timeout: float = 5.0,
) -> "dict | None":
    """One length-prefixed JSON RPC to a worker socket (NEVER raises).

    Same 4-byte-LE-u32 + JSON framing as dispatch._daemon_rpc, but to an
    arbitrary worker socket (the stream worker serves ``stream.*`` directly).
    Returns the ``result`` dict, or None on any transport/error response.
    """
    import socket
    import struct

    payload = json.dumps({"id": 1, "method": method, "params": params}).encode("utf-8")
    frame = struct.pack("<I", len(payload)) + payload
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.settimeout(connect_timeout)
        try:
            sock.connect(str(sock_path))
        except (FileNotFoundError, ConnectionRefusedError, OSError):
            return None
        sock.settimeout(read_timeout)
        sock.sendall(frame)
        header = b""
        while len(header) < 4:
            chunk = sock.recv(4 - len(header))
            if not chunk:
                return None
            header += chunk
        (length,) = struct.unpack_from("<I", header)
        if length > 16 * 1024 * 1024:
            return None
        data = b""
        while len(data) < length:
            chunk = sock.recv(length - len(data))
            if not chunk:
                return None
            data += chunk
        resp = json.loads(data.decode("utf-8"))
        if not isinstance(resp, dict) or "error" in resp:
            return None
        return resp.get("result")
    except (OSError, ValueError):
        return None
    finally:
        sock.close()


def _render_stream_frame(frame: dict) -> "str | None":
    """Map one stream frame to a display line (None = nothing to show).

    Renders the turn lifecycle visibly (AC2-CLI: never a silent stall):
    delivered (user-echo receipt) -> streaming (partials) -> reply -> complete.
    """
    kind = frame.get("kind")
    if kind == "system":
        return f"  · session ready ({frame.get('subtype', '')})"
    if kind == "user_echo":
        return "  · delivered (turn received)"
    if kind == "stream_event":
        delta = frame.get("delta")
        return f"  · {delta}" if delta else None
    if kind == "assistant":
        return f"  -> {frame.get('text', '')}"
    if kind == "result":
        return "  x turn errored" if frame.get("is_error") else "  v turn complete"
    if kind == "malformed":
        return "  · (skipped malformed frame)"
    return None


def _watch_loop(read_frames, *, max_polls=None, sleep_fn=None, out=None) -> int:
    """Poll a thread's frame log and render turns until it exits / max_polls.

    ``read_frames(cursor) -> dict | None`` is injected so the loop is testable
    without a socket. Returns 0 on a clean exit (child not alive), 1 when the
    worker is unreachable (thread not live).
    """
    out = out or sys.stdout
    if sleep_fn is None:
        import time as _time

        def sleep_fn() -> None:
            _time.sleep(0.25)

    cursor = 0
    polls = 0
    while max_polls is None or polls < max_polls:
        polls += 1
        res = read_frames(cursor)
        if res is None:
            print("fno agents watch: thread not live (worker unreachable)", file=sys.stderr)
            return 1
        cursor = res.get("next", cursor)
        for fr in res.get("frames", []):
            line = _render_stream_frame(fr)
            if line is not None:
                print(line, file=out)
        if not res.get("child_alive", True):
            print("  -- thread exited", file=out)
            return 0
        if max_polls is None or polls < max_polls:
            sleep_fn()
    return 0


@agents_app.command("watch")
def cmd_watch(
    name: str = typer.Argument(..., help="Agent name (a held stream-json thread)."),
    poll_interval: float = typer.Option(
        0.25, "--interval", "-i", help="Seconds between frame polls."
    ),
) -> None:
    """Observe a held stream-json thread's turns in real time (read-only).

    Renders delivered -> streaming -> reply -> complete per turn by polling the
    worker's frame log. Ctrl-C to stop. Exits 0 when the thread is no longer
    live, 1 when no live worker exists, 2 when the name is unknown.
    """
    from fno.agents.registry import AgentResolutionError, resolve_agent

    try:
        resolved = resolve_agent(name)
    except AgentResolutionError as exc:
        print(f"fno agents watch: {exc}", file=sys.stderr)
        raise typer.Exit(exc.exit_code) from exc
    short_id = resolved.worker_short_id
    if short_id is None:
        print(
            f"fno agents watch: agent {resolved.entry.name!r} has no worker "
            "short id on file; nothing to watch",
            file=sys.stderr,
        )
        raise typer.Exit(2)
    sock = _agents_home_dir() / short_id / "worker.sock"
    import time as _time

    def _read(cursor: int) -> "dict | None":
        return _worker_rpc(sock, "stream.read_frames", {"cursor": cursor})

    try:
        rc = _watch_loop(_read, sleep_fn=lambda: _time.sleep(poll_interval))
    except KeyboardInterrupt:
        print("\n  -- watch stopped", file=sys.stderr)
        rc = 0
    raise typer.Exit(rc)


@agents_app.command("crown", hidden=True)
def cmd_crown(
    handle: str = typer.Argument(
        ...,
        help="Existing registered session handle to crown in place.",
    ),
    scopes: list[str] = typer.Option(
        ...,
        "--scope",
        help=(
            "Territory to grant. Repeat for a multi-project portfolio; the "
            "crown level is derived and cannot be supplied."
        ),
    ),
) -> None:
    """Crown an existing session from an attended shell.

    Run `fno agents register` inside the target session, then run this command
    with its printed handle from another terminal. Agent-originated calls are
    refused; subordinate grants and succession stay on `spawn --crown`. A row
    already holding a crown is re-scoped rather than refused: the new territory
    replaces the old in one atomic write, the level is derived from the new
    scope, and the receipt reports what was vacated.
    """
    from fno.agents import events
    from fno.agents.crown import CrownPromotionError, promote_existing_session

    try:
        receipt = promote_existing_session(handle, scopes)
    except CrownPromotionError as exc:
        print(f"crown: {exc}", file=sys.stderr)
        raise typer.Exit(code=2) from exc

    events.emit(
        "agent_crowned",
        name=receipt["crowned"],
        level=receipt["level"],
        scope=receipt["scope"],
        grantor=receipt["grantor"],
        vacated_scope=receipt["vacated_scope"],
        vacated_level=receipt["vacated_level"],
        stranded_subordinates=receipt["stranded_subordinates"],
    )
    print(json.dumps(receipt))




@agents_app.command("spawn")
def cmd_spawn(
    message: str = typer.Argument("", help="The prompt to seed the worker with."),
    passthrough: list[str] | None = typer.Argument(
        None,
        help=(
            "Provider CLI flags after a `--` separator (x-1caa): `spawn \"hi\" "
            "-- --verbose` forwards --verbose to the harness's own CLI, so a "
            "flag fno never declared needs no code change. The parser stays "
            "strict - an unknown fno flag before `--` (e.g. --modle) still "
            "fails here rather than being silently forwarded. Pane substrate "
            "only; the tokens ride the composed argv through the same "
            "refusals (-p/--print, --settings, --session-id) that govern fno's "
            "own flags."
        ),
    ),
    name: str = typer.Option(
        "",
        "--name",
        help=(
            "Agent name (optional; an adjective-noun slug is minted when omitted). "
            "A name is a handle you rarely care about, so it moved off the "
            "positional: the one positional is the prompt."
        ),
    ),
    harness: str | None = typer.Option(
        None,
        "--harness",
        "-H",
        help=(
            "The CLI binary to launch: claude | codex | gemini | opencode | agy "
            "(optional). Defaults to the invoking harness, then claude. NOTE: -H "
            "no longer means headless; for a one-shot use --substrate headless / "
            "--headless / --once."
        ),
    ),
    vendor: str | None = typer.Option(
        None,
        "--provider",
        "-P",
        help=(
            "The model VENDOR the harness talks to: zai, or any "
            "model_routing.providers name. Pairs with --model to name the route "
            "(--provider zai --model glm-5.3 == --route zai,glm-5.3). This is NOT "
            "the CLI binary -- that is --harness/-H. Capital -P: -p is headless."
        ),
    ),
    once: bool = typer.Option(
        False,
        "--once",
        "-o",
        help=(
            "Ephemeral one-shot: create + exchange + teardown. "
            "Supported for codex and gemini only. "
            "claude peers are persistent bg threads; use plain spawn."
        ),
    ),
    substrate: str = typer.Option(
        "pane",
        "--substrate",
        help=(
            "Session substrate (x-2c27): pane (mux-hosted PTY, the default; "
            "4a-G2) | bg (claude --bg thread) | headless (-p/--exec one-shot). "
            "Python owns the pane back half (fno mux pane run + registry mux "
            "ref); bg/headless keep their existing lanes."
        ),
    ),
    headless: bool = typer.Option(
        False,
        "--headless",
        "-p",
        help=(
            "Shortcut for --substrate headless: a one-shot worker. Wins over "
            "--substrate; equivalent to --once/-o. `-p` mirrors the harnesses' own "
            "one-shot short (claude -p / codex exec); the vendor axis takes the "
            "capital -P to keep the letter free for it."
        ),
    ),
    cwd: str | None = typer.Option(
        None, "--cwd", "-c", help="Working directory for the agent subprocess."
    ),
    timeout: int | None = typer.Option(
        None,
        "--timeout",
        "-t",
        help="Per-spawn timeout in seconds (default 600).",
    ),
    from_name: str = typer.Option(
        "fno",
        "--from-name",
        help=("Identity advertised in the message envelope. Must be XML-attribute-safe."),
    ),
    yolo: bool = typer.Option(
        False,
        "--yolo",
        "-Y",
        help=(
            "Provider-specific dangerous-mode bypass. For codex: passes "
            "--dangerously-bypass-approvals-and-sandbox. "
            "For claude: maps to --permission-mode bypassPermissions. "
            "Mutually exclusive with --permission-mode (pass one; exit 2)."
        ),
    ),
    fresh: bool = typer.Option(
        False,
        "--fresh",
        help=(
            "Accepted no-op alias: the worker cwd already defaults to the "
            "canonical (main) repo root (x-85fe). Kept for dispatcher compat."
        ),
    ),
    here: bool = typer.Option(
        False,
        "--here",
        "--in-place",
        help=(
            "Keep the worker in the caller's cwd instead of the canonical-root "
            "default. The explicit opt-in for extending WIP right here."
        ),
    ),
    role: str | None = typer.Option(
        None,
        "--role",
        help=(
            "Routing role for per-spawn model selection (x-d2fe). Auxiliary "
            "roles (coordinate|tidy|orient|consolidate|post-merge) and the "
            "delivery lane (build) route to a secondary provider (z.ai GLM by "
            "default) when configured; the build lane is opt-in by config "
            "presence (set model_routing.roles.build). Production roles "
            "(implement|review-verdict) and the default (no --role) stay on the "
            "primary Anthropic model."
        ),
    ),
    route: str | None = typer.Option(
        None,
        "--route",
        help=(
            "Explicit per-dispatch model route as provider/model (e.g. "
            "zai/glm-5.3; legacy comma zai,glm-5.3 also accepted). Bypasses the "
            "--role table and guard (explicit intent "
            "is not auto-routing) and wins over any configured lane. FAILS CLOSED: "
            "an unknown provider, non-anthropic protocol, or missing key refuses "
            "the spawn - never a silent primary-model launch. claude only."
        ),
    ),
    monitor: str | None = typer.Option(
        None,
        "--monitor",
        help=(
            "Expose this spawn through a monitor. Initial support is exactly "
            "'happy' with --harness claude --provider zai on the pane substrate."
        ),
    ),
    account: str | None = typer.Option(
        None,
        "--account",
        help=(
            "Pin this ONE worker to a registered claude account (x-d012) without "
            "touching the daemon-wide active ~/.claude slot. Resolves a "
            "ProviderRecord to an env overlay: an account with its own config_dir "
            "(the verified-correct mechanism, bills right) sets CLAUDE_CONFIG_DIR; "
            "a managed account rides the shared slot only when it IS the active "
            "occupant. A managed non-active account is refused with a pointer to "
            "config-dir registration (the setup-token env lane bills the wrong "
            "account and is not used). Explicit operator intent only - never "
            "inferred by failover/dispatch. claude only; fail-closed, nothing "
            "spawned on refusal."
        ),
    ),
    dispatch_account: str | None = typer.Option(
        None,
        "--dispatch-account",
        help=(
            "The destination provider RECORD of an autonomous quota cutover, as "
            "chosen by `fno dispatch resolve --autonomous`. Unlike --account "
            "(operator intent, claude-only) this carries the record's dispatch "
            "env for ANY harness, which is what a claude->codex cutover needs. "
            "The record id travels on argv; its credentials never do. "
            "Fail-closed: an unknown or unstageable record spawns nothing."
        ),
    ),
    model: str | None = typer.Option(
        None,
        "--model",
        "-m",
        help=(
            "Model for the worker, forwarded as --model <m> to the provider's "
            "own CLI (exact passthrough, no fuzzy resolution). On the default "
            "pane substrate every provider honors it (claude/codex/gemini/agy/"
            "opencode); on --substrate bg/headless it reaches claude and agy. "
            "Unset = provider default; opencode defaults to z-ai/glm-5.3."
        ),
    ),
    permission_mode: str | None = typer.Option(
        None,
        "--permission-mode",
        help=(
            "Permission/approval mode forwarded to the provider (x-dfa4). "
            "Provider-native values, fail-closed: claude default|acceptEdits|"
            "plan|bypassPermissions (exact passthrough); gemini --approval-mode "
            "(or 'yolo'); codex a shortcut (full-auto|yolo) or <sandbox>:"
            "<approval> (e.g. workspace-write:on-request); opencode 'auto'; agy "
            "'skip'. An unmappable value errors before spawn. Mutually exclusive "
            "with --yolo. Honored on claude bg/headless (Rust or Python fallback); "
            "codex/gemini bg/headless one-shots reject it (use --substrate pane)."
        ),
    ),
    effort: str | None = typer.Option(
        None,
        "--effort",
        help=(
            "Reasoning effort: minimal|low|medium|high|xhigh|max. Values are "
            "validated against the selected provider; unset uses its default."
        ),
    ),
    resume: str | None = typer.Option(
        None,
        "--resume",
        "-r",
        help=(
            "Resume an existing claude session instead of starting fresh: the new "
            "--bg supervisor continues that transcript (US4 bg-thread revival). "
            "Accepts a full session uuid OR the 8-hex short-id shown in receipts "
            "(x-f76e); with no --substrate it implies bg. claude + bg only."
        ),
    ),
    add_dir: str | None = typer.Option(
        None,
        "--add-dir",
        help=(
            "Grant the worker extra write access to a directory (x-b6e2). Maps to "
            "the harness's own --add-dir on claude/codex/agy (additive to the "
            "worker's own workspace); opencode/gemini reject it (fail-closed)."
        ),
    ),
    agent: str | None = typer.Option(
        None,
        "--agent",
        help=(
            "Pin the worker's sub-agent by name (x-b6e2). Maps to --agent on "
            "claude/opencode; codex/agy/gemini reject it (fail-closed)."
        ),
    ),
    tools: str | None = typer.Option(
        None,
        "--tools",
        help=(
            "Scope the worker's allowed tools (x-b6e2). Opaque list forwarded to "
            "claude --allowedTools; other providers reject it (fail-closed)."
        ),
    ),
    deny_tools: str | None = typer.Option(
        None,
        "--deny-tools",
        help=(
            "Scope the worker's disallowed tools (x-b6e2). Opaque list forwarded "
            "to claude --disallowedTools; other providers reject it (fail-closed)."
        ),
    ),
    output_format: str | None = typer.Option(
        None,
        "--output-format",
        hidden=True,
        help="Internal headless Claude output format; only 'json' is supported.",
    ),
    squad: str | None = typer.Option(
        None,
        "--workspace",
        "-s",
        help=(
            "Pane placement (x-3e38): send the new pane to a workspace by its visible "
            "name instead of the cwd-derived default. --substrate pane only."
        ),
    ),
    squad_compat: str | None = typer.Option(
        None,
        "--squad",
        hidden=True,
        help="Deprecated alias for --workspace.",
    ),
    split: str | None = typer.Option(
        None,
        "--split",
        "-x",
        help=(
            "Pane placement (x-3e38): tile the new pane left|right|up|down of the "
            "squad's focused pane instead of a new tab. --substrate pane only."
        ),
    ),
    at: str | None = typer.Option(
        None,
        "--at",
        help=(
            "Exact origin placement (x-6928): pin the new pane next to the calling "
            "pane. `--at current` resolves the caller from FNO_PANE (run inside a "
            "mux pane) and fails closed instead of falling back. Requires --split "
            "and --substrate pane."
        ),
    ),
    crown: list[str] = typer.Option(
        [],
        "--crown",
        "-k",
        help=(
            "Bestow an orchestrator crown on the spawned worker, over the "
            "territory named here. Repeatable: pass ONE epic id (a Director), "
            "ONE project name (a project king), or SEVERAL project names for a "
            "portfolio (`-k etl -k web`). The ladder altitude is derived from "
            "what you name - there is no --level, and a node that is not an epic "
            "is refused, since implementers get no crowns. Stamped with the "
            "grantor derived from THIS session, never self-declared. Works on "
            "--substrate pane and bg (bg is claude-only); refused on headless, "
            "whose one-shot exits before it can reign."
        ),
    ),
    node: str | None = typer.Option(
        None,
        "--node",
        help=(
            "Backlog node id (or slug) this pane is working (x-84a8). Node-driven "
            "pane spawns export FNO_NODE/FNO_SLUG/FNO_PLAN into the pane so the "
            "prompt (starship) can render provenance. Ad-hoc spawns omit it. "
            "FNO_SLUG/FNO_PLAN resolve from the graph unless --slug/--plan given."
        ),
    ),
    slug: str | None = typer.Option(
        None, "--slug", help="Provenance FNO_SLUG override (skips the graph read)."
    ),
    plan: str | None = typer.Option(
        None, "--plan", help="Provenance FNO_PLAN override (skips the graph read)."
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help=(
            "Spawn-gate bypass (x-c5cc): skip the max_live cap AND the "
            "min_free_gb RAM floor. Workers are still QoS-demoted and still "
            "counted by the next un-forced spawn."
        ),
    ),
    no_wait: bool = typer.Option(
        False,
        "--no-wait",
        help=("Fail immediately when max_live is reached instead of queueing for a free slot."),
    ),
) -> None:
    """Spawn a new agent.

    ``spawn`` creates a new peer. Use ``ask`` for follow-up messages to
    an already-running agent.

    Default substrate ``pane`` (4a-G2): the agent runs as a mux pane
    (``fno mux pane run``), the registry row carries ``mux: {session,
    pane_id}``, and the receipt is one JSON line with ``mux_session`` +
    ``pane_id``.

    claude ``--substrate bg``: creates a persistent bg thread; prints a
    compact JSON receipt on stdout: {\"name\": ..., \"short_id\": ...,
    \"harness\": \"claude\", \"status\": \"live\"}, plus \"provider\" (the model
    vendor) and \"model\" keys only when a route was applied (-P/--route) or a
    model named.

    codex/gemini --once: creates + exchanges + tears down the registry
    row. stdout = provider reply verbatim. stderr = teardown receipt.

    Plain spawn for codex/gemini (no --once) requires the fno-agents daemon
    (Rust runtime); this Python path exits 13 with guidance.
    """
    # --squad is a hidden back-compat alias for --workspace (US2); --workspace wins.
    squad = squad if squad is not None else squad_compat

    from fno.agents.dispatch import DispatchAskError, SpawnResult, dispatch_spawn
    from fno.dispatch_flags import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )

    workdir = _resolve_dispatch_workdir(cwd, fresh, here)
    # x-85fe: the effective launch dir surfaces in the receipt on the DEFAULT
    # move (a node-less spawn now lands on canonical), coupled with the stderr
    # redirect note. An explicit --cwd (incl. -P/node-resolved) is the caller's
    # own choice and never surfaces -- gate on `not cwd` so the receipt stays
    # byte-identical for explicit-cwd and stay-put spawns (AC1-EDGE).
    _moved_cwd = str(workdir) if not cwd and workdir != Path(os.getcwd()).resolve() else None

    # Three orthogonal axes: --harness names the CLI binary, --provider the model
    # vendor that binary talks to, --model the model at that vendor. `provider` is
    # the local name for the HARNESS axis all the way down -- it is the
    # dispatch_spawn kwarg and the receipt key every consumer parses, so the wire
    # name outranks the tidier local one.
    provider = harness
    # The caller's own spelling of the route, for refusal messages further
    # down: a route only the claude harness can carry may get refused after
    # the vendor+model collapse below, and naming the collapsed `--route`
    # form there would send the operator looking for a flag they never typed
    # (AC5-HP).
    route_spelling = f"--route {route}" if route is not None else None
    if vendor is not None:
        vendor = vendor.strip()
        # The historical confusion, refused by name rather than silently launching
        # the wrong thing: `--provider claude` used to select the CLI binary.
        from fno.agents.harnesses import READABLE_PROVIDERS

        if vendor in READABLE_PROVIDERS:
            print(
                f"{vendor} is a harness, not a provider; use --harness {vendor}",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        if route is not None:
            print(
                "--provider/--model and --route are two spellings of one route; pass one",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        if not model:
            print(
                f"--provider {vendor!r} names a vendor, not a model; add --model "
                "(the vendor's own model id, e.g. --model glm-5.3)",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        # The model belongs to the route from here: it reaches the worker as the
        # routed ANTHROPIC_MODEL, never as a `claude --model` token (which would
        # hand the claude CLI a vendor model id it cannot resolve).
        route_spelling = f"--provider {vendor} --model {model}"
        route, model = f"{vendor}/{model}", None

    # --provider is optional: resolve it (explicit > invoking harness > claude)
    # and reject an empty --model before anything spawns. `provider` is a
    # concrete string from here down; the provider-name set is validated
    # substrate-aware further in.
    try:
        provider, provider_source = resolve_dispatch_provider(provider)
        model = reject_empty_model(model)
    except DispatchFlagError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=2) from exc
    # Provenance rides the pane receipt's harness_source field below (the
    # default substrate) - it is the HARNESS axis's provenance, so it must not
    # be named provider_*, which now holds the vendor. The bg/once stdout
    # receipts stay byte-parity-locked with the Rust client, so they don't
    # carry it.

    # x-2c27 named the substrate axis; 4a-G2 retargeted its default: `pane`
    # is mux-hosted and Python OWNS that back half (rust_runtime carves pane
    # spawns out of the binary route), `bg`/`headless` keep their existing
    # lanes. Validate to parity with the Rust client (exit 2 on a bad value);
    # headless still maps onto the `once` lever.
    # --headless is the ergonomic shortcut for --substrate headless (x-c772). It
    # wins over an explicit --substrate so `--headless` always resolves to the
    # one-shot lane. (The -H short moved to --harness in x-6de8.)
    if headless:
        substrate = "headless"
    # `--once` is the pre-substrate spelling of headless (the Rust client maps it to
    # --substrate headless; the spawn gate counts it as headless) but Python leaves
    # it on the pane default. That only bites the routed lane, where the substrate
    # decides whether the route is materialized at all: without this a routed
    # `--once` reaches dispatch as claude+once+not-headless and dies on the
    # "claude peers are persistent bg threads" refusal.
    if once and substrate == "pane":
        substrate = "headless"
    if substrate not in ("pane", "bg", "headless"):
        print(
            f"--substrate must be one of: pane, bg, headless (got {substrate})",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    # x-1caa AC7: passthrough tokens only ride the PANE argv, where the
    # composed-argv refusals live. The seam refuses the explicit-flag spelling
    # for the Rust-routed lane; this is the same refusal for the Python lane,
    # including a substrate that arrived by config default after the seam.
    if passthrough and (substrate != "pane" or once):
        from fno.agents.spawn_defaults import PASSTHROUGH_PANE_ONLY

        print(PASSTHROUGH_PANE_ONLY, file=sys.stderr)
        raise typer.Exit(code=2)

    if monitor is not None and monitor != "happy":
        print(f"--monitor must be 'happy' (got {monitor!r})", file=sys.stderr)
        raise typer.Exit(code=2)
    if monitor == "happy" and (substrate != "pane" or once):
        print(
            "--monitor happy is pane-only; bg and headless workers do not pass "
            "the happy launcher seam",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if monitor == "happy" and provider != "claude":
        print(
            f"--monitor happy requires the claude harness; got harness {provider!r}",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    if output_format is not None and (
        provider != "claude" or substrate != "headless" or output_format != "json"
    ):
        print(
            "--output-format supports only 'json' on claude headless spawns",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # US4 revival: --resume continues an existing claude --bg transcript, so it
    # only applies to the claude bg lane (the Python bg_create path forwards
    # --resume <uuid>). provider None defaults to claude downstream.
    if resume is not None and (substrate != "bg" or provider not in (None, "claude")):
        print(
            "--resume requires --substrate bg on provider claude "
            "(it continues an existing claude --bg session)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    if effort is not None:
        from fno.agents.mux_spawn import effort_tokens

        try:
            effort_tokens(provider, effort)
        except DispatchAskError as exc:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc

    # AC5-ERR: --permission-mode and --yolo are one knob at a time.
    if permission_mode is not None and yolo:
        print(
            "--permission-mode and --yolo are mutually exclusive; pass one",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    # Fail-closed for non-claude bg/headless (mirrors the Rust intercept): only
    # claude's bg lane honors a mapped --permission-mode via the Python fallback
    # (dispatch_spawn -> _claude_create_path); codex/gemini one-shot lanes
    # hardcode their own bypass and can't express a mapped mode. The pane
    # substrate maps every provider, so it's exempt here. (x-dfa4)
    if permission_mode is not None and provider != "claude" and (substrate != "pane" or once):
        print(
            f"--permission-mode is not supported for provider {provider!r} on "
            "--substrate bg/headless (its one-shot lane hardcodes its own bypass "
            "form); use --substrate pane",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # x-b6e2: Tier-3 fail-closed for the bg/headless lanes (the pane substrate
    # maps every provider via build_pane_argv, so it's exempt and validated
    # there). Mirrors the --permission-mode guard above; the same per-cell matrix
    # as the Rust client. Validate BEFORE any spawn.
    if substrate != "pane" or once:
        # Truthiness, not `is not None`: an empty value is UNSET (the builders
        # omit an empty flag), so `--add-dir=""` must NOT trip the guard.
        bad = None
        if add_dir and provider not in ("claude", "codex", "agy"):
            bad = "--add-dir"
        elif agent and provider != "claude":
            bad = "--agent"
        elif tools and provider != "claude":
            bad = "--tools"
        elif deny_tools and provider != "claude":
            bad = "--deny-tools"
        if bad is not None:
            # No "use --substrate pane" advice: pane rejects the same tier3 cells
            # (gemini --add-dir, codex --agent), so it would mislead. Mirror the
            # tier3_pane_tokens wording instead.
            print(
                f"{bad} is not supported for provider {provider!r}; "
                "drop it or use a provider that maps it",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)

    # x-3e38 pane placement: squad/split name mux geometry, which only the
    # pane substrate has. bg/headless have no pane tree, so the controls are
    # refused fail-closed before any spawn (mirrors the tier-3 guard shape above).
    placement_requested = squad is not None or split is not None or at is not None
    if squad is not None and not squad.strip():
        print("--workspace/-s needs a nonblank workspace name", file=sys.stderr)
        raise typer.Exit(code=2)
    if placement_requested and (substrate != "pane" or once):
        print(
            "--workspace/-s, --split/-x, and --at apply only to --substrate pane "
            "(bg/headless have no pane geometry)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if split is not None and split not in ("left", "right", "up", "down"):
        print(
            f"--split/-x must be left, right, up, or down (got {split!r})",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if at is not None:
        # `--at current` is the exact-anchor spelling: the mux CLI resolves the
        # calling pane from FNO_PANE and sets the strict (Refuse) policy, so the
        # spawn always carries the placement receipt and the readiness gate. A
        # numeric anchor is a low-level `mux pane run` concern (it keeps the
        # legacy new-tab fallback) and is intentionally not exposed here, where
        # every `--at` value implies the exact-placement contract.
        if at != "current":
            print("--at must be `current` (the exact-anchor spelling)", file=sys.stderr)
            raise typer.Exit(code=2)
        if split is None:
            print("--at requires --split (the side to place on)", file=sys.stderr)
            raise typer.Exit(code=2)

    # --crown/-k <scope>... : the operator names the TERRITORY and the ladder
    # altitude is derived from it (crown.derive_crown_level). The grantor is
    # stamped ambiently at spawn from this session, so the child's row records who
    # actually bestowed the crown, never a value it could forge.
    #
    # The substrate axis the crown actually cares about is REIGN LENGTH, not pane
    # geometry. A crown is three registry fields; nothing in it needs a PTY. What
    # it needs is a session that outlives the grant, because a king that exits
    # mid-wave orphans its scope. `pane` and `bg` both qualify - a bg worker is a
    # full persistent conversation in claude's agent view, attachable, replyable,
    # and resumable, differing from a pane only in who draws it. `headless` is the
    # one-shot: it answers once and exits, so a crown on it names a dead ruler
    # before the grantor's next turn. That one stays refused.
    #
    # A bg king does lose the pane-layer PLACEMENT primitive (`--at current`
    # resolves the calling pane from FNO_PANE, which a bg session has none of), so
    # it seats minions in fresh tabs rather than beside itself. That degrades the
    # court's ergonomics, not its authority: mail, peek, top, and wait are all
    # substrate-blind. Court-mode briefs that need adjacency should ask for a pane
    # king; the crown itself does not.
    crown_level: int | None = None
    crown_scope: str | None = None
    if crown:
        if once or substrate == "headless":
            print(
                "--crown needs a session that outlives the grant; headless is a "
                "one-shot that exits after one answer, so its crown would be "
                "orphaned at birth. Use --substrate pane or --substrate bg.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        from fno.agents.crown import CrownScopeError, resolve_crown

        try:
            crown_level, crown_scope = resolve_crown(list(crown))
        except CrownScopeError as exc:
            print(f"--crown: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc

    # --account names a claude account PROFILE (config_dir/settings/plugins); a
    # vendor route (-P/--route/--role) names endpoint+auth+model. They are
    # independent axes and COMPOSE (x-5ed4): `--account readyrule -P zai` runs
    # z.ai's model under readyrule's profile. Only a non-claude harness is
    # refused here (an account rides the claude binary). The composition is made
    # atomic at the provider layer (harnesses/claude.py: the route wins
    # endpoint+auth+model as one unit, the account keeps CLAUDE_CONFIG_DIR), so
    # the x-2af5 split-brain (overlay winning endpoint+auth while the route won
    # the model) cannot recur. Refused BEFORE route resolution so a keyless route
    # never masks this receipt.
    if account is not None and provider != "claude":
        print(f"--account is claude-only; got provider {provider!r}", file=sys.stderr)
        raise typer.Exit(code=2)

    # Explicit --route override (x-b0b4). Resolve + FAIL CLOSED here, BEFORE the
    # gate, so a refusal spawns nothing, acquires no gate slot, and leaves the
    # node dispatchable. resolve_explicit_route bypasses the role table + guard
    # (explicit intent) and returns None for unknown/non-anthropic/keyless - which
    # for --route is a hard refusal, not the role lane's silent fallback.
    route_env: dict[str, str] | None = None
    route_provider: str | None = None
    # The model axis for the receipt: the model token an explicit route named
    # (-P vendor/model or --route vendor,model). Absent when no route was
    # applied. A bare --model (no route) is still reported via the `model`
    # local below, since claude bg_create applies it as `claude --model`.
    route_model: str | None = None
    if route is not None:
        # Pane routing is a per-harness evidence claim, independent from both
        # pane autonomy and substrate preference. Missing capability stays
        # closed so a newly added harness cannot inherit Claude's route contract.
        if substrate == "pane":
            from fno.agents.harness_map import capabilities

            if not capabilities(provider).get("route_on_pane", False):
                print(
                    f"harness {provider!r} does not have the evidence-backed "
                    "route_on_pane capability; no worker launched, node stays "
                    "dispatchable.",
                    file=sys.stderr,
                )
                raise typer.Exit(code=2)
        if provider != "claude":
            print(
                f"{route_spelling} requires the claude harness; "
                f"got harness {provider!r} substrate {substrate!r}.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        from fno.agents.model_routing import (
            _parse_target,
            bind_route_provider,
            resolve_explicit_route,
        )

        parsed = _parse_target(route)
        if parsed is None:
            print(
                f"--route must be 'provider,model' with a non-empty model token; got {route!r}",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        route_provider = parsed[0]
        route_model = parsed[1]
        if monitor == "happy" and route_provider != "zai":
            print(
                "--monitor happy currently supports only the zai provider",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        if monitor == "happy" and model is not None:
            print(
                "--monitor happy refuses a separate --model override; put the "
                "Z.ai model in --route or use --provider zai --model <model>",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        notes: list[str] = []
        route_env = resolve_explicit_route(parsed[0], parsed[1], notice=notes.append)
        if not route_env:
            reason = "; ".join(notes) or "provider unknown, non-anthropic, or keyless"
            print(
                f"--route {route!r} refused ({reason}); no worker launched, node "
                "stays dispatchable.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        route_env = bind_route_provider(route_env, route_provider)

    if monitor == "happy" and route_provider != "zai":
        print(
            "--monitor happy currently requires --provider zai with --model",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # Resolve/validate the route once before pane/bg/headless fan out. The same
    # helper is called by the in-process spawn APIs, so bypassing the CLI cannot
    # recreate a managed-OAuth half-composition.
    if provider == "claude" and (role is not None or route_env):
        from fno.agents.model_routing import (
            RouteCompositionError,
            resolve_spawn_route,
        )

        intent = f"routed role {role!r}" if role is not None else f"route {route!r}"
        resolved_providers: list[str] = []
        try:
            route_env = resolve_spawn_route(
                role,
                route_env,
                intent=intent,
                notice=lambda note: print(note, file=sys.stderr),
                resolved_provider=resolved_providers.append,
            )
        except RouteCompositionError as exc:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=2) from exc
        if route_provider is None and resolved_providers:
            route_provider = resolved_providers[-1]

    # Per-spawn account overlay (x-d012). Resolve + FAIL CLOSED here, BEFORE the
    # gate, like --route: a refusal spawns nothing, takes no gate slot, and
    # leaves the node dispatchable. Only a non-claude harness was refused above;
    # --account composes with --route/--role (x-5ed4).
    account_env: dict[str, str] | None = None
    if account is not None:
        from fno.agents.account_env import resolve_account_overlay_or_exit

        overlay = resolve_account_overlay_or_exit(account)
        account_env = overlay.env if overlay else None

    # The autonomous-cutover carrier. Same fail-closed posture as --account, and
    # deliberately a separate flag: --account's claude-only refusal is an operator
    # contract, while a cutover's whole point is landing on another harness.
    if dispatch_account is not None:
        from fno.adapters.providers.dispatch import dispatch_env
        from fno.adapters.providers.loader import load_providers

        try:
            # Resolve against the WORKER's root, not the dispatcher's cwd: the
            # record was selected out of the node's project registry, and reading
            # a different one here would stage another project's account.
            rec = load_providers(repo_root=workdir).by_id.get(dispatch_account)
            if rec is None:
                raise ValueError("not a registered provider record")
            rec_harness = (getattr(rec, "harness", "") or "").strip()
            # The overlay and the binary must agree. A codex record's CODEX_HOME
            # handed to a claude spawn authenticates nothing and launches the
            # wrong binary - the exact miss this carrier exists to prevent, so it
            # is checked here rather than assumed from the caller's bookkeeping.
            # Compare against the RESOLVED harness, not the raw --harness option:
            # omitting the flag leaves it None, and trusting that would stage a
            # codex account onto the resolved claude default unchecked.
            # Require a harness AND exact equality. Treating an empty harness as
            # "no objection" would wave through the one record we can say least
            # about, which is the opposite of what a fail-closed guard is for.
            if rec_harness != provider:
                raise ValueError(
                    f"record is a {rec_harness or '<no harness>'} account but "
                    f"the spawn resolves {provider}"
                )
            account_env = {
                **(account_env or {}),
                **dispatch_env(dispatch_account, repo_root=workdir),
            }
        except Exception as exc:  # noqa: BLE001 - never spawn onto an unresolved record
            print(
                f"refusing --dispatch-account {dispatch_account!r}: {exc}; "
                "no worker launched",
                file=sys.stderr,
            )
            raise typer.Exit(code=2) from exc

    # x-8552: the receipt's credential facts, read off the composed overlays
    # (never off the flags - a caller who typed `--account makers -P zai` reads
    # auth/bills and learns immediately that makers contributed a profile and
    # nothing else). Gated on the resolved overlays, not the flag spellings, so
    # a routed --role or a --dispatch-account merge gets the same facts as an
    # explicit --route; an account-only or route-only receipt stays
    # byte-identical to main (AC3) because the other overlay is absent.
    credential = None
    if account_env is not None and route_env is not None:
        from fno.agents.account_env import compose_worker_credentials

        account_label = account if account is not None else dispatch_account
        _, credential = compose_worker_credentials(
            account_env, route_env, {}, account_id=account_label
        )
        print(
            f"account: {account_label} (profile only; auth {credential.auth}, "
            f"bills {credential.bills})",
            file=sys.stderr,
        )

    # Resolve node provenance once for every substrate. A node-bearing spawn is
    # itself a dispatcher route, so it must cross the same family-2 decision and
    # dispatch reservation as advance, reconcile, and the shell entry points.
    from fno.agents.mux_spawn import resolve_provenance

    prov_env = resolve_provenance(node, slug, plan)
    # x-9d11 refusal carrier: a direct `fno agents spawn` message never passes
    # through resolve_dispatch, so the SAME vocabulary the resolver judges is
    # applied here. The legacy bare token in a /target-family message is
    # migrated to the flag (receipts show the effective message), and the env
    # arm stays scoped to the family: prose and other verbs arm nothing.
    from fno.agents.harness_map import (
        is_target_family,
        message_carries_no_merge,
        normalize_legacy_no_merge,
    )

    message = normalize_legacy_no_merge(message)
    if prov_env is not None and message_carries_no_merge(message):
        prov_env["TARGET_NO_MERGE"] = "1"
    node_reservation: tuple[str, str] | None = None
    node_claim: tuple[str, str] | None = None
    if node is not None:
        guarded_node = prov_env.get("FNO_NODE")
        if not guarded_node:
            print(
                f"refusing unresolved --node {node!r}: cannot run the shared "
                "family-2 dispatch guard; no worker launched",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        guard_holder = f"spawn-cli:{os.getpid()}"
        from fno.claims.cli import HANDOVER_HOLDER_PREFIX

        # The worker's name is the whole proof. A bare `spawn-handover:` is a
        # string anyone can type, and naming it back is exactly what
        # `compare_and_rebind` accepts as evidence of successorship - so an
        # empty name would hand the takeover to any process that guessed the
        # prefix. Fall back to this dispatch's own pid, which is at least not
        # guessable, and which the launch-window exemptions still recognize.
        handover_holder = f"{HANDOVER_HOLDER_PREFIX}{name or f'pid-{os.getpid()}'}"
        guard, guard_exit = _spawn_guard_decision(
            guarded_node,
            guard_holder,
            cwd=str(workdir),
            handover_holder=handover_holder,
        )
        if guard.get("verdict") != "dispatchable":
            guard_reason = (
                guard.get("detail") or guard.get("reason") or guard.get("verdict") or "unknown"
            )
            prior = f" prior_holder={guard['holder']}" if guard.get("holder") else ""
            print(
                f"node dispatch refused: node={guarded_node} "
                f"verdict={guard.get('verdict')} reason={guard_reason}{prior}; "
                "no worker launched",
                file=sys.stderr,
            )
            # This is the launch path, so a remedy here HAS earned itself:
            # recovery ran and could not prove the holder dead. Printing it is
            # what makes the way out reach an operator; the shell callers read
            # this stream and pass it through.
            if guard.get("remedy"):
                print(guard["remedy"], file=sys.stderr)
            raise typer.Exit(code=guard_exit or 2)
        node_reservation = (
            guard["reservation_key"],
            guard["reservation_holder"],
        )
        if guard.get("node_claim_key"):
            # Released on the SAME two failure paths as the reservation. A
            # launch that dies after the claim must not strand the node for the
            # whole handover window; that is the wedge this PR exists to delete,
            # reintroduced by its own fix.
            node_claim = (guard["node_claim_key"], guard["node_claim_holder"])
            # The worker proves it is the intended successor by naming this
            # holder back. It travels in the environment, never on the command
            # line, so it reaches exactly the process spawned for this node.
            prov_env["FNO_NODE_CLAIM_HOLDER"] = guard["node_claim_holder"]
        elif guard.get("node_claim_error"):
            print(
                f"note: node:{guarded_node} claim not taken at dispatch "
                f"({guard['node_claim_error']}); the worker claims it at init",
                file=sys.stderr,
            )

    # A resume may restore a recorded route inside dispatch_spawn. Resolve its
    # separately stored provider axis before admission so the gate judges the
    # destination the revived worker will actually use.
    if resume is not None and route_provider is None:
        from fno.agents.registry import load_registry

        try:
            loaded = load_registry()
            if getattr(loaded, "complete", True) is not True:
                raise RuntimeError("registry forward read is incomplete")
        except Exception as exc:
            print(
                f"resume provider unreadable ({exc}); refusing because its provider "
                "cap cannot be evaluated; no worker launched",
                file=sys.stderr,
            )
            raise typer.Exit(code=2) from exc
        source_row = next(
            (
                row
                for row in loaded
                if row.name == name and getattr(row, "route_settings_path", None)
            ),
            None,
        ) or next(
            (
                row
                for row in loaded
                if getattr(row, "harness_session_id", None) == resume
                and getattr(row, "route_settings_path", None)
            ),
            None,
        )
        if source_row is not None:
            recorded_provider = getattr(source_row, "provider", None)
            if not recorded_provider:
                print(
                    f"route recorded for {source_row.name!r} has no model-provider "
                    "axis; refusing because its provider cap cannot be evaluated; "
                    "no worker launched",
                    file=sys.stderr,
                )
                raise typer.Exit(code=2)
            route_provider = recorded_provider

    # Spawn gate (x-c5cc): cap + RAM floor at the top of the primitive, before
    # the substrate fan-out. This Python gate is the SOLE gate on every path
    # that reaches cmd_spawn (the front door execs the binary for bg/headless,
    # so those normally gate in Rust; the Rust pane arm re-execs back here) —
    # exactly one gate evaluation per spawn (LD1). `--once` is the
    # pre-substrate spelling of a headless one-shot, so it gates as headless.
    from fno.agents.spawn_gate import run_gate

    try:
        gate = run_gate(
            name,
            "headless" if (once or substrate == "headless") else substrate,
            force=force,
            no_wait=no_wait,
            route_provider=route_provider,
        )
    except BaseException:
        _release_dispatch_claims(node_reservation, node_claim)
        raise

    # Prior values of the provenance keys the bg/headless arm exports below, so
    # the finally can put the process env back.
    prov_prev: dict[str, "str | None"] = {}
    # x-9d11 set-or-clear BEFORE any substrate branch: the pane transport
    # inherits os.environ directly, so an inherited carrier must be cleared
    # here too, not only on the bg/headless export path (review round 7). The
    # message is authoritative in both directions - but only for
    # /target-family messages, the one vocabulary that can carry the flag. A
    # prose spawn clears NOTHING: an operator's exported TARGET_NO_MERGE is a
    # documented control input, and a leaked carrier surviving a prose worker
    # errs toward refusing merges, the safe side (round 8).
    prov_prev["TARGET_NO_MERGE"] = os.environ.get("TARGET_NO_MERGE")
    if message_carries_no_merge(message):
        # The only other writer of prov_env["TARGET_NO_MERGE"] (above) keys on
        # the same predicate over the same message, so one check here decides
        # every substrate (round 10 review).
        os.environ["TARGET_NO_MERGE"] = "1"
    elif is_target_family(message) and " no-merge " in f" {message} ":
        # A family message with a bare token OUTSIDE flag position (the legacy
        # token migrated above was position-scoped): ambiguous - it may be the
        # pre-x-9d11 FLAGS-then-token spelling or a feature description that
        # mentions the word. Neither arm nor clear: prose manufactures nothing,
        # and dropping an operator's exported refusal on an ambiguous message
        # errs toward granting merges (round 11). Init's loud no-op note fires
        # in the worker either way.
        pass
    elif is_target_family(message):
        # A family message with NO refusal token at all clears the carrier: the
        # flag is the authority. A non-family (prose/other-verb) message clears
        # NOTHING - see the comment above. Clearing an INHERITED carrier is
        # never silent: an operator's exported TARGET_NO_MERGE is a documented
        # control input, and the message overriding it deserves a visible line.
        if os.environ.get("TARGET_NO_MERGE"):
            print(
                "fno agents spawn: inherited TARGET_NO_MERGE cleared; the "
                "/target-family message carries no --no-merge flag and the "
                "message is authoritative",
                file=sys.stderr,
            )
        os.environ.pop("TARGET_NO_MERGE", None)

    # `--once` is the pre-substrate spelling of headless (the Rust client maps
    # it to --substrate headless): it always means a one-shot, never a pane.
    spawn_succeeded = False
    try:
        if substrate == "pane" and not once:
            from fno.agents.mux_spawn import dispatch_spawn_pane

            try:
                pane_result = dispatch_spawn_pane(
                    name=name,
                    message=message,
                    provider=provider,
                    cwd=workdir,
                    yolo=yolo,
                    role=role,
                    model=model,
                    permission_mode=permission_mode,
                    effort=effort,
                    add_dir=add_dir,
                    agent=agent,
                    tools=tools,
                    deny_tools=deny_tools,
                    squad=squad,
                    split=split,
                    at=at,
                    crown_level=crown_level,
                    crown_scope=crown_scope,
                    provenance=prov_env,
                    account_env=account_env,
                    route_env=route_env,
                    monitor=monitor,
                    route_provider=route_provider,
                    provider_gate=gate,
                    passthrough=passthrough,
                )
            except DispatchAskError as exc:
                print(str(exc), file=sys.stderr)
                raise typer.Exit(code=exc.exit_code) from exc
            spawn_succeeded = True
            # Compact one-line receipt, superset of the daemon-spawn receipt shape
            # ({"name","short_id","harness","status"}) so line-parsing consumers
            # keep working. A Codex pane is `spawning` until its rollout identity
            # is bound; a `live` Codex receipt always carries the full identity.
            receipt_obj = {
                "name": pane_result.name,
                "short_id": pane_result.short_id,
                "harness": pane_result.provider,
                "harness_source": provider_source,
                "status": pane_result.status,
                "mux_session": pane_result.session,
                "pane_id": pane_result.pane_id,
                # Two facts the receipt used to conflate. `bound` says the
                # worker reached its provider; `status` alone could not, so a
                # pane about to bind and one already dead read identically.
                "bound": pane_result.bound,
                # Independent delivery fact. Null means no seed was requested;
                # unconfirmed is printed before the command exits non-zero.
                "seed": pane_result.seed,
            }
            if pane_result.seed_source is not None:
                receipt_obj["seed_source"] = pane_result.seed_source
            if pane_result.fno_id is not None:
                receipt_obj["fno_id"] = pane_result.fno_id
            if pane_result.bound is False:
                # `is False`, not falsy: `bound` is tri-state and None means this
                # harness binds no session at all (gemini, agy), which is not a
                # failure and owes no explanation. Only on the genuinely unbound
                # receipt, so a bound one stays byte-stable apart from `bound`
                # itself. An empty short_id is a SIGNAL, and these two keys are
                # what it signals.
                receipt_obj["pane_alive"] = pane_result.pane_alive
                receipt_obj["unbound_reason"] = pane_result.unbound_reason
            # Three orthogonal axes: harness always; provider (the model vendor)
            # and model only when an explicit route was applied (-P/--route) or a
            # model was named, absent otherwise. No key may hold another axis's
            # literal: provider never carries a harness value.
            # `model` reports the EFFECTIVE model, so an explicit --model beats
            # the routed one: mux_spawn/dispatch pass it as the harness's own
            # `--model` flag, which wins over the route's ANTHROPIC_MODEL.
            # Reporting only route_model here would re-introduce the
            # receipt-can-lie defect: a `--route zai,glm-5.3 --model opus` spawn
            # would name glm-5.3 in the receipt while the worker runs opus.
            if route_provider is not None:
                receipt_obj["provider"] = route_provider
            receipt_model = model or route_model
            if receipt_model is not None:
                receipt_obj["model"] = receipt_model
            if pane_result.session_uuid is not None:
                receipt_obj["session_id"] = pane_result.session_uuid
            effective_message = getattr(pane_result, "effective_message", None)
            if effective_message is not None:
                receipt_obj["effective_message"] = effective_message
            if pane_result.placement is not None:
                # Server-authored exact-placement receipt (anchor/direction/
                # fallback/squad/tab); never synthesized from the request.
                receipt_obj["placement"] = pane_result.placement
            if pane_result.recovered:
                # LD5: this pane was adopted after an unanswered
                # control read, not created by this run. Proves a booted
                # session, never that the prompt was consumed - the receipt
                # must say so rather than reading identically to a normal
                # spawn.
                receipt_obj["recovered"] = True
            if pane_result.readiness is not None:
                receipt_obj["readiness"] = pane_result.readiness
            if pane_result.readiness_rule is not None:
                receipt_obj["readiness_rule"] = pane_result.readiness_rule
            # Locked Decision 5: name the applied mode so an audit of "why did
            # this worker have edit rights" has a durable answer. Only when set,
            # so the unset receipt is unchanged.
            if permission_mode is not None:
                receipt_obj["permission_mode"] = permission_mode
            # x-d012: name the pinned account so a mis-pin is visible at spawn
            # time, not at billing time. Only when set (receipt byte-stable else).
            if account is not None:
                receipt_obj["account"] = account
            # x-8552: for the composed spawn, which credential fno made live and
            # who is billed - derived from the composed env, never the flags.
            if credential is not None:
                receipt_obj["auth"] = credential.auth
                receipt_obj["bills"] = credential.bills
            if _moved_cwd is not None:
                receipt_obj["cwd"] = _moved_cwd
            receipt = json.dumps(receipt_obj)
            sys.stdout.write(receipt + "\n")
            sys.stdout.flush()
            if pane_result.seed == "unconfirmed":
                raise typer.Exit(code=22)
            return
        if substrate == "headless":
            once = True

        # Carry the bound node to bg/headless workers. The pane path gets this
        # through dispatch_spawn_pane's explicit provenance wrapper; bg and
        # headless build their child env from os.environ, so exporting here is
        # what reaches them.
        #
        # All three keys are set or cleared together, never merged with what
        # this process inherited: a worker dispatching a child for a plan-less
        # node would otherwise pass down its OWN FNO_PLAN alongside the child's
        # FNO_NODE. Restored in the finally, so the child inherits during the
        # dispatch call and an in-process caller spawning twice cannot leak the
        # first spawn's node into the second.
        from fno.agents.mux_spawn import PROVENANCE_KEYS

        prov_prev.update({k: os.environ.get(k) for k in PROVENANCE_KEYS})
        for _k in PROVENANCE_KEYS:
            os.environ.pop(_k, None)
        os.environ.update(prov_env)
        # TARGET_NO_MERGE was set-or-cleared above, before the substrate
        # branch, so both the pane transport and this export path see the
        # message-authoritative value; prov_prev restores it in the finally.

        try:
            result: SpawnResult = dispatch_spawn(
                name=name,
                message=message,
                provider=provider,
                cwd=workdir,
                once=once,
                timeout=timeout,
                from_name=from_name,
                yolo=yolo,
                role=role,
                route_env=route_env,
                model=model,
                permission_mode=permission_mode,
                effort=effort,
                add_dir=add_dir,
                agent=agent,
                tools=tools,
                deny_tools=deny_tools,
                headless=substrate == "headless",
                output_format=output_format,
                resume_session_id=resume,
                account_env=account_env,
                crown_level=crown_level,
                crown_scope=crown_scope,
                route_provider=route_provider,
                provider_gate=gate,
            )
            spawn_succeeded = result.kind == "created" or bool(
                result.reply and result.reply.strip()
            )
        except DispatchAskError as exc:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc
    finally:
        # Release the gate's claims once the dispatch result exists (or the
        # spawn failed): registry/roster rows carry the count from here.
        gate.release()
        if not spawn_succeeded:
            _release_dispatch_claims(node_reservation, node_claim)
        for _k, _v in prov_prev.items():
            if _v is None:
                os.environ.pop(_k, None)
            else:
                os.environ[_k] = _v

    if result.kind == "created":
        # claude plain spawn: compact hand-rolled JSON receipt on stdout.
        # Hand-rolled f-string (NOT json.dumps) for byte-parity with Rust Task 1.3.
        # Escape `"` in the name so the receipt stays valid JSON for jq
        # consumers (name validation blocks backslash already, so this is the
        # only escapable character; sigma-review hardening finding).
        safe_name = result.name.replace('"', '\\"')
        # Locked Decision 5 / Rust parity: name the applied mode (flag or the
        # yolo-derived bypassPermissions) so an audit can tell elevated
        # permissions were applied on this fallback path. Only when set, so the
        # unset receipt is byte-identical.
        eff_mode = permission_mode or ("bypassPermissions" if yolo else None)
        perm_field = (
            f', "permission_mode": "{eff_mode.replace(chr(34), chr(92) + chr(34))}"'
            if eff_mode
            else ""
        )
        # x-85fe: append the effective cwd only on the default move. json.dumps
        # (not a bare `"`-escape) so a path with a backslash or control char stays
        # valid JSON for receipt consumers (review); it matches Rust's
        # json_string_ascii byte-for-byte. LAST field so an unmoved receipt is
        # byte-identical.
        cwd_field = f', "cwd": {json.dumps(_moved_cwd)}' if _moved_cwd is not None else ""
        # x-d012: name the pinned account. Only when set, so a non-account bg
        # receipt stays byte-identical to the Rust client's (which never emits
        # it - an --account spawn always re-execs into this Python path).
        account_field = f', "account": {json.dumps(account)}' if account else ""
        # x-8552: the composed spawn's live credential and payer, from the
        # composed env (see the pane branch); composed-only so an account-only
        # bg receipt stays byte-identical (AC3).
        cred_field = (
            f', "auth": {json.dumps(credential.auth)}, '
            f'"bills": {json.dumps(credential.bills)}'
            if credential is not None
            else ""
        )
        effective_message = getattr(result, "effective_message", None)
        message_field = (
            f', "effective_message": {json.dumps(effective_message)}'
            if effective_message is not None
            else ""
        )
        # provider/model axes: present only when an explicit route was applied
        # (-P/--route) or a model named; absent otherwise. provider holds the
        # model vendor, never a harness literal (the defect this corrects).
        # `model` is the EFFECTIVE model: an explicit --model reaches claude as
        # its own `--model` flag and beats the route's ANTHROPIC_MODEL, so it
        # wins the receipt too (see the pane branch above).
        provider_field = (
            f", \"provider\": {json.dumps(route_provider)}" if route_provider else ""
        )
        receipt_model = model or route_model
        model_field = f", \"model\": {json.dumps(receipt_model)}" if receipt_model else ""
        receipt = (
            f'{{"name": "{safe_name}", "short_id": "{result.short_id}", '
            f'"harness": "{result.provider}"{provider_field}{model_field}, "status": "live"'
            f"{perm_field}{cwd_field}{account_field}{cred_field}{message_field}}}"
        )
        sys.stdout.write(receipt + "\n")
        sys.stdout.flush()
        # QoS (x-c5cc): a bg worker is claude's child, so its exec can't be
        # wrapped — demote post-hoc via the roster, bounded and non-fatal.
        # After the receipt flush so line-parsing consumers never wait on it.
        if substrate == "bg" and result.provider == "claude" and result.short_id:
            from fno.agents.spawn_gate import qos_demote_bg_worker

            qos_demote_bg_worker(result.short_id)
    else:
        # once path: reply verbatim on stdout (no added newline per ask contract).
        sys.stdout.write(result.reply or "")
        sys.stdout.flush()


#: Exit status `fno agents name` uses for a naming refusal. Deliberately not 2:
#: Click already spends 2 on usage errors including "no such command", so a
#: shell caller cannot distinguish a refusal from a stale `fno` at exit 2.
NAME_REFUSED_EXIT = 3


@agents_app.command("name", hidden=True)
def cmd_name(
    prefix: str = typer.Argument(..., help="Operation prefix (target|spawn|handoff|...)."),
    node_id: str = typer.Argument(..., help="Full backlog node id; never abbreviated."),
    slug: str = typer.Option("", "--slug", help="Human-readable tail; the only expendable part."),
    qualifier: str = typer.Option("", "--qualifier", help="Lifecycle reason, e.g. retro."),
    discriminator: str = typer.Option(
        "", "--discriminator", help="Per-invocation uniqueness token; never shaved."
    ),
) -> None:
    """Mechanical bridge to the canonical agent-name owner, for shell dispatchers.

    Prints one name on stdout. Shell callers delegate here instead of
    reimplementing the budget: the assembled 64-char precedence rule differs
    from a `cut -c1-64`, which shaves the uniqueness discriminator and collapses
    two dispatches onto one dedup token.

    Exit 3 (NOT 2) is the naming refusal. Exit 2 is Click's usage error, which
    an `fno` too old to know this verb also returns for "no such command" - a
    caller treating 2 as a refusal would read every ordinary node as
    unrepresentable and refuse the whole fleet on a stale install.
    """
    from fno.agents.naming import AgentNameError, agent_name as _agent_name

    try:
        name = _agent_name(
            prefix,
            node_id,
            slug=slug or None,
            qualifier=qualifier or None,
            discriminator=discriminator or None,
        )
    except AgentNameError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(NAME_REFUSED_EXIT)
    typer.echo(name)


@agents_app.command("spawn-guard", hidden=True)
def cmd_spawn_guard(
    node_id: str = typer.Argument(
        ..., help="Backlog node id; the node:<id> claim is probed (Guard 1)."
    ),
    holder: str = typer.Option(
        ...,
        "--holder",
        help=(
            "Reservation holder string. On a `dispatchable` verdict the verb "
            "acquires dispatch:<id> for this holder; the caller releases it on a "
            "spawn failure and lets it TTL-expire on success."
        ),
    ),
    ttl: str = typer.Option("3m", "--ttl", help="TTL for the dispatch:<id> reservation (Guard 2)."),
    no_reserve: bool = typer.Option(
        False,
        "--no-reserve",
        help=(
            "Run Guard 1 (the node-claim probe) ONLY and never acquire the "
            "dispatch:<id> reservation. Side-effect-free; for a --dry-run / "
            "read-only verdict."
        ),
    ),
    cwd: str | None = typer.Option(
        None,
        "--cwd",
        help="Node project root for project-local failure policy and defer.",
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the verdict as a JSON object."
    ),
) -> None:
    """Shared bg-dispatch guard: the single source of truth for the dispatch mutex.

    Runs Guard 1 (the ``node:<id>`` claim probe, fail-closed) then Guard 2 (the
    create-only ``dispatch:<id>`` reservation) in one process, so the
    probe-then-reserve window is no wider than the two ``fno claim`` shell-outs it
    replaces. Both ``/target bg`` (``dispatch-node.sh``) and ``/agent spawn``
    (``spawn.sh``) call this so the two can never disagree about whether a node is
    dispatchable (x-73cc).

    Emits ONE verdict on stdout (a ``verdict=<v> key=value`` line, or a ``--json``
    object) in ``{dispatchable, already-running, refused, corrupted, error}``:

    \b
    - dispatchable    node free/stale. On a reserving call ``dispatch:<id>`` is
                      now held by ``--holder`` (the line carries reservation_key +
                      reservation_holder); under ``--no-reserve`` no reservation is
                      taken.
    - already-running a live ``node:<id>`` claim (reason=live-claim, holder=<owner>),
                      a suspect claim (reason=suspect-claim: TTL-unexpired dead pid,
                      a respawned worker - the caller maps this to skipped-contested,
                      x-ba4b), OR a racing dispatcher already holds ``dispatch:<id>``
                      (reason=reservation-held). No reservation acquired.
    - refused        the durable dead-dispatch limit blocked another birth
                      (reason=auto-deferred|defer-failed). No reservation acquired.
    - corrupted       the ``node:<id>`` claim is corrupted; launch nothing.
    - error           the claim probe failed or the reservation could not be
                      acquired (fail-closed); launch nothing.

    Exit 0 for every clean verdict (incl. already-running and corrupted). Exit
    non-zero ONLY for a usage error or a fail-closed guard error (verdict=error),
    so a stale ``fno`` without this verb (Typer "No such command") also fails
    closed in the caller.
    """
    obj, exit_code = _spawn_guard_decision(
        node_id,
        holder,
        ttl=ttl,
        no_reserve=no_reserve,
        cwd=cwd,
    )
    if json_output:
        line = json.dumps(obj)
    else:
        parts = [f"verdict={obj['verdict']}"]
        for key, value in obj.items():
            if key == "verdict":
                continue
            parts.append(f'{key}="{value}"' if key == "detail" else f"{key}={value}")
        line = " ".join(parts)
    sys.stdout.write(line + "\n")
    sys.stdout.flush()
    raise typer.Exit(code=exit_code)


@agents_app.command("ask", hidden=True)
def cmd_ask(
    name: str | None = typer.Argument(None, help="Agent name. Omit when using --to-project."),
    message: str | None = typer.Argument(None, help="Message to send."),
    harness: str | None = typer.Option(
        None,
        "--harness",
        "-H",
        help="The CLI binary to talk to: claude | codex | gemini (required on first ask).",
    ),
    _provider_tombstone: str | None = typer.Option(
        None,
        "--provider",
        hidden=True,
        help="Retired: the harness axis is --harness/-H; a model vendor routes "
        "only at spawn.",
    ),
    cwd: str | None = typer.Option(
        None, "--cwd", "-c", help="Working directory for the agent subprocess."
    ),
    timeout: int | None = typer.Option(
        None,
        "--timeout",
        "-t",
        help="Per-ask timeout in seconds (follow-up reply wait, default 600).",
    ),
    from_name: str = typer.Option(
        "fno",
        "--from-name",
        help=(
            "Identity advertised in the cross-session-message envelope "
            "on follow-up. Ignored on create. Must be XML-attribute-safe."
        ),
    ),
    yolo: bool = typer.Option(
        False,
        "--yolo",
        "-Y",
        help=(
            "Provider-specific dangerous-mode bypass. For codex: passes "
            "--dangerously-bypass-approvals-and-sandbox (replaces the "
            "default --sandbox workspace-write). For claude: no-op with "
            "a single-line stderr note. Opt-in; you own the blast radius."
        ),
    ),
    to_project: str | None = typer.Option(
        None,
        "--to-project",
        help=(
            "Anycast: ask whoever works on this project. ask is synchronous, so "
            "this resolves to exactly one live peer; none/ambiguous is an error "
            "(use `send --to-project` for the durable-queue path). Use instead of <name>."
        ),
    ),
    any_live: bool = typer.Option(
        False,
        "--any",
        help="With --to-project, break a multi-live-peer tie (most recent activity wins).",
    ),
    fresh: bool = typer.Option(
        False,
        "--fresh",
        help=(
            "Accepted no-op alias: the worker cwd already defaults to the "
            "canonical (main) repo root (x-85fe). Kept for dispatcher compat."
        ),
    ),
    here: bool = typer.Option(
        False,
        "--here",
        "--in-place",
        help=(
            "Keep the worker in the caller's cwd instead of the canonical-root "
            "default (WIP-scoped ask). The explicit opt-in."
        ),
    ),
) -> None:
    """Send a message to a registered agent (follow-up only).

    ``ask`` requires the agent to already exist. Unknown names exit 16
    with a hint pointing at ``fno agents spawn <name> --harness <harness>``.
    Use ``spawn`` / ``host`` for initial agent creation.

    Project mode (``ask --to-project <X> <message>``) resolves over the
    registry; because ask blocks for a reply it requires exactly one live
    peer (none/ambiguous exit nonzero).

    Prints the recipient's reply verbatim on stdout (US2 AC2-HP: no
    banner, no trailing newline added by fno). Failures surface on
    stderr with deterministic exit codes (see ``DispatchAskError``).
    """
    from fno.agents.dispatch import (
        AMBIGUOUS_PROJECT_EXIT_CODE,
        UNKNOWN_AGENT_EXIT_CODE,
        DispatchAskError,
        dispatch_ask,
        resolve_to_project,
    )
    from fno._flag_aliases import refuse_retired_provider

    refuse_retired_provider(_provider_tombstone)

    # ask is a follow-up to an existing session and never launches in workdir, so
    # it stays in the caller cwd (here=True): never the canonical default nor the
    # redirect note, which would be a false diagnostic for a non-consuming op
    # (x-85fe review). An explicit --cwd still wins inside the resolver.
    workdir = _resolve_dispatch_workdir(cwd, fresh, here=True)

    # Project mode: resolve to a single live peer, then ask by name. The message
    # is the sole positional, so it may land in the `name` slot.
    if to_project:
        content = message if message is not None else name
        if not content:
            print(
                "usage: fno agents ask --to-project <project> <message>",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        try:
            res = resolve_to_project(to_project, any_=any_live)
        except DispatchAskError as exc:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc
        if res.ambiguous:
            listing = ", ".join(res.live_candidates)
            print(
                f"--to-project {to_project!r} is ambiguous: {len(res.live_candidates)} "
                f"live peers ({listing}); pass --any or address one by name.",
                file=sys.stderr,
            )
            raise typer.Exit(code=AMBIGUOUS_PROJECT_EXIT_CODE)
        if res.recipient is None:
            print(
                f"no live peer working on project {to_project!r} to ask; "
                f"use `fno mail send --to-project {to_project} ...` to queue durable.",
                file=sys.stderr,
            )
            raise typer.Exit(code=UNKNOWN_AGENT_EXIT_CODE)
        name, message = res.recipient, content

    if not name or message is None:
        print(
            "usage: fno agents ask <name> <message>  (or --to-project <project> <message>)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    try:
        result = dispatch_ask(
            name=name,
            message=message,
            provider=harness,
            cwd=workdir,
            timeout=timeout,
            from_name=from_name,
            yolo=yolo,
        )
    except DispatchAskError as exc:
        # AC1-UI / AC2-UI: stderr surfaces the error, no extra wrapping.
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc

    # AC2-HP / AC2-UI: stdout is the reply verbatim, no added newline.
    # dispatch_ask only returns kind="followup" after this change;
    # kind="create" is returned by the spawn verb's helper, not here.
    sys.stdout.write(result.reply or "")
    sys.stdout.flush()


@agents_app.command("list")
def cmd_list(
    cwd: str = typer.Option(None, "--cwd", help="Filter by working directory."),
    harness: str = typer.Option(
        None, "--harness", "-H", help="Filter by harness (claude | codex | gemini)."
    ),
    _provider_tombstone: str = typer.Option(
        None,
        "--provider",
        hidden=True,
        help="Retired: filter by --harness.",
    ),
    status: AgentStatusFilter = typer.Option(
        None, "--status", help="Filter by liveness (live | orphaned | unknown)."
    ),
    progress: AgentProgressFilter = typer.Option(
        None,
        "--progress",
        help="Filter by the SECOND axis, progress -- not a finer --status "
        "(advancing | awaiting-operator | parked | refused | unknown).",
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit JSON regardless of TTY."),
    discovered: bool = typer.Option(
        True,
        "--discovered/--no-discovered",
        help="Include the host-local live-session lane (default on; "
        "--no-discovered skips the ~/.claude/sessions scan).",
    ),
) -> None:
    """List registered agents with optional filters.

    Output format follows Locked Decision 4: JSON when stdout is not a
    TTY OR ``--json`` is passed; human-readable table otherwise.

    The discovered-live-sessions lane (ab-098967b4) surfaces host-local,
    un-adopted Claude Code sessions so they are addressable by handle; pass
    ``--no-discovered`` to skip the registry scan.
    """
    from fno.agents.read import list_agents
    from fno._flag_aliases import refuse_retired_provider

    refuse_retired_provider(_provider_tombstone)

    status_value: str | None = status.value if status is not None else None
    progress_value: str | None = progress.value if progress is not None else None
    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())

    result = list_agents(
        cwd=cwd,
        provider=harness,
        status=status_value,
        progress=progress_value,
        json_out=json_out,
        tty=is_tty,
        discover=discovered,
    )
    for warn in result.warnings:
        sys.stderr.write(f"WARN: {warn}\n")
    if result.output:
        sys.stdout.write(result.output)
        if not result.output.endswith("\n"):
            sys.stdout.write("\n")
        sys.stdout.flush()
    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)


@agents_app.command("sweep", hidden=True)
def cmd_sweep(
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON row per worker, healthy ones included."
    ),
    deadline: int = typer.Option(
        None, "--deadline",
        help="Seconds of silence that make a worker a finding "
             "(default config.agents.silence_deadline_seconds, else 600).",
    ),
    budget: float = typer.Option(
        None, "--budget",
        help="Wall-clock budget in seconds; rows past it report unread rather "
             "than being dropped (default 20, which is what the daemon tick uses).",
    ),
) -> None:
    """Report workers whose transcripts have gone quiet past their deadline.

    The backstop for a refusal the taxonomy does not recognise. A harness can
    reword "usage limit reached" tomorrow, and a cap can arrive as a hang
    rather than a sentence; a clock closes what a marker list cannot.

    It reads the FULL registry rather than recovery's candidate set, because
    that set drops every non-claude row and every row with no live bg socket -
    so a codex successor spawned by failover is invisible to the very sweep
    that would catch ITS cap.

    It never acts. No stop, no spawn, no claim mutation, no row write. Silence
    has two explanations and a component that ACTS on the wrong one loses work,
    while one that merely reports it raises a false alarm. A worker whose
    transcript age is unknowable emits nothing at all: absence of evidence must
    not become a finding.
    """
    import json as _json

    from fno.agents.sweep import DEFAULT_SWEEP_BUDGET_S, run_sweep

    rows, silent = run_sweep(
        deadline_s=deadline,
        budget_s=DEFAULT_SWEEP_BUDGET_S if budget is None else float(budget),
        # A hand-run report is not a daemon observation, and the dedup memo
        # belongs to the daemon's cadence: a human running this twice wants two
        # answers, not one answer and a silence.
        source="cli",
        dedup=False,
    )
    unread = sum(1 for r in rows if r.unread)

    if json_out or not bool(getattr(sys.stdout, "isatty", lambda: False)()):
        typer.echo(_json.dumps([r.as_dict() for r in rows]))
        return

    if not rows:
        typer.echo("no registered workers")
        return
    for row in rows:
        age = "unknown" if row.age_s is None else f"{row.age_s}s"
        mark = "SILENT" if row.silent else "ok"
        typer.echo(
            f"{row.handle}  [{row.harness}]  {mark:<6} age={age} "
            f"deadline={row.deadline_s}s"
        )
    # Never a silent cap: a truncation nobody can see reads as full coverage.
    tail = f", {unread} unread (budget)" if unread else ""
    typer.echo(f"{silent} silent of {len(rows)}{tail}")


@agents_app.command("discovered-json", hidden=True)
def cmd_discovered_json(
    cwd: str = typer.Option(None, "--cwd", help="Filter discovered rows by cwd."),
    harness: str = typer.Option(
        None, "--harness", help="Filter discovered rows by harness."
    ),
    _provider_tombstone: str = typer.Option(
        None,
        "--provider",
        hidden=True,
        help="Retired: filter by --harness.",
    ),
) -> None:
    """Internal: emit the discovered-live-sessions lane as JSON.

    The real ``fno agents list`` auto-routes to the Rust client, which owns
    the rendered surface; that path shells out to THIS verb to merge the P1
    host-local live-session lane (ab-098967b4). Output is
    ``{"discovered_sessions": [...]}``. Fail-open: any error prints an empty
    lane and exits 0 so ``agents list`` is never broken by discovery.
    """
    import json as _json

    from fno._flag_aliases import refuse_retired_provider

    refuse_retired_provider(_provider_tombstone)

    out: dict = {"discovered_sessions": []}
    try:
        from pathlib import Path as _Path

        from fno.agents import discover as discover_mod
        from fno.agents.registry import load_registry

        try:
            entries = load_registry()
            exclude = {e.short_id for e in entries if e.short_id}
            # Projects-store rows key on full session_id (x-a1d5: no double-list).
            exclude_sids = {e.cc_session_id for e in entries if e.cc_session_id}
        except Exception:  # noqa: BLE001 — discovery never depends on a clean registry
            exclude = set()
            exclude_sids = set()

        rows = [
            s.to_row()
            for s in discover_mod.discover_live_sessions(
                exclude_short_ids=exclude, exclude_session_ids=exclude_sids
            )
            if s.is_alive
        ]
        if harness:
            rows = [r for r in rows if r.get("agent") == harness]
        if cwd:
            try:
                resolved = str(_Path(cwd).resolve())
            except OSError:
                resolved = cwd
            kept = []
            for r in rows:
                rc_raw = r.get("cwd") or ""
                # An empty cwd must NOT resolve to the process cwd and then
                # spuriously match the --cwd filter (gemini review).
                if not rc_raw:
                    continue
                try:
                    rc = str(_Path(rc_raw).resolve())
                except OSError:
                    rc = rc_raw
                if rc == resolved:
                    kept.append(r)
            rows = kept
        out["discovered_sessions"] = rows
    except Exception:  # noqa: BLE001 — fail-open: empty lane, never crash list
        pass
    sys.stdout.write(_json.dumps(out))


@agents_app.command("registry-json", hidden=True)
def cmd_registry_json() -> None:
    """Internal: emit registry rows DAEMON-FREE.

    Hooks (context-nudge.sh) need the stored crown + spawn-edge fields
    without the live-status enrichment that ``fno agents list`` lazy-starts the
    daemon for. Output is ``{"agents": [...]}`` with name / session ids /
    status / crown fields / spawned_by_session per row - a file read via
    load_registry, no daemon, so a Stop hook never stalls on a daemon start.
    """
    import json as _json

    from fno.agents.registry import load_registry

    rows = [
        {
            "name": e.name,
            "session_id": e.session_id,
            "harness_session_id": e.harness_session_id,
            "status": e.status,
            "crown_level": e.crown_level,
            "crown_scope": e.crown_scope,
            "spawned_by_session": e.spawned_by_session,
        }
        for e in load_registry()
    ]
    sys.stdout.write(_json.dumps({"agents": rows}))


#: `heal-token` exit codes. 13 mirrors the lifecycle verbs' not-found code; the
#: ambiguity code is distinct from BOTH that and typer's internal-error 1 so the
#: Rust caller can tell "refuse loudly with these candidates" from "degrade to
#: the original not-found error" (x-da8c AC4 vs AC5).
HEAL_TOKEN_MISS_EXIT = 13
HEAL_TOKEN_AMBIGUOUS_EXIT = 3
HEAL_TOKEN_UNAVAILABLE_EXIT = 12


@agents_app.command("heal-token", hidden=True)
def cmd_heal_token(
    token: str = typer.Argument(..., help="Session-shaped token (8-hex, UUID, ses_...)."),
    registry: str = typer.Option(
        None,
        "--registry",
        help="Adopt into THIS registry file (default: the configured one).",
    ),
    all_sources: bool = typer.Option(
        False,
        "--all-sources",
        hidden=True,
        help="Resolve against the registry and stores as one namespace.",
    ),
) -> None:
    """Internal: adopt the session TOKEN names from its harness store, as JSON.

    The one x-9cc5 healer behind ``registry.resolve_agent``, exposed so the Rust
    lifecycle verbs (logs/attach/resume) use the SAME probes rather than growing
    a second implementation. ``--all-sources`` also includes registry rows in
    the uniqueness decision. Exit 0 with the resolved row on stdout; 13 on a
    miss or a non-session-shaped token; 3 with the candidate list on stderr when
    the token is ambiguous; 12 when identity evidence is unavailable.

    ``--registry`` exists because the two runtimes resolve the registry
    differently -- Rust honors ``FNO_AGENTS_HOME``, this side does not -- so a
    caller that read one file would otherwise heal into another and re-heal on
    every later call. The caller names the file it read from; agreement is then
    by construction rather than by two resolvers happening to match.

    Python-only by construction: keeping it out of ``RUST_CLIENT_VERBS`` is what
    stops the Rust shellout from re-entering the Rust client.
    """
    import json as _json
    from dataclasses import asdict

    from fno.agents.registry import AgentResolutionError, resolve_from_harness_store

    if all_sources:
        from fno.agents.registry import resolve_agent

        try:
            resolved_entry = resolve_agent(
                token, path=Path(registry) if registry else None
            ).entry
        except AgentResolutionError as exc:
            if exc.ambiguous:
                sys.stderr.write(f"{exc}\n")
                raise typer.Exit(code=HEAL_TOKEN_AMBIGUOUS_EXIT)
            if exc.unavailable:
                sys.stderr.write(f"{exc}\n")
                raise typer.Exit(code=HEAL_TOKEN_UNAVAILABLE_EXIT)
            raise typer.Exit(code=HEAL_TOKEN_MISS_EXIT)
        sys.stdout.write(_json.dumps(asdict(resolved_entry)))
        sys.stdout.write("\n")
        return

    try:
        entry = resolve_from_harness_store(
            token, registry_path=Path(registry) if registry else None
        )
    except AgentResolutionError as exc:
        sys.stderr.write(f"{exc}\n")
        raise typer.Exit(code=HEAL_TOKEN_AMBIGUOUS_EXIT)
    if entry is None:
        raise typer.Exit(code=HEAL_TOKEN_MISS_EXIT)
    sys.stdout.write(_json.dumps(asdict(entry)))
    sys.stdout.write("\n")


@agents_app.command("codex-session-for-pid", hidden=True)
def cmd_codex_session_for_pid(pid: int = typer.Argument(..., help="Pane pid to probe.")) -> None:
    """Internal: resolve a codex pane's session id from its open rollout.

    Wraps ``mux_spawn._codex_session_id_for_pid`` (the pane-tree rollout walk
    already used at spawn time) so the Rust reconcile tick can late-bind a row
    whose spawn-time bind window expired, without a second implementation of
    the walk. Prints ``session_id=<id>`` and exits 0 on an unambiguous match;
    exits 13 with no stdout when the pid is gone, no rollout is open yet, or
    the tree holds more than one distinct session.
    """
    from fno.agents.mux_spawn import _codex_session_id_for_pid

    sid = _codex_session_id_for_pid(pid)
    if not sid:
        raise typer.Exit(code=HEAL_TOKEN_MISS_EXIT)
    sys.stdout.write(f"session_id={sid}\n")
    sys.stdout.flush()


@agents_app.command("nudge-peek", hidden=True)
def cmd_nudge_peek(
    session: str = typer.Option(..., "--session-id", help="Loop session id."),
    cwd: str = typer.Option(..., "--cwd", help="Session working directory."),
) -> None:
    """Internal: emit a one-line nudge for the oldest unread inbox message
    addressed to this session's project, advancing a per-session cursor so it
    surfaces once (P2, ab-098967b4). The loop-check verb shells out to this on
    a `block` decision. Prints nothing when there is no fresh unread; fail-open
    on any error so the loop is never broken.
    """
    from fno.agents.nudge import peek_nudge

    line = peek_nudge(session, cwd)
    if line:
        sys.stdout.write(line)


@agents_app.command("logs")
def cmd_logs(
    name: str = typer.Argument(..., help="Agent name (from `fno agents list`)."),
    tail: int = typer.Option(
        100,
        "--tail",
        "-n",
        help="Show only the last N lines of output (default 100; pass 0 for none).",
    ),
    follow: bool = typer.Option(
        False, "--follow", "-f", help="Stream output as the agent emits new lines."
    ),
    json_out: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit JSON-Lines (codex/gemini only; Claude is raw passthrough).",
    ),
) -> None:
    """Tail or follow an agent's log output.

    Claude agents pass through raw output from ``claude logs <short_id>``;
    exit code mirrors claude's. Codex/gemini agents that ship in US4 will
    read from their tee'd JSONL file; until then the verb returns exit
    13 with a precise "provider not yet shipped" message on stderr.
    """
    from fno.agents.read import read_logs

    if tail is not None and tail < 0:
        sys.stderr.write(f"--tail must be >= 0 (got {tail})\n")
        raise typer.Exit(code=2)

    # Distinguish "unbounded" (None) from "explicit zero" (0). The
    # boundary states `--tail 0` emits empty output and exits 0.
    effective_tail: int | None
    if tail is None:
        effective_tail = None
    else:
        effective_tail = tail

    result = read_logs(
        name=name,
        tail=effective_tail,
        follow=follow,
        json_out=json_out,
        stdout=sys.stdout,
        stderr=sys.stderr,
    )
    for warn in result.warnings:
        sys.stderr.write(f"WARN: {warn}\n")
    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)


@agents_app.command("peek", hidden=True)
def cmd_peek(
    handle: str = typer.Argument(
        ...,
        help="Peer handle (same as `fno mail send`: alias or bare hex short-id).",
    ),
    lines: int = typer.Option(
        15, "--lines", "-n", help="Show the last N transcript records (default 15; 0 for none)."
    ),
    follow: bool = typer.Option(
        False, "--follow", "-f", help="Stream new records as the peer emits them (read-only)."
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON-Lines rows instead of human lines."
    ),
) -> None:
    """Observe a peer read-only.

    Resolves ``<handle>`` to a live session and tails its transcript
    (claude/codex), preferring normalized status events when present. A
    pane-substrate worker (the default substrate) has no transcript; peek
    resolves it through the registry's mux ref and reads its pane. Never
    writes anything the peer reads. Exit 13 = unknown peer, 1 = known peer
    whose substrate has no reader or whose mux pane did not answer,
    0 = observed (or "no activity yet").
    """
    from fno.agents.peek import peek
    from fno.paths import state_dir

    if lines < 0:
        sys.stderr.write(f"--lines must be >= 0 (got {lines})\n")
        raise typer.Exit(code=2)

    events_path = state_dir() / "events.jsonl"
    rc = peek(
        handle,
        lines=lines,
        follow=follow,
        json_out=json_out,
        stdout=sys.stdout,
        stderr=sys.stderr,
        events_path=events_path if events_path.exists() else None,
    )
    if rc != 0:
        raise typer.Exit(code=rc)


@agents_app.command("whoami", hidden=True)
def cmd_whoami(
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit JSON regardless of TTY."),
) -> None:
    """Print THIS mesh worker's own registered name (+ registry enrichment).

    The derived-name peers use to address you via ``fno mail send <name>``.
    Resolves identity from ``FNO_AGENT_SELF`` (the env the spawn path
    injects), falling back to a registry row matching the active harness's
    session marker when the env is absent. Read-only: it never
    mutates the registry, emits an event, or writes state.

    Exit 0 when a name is resolved; exit 3 ("not a registered mesh agent")
    for a human / top-level session with no mesh identity. Distinct from
    ``fno whoami`` (top-level), which reports operating CONTEXT
    (fleet -> walker -> session -> harness), not the mesh name.
    """
    from fno.agents import whoami as whoami_mod
    from fno.agents.registry import RegistryVersionError, load_registry

    registry: list = []
    registry_error: str | None = None
    try:
        registry = load_registry()
    except RegistryVersionError as exc:
        registry_error = str(exc)

    # claude_agents_json() returns ({}, [warnings]) on a shellout failure
    # (missing binary / timeout / non-zero / parse) WITHOUT raising, so the
    # closure must forward those warnings out-of-band to be surfaced — else a
    # failed shellout would yield live_status: null with no WARN (the design
    # requires both).
    # Resolve THIS process's session id from whichever harness marker is set
    # (x-ec59): a codex/gemini worker resolves its own row via harness_session_id,
    # not just CLAUDE_CODE_SESSION_ID.
    from fno.harness_identity import resolve_harness_identity

    _ident = resolve_harness_identity()
    session_uuid = _ident.session_id
    # Scope registry matching to this process's harness so a provider-local session
    # id can't match a same-id row of another harness (x-ec59).
    session_harness = _ident.harness or ("claude" if session_uuid else None)
    live_warnings: list[str] = []

    def _live_status_fn(short_id: str) -> str | None:
        from fno.agents.harnesses import claude as claude_mod

        live_map, warns = claude_mod.claude_agents_json()
        live_warnings.extend(warns)
        return (live_map.get(short_id) or {}).get("live_status")

    result = whoami_mod.resolve_self(
        env=os.environ,
        registry=registry,
        registry_error=registry_error,
        session_uuid=session_uuid,
        live_status_fn=_live_status_fn,
        node_fn=lambda: whoami_mod.find_held_node(session_uuid=session_uuid),
        harness=session_harness,
    )

    for warn in (*result.warnings, *live_warnings):
        sys.stderr.write(f"WARN: {warn}\n")

    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())
    if json_out or not is_tty:
        sys.stdout.write(whoami_mod.render_json(result) + "\n")
    elif result.registered:
        sys.stdout.write(whoami_mod.render_human(result) + "\n")
    else:
        sys.stderr.write("not a registered mesh agent (human / top-level session)\n")

    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)


@agents_app.command("register", hidden=True)
def cmd_register(
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit JSON."),
    delivery_policy: str | None = typer.Option(
        None,
        "--delivery-policy",
        help=(
            "This session's mail delivery policy. 'bus-only' forbids prompt-line "
            "injection: mail to this session never pastes into its input buffer "
            "and always queues durable, surfaced at each turn boundary. 'off' "
            "clears back to the default injectable policy. A delivery-policy "
            "fact, never a liveness verdict. Omitted: leave the row unchanged "
            "(a re-firing SessionStart hook must not clobber a stamp)."
        ),
    ),
) -> None:
    """Join THIS session to the mesh roster so peers can `fno mail send` to it.

    The self-service seam behind ``/fno-me``: a session a human started by hand
    has no spawn-created roster row. This resolves the ambient harness identity
    (CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID / ...) and writes an ``idle`` row
    named by the canonical bare ``<shortid>`` handle, the same string the
    session self-stamps and drains, so a durable ``fno mail send`` to it lands.
    ``fno agents whoami`` then reports ``registered: true`` via its session-id
    fallback, no ``FNO_AGENT_SELF`` env needed.

    The handle is ALWAYS the canonical one (no custom-name override): a custom
    alias would not be drained by ``mail drain-self`` (which scans only the
    canonical handle), so mail to it would silently strand.

    Idempotent (re-running refreshes the row). Exit 3 for a session with no
    ambient harness identity (nothing addressable to register).
    """
    from fno.agents import events
    from fno.agents.registry import register_existing_session
    from fno.harness_identity import resolve_harness_identity

    if delivery_policy is not None and delivery_policy not in ("bus-only", "off"):
        sys.stderr.write(
            f"error: --delivery-policy accepts 'bus-only' or 'off', "
            f"got {delivery_policy!r}\n"
        )
        raise typer.Exit(code=2)

    ident = resolve_harness_identity()
    session_id = ident.session_id
    harness = ident.harness or ("claude" if session_id else None)
    if not session_id or not harness:
        sys.stderr.write(
            "no ambient harness identity - nothing to register "
            "(run /fno-me inside a claude/codex session)\n"
        )
        raise typer.Exit(code=3)

    try:
        entry = register_existing_session(
            provider=harness, session_id=session_id, cwd=os.getcwd(),
            origin="operator",
            delivery_policy=delivery_policy,
        )
    except Exception as exc:  # a deliberate manual join reports failure (unlike the fail-open hook)
        sys.stderr.write(f"register failed: {exc}\n")
        raise typer.Exit(code=1) from exc

    events.emit(
        "session_registered",
        provider=entry.harness,
        name=entry.name,
        session_id=session_id,
        cwd=entry.cwd,
    )
    if json_out or not bool(getattr(sys.stdout, "isatty", lambda: False)()):
        import json as _json

        sys.stdout.write(
            _json.dumps({
                "registered": True,
                "name": entry.name,
                "harness": entry.harness,
                "delivery_policy": entry.delivery_policy,
            }) + "\n"
        )
    else:
        policy_note = (
            " [bus-only: mail to this session queues durable, never injects]"
            if getattr(entry, "delivery_policy", None) == "bus-only"
            else ""
        )
        sys.stdout.write(
            f"joined the mesh as {entry.name}{policy_note} - peers can now reach you with "
            f"`fno mail send {entry.name} \"...\"`\n"
        )


@agents_app.command("top", hidden=True)
def cmd_top(
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit the same rows as JSON (script parity)."
    ),
    show_subagents: bool = typer.Option(
        False,
        "--subagents",
        help="Also list harness-native subagents (sidechain limbs) the census "
        "cannot see: read-only, claude-only, not slot-counted.",
    ),
) -> None:
    """Show every live worker process - fno-spawned and foreign claude bg
    alike - with pid, RSS (MB), and status (x-c5cc US4).

    The same union the spawn gate counts, so this is the audit surface every
    gate message points at. Python-only (RSS via psutil; not routed to the
    Rust client). ``--subagents`` (x-af92) appends a read-only sidechain
    section; those rows are observable but not addressable.
    """
    from fno.agents.top import render_top

    print(render_top(as_json=as_json, include_subagents=show_subagents))


@agents_app.command("orphans", hidden=True)
def cmd_orphans(
    reap: bool = typer.Option(
        False,
        "--reap",
        help="Kill findings that are BOTH fno-named and older than 10 minutes. "
        "Everything else is reported and left alone.",
    ),
    quiet_unless_new: bool = typer.Option(
        False,
        "--quiet-unless-new",
        help="Print nothing when every finding was already reported by a "
        "previous run. For the SessionStart nudge; a broken scan still speaks.",
    ),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit the same content as JSON."
    ),
) -> None:
    """Report processes that outlived whatever started them, with a control.

    The counterpart to ``hooks/bg-process-guard.py``: the guard refuses a
    process that can never end, this finds the ones that already survived. It
    is the only path-agnostic layer in that design, so a test fixture, a
    non-Claude harness and a leaking hook all land here.

    Before counting anything it plants two orphans of its own, one per arm of
    the attribution predicate, and must find both. When it cannot, it prints
    ``verdict withheld (scan-broken)`` and exits 2 WITHOUT an orphan count: a
    clean machine and a half-blind instrument must never print the same line.
    Break an arm on purpose with ``FNO_ORPHANS_SKIP_PROBE=name|cwd``.

    ``--reap`` kills only what we named ourselves. Attribution is a heuristic,
    and a heuristic must not hold a kill signal.
    """
    import json as _json

    from fno.agents.orphans import filter_new, render, scan, seen_path, to_json

    skip = os.environ.get("FNO_ORPHANS_SKIP_PROBE") or None
    result = scan(reap=reap, skip_probe=skip)
    speak = True
    if quiet_unless_new:
        # A broken scan always speaks: silence there is the exact failure this
        # command exists to make impossible.
        # `filter_new` is called FIRST, never behind a short-circuiting `or`.
        # Recording this scan's findings is its side effect, and skipping it on
        # a reaping run left the seen-file stale after exactly the most
        # interesting sweep. (A BROKEN scan records nothing - `filter_new`
        # handles that itself, because `render` withheld the list.)
        is_new = filter_new(result, seen_path())
        # A reap ALWAYS speaks. A finding reported at 8 minutes is no longer new
        # when the next sweep kills it past the age gate, and that path
        # SIGKILLed a process and printed nothing. A broken scan speaks for the
        # same reason.
        speak = result.broken or bool(result.reaped) or is_new
    if speak:
        print(_json.dumps(to_json(result), indent=2) if as_json else render(result))
    if result.broken:
        raise typer.Exit(2)


def _registry_falsifier(handle: str) -> str | None:
    """The falsifier for ``handle``, read off its registry row. Never raises.

    A handle with no registry row (a discovered-but-unadopted session) carries
    no falsifier, which is absence of evidence and NOT a death sentence.

    Matched on name, session id, AND short id, because callers key on different
    ones: a human types the name, while the Rust list path passes
    ``harness_session_id`` (``registry_truth_handle`` in daemon.rs). A
    name-only lookup silently returns "no falsifier" for every row on that path,
    which reads exactly like a healthy process and is how a guard ends up
    decorative on one of two reachable paths.
    """
    from fno.agents.reachability import registry_falsifier

    if not handle:
        return None
    try:
        from fno.agents.registry import load_registry

        row = next(
            (
                r
                for r in load_registry()
                if handle in {r.name, r.harness_session_id, r.short_id}
            ),
            None,
        )
    except Exception:  # noqa: BLE001 -- an unreadable registry falsifies nothing
        return None
    if row is None:
        return None
    return registry_falsifier(row)


def _truth_payload(result: dict, *, falsifier: str | None = None) -> dict:
    """The ``truth --json`` wire shape.

    This is the Python/Rust boundary: ``family1_truth_probe`` in
    ``crates/fno-agents/src/claude_ask.rs`` reads this, and ``resume`` decides
    "is live" from it. The reachability verdict has to be ON this
    wire or Rust keeps re-deriving liveness from the raw transcript ``state``
    and never sees the falsifier -- the same trap, one language over.

    ``state`` stays exactly as it was. Existing Rust consumers parse it, and
    overloading a field they already match on would be a silent contract break.
    """
    from fno.agents.reachability import classify_reachability

    reach = classify_reachability(
        truth_state=result.get("state"),
        age_s=result.get("last_activity_age_s"),
        falsifier=falsifier,
    )
    payload = {
        k: result.get(k)
        for k in (
            "handle",
            "state",
            "reason",
            "last_activity_age_s",
            "last_event_at",
            "last_message",
            "session_id",
            "observed_model",
        )
    }
    payload["reachability"] = reach.verdict
    payload["basis"] = reach.basis
    return payload


@agents_app.command("truth", hidden=True)
def cmd_truth(
    handle: str = typer.Argument(
        ..., help="Worker handle / short id / session id (as in `fno agents list`)."
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit a single JSON object instead of a line."
    ),
) -> None:
    """Classify a worker's supervision state from its transcript TAIL.

    done | watching | your-move | working | stalled | unknown -- read from the
    transcript, the only surface that does not lie about a live bg worker (argv,
    pid, the daemon record, and state.json's state field were each caught lying
    in one evening). This is the supervision state agent-view's working/idle
    cannot express. Read-only; exits 13 on an unresolvable handle (peek parity),
    0 otherwise.

    The line also names the model the worker is ACTUALLY answering as, read
    from the same transcript -- so a route that silently fell back to the
    primary vendor shows a `claude-*` id here and disagrees visibly with what
    the spawn asked for. A worker that came up and never answered reads "no
    model yet"; one with no transcript yet omits the clause entirely.
    """
    import json as _json

    from fno.agents.session_truth import render_truth, resolve_session_truth

    result = resolve_session_truth(handle)
    falsifier = _registry_falsifier(handle)
    if json_out:
        sys.stdout.write(_json.dumps(_truth_payload(result, falsifier=falsifier)) + "\n")
    else:
        payload = _truth_payload(result, falsifier=falsifier)
        sys.stdout.write(f"{render_truth(result)} [{payload['reachability']}: {payload['basis']}]\n")
    sys.stdout.flush()
    # Both are unresolvable-handle exits (13, the lifecycle not-found code); the
    # reason distinguishes the routine miss from a crashing resolver, which
    # callers use to decide whether the failure is worth surfacing.
    if result.get("state") == "unknown" and result.get("reason") in (
        "not-found",
        "resolver-error",
    ):
        raise typer.Exit(code=13)


@agents_app.command("watchdog")
def cmd_watchdog(
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit the machine-readable payload."
    ),
    apply: bool = typer.Option(
        False,
        "--apply",
        help=(
            "Execute the wake lane only - the one action that cannot destroy "
            "work. A ghost never auto-acts at any level."
        ),
    ),
    apply_all: bool = typer.Option(
        False,
        "--apply-all",
        help=(
            "Execute every lane: wake plus reap and reroute, which both stop "
            "a session. Implies --apply."
        ),
    ),
    only: Optional[str] = typer.Option(
        None, "--only",
        help="Filter to one verdict: wake|reroute|reap|ghost|stale|leave.",
    ),
    mail_to: Optional[str] = typer.Option(
        None,
        "--mail",
        help=(
            "Mail the digest to this handle (an agent name, short id, or "
            "project:<slug>). Defaults to config.recovery.watchdog_mail_to. "
            "Skipped when the non-leave verdict set is unchanged."
        ),
    ),
) -> None:
    """Sweep the fleet from transcript truth and decide, per row: wake,
    reroute, reap, or leave.

    The transcript is the truth source (keyed by session id); the registry
    and claude's agent view are hints. Dry run (default) prints every row
    with its verdict and the measurement that decided it, and emits one
    watchdog_verdict event per non-leave row.
    """
    import time as _time

    from fno.agents import watchdog as wd

    if only is not None and only not in wd.VERDICTS:
        print(f"fno agents watchdog: unknown verdict {only!r}", file=sys.stderr)
        raise typer.Exit(code=2)

    now = _time.time()
    payload, rows = wd.run_sweep(now_s=now)
    if payload.get("refused"):
        # x-4c87: a zero-row roster is an unreadable instrument, not an empty
        # fleet. Write no sweep file and advance no gate, so staleness reads
        # loud instead of certifying a healthy quiet fleet that was never read.
        print(f"fno agents watchdog: {payload['refused']}", file=sys.stderr)
        # The refusal says the roster was unreadable; the warnings say WHY
        # (timed out, binary missing, non-zero exit, budget headroom).
        # Dropping them leaves the one actionable line on the floor.
        for warning in payload.get("warnings") or []:
            print(f"  {warning}", file=sys.stderr)
        raise typer.Exit(code=3)
    pairs = [
        (wd.Verdict(**d), r) for d, r in zip(payload["verdicts"], rows)
    ]
    shown_counts = payload["counts"]
    if only is not None:
        pairs = [p for p in pairs if p[0].verdict == only]
        # A filtered view must not report the full sweep's counts: anything
        # cross-checking the rows it was handed against the counts would
        # disagree with both.
        shown_counts = {}
        for v, _row in pairs:
            shown_counts[v.verdict] = shown_counts.get(v.verdict, 0) + 1

    # Push, not pull: a verdict the king has to remember to fetch goes
    # unread. Mail before writing the sweep file, so the change gate compares
    # against the PREVIOUS sweep's signature - and only a delivered digest
    # advances it (mail_gate), or a transient send failure would permanently
    # swallow the verdict behind an unchanged signature.
    recipient = mail_to
    if recipient is None:
        try:
            from fno.config import load_settings

            recipient = str(getattr(
                load_settings().recovery, "watchdog_mail_to", "") or ""
            )
        except Exception:  # noqa: BLE001 - config read miss means no mail
            recipient = ""
    signature = ""
    try:
        ok, receipt, signature = wd.mail_gate(payload, recipient or "")
        if not ok:
            print(f"watchdog mail: {receipt}", file=sys.stderr)
    except Exception as exc:  # noqa: BLE001 - mail never breaks the sweep
        print(f"watchdog mail failed: {exc}", file=sys.stderr)
    # A filtered run publishes only its own rows, so it must not stamp the
    # whole non-leave set: doing so tells the next tick that ghost/wake rows
    # it never emitted were already published.
    events_payload = (
        payload if only is None
        else {**payload, "verdicts": [v._asdict() for v, _ in pairs]}
    )
    # A filtered run publishes a SUBSET, so its stamp has to be the union of
    # what it just published and what was already published. Stamping the
    # subset alone drops every filtered-out row from the record, and the next
    # tick re-emits all of them.
    prev_events_sig = wd._last_events_signature()
    signature_to_stamp = wd.union_signature(
        prev_events_sig, wd.verdict_signature(events_payload)
    ) if only is not None else wd.verdict_signature(events_payload)
    wd.write_sweep_file(
        "manual", payload["counts"], now, signature,
        events_signature=signature_to_stamp,
        terminal_harness_rows=payload.get("terminal_harness_rows", 0),
    )

    # Classification events ride every mode: a verdict emitted only under a
    # dry run left apply modes with no event record at all, while the tick
    # emits per non-leave row regardless of mode. The two lanes must not
    # diverge on what the record shows - and that cuts both ways. Emitting
    # ungated here duplicated every row the tick had already published, and
    # the stamp above then told the next tick they were all published, so a
    # filtered hand-run made the tick re-emit most of the fleet.
    fresh_ids = wd.fresh_non_leave(events_payload, prev_events_sig)
    for v, _row in pairs:
        if v.verdict != wd.LEAVE and v.row_id in fresh_ids:
            wd.emit_event(
                "watchdog_verdict",
                {"row_id": v.row_id, "name": v.name,
                 "verdict": v.verdict, "basis": v.basis},
            )

    if not apply and not apply_all:
        if json_out:
            filtered = {
                **payload,
                "verdicts": [v._asdict() for v, _ in pairs],
                "counts": shown_counts,
            }
            sys.stdout.write(json.dumps(filtered) + "\n")
            sys.stdout.flush()
            return
        for v, _row in pairs:
            typer.echo(f"{v.name:34} {v.state:9} {v.verdict:8} {v.basis}")
        for warning in payload["warnings"]:
            print(f"warning: {warning}", file=sys.stderr)
        counts = " ".join(f"{k}={v}" for k, v in sorted(shown_counts.items()))
        typer.echo(f"{len(pairs)} row(s): {counts}")
        typer.echo(
            f"terminal harness rows: {payload.get('terminal_harness_rows', 0)}"
        )
        return

    lanes = "all" if apply_all else "wake"
    results = []
    # One global provider rotation per sweep, shared across every row.
    rotation = wd.RotationBudget()
    for v, row in pairs:
        try:
            outcome, detail = wd.apply_verdict(
                v, lanes=lanes, cwd=row.cwd, rotation=rotation
            )
        except Exception as exc:  # noqa: BLE001 - one broken row never aborts the rest
            outcome, detail = "refused", f"{v.verdict} action crashed: {exc!r}"
        results.append({"row_id": v.row_id, "verdict": v.verdict,
                        "outcome": outcome, "detail": detail})
        if outcome == wd.SKIPPED:
            # The ONE silent outcome: a verdict outside the lane (every leave
            # row on a bare --apply). Printing one line per healthy row
            # drowns the few that acted. Everything else surfaces, so a new
            # outcome cannot go silent by not being listed here.
            continue
        line = f"{outcome:9} {v.name:34} {detail}"
        if outcome != "applied":
            print(line, file=sys.stderr)
        elif not json_out:
            # Human lines on stdout ahead of the JSON object make the whole
            # document unparseable; the dry-run path already guards this.
            typer.echo(line)
        # Every non-skipped outcome emits: the `outcome` field carries which
        # one it was, so no list here decides what is worth recording.
        wd.emit_event(
            "watchdog_applied" if outcome == "applied" else "watchdog_refused",
            {"row_id": v.row_id, "verdict": v.verdict, "detail": detail,
             "outcome": outcome},
        )
    if json_out:
        sys.stdout.write(json.dumps({"results": results}) + "\n")
        sys.stdout.flush()


@agents_app.command("ping", hidden=True)
def cmd_ping() -> None:
    """Health check (placeholder).

    The US4-lifecycle story converts this from a phase-1 stub into an
    informational message that defers the real probe to a future story.
    Returns exit 0 so the catalog of ``_NOT_IMPLEMENTED`` markers in
    ``cli.py`` shrinks to zero without growing a parallel verb surface.
    """
    typer.echo("(not yet implemented; planned for a future story)")


@agents_app.command("drive-authority", hidden=True)
def cmd_drive_authority(
    json_out: bool = typer.Option(False, "--json", "-J", help="Machine-readable output."),
) -> None:
    """Report whether an operator holds a gate-hardening drive window.

    Exits 0 when at least one agent has an interactive/step/paranoid drive
    window open, 1 when none -- so a hook can branch with
    ``if fno agents drive-authority --json >/dev/null; then ...``. Read-only.
    Gate-hardening consumers (stop hook, PreToolUse) use this to treat a
    ``<promise>`` or gate edit during a drive as operator-initiated (LD3).
    """
    import json as _json

    from fno.drive_authority import active_drive_sessions

    sessions = active_drive_sessions()
    if json_out:
        typer.echo(_json.dumps({"active": bool(sessions), "sessions": sessions}))
    elif sessions:
        for s in sessions:
            typer.echo(f"{s['short_id']} {s['mode']} {s['session_id']}")
    else:
        typer.echo("no active drive authority")
    raise typer.Exit(0 if sessions else 1)


@agents_app.command("stop")
def cmd_stop(
    name: str = typer.Argument(..., help="Agent name (from `fno agents list`)."),
) -> None:
    """Stop an agent's underlying session.

    Claude agents: shells out to ``claude stop <short_id>`` and prints
    ``stopped: <name> (<short_id>)`` on success. Codex / gemini agents
    are synchronous between asks - the verb is a no-op with an
    explanatory stderr line.
    """
    from fno.agents.dispatch import DispatchAskError, stop_agent

    try:
        stop_agent(name)
    except DispatchAskError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc


@agents_app.command("rm", hidden=True)
def cmd_rm(
    name: str = typer.Argument(..., help="Agent name (from `fno agents list`)."),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help=(
            "Drop the registry entry even when the row is LIVE or the harness "
            "teardown fails or refuses (e.g. uncommitted worktree changes). "
            "The Rust route (the default) kills a mux-hosted pane with it, "
            "but still refuses a live pane worker it cannot stop; the Python "
            "route drops the row only. The process survives for bg and "
            "headless rows. WARNING: leaves an orphan session record in that "
            "harness's own store, named on stderr, for you to clean manually."
        ),
    ),
) -> None:
    """Remove an agent: harness or mux session first, registry row after.

    Per-harness teardown:

    \b
      claude    bg session: `claude rm <short_id>`; pane session:
                `fno mux pane kill --session <session> <pane_id>`
      codex     drops the session's entry from ~/.codex/session_index.jsonl
      opencode  registry-only; `rm` will not delete an opencode session,
                because that also deletes its child sessions and its whole
                message history. Run `opencode session delete <id>` if you
                want the conversation gone.
      gemini    registry-only (no teardown arm for a deprecated provider)

    Your history is never removed here -- teardown drops the harness's
    index record, not the conversation. On teardown failure the registry
    row is kept so you can retry; ``--force`` drops it anyway and names
    the orphan in the receipt. A live row is refused by the Rust runtime
    (the default route; the Python runtime does not gate on liveness), and
    a blocked row names model rotation as its remedy. Terminal rows need
    no separate stop first.

    Worktrees are NOT removed here (the harness row does not prove that its
    cwd is disposable). Reap them with
    ``fno worktree cleanup --merged --apply``.
    """
    from fno.agents.dispatch import DispatchAskError, rm_agent

    try:
        rm_agent(name, force=force)
    except DispatchAskError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc


@agents_app.command("reconcile", hidden=True)
def cmd_reconcile(
    json_out: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit JSON regardless of TTY (mirrors `fno agents list --json`).",
    ),
) -> None:
    """Sync registry status with provider reality.

    For each registered agent, probe the underlying provider:

    - claude: ``claude logs <short_id> --tail 1`` exit code decides
      reachability.
    - codex: presence in ``~/.codex/session_index.jsonl`` decides.
    - gemini: skipped until US4-gemini ships.

    Status flips bidirectionally (``live`` ↔ ``orphaned``) and never
    deletes a row - operator decides removal via ``fno agents rm``.
    Output is human-readable by default, JSON when ``--json`` is passed
    or stdout is not a TTY (Locked Decision 4 mirror from ``list``).
    """
    import json

    from fno.agents.dispatch import DispatchAskError, reconcile_agents

    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())
    emit_json = json_out or not is_tty

    try:
        result = reconcile_agents()
    except DispatchAskError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc

    if emit_json:
        payload = {
            "scanned": result.scanned,
            "orphaned": result.orphaned,
            "recovered": result.recovered,
            "skipped": result.skipped,
            "errors": result.errors,
            # Always present (empty when nothing healed) so "ran, nothing to heal"
            # is distinguishable from "healed w1" in the JSON (x-ec59).
            "backfilled": result.backfilled,
        }
        sys.stdout.write(json.dumps(payload, sort_keys=False) + "\n")
        sys.stdout.flush()
        return

    render_reconcile_human(result, out=sys.stdout)
    sys.stdout.flush()


def render_reconcile_human(result, *, out) -> None:
    """Write one human-readable line per status change, then a roll-up.

    Extracted so test_cli_lifecycle can exercise the format without
    fighting Typer's CliRunner stdout capture (which never reports
    isatty=True). The aggregate counts mirror the JSON payload's keys
    so operators see the same numbers in both render modes.
    """
    for entry in result.orphaned:
        sid = entry.get("id") or "?"
        out.write(f"{entry['name']} ({entry['provider']}/{sid}): live → orphaned\n")
    for entry in result.recovered:
        sid = entry.get("id") or "?"
        out.write(f"{entry['name']} ({entry['provider']}/{sid}): orphaned → live\n")
    for entry in result.skipped:
        out.write(
            f"{entry['name']} ({entry['provider']}): skipped "
            f"({entry.get('reason', 'unspecified')})\n"
        )
    for entry in result.errors:
        out.write(
            f"{entry['name']} ({entry['provider']}): error ({entry.get('reason', 'unspecified')})\n"
        )
    for entry in getattr(result, "backfilled", []):
        sid = entry.get("harness_session_id") or "?"
        out.write(f"{entry['name']} ({entry['provider']}): harness_session_id backfilled ({sid})\n")

    out.write(
        f"{result.scanned} entries scanned: "
        f"{len(result.orphaned)} orphaned, "
        f"{len(result.recovered)} recovered, "
        f"{len(result.skipped)} skipped"
    )
    if result.errors:
        out.write(f", {len(result.errors)} errors")
    if getattr(result, "backfilled", []):
        out.write(f", {len(result.backfilled)} backfilled")
    out.write("\n")


@agents_app.command("attach")
def cmd_attach(
    name: str = typer.Argument(..., help="Agent name (from `fno agents list`)."),
) -> None:
    """Attach to a running claude agent session interactively.

    Claude path: shells out to ``claude attach <short_id>`` with inherited
    stdin/stdout/stderr - the claude TUI takes over until you detach.
    fno's exit code mirrors claude's on detach.

    Codex / gemini: refused with exit 13 and a hint pointing at Phase 6
    (the fno-owned supervisor) as the planned landing for cross-provider
    attach.
    """
    from fno.agents.dispatch import DispatchAskError, attach_agent

    try:
        result = attach_agent(name)
    except DispatchAskError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc

    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)


# ---------------------------------------------------------------------------
# Observability verbs: trace + resume (Tasks 3.3 / 3.4 / 3.5)
# ---------------------------------------------------------------------------
# Both commands live in their own modules so this CLI file stays focused
# on shape + wiring. The cmd_<verb> functions are re-bound here as
# Typer subcommands; tests can still monkeypatch cli.cmd_<verb> for
# spy injection.

from fno.agents.trace_cli import cmd_trace as _cmd_trace  # noqa: E402
from fno.agents.resume_cli import cmd_resume as _cmd_resume  # noqa: E402

agents_app.command("trace", hidden=True)(_cmd_trace)
agents_app.command("resume")(_cmd_resume)


# ---------------------------------------------------------------------------
# Gate verb (Task 2.3): per-provider injection verification gate management
# ---------------------------------------------------------------------------


@agents_app.command("gate", hidden=True)
def cmd_gate(
    provider: str = typer.Argument("", help="(retired at G4)"),
    probe: bool = typer.Option(False, "--probe", hidden=True),
    record: str | None = typer.Option(None, "--record", hidden=True),
    notes: str = typer.Option("", "--notes", hidden=True),
) -> None:
    """(retired at G4) The injection gate gated the daemon PTY-inject lane.

    ``agent.deliver`` + the injection gate were deleted when daemon PTY hosting
    moved to the mux, so there is no gate to probe or record. Prints a one-line
    pointer and exits non-zero rather than hitting ``UnknownMethod`` (codex P2).
    """
    _ = (provider, probe, record, notes)
    print(
        "fno agents gate was retired at G4: the injection gate gated the daemon "
        "PTY-inject lane (agent.deliver), deleted when agent panes moved to the mux. "
        "There is no gate to probe or record.",
        file=sys.stderr,
    )
    raise typer.Exit(code=2)
