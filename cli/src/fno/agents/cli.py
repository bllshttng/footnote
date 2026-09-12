"""`fno agents` Typer subapp.

``ask`` resolves its recipient and execs the Rust client binary (the Python
adapters are ported, parity frozen); ``list`` and ``logs`` are live.
"""

from __future__ import annotations

import enum
import json
import os
import re
import sys
import time
from pathlib import Path
from typing import Any, Optional

import typer

from fno.agents import launch_provenance
from fno.agents.harness_map import (
    PERMISSION_MODE_HELP,
    spawn_seed_receipt_fields,
    spawn_seed_receipt_fragment,
)
from fno.agents.rust_runtime import make_agents_group_cls

agents_app = typer.Typer(
    name="agents",
    help=(
        "Cross-CLI agent lifecycle (claude / codex / gemini / cursor-agent): "
        "spawn / watch / list / logs / stop. "
        "To message a peer, use `fno agents mail send <name>` (or the `/mail` skill)."
    ),
    no_args_is_help=True,
    # Default Rust runtime (Phase 6 W6 / cv-d28b266a): by default this group
    # execs the installed `fno-agents` binary for the verbs it implements, and
    # falls back to the Python dispatch below otherwise. FNO_AGENTS_RUNTIME=rust
    # forces the binary; =python forces this Python path. See rust_runtime.py.
    cls=make_agents_group_cls(),
)

class AgentStatusFilter(str, enum.Enum):
    """Served-activity words accepted by ``list --status`` (AC7): what the
    session is DOING, never a `live` token. See fno.agents.reachability."""

    writing = "writing"
    quiet = "quiet"
    parked = "parked"
    refused = "refused"
    orphaned = "orphaned"
    unknown = "unknown"


class AgentProgressFilter(str, enum.Enum):
    """Progress-axis values accepted by ``list --progress``.

    A SECOND axis beside ``--status``: the status word answers "what is it
    doing right now"; progress answers "is it advancing, awaiting the
    operator, parked, or refused". The two filter independently.
    """

    advancing = "advancing"
    awaiting_operator = "awaiting-operator"
    parked = "parked"
    refused = "refused"
    unknown = "unknown"


def _worker_token(worker: str) -> str:
    """Bare worker names, comma-joined, no spaces.

    Both shell consumers read the ``worker=`` token with ``sed -n 's/.*
    worker=\\([^ ;]*\\).*/\\1/p;q'``, which stops at the first space, so the
    worked overlay's ``<name> (unmeasurable: ...)`` label would truncate a
    name mid-token. The full labelled string stays on the JSON field.
    """
    names = [part.strip().split(" ")[0] for part in worker.split(",")]
    return ",".join(n for n in names if n)


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
        return f"  Clear it:  fno agents claim release {key} --force --reason '<why>'"
    return (
        f"  Clear it:  fno agents claim reap --apply      "
        f"(takes it only if no live worker is on the node)\n"
        f"  Override:  fno agents claim release {key} --force --reason '<why>'"
    )


#: Holder prefix of a reservation THIS verb writes. The targeted clear below is
#: scoped to it: every other producer of a `dispatch:` key has its own launch
#: contract, and `fno backlog advance` in particular relies on that reservation
#: outliving its own exit because it takes no node claim to replace it.
_SPAWN_CLI_HOLDER_PREFIX = "spawn-cli:"

#: Buckets where force-release advice is HONEST: recovery ran, nobody was found
#: on the node, and the claim is still there. Every other bucket is either a
#: measured live holder or an unmeasured one, and telling an operator to clear
#: something nobody checked is worse advice than none.
_REMEDIABLE_BUCKETS = frozenset({"release-failed", "suspect", "suspect_unprobed"})

#: `_reclaim_if_provably_dead` bucket meaning "a holder we PROVED is alive".
#: The discriminator between benign dedup and a wedge: somebody is genuinely
#: working, so the refusal is the system behaving correctly and there is nothing
#: for an operator to clear. Every other unrecovered bucket is a wedge.
_HOLDER_ALIVE = "live"


def _reclaim_if_provably_dead(
    key: str, *, probe=None, settlement=None
) -> tuple[str | None, str]:
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
    from fno.claims.verdict import claim_verdicts
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
        native = claim_verdicts([key], root=claims_root_for(key)).get(key)
        if native is None:
            return None, "unreadable"
        if key.startswith("dispatch:"):
            # The reservation's own predicate, deliberately NOT in the shared
            # sweep classifier. `spawn-cli:<pid>` launches a worker and exits, so
            # a dead pid means no launch is in flight from that process (the TTL
            # is the boot window; see the native classify_for_sweep decision),
            # but THIS caller is the next dispatcher, standing at the moment of
            # launch: the node claim it takes covers the window the reservation
            # protected. ONLY this dispatcher's own holder shape. `fno backlog
            # advance` reserves the same key as `advance:<pid>` and spawns
            # WITHOUT --node, so that reservation is the only barrier its
            # booting worker has; its pid is dead by design too, so a predicate
            # reading dead-pid-and-same-host alone cleared it and launched a
            # second worker onto the node advance had just staffed. LIVENESS
            # FIRST. A live holder is benign dedup whoever wrote it: answering
            # `foreign-reservation` there would print force-release advice
            # against a reservation somebody is actively launching under.
            if native.get("state") == "live":
                return None, _HOLDER_ALIVE
            if native.get("bucket") == "offhost":
                return None, "offhost"
            if not claim.holder.startswith(_SPAWN_CLI_HOLDER_PREFIX):
                return None, "foreign-reservation"
            provably_dead, bucket = True, ""
        else:
            try:
                provably_dead, bucket = sweep_verdict(
                    claim,
                    abandonment_probe=probe,
                    node_settlement=settlement,
                    native_verdict=native,
                )
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


def _init_reached(node_id: str, holder: str | None, cwd: str | None) -> bool:
    """True when a `fno target init` PROVABLY took this node claim.

    A live `node:<id>` claim proves a HOLDER exists. It does not prove a WORKER
    exists: a `spawn-handover:` claim covers a launch window whose worker can
    die before it boots, and a hand `fno agents claim acquire` from a live process
    takes the key with nothing launched at all. Reporting either as a live
    worker tells a king the opposite of the truth at the moment the king
    decides whether to staff the node.

    Two markers, both POSITIVE. This never reads an absence as a yes:

    1. The holder shape. Init acquires under `target-session:<id>`
       (``hooks/helpers/init-target-state.sh``). This is a CONVENTION, not
       proof: `fno agents claim acquire --holder target-session:anything` writes the
       same prefix, and a hand acquire is one of the cases this function exists
       to exclude. It is kept because it is the only marker that reaches a
       worker running in its own worktree, whose manifest this process cannot
       see. Marker 2 is the non-forgeable one.
    2. A manifest under CWD binding `target_claim_key: node:<id>` AND naming
       the OBSERVED holder in `target_claim_holder`. The stronger fact, and
       never the only one: a worktree worker's manifest lives under its own
       root, not the dispatcher's, so requiring it alone reports every real
       worktree worker as unproven.

    The holder match on marker 2 is what makes it a measurement rather than a
    snapshot. Manifest claim fields are written once at init and are never
    ownership truth, and a dispatcher is handed the NODE's project root as
    `--cwd`, which is exactly where a finished session leaves its manifest. A
    key-only test therefore reads a dead session's file and calls an unrelated
    holder a live worker, which is the lie this function exists to delete.

    Any read fault answers False. An unreadable manifest must never manufacture
    a worker.
    """
    from fno.agents.truth_status import _HOLDER_PREFIX

    holder = str(holder or "")
    if holder.startswith(_HOLDER_PREFIX):
        return True
    if not cwd or not holder:
        return False
    try:
        from pathlib import Path

        from fno.target.manifest import read_target_manifest

        raw = read_target_manifest(Path(cwd)) or {}
        return (
            str(raw.get("target_claim_key") or "") == f"node:{node_id}"
            and str(raw.get("target_claim_holder") or "") == holder
        )
    except Exception:  # noqa: BLE001 - an unreadable manifest proves nothing
        return False


def _claim_refused(action: str, common: dict[str, object]) -> dict[str, object]:
    """The shared refusal verdict shape for an auto-deferred node."""
    return {"verdict": "refused", "reason": action, **common}


def _spawn_guard_decision(
    node_id: str,
    holder: str,
    *,
    ttl: str = "3m",
    no_reserve: bool = False,
    cwd: str | None = None,
    handover_holder: str | None = None,
) -> tuple[dict[str, object], int]:
    """Return the shared family-2 pre-birth verdict without rendering it.

    ``handover_holder``, when given, also takes the ``node:<id>`` claim under
    that holder for the launch window, so the node reads as worked from the
    moment it is dispatched rather than from whenever the worker reaches its
    own ``fno do target init``.

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
        # Whether a `fno target init` provably took this claim. It travels in
        # `common` so every consumer branch can tell a worker that is starting
        # from one that never started, instead of every holder reading as a
        # live worker.
        "init_reached": _init_reached(node_id, observation.holder, cwd),
    }
    if observation.worker:
        # The overlay's answer, named beside `holder`: when the block came
        # from the worked overlay rather than the claim, `holder` reads the
        # literal "unknown" and the worker name is the only actionable field
        # on the payload.
        common["worker"] = observation.worker
    if observation.action in ("auto-deferred", "defer-failed"):
        return _claim_refused(observation.action, common), 0
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
            from fno.claims.cli import (
                RosterReading,
                _abandonment_probe,
                _node_settlement,
                read_roster,
            )

            # The roster is READ HERE, outside the recovery mutex. Reading it
            # under the lock shells out to the harness while holding a mutex
            # `compare_and_rebind` waits only five seconds for, so a peer's
            # probe could make the worker's own `fno do target init` handover fail
            # as claim-held-by-other. Handing the reading in leaves nothing
            # under the lock but a dictionary lookup. A FAILED read is handed
            # down as an honestly unconsulted reading for the same reason:
            # both instruments would otherwise lazily re-read inside the
            # mutex, and unknown keeps is the safe answer anyway.
            try:
                reading = read_roster()
            except Exception:  # noqa: BLE001 - an unread roster proves nothing
                reading = RosterReading(False, 0, {}, "roster read failed before the guard")
            prior, _bucket = _reclaim_if_provably_dead(
                node_key,
                probe=_abandonment_probe(reading),
                # Same reading, same reason: the settlement's roster lookups
                # must stay outside the recovery mutex too (x-94f8).
                settlement=_node_settlement(reading),
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
                    "init_reached": _init_reached(
                        node_id, observation.holder, cwd
                    ),
                }
                if observation.worker:
                    common["worker"] = observation.worker
                # The cleared claim makes this the first reading with the node
                # free, so the failure-limit arm can fire here for the first
                # time. Report what it decided. Falling through would label an
                # auto-deferred node `already-running` and hand back a
                # force-release remedy that does nothing for it.
                if observation.action in ("auto-deferred", "defer-failed"):
                    return _claim_refused(observation.action, common), 0
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
            # THREE reasons, not two. `live-claim` now asserts only what was
            # measured: a holder AND a target init behind it. A held claim
            # nobody has booted a worker for reads `unproven-claim`, so a
            # reader can tell a worker that is starting from one that never
            # started. `live-claim` and `suspect-claim` stay byte-identical
            # wherever init was reached, so no existing consumer branch moves.
            # block_reason wins only for the authority outage; an occupied
            # node keeps the stable machine token, evidence rides the refusal.
            block = observation.block_reason
            # ONE reading of "the block_reason itself is the answer", used by
            # both arms below. Spelling it twice is how they drift.
            block_wins = bool(block) and not str(block).startswith("held:")
            # The worker ROW is the occupant whenever the claim is not. A
            # stale claim plus a worker the overlay named read `unproven-claim`
            # and named the claim's prior holder, which sent the reader after
            # a release that frees nothing: the operator followed that text to
            # a claim `fno agents claim status` read as UNCLAIMED. The row is
            # what blocked, so the receipt names it and the remedy peeks it.
            # block_reason stays first: an authority outage is not a worker.
            if (
                not block_wins
                and observation.worker
                and observation.verdict not in ("ours", "foreign_live")
            ):
                from fno.graph.statuses import UNMEASURABLE_LABEL_MARK

                first = _worker_token(observation.worker).split(",")[0]
                # The overlay admits a row whose liveness it could NOT measure,
                # marked. That mark is the only thing saying so, and the bare
                # machine token drops it, so it rides its own field: a receipt
                # that reads the same for a measured and an unmeasured row
                # asserts more than anything observed, and peek can come back
                # showing nothing at all for the unmeasured one.
                unmeasured = UNMEASURABLE_LABEL_MARK in observation.worker
                return {
                    "verdict": "already-running",
                    "reason": "worker-row",
                    "worker": observation.worker,
                    "truth_status": observation.truth_status,
                    **({"worker_unmeasured": True} if unmeasured else {}),
                    "remedy": (
                        f"fno agents peek {first}; if its run is finished, "
                        f"fno agents stop {first}"
                        + (
                            f"; liveness was never measured for this row, so read "
                            f"fno agents claim status node:{node_id} too"
                            if unmeasured else ""
                        )
                    ),
                }, 0
            reason = (
                block if block_wins
                else "suspect-claim" if wedged
                else "live-claim" if common["init_reached"]
                else "unproven-claim"
            )
            return {
                "verdict": "already-running",
                "reason": reason,
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

    def _reserve_dispatch_slot() -> None:
        acquire_claim(
            res_key,
            holder,
            reason=f"bg-dispatch reservation for {node_id}",
            ttl_ms=_parse_ttl(ttl),
            root=claims_root_for(res_key),
        )

    try:
        _reserve_dispatch_slot()
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
                # A remedy is EARNED, and this names the buckets that earn it
                # rather than the ones that do not. The exclusion polarity gave
                # force-release advice to every bucket nobody had thought about
                # yet, `foreign-reservation` among them - and that one is `fno
                # backlog advance`'s boot barrier, the single reservation this
                # file says must never be cleared. An operator following the
                # printed advice double-dispatches onto a node advance just
                # staffed.
                **({"remedy": _remedy_for(res_key)} if bucket in _REMEDIABLE_BUCKETS
                   else {}),
            }, 0
        try:
            _reserve_dispatch_slot()
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
    out: dict[str, object] = {
        "verdict": "dispatchable",
        "reservation_key": res_key,
        "reservation_holder": holder,
    }
    if handover_holder:
        # THE node claim, not another reservation. dispatch:<id> is a launch-
        # window mutex on a key nobody reads: five workers were spawned with an
        # explicit --node tonight and not one of them was visible to `fno agents claim
        # status node:<id>`, so four kings read those nodes as free (x-cd1e).
        #
        # --node is the only dispatch path holding the node id as a TYPED
        # argument rather than as prose to be re-derived, which is why the claim
        # belongs here and why there are exactly two producers of this key, not
        # more. The other is `fno do target init`, and the worker inherits this
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
            # already claimed this node through its own `fno do target init` is
            # invisible to it. Swallowing this as best-effort put a second
            # worker on a node a live session was building, which is the whole
            # failure this PR exists to close.
            # Hand back the reservation THIS call took. `cmd_spawn` records it
            # only after a dispatchable verdict, so nothing downstream releases
            # it, and `classify_for_sweep` refuses to reap a `dispatch:` key
            # inside its TTL. Leaving it blocked every dispatcher for 3m over a
            # node somebody else legitimately holds.
            _release_dispatch_claims((res_key, holder))
            # Same split as the verdict above, for the same reason. A guard
            # placed on one of two paths producing one lie is decorative: this
            # arm fires when the read said free and the acquire lost the race,
            # and the winner is no more proven to be a worker than any other
            # holder.
            race_holder = getattr(exc, "holder", "") or "unknown"
            return {
                "verdict": "already-running",
                "reason": (
                    "live-claim"
                    if _init_reached(node_id, race_holder, cwd)
                    else "unproven-claim"
                ),
                "holder": race_holder,
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
                # Same release as the branch above: this refusal must not keep
                # the reservation it took.
                _release_dispatch_claims((res_key, holder))
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
#: to boot and reach its first `fno do target init`. It stays short because a spawn
#: that dies inside it strands the node until expiry, and an expired claim is
#: provably dead on its own so the wedge self-clears.
HANDOVER_TTL = "15m"


def _release_dispatch_claims(*claims) -> None:
    """Release every claim a failed dispatch took, best-effort.

    One helper for both failure paths. They used to release only the
    reservation, in two copies, and adding the node claim to one of them is how
    a guard ends up on one of N paths.

    A failed release is SWALLOWED but never SILENT. Swallowing is right: this
    runs on the way out of a failure, and raising here would mask the error
    being handled. Saying nothing is not. A release that quietly no-ops leaves
    the key held for its whole TTL under a holder that is gone, which is the
    exact wedge this file exists to delete - so the no-op path names the key it
    failed to free instead of reporting the same nothing as success.
    """
    from fno.claims.core import release_claim
    from fno.claims.io import claims_root_for

    for pair in claims:
        if pair is None:
            continue
        key, holder = pair
        try:
            release_claim(key, holder, root=claims_root_for(key))
        except Exception as exc:  # noqa: BLE001 - must not mask the real error
            print(
                f"WARNING: could not release {key} held by {holder} ({exc}); "
                f"it stays held until its TTL expires. Free it with: "
                f"fno agents claim release {key} --holder {holder}",
                file=sys.stderr,
            )


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

    The shared dispatch.rpc_roundtrip framing, to an arbitrary worker socket
    (the stream worker serves ``stream.*`` directly). Returns the ``result``
    dict, or None on any transport/error response.
    """
    from fno.agents.dispatch import rpc_roundtrip

    return rpc_roundtrip(
        sock_path,
        method,
        params,
        connect_timeout=connect_timeout,
        read_timeout=read_timeout,
    )


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
        "",
        help=(
            "Existing registered session handle to crown in place, or the "
            "current heir when --reclaim is run from an attended shell."
        ),
    ),
    scopes: list[str] = typer.Option(
        [],
        "--scope",
        help=(
            "Territory to grant. Repeat for a multi-project portfolio or a "
            "set of epics; the crown level is derived and cannot be supplied."
        ),
    ),
    reclaim: bool = typer.Option(
        False,
        "--reclaim",
        help=(
            "Return the current holder's crown to its recorded grantor without "
            "creating a session."
        ),
    ),
) -> None:
    """Crown an existing session from an attended shell, or from an agent whose
    own crown strictly contains the requested scope.

    Run `fno agents register` inside the target session, then run this command
    with its printed handle. Same-scope succession stays on the spawn-time
    transfer path. A row already holding a crown is re-scoped rather
    than refused: the new territory replaces the old in one atomic write, the
    level is derived from the new scope, the registry records the actual
    grantor, and the receipt reports what was vacated. `--reclaim` returns a
    transferred crown to its recorded grantor and never creates a session.
    """
    from fno.agents import events
    from fno.agents.crown import CrownPromotionError, promote_existing_session

    if reclaim:
        from fno.agents.crown import reclaim_crown

        try:
            receipt = reclaim_crown(handle or None)
        except CrownPromotionError as exc:
            print(f"crown reclaim: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc
        events.emit(
            "agent_crown_reclaimed",
            reclaimed=receipt["reclaimed"],
            from_holder=receipt["from_holder"],
            scope=receipt["scope"],
            grantor=receipt["grantor"],
        )
        print(json.dumps(receipt))
        return

    if not handle or not scopes:
        print("crown: HANDLE and at least one --scope are required", file=sys.stderr)
        raise typer.Exit(code=2)

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


# The court command moved to fno.agents.court (file budget); the
# composition stays on the agents app here.
from fno.agents.court import register_court_command  # noqa: E402

register_court_command(agents_app)


# Moved to fno.agents.spawn_lineage (x-5c25, file budget); re-exported here.
from fno.agents.spawn_lineage import (  # noqa: E402
    _stamp_launch_edge,
    _stamp_spawned_session_row,
)

def _parse_wait_seconds(raw: str) -> float:
    """``--wait`` duration: seconds by default, s/m/h suffixes. Raises ValueError."""
    match = re.fullmatch(r"(\d+(?:\.\d+)?)([smh]?)", raw.strip(), re.IGNORECASE)
    if not match:
        raise ValueError(raw)
    return float(match.group(1)) * {"": 1, "s": 1, "m": 60, "h": 3600}[match.group(2).lower()]



@agents_app.command("spawn")
def cmd_spawn(
    message: str = typer.Argument("", help="The prompt to seed the worker with."),
    passthrough: list[str] | None = typer.Argument(
        None,
        help=(
            "Provider CLI flags after a `--` fence; pane substrate only."
        ),
    ),
    name: str = typer.Option(
        "",
        "--name",
        help=(
            "Agent name; an adjective-noun slug is minted when omitted. The "
            "one positional is the prompt."
        ),
    ),
    harness: str | None = typer.Option(
        None,
        "--harness",
        "-H",
        help=(
            "The CLI binary to launch; default: the invoking harness, then "
            "claude. Any other binary on PATH also spawns, into a pane with "
            "fno as the viewport; pass its init flags after '--'."
        ),
    ),
    vendor: str | None = typer.Option(
        None,
        "--provider",
        "-P",
        help=(
            "The model VENDOR (zai or a model_routing.providers name), paired "
            "with --model. NOT the CLI binary (-H). -p is headless."
        ),
    ),
    recorded_provider: str | None = typer.Option(
        None,
        "--recorded-provider",
        hidden=True,
        help=(
            "Machine-recorded model-vendor identity for a recovery spawn. "
            "Unlike --provider, this does not configure a Claude route."
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
        "",
        "--substrate",
        help=(
            "thread (default where the harness seats one) | pane (mux PTY) | "
            "headless (one-shot). Placement flags and a `--` fence imply pane."
        ),
    ),
    headless: bool = typer.Option(
        False,
        "--headless",
        "-p",
        help="Shortcut for --substrate headless: a one-shot worker. Wins over --substrate.",
    ),
    sandbox_write_policy: str | None = typer.Option(
        None,
        "--sandbox-write-policy",
        help=(
            "JSON policy whose `sandbox` block joins the worker's ONE "
            "--settings file beside the hook deny_edit list. Pane refuses."
        ),
    ),
    cwd: str | None = typer.Option(
        None,
        "--cwd",
        "-c",
        help=(
            "Working directory for the agent subprocess. On spawn, -c is "
            "--cwd; a harness's own -c config spelling rides the -- fence "
            "instead (codex: -- -c key=value)."
        ),
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
            "Provider dangerous-mode bypass: codex --dangerously-bypass-"
            "approvals-and-sandbox; claude bypassPermissions. Conflicts with "
            "--permission-mode."
        ),
    ),
    fresh: bool = typer.Option(
        False,
        "--fresh",
        help="No-op alias: the worker cwd already defaults to the canonical root (x-85fe).",
    ),
    here: bool = typer.Option(
        False,
        "--here",
        "--in-place",
        help="Keep the worker in the caller's cwd instead of the canonical-root default.",
    ),
    role: str | None = typer.Option(
        None,
        "--role",
        help=(
            "Per-spawn model-selection role; auxiliary roles route to a secondary provider."
        ),
    ),
    route: str | None = typer.Option(
        None,
        "--route",
        help=(
            "Explicit route provider/model (zai/glm-5.3). Wins over --role; "
            "FAILS CLOSED on an unknown provider or missing key. claude only."
        ),
    ),
    monitor: str | None = typer.Option(
        None,
        "--monitor",
        help="Expose this spawn through a monitor; 'happy' only, claude+zai, pane.",
    ),
    account: str | None = typer.Option(
        None,
        "--account",
        help=(
            "Pin this ONE worker to a registered claude account; fail-closed, "
            "claude only. Semantics: docs/guides/agents-spawn-flags.md."
        ),
    ),
    dispatch_account: str | None = typer.Option(
        None,
        "--dispatch-account",
        help=(
            "Provider RECORD from `dispatch resolve --autonomous`; its env "
            "rides for ANY harness, credentials never travel. Fail-closed."
        ),
    ),
    model: str | None = typer.Option(
        None,
        "--model",
        "-m",
        help="Forwarded as --model <m> to the provider's own CLI. Unset = provider default.",
    ),
    permission_mode: str | None = typer.Option(
        None,
        "--permission-mode",
        help=PERMISSION_MODE_HELP,
    ),
    effort: str | None = typer.Option(
        None,
        "--effort",
        help=(
            "Reasoning effort, passed through to the selected provider/model; "
            "unset uses its default."
        ),
    ),
    resume: str | None = typer.Option(
        None,
        "--resume",
        "-r",
        help=(
            "Seed a NEW claude session from a transcript: content carries "
            "over, the id does NOT. Same-id revival: fno agents resume."
        ),
    ),
    add_dir: str | None = typer.Option(
        None,
        "--add-dir",
        help=(
            "Extra write access for the worker; opencode/gemini reject it."
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
            "Pin the new pane next to the caller; `--at current` reads FNO_PANE, fails closed. Needs --split."
        ),
    ),
    tab: str | None = typer.Option(
        None,
        "--tab",
        help=(
            "Mux tab selector (number, id:<n>, name:<s>, active/new, or group name). Pane only."
        ),
    ),
    bounded_placement: bool = typer.Option(
        False,
        "--bounded-placement",
        hidden=True,
        help=(
            "Automated placement: serialized under the mux lease, max four "
            "panes per tab."
        ),
    ),
    crown: list[str] = typer.Option(
        [],
        "--crown",
        "-k",
        help=(
            "Grant an orchestrator crown: epic id(s), one project, or "
            "several. Refused on headless. Contract: "
            "docs/guides/agents-spawn-flags.md."
        ),
    ),
    succeed: bool = typer.Option(
        False,
        "--succeed",
        help=(
            "Explicitly transfer a caller-held --crown territory to the spawned "
            "heir. Without this flag, a same-scope crown is refused and the "
            "caller keeps its crown."
        ),
    ),
    node: str | None = typer.Option(
        None,
        "--node",
        help=(
            "Backlog node this pane works: exports FNO_NODE/FNO_SLUG/FNO_PLAN "
            "for prompt provenance (--slug/--plan override the graph read)."
        ),
    ),
    slug: str | None = typer.Option(
        None, "--slug", help="Provenance FNO_SLUG override (skips the graph read)."
    ),
    plan: str | None = typer.Option(
        None, "--plan", help="Provenance FNO_PLAN override (skips the graph read)."
    ),
    session_phase: str = typer.Option(
        "",
        "--session-phase",
        help=(
            "Lifecycle phase for the sessions row a node-bearing spawn opens. "
            "Empty infers from the message; no node resolved means no row."
        ),
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help="Bypass the max_live cap and the RAM floor; the worker is still counted.",
    ),
    no_wait: bool = typer.Option(
        False,
        "--no-wait",
        help=("Fail immediately when max_live is reached instead of queueing for a free slot."),
    ),
    wait: str | None = typer.Option(
        None,
        "--wait",
        help=(
            "Retry a REFUSED gate axis for up to this long (5m, 90s, 1h). "
            "The waitable set is the gate's own capacity refusals ("
            "WAITABLE_REFUSAL_REASONS); anything else exits at once with its "
            "receipt. Conflicts with --no-wait."
        ),
    ),
    prompt_file: str | None = typer.Option(
        None,
        "--prompt-file",
        help="Read the prompt from a file ('-' = stdin) instead of the positional.",
    ),
) -> None:
    """Spawn a new agent; ``ask`` is the follow-up lane.

    Receipts: pane = one JSON line with mux_session + pane_id; claude bg
    thread = compact JSON ({\"name\", \"short_id\", \"harness\", \"status\"}, plus
    \"provider\"/\"model\" only when a route or model was applied); --once = the
    provider reply verbatim. Plain codex/gemini spawn needs the fno-agents
    daemon; this Python path exits 13 with guidance. Flag reference:
    docs/guides/agents-spawn-flags.md.
    """
    # --squad is a hidden back-compat alias for --workspace (US2); --workspace wins.
    squad = squad if squad is not None else squad_compat

    if prompt_file is not None:
        from fno.text_or_file import read_text_arg

        message = read_text_arg(message or None, prompt_file, what="the prompt") or ""

    from fno.agents.dispatch import DispatchAskError, SpawnResult, dispatch_spawn
    from fno.dispatch_flags import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_harness,
    )

    workdir = _resolve_dispatch_workdir(cwd, fresh, here)
    # `-c` is `--cwd` on spawn: codex's own `-c key=value` config spelling
    # silently becomes a working directory. Stop before launch, name the fence.
    if cwd and not Path(cwd).exists():
        print(
            f"working directory {cwd!r} does not exist; no worker launched. "
            "On spawn, -c is --cwd. Codex config overrides ride the fence: "
            "-- -c key=value",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    # x-85fe: the effective launch dir surfaces in the receipt on the DEFAULT
    # move (a node-less spawn now lands on canonical), coupled with the stderr
    # redirect note. An explicit --cwd (incl. -P/node-resolved) is the caller's
    # own choice and never surfaces -- gate on `not cwd` so the receipt stays
    # byte-identical for explicit-cwd and stay-put spawns (AC1-EDGE).
    _moved_cwd = str(workdir) if not cwd and workdir != Path(os.getcwd()).resolve() else None

    # Three orthogonal axes: --harness names the CLI binary, --provider the model
    # vendor that binary talks to, --model the model at that vendor. The local
    # carrying the harness is now spelled for its own axis: 40-odd refusals below
    # read from it, and each one taught the operator the wrong word for the thing
    # they had to change. The dispatch_spawn/dispatch_ask kwargs moved with it;
    # the RECEIPT key stays `provider` until wave 4 moves it with its consumers,
    # which is why a `provider=harness` call survives at the pane seam below.
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

    # --harness is optional: resolve it (explicit > invoking harness > claude)
    # and reject an empty --model before anything spawns. Every rung of that
    # chain is a harness, which is why the resolver is spelled for that axis.
    # The value is a concrete string from here down; the harness-name set is
    # validated substrate-aware further in.
    try:
        harness, harness_source = resolve_dispatch_harness(harness)
        model = reject_empty_model(model)
    except DispatchFlagError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=2) from exc
    if recorded_provider is not None:
        recorded_provider = recorded_provider.strip()
        if not recorded_provider or model is None:
            print(
                "--recorded-provider requires a non-empty --model",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        if vendor is not None and recorded_provider != vendor:
            print(
                "--recorded-provider must match --provider when both are supplied",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
    # Provenance rides the pane receipt's harness_source field below - it is
    # the HARNESS axis's provenance, not the vendor's. The bg/once stdout
    # receipts stay byte-parity-locked with the Rust client, so they skip it.

    # The substrate axis (x-2c27): headless is the ergonomic shortcut (x-c772);
    # an empty value resolves to the built-in default (thread where seated).
    from fno.agents.harness_map import DispatchResolveError, thread_seatable, thread_uncarried

    # A flag the harness's thread lane cannot carry (a typed axis or a fenced
    # token) resolves pane, the same way pane geometry does. One fenced token
    # with no message is the legacy seed idiom; the pane is where that seed
    # has ever been read, so it demotes there too.
    uncarried = thread_uncarried(
        harness,
        {
            "model": model, "yolo": yolo, "permission_mode": permission_mode,
            "effort": effort, "add_dir": add_dir, "launch_role": role,
            "agent": agent, "tools": tools, "deny_tools": deny_tools,
        },
        passthrough,
    )
    defaulted = False
    if headless:
        substrate = "headless"
    if not substrate and once:  # --once always means a one-shot
        substrate = "headless"
    if not substrate:
        # Empty = unset: pane capability implies pane; else thread where seated.
        defaulted = True
        pane_implied = bool(
            passthrough or split or at or tab or bounded_placement or squad
            or monitor is not None or uncarried is not None
        )
        try:
            seatable = thread_seatable(harness)
        except DispatchResolveError:  # an undeclared harness seats no thread
            seatable = False
        if pane_implied or not seatable:
            substrate = "pane"
        else:
            substrate = "thread"
    # `--once` is the pre-substrate spelling of headless, but Python leaves it on
    # the pane default; a routed `--once` would then reach dispatch as
    # claude+once+not-headless and die on the "persistent bg threads" refusal.
    if once and substrate == "pane":
        substrate = "headless"
    # A thread seat meeting an uncarried flag demotes loudly, named seat or
    # defaulted; an explicit pane or headless one-shot stays silent.
    if (
        uncarried is not None
        and substrate != "headless"
        and (defaulted or substrate in ("thread", "bg"))
    ):
        if substrate in ("thread", "bg"):
            substrate = "pane"
        print(
            f"fno agents spawn: substrate: pane (the {harness} thread lane "
            f"has no carrier for {uncarried})",
            file=sys.stderr,
        )

    from fno.agents.spawn_defaults import resolve_spawn_gates, seedless_thread_refusal

    substrate = resolve_spawn_gates(substrate, monitor, once=once, harness=harness)
    seedless = seedless_thread_refusal(
        harness, substrate, message, resume=resume, crown=bool(crown), name=name, node=node
    )
    if seedless:
        print(f"fno agents spawn: {seedless}", file=sys.stderr)
        raise typer.Exit(code=2)

    if output_format is not None and (
        harness != "claude" or substrate != "headless" or output_format != "json"
    ):
        print(
            "--output-format supports only 'json' on claude headless spawns",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # US4 revival: --resume continues an existing claude --bg transcript, so it
    # only applies to the claude bg lane (the Python bg_create path forwards
    # --resume <uuid>). An unset harness defaults to claude downstream.
    if resume is not None and (substrate != "bg" or harness not in (None, "claude")):
        print(
            "--resume requires --substrate bg on harness claude "
            "(it continues an existing claude --bg session)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    if effort is not None:
        from fno.agents.mux_spawn import effort_tokens

        try:
            effort_tokens(harness, effort)
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
    # substrate maps every provider, so it's exempt here. (x-dfa4) The codex
    # thread lane is exempt too: the shared app-server resolves the posture
    # (resolve_thread_posture), so a mapped mode rides it natively.
    codex_thread_lane = harness == "codex" and substrate in ("thread", "bg") and not once
    if (
        permission_mode is not None
        and harness != "claude"
        and (substrate != "pane" or once)
        and not codex_thread_lane
    ):
        remedy = (
            "drop --permission-mode and pass -Y/--yolo"
            if harness == "codex"
            else "use --substrate pane"
        )
        print(
            f"--permission-mode is not supported for harness {harness!r} on "
            "--substrate bg/headless (its one-shot lane hardcodes its own bypass "
            f"form); {remedy}",
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
        if add_dir and harness not in ("claude", "codex", "agy"):
            bad = "--add-dir"
        elif agent and harness != "claude":
            bad = "--agent"
        elif tools and harness != "claude":
            bad = "--tools"
        elif deny_tools and harness != "claude":
            bad = "--deny-tools"
        if bad is not None:
            # No "use --substrate pane" advice: pane rejects the same tier3 cells
            # (gemini --add-dir, codex --agent), so it would mislead. The fence
            # is the one carrier for the harness's own flag spelling.
            print(
                f"{bad} is not supported for harness {harness!r}; if it is the "
                "harness's own flag, pass it after the -- fence",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)

    from fno.agents.spawn_defaults import placement_refusal

    refusal = placement_refusal(
        substrate=substrate, once=once, squad=squad, split=split, at=at,
        tab=tab, bounded_placement=bounded_placement,
    )
    if refusal is not None:
        print(refusal, file=sys.stderr)
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
    if account is not None and harness != "claude":
        print(f"--account is claude-only; got harness {harness!r}", file=sys.stderr)
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
            from fno.agents.harness_map import capabilities_or_undeclared

            # x-f579: the posture answers route_on_pane=False for an
            # undeclared harness, so the refusal stays clean. `capabilities()`
            # would raise an uncaught DispatchResolveError here (exit 1
            # traceback) on a lane this change made reachable.
            if not capabilities_or_undeclared(harness).get("route_on_pane", False):
                print(
                    f"harness {harness!r} does not have the evidence-backed "
                    "route_on_pane capability; no worker launched, node stays "
                    "dispatchable.",
                    file=sys.stderr,
                )
                raise typer.Exit(code=2)
        if harness != "claude":
            print(
                f"{route_spelling} requires the claude harness; "
                f"got harness {harness!r} substrate {substrate!r}.",
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
    if harness == "claude" and (role is not None or route_env):
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

    # The proven-credential carrier (outage handoff) is claimed unconditionally:
    # it carries real credential values, so a spawn that somehow sets it without
    # --dispatch-account must refuse here rather than let the child inherit the
    # carrier verbatim (a leak outside the one staged overlay that was proved).
    proven_env_raw = os.environ.pop("FNO_DISPATCH_ACCOUNT_ENV", None)
    if proven_env_raw is not None and dispatch_account is None:
        print(
            "refusing: the proven-credential carrier is set but no "
            "--dispatch-account was given; no worker launched",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # The autonomous-cutover carrier. Same fail-closed posture as --account, and
    # deliberately a separate flag: --account's claude-only refusal is an operator
    # contract, while a cutover's whole point is landing on another harness.
    if dispatch_account is not None:
        from fno.adapters.providers.dispatch import dispatch_env
        from fno.adapters.providers.loader import load_providers

        # The proven-credential carrier (outage handoff): the supervisor's
        # health canary proved a SPECIFIC env for this account and passed it
        # through the FNO_DISPATCH_ACCOUNT_ENV environment carrier (never
        # argv). When present, it IS the dispatch overlay - re-deriving from
        # the record here could stage different credentials than the ones
        # proved, which is the exact disconnect that made the canary's proof
        # decorative. The record still must exist and match the harness: the
        # env proves the values, this check proves the target.
        proven_env: dict[str, str] | None = None
        if proven_env_raw:
            try:
                parsed = json.loads(proven_env_raw)
                if not isinstance(parsed, dict) or not all(
                    isinstance(k, str) and isinstance(v, str)
                    for k, v in parsed.items()
                ):
                    raise ValueError("carrier payload must be a flat string map")
                if not parsed:
                    raise ValueError("carrier payload is empty")
                proven_env = parsed
            except (TypeError, ValueError) as exc:
                print(
                    f"refusing --dispatch-account {dispatch_account!r}: the "
                    f"proven-credential carrier is unreadable ({exc}); "
                    "no worker launched",
                    file=sys.stderr,
                )
                raise typer.Exit(code=2) from exc

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
            if rec_harness != harness:
                raise ValueError(
                    f"record is a {rec_harness or '<no harness>'} account but "
                    f"the spawn resolves {harness}"
                )
            dispatch_overlay = (
                proven_env if proven_env is not None
                else dispatch_env(dispatch_account, repo_root=workdir)
            )
            account_env = {
                **(account_env or {}),
                **dispatch_overlay,
            }
            if proven_env is not None:
                credential_source = "canary-proven carrier"
                credential_env_keys = sorted(proven_env)
            else:
                credential_source = "record-derived"
                credential_env_keys = sorted(dispatch_overlay)
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
        DispatchResolveError,
        apply_merge_posture_env,
        check_loop_participation,
        message_carries_no_merge,
        normalize_legacy_no_merge,
    )

    message = normalize_legacy_no_merge(message)
    if prov_env is not None and message_carries_no_merge(message):
        prov_env["TARGET_NO_MERGE"] = "1"

    # The loop gate, on the same message and for the same reason as the carrier
    # above. resolve_dispatch runs this check too, and the comment three lines
    # up is why it is not enough: a direct spawn never reaches that resolver, so
    # the guard would have covered every path but the one operators use most. A
    # harness that cannot close a loop takes the /target and runs forever with
    # nothing to stop it, and no instrument reports that.
    try:
        check_loop_participation(harness, message)
    except DispatchResolveError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=2)

    # x-4342: the sessions row a node-bearing spawn opens. An explicit
    # --session-phase is the operator's label and wins; empty infers from the
    # work's own shape - a /target-family message names a do worker (whose
    # claim-acquire stamp duplicate-fills the same row), a review verb names
    # the reviewer, a blueprint or think verb names the planner. Arbitrary
    # prose is a label this code cannot guess and never defaults to review: a
    # review row is a retirement blocker for life, so an unlabeled task keeps
    # no row rather than a lying one. Fail-closed on an unknown explicit
    # value, like the guards above, before anything spawns.
    from fno.graph.types import SESSION_PHASES

    if session_phase:
        if session_phase not in SESSION_PHASES:
            print(
                f"--session-phase must be one of {sorted(SESSION_PHASES)} "
                f"(got {session_phase!r})",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        stamp_phase = session_phase
    else:
        from fno.agents.spawn_phase import infer_phase

        stamp_phase = infer_phase(message)
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

    # The node guard sits BELOW the resume-provider resolution on purpose.
    # It acquires `dispatch:<id>` (and the handover `node:<id>`), and every
    # exit above it is an exit that would strand those keys for their whole
    # TTL with nothing launched. Taking the reservation after the launch is
    # proven is one placement; a release bolted onto each exit is a guard on
    # one of N paths, and the next exit added to that stretch leaks again.
    # Below this point the next exit is `run_gate`, whose `except BaseException`
    # releases both keys. Not the ONLY one: the TARGET_NO_MERGE set-or-clear
    # block sits between a successful `run_gate` and the `try` whose `finally`
    # releases, so an exception there still leaks both. Narrow, and named rather
    # than papered over.
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
            # `reason` FIRST, and detail as its own field. Both shell consumers
            # read this line with `sed -n 's/.* reason=\([^ ;]*\).*/\1/p;q'`,
            # so whatever lands in `reason=` is the machine token they switch
            # on. `detail` is a prose sentence, and the acquire-race return sets
            # BOTH - so leading with detail put `node:<id>` in the slot, matched
            # no case arm in either consumer, and dropped a benign refusal into
            # the generic failure handler. Detail is still printed, just not in
            # the slot something parses.
            guard_reason = (
                guard.get("reason") or guard.get("verdict") or "unknown"
            )
            prior = f" prior_holder={guard['holder']}" if guard.get("holder") else ""
            worker = (
                f" worker={_worker_token(str(guard['worker']))}"
                if guard.get("worker") else ""
            )
            # The post-spawn consumers read tokens, not the JSON, so the
            # unmeasured qualifier has to travel as one or the two receipts
            # disagree about the same row.
            if guard.get("worker_unmeasured"):
                worker += " worker_unmeasured=true"
            detail = f" detail={guard['detail']!r}" if guard.get("detail") else ""
            print(
                f"node dispatch refused: node={guarded_node} "
                f"verdict={guard.get('verdict')} reason={guard_reason}{prior}"
                f"{worker}; no worker launched{detail}",
                file=sys.stderr,
            )
            # This is the launch path, so a remedy here HAS earned itself:
            # recovery ran and could not prove the holder dead. Printing it is
            # what makes the way out reach an operator; the shell callers read
            # this stream and pass it through.
            if guard.get("remedy"):
                print(guard["remedy"], file=sys.stderr)
            raise typer.Exit(code=guard_exit or 2)
        # str() at the boundary: the verdict dict carries a bool
        # (`init_reached`) beside its strings, so these keys are typed `object`
        # coming out and the claim keys are strings going in.
        node_reservation = (
            str(guard["reservation_key"]),
            str(guard["reservation_holder"]),
        )
        if guard.get("node_claim_key"):
            # Released on the SAME two failure paths as the reservation. A
            # launch that dies after the claim must not strand the node for the
            # whole handover window; that is the wedge this PR exists to delete,
            # reintroduced by its own fix.
            node_claim = (
                str(guard["node_claim_key"]),
                str(guard["node_claim_holder"]),
            )
            # The worker proves it is the intended successor by naming this
            # holder back. It travels in the environment, never on the command
            # line, so it reaches exactly the process spawned for this node.
            prov_env["FNO_NODE_CLAIM_HOLDER"] = str(guard["node_claim_holder"])
        elif guard.get("node_claim_error"):
            print(
                f"note: node:{guarded_node} claim not taken at dispatch "
                f"({guard['node_claim_error']}); the worker claims it at init",
                file=sys.stderr,
            )

    # Spawn gate (x-c5cc): the SOLE gate on every path reaching cmd_spawn,
    # exactly one evaluation per spawn (LD1). --wait loops HERE in the CLI,
    # not in the gate: the gate core has a Rust twin under a parity harness,
    # and a retry wrapper touches neither.
    if wait is not None and no_wait:
        print("error: --wait and --no-wait are mutually exclusive", file=sys.stderr)
        raise typer.Exit(code=2)
    wait_seconds = 0.0
    if wait is not None:
        try:
            wait_seconds = _parse_wait_seconds(wait)
        except ValueError:
            print(
                f"error: --wait wants a positive duration like 5m, 90s or 1h (got {wait!r})",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)

    from fno.agents.spawn_gate import WAITABLE_REFUSAL_REASONS, GateRefused, run_gate

    # Anything outside the gate's own waitable set (policy or config)
    # exits at once - waiting out a verdict the gate will not revisit is
    # a hang wearing a retry's clothes.
    waitable_reasons = WAITABLE_REFUSAL_REASONS
    wait_deadline = time.monotonic() + wait_seconds if wait is not None else None
    last_wait_note = 0.0
    while True:
        try:
            gate = run_gate(
                name,
                "headless" if (once or substrate == "headless") else substrate,
                force=force,
                no_wait=no_wait or wait is not None,
                route_provider=route_provider,
            )
            break
        except GateRefused as exc:
            reason = (
                exc.receipt.get("reason")  # type: ignore[assignment]
                if isinstance(exc.receipt, dict)
                else None
            )
            now = time.monotonic()
            if wait_deadline is None or reason not in waitable_reasons or now >= wait_deadline:
                _release_dispatch_claims(node_reservation, node_claim)
                if exc.receipt is not None:
                    print(json.dumps(exc.receipt))
                raise
            if last_wait_note == 0.0 or now - last_wait_note >= 60.0:
                sys.stderr.write(
                    f"spawn-gate: {reason}; --wait retries for "
                    f"{int(wait_deadline - now)}s more\n"
                )
                last_wait_note = now
            time.sleep(min(10.0, wait_deadline - now))
        except BaseException:
            _release_dispatch_claims(node_reservation, node_claim)
            raise

    # Prior values of the provenance keys the bg/headless arm exports below, so
    # the finally can put the process env back.
    prov_prev: dict[str, "str | None"] = {}
    # x-9d11 set-or-clear BEFORE any substrate branch: the pane transport
    # inherits os.environ directly, so an inherited carrier must be cleared
    # here too, not only on the bg/headless export path (review round 7).
    # The helper returns the PRIOR value, captured before the mutation.
    prov_prev["TARGET_NO_MERGE"] = apply_merge_posture_env(message)

    # Per-worker identity for task claims: the roster name is the only
    # identity a spawned worker can prove is ITS OWN (a session id inherited
    # from a shared manifest or a daemon-anchored pid collapses siblings onto
    # one holder). Overwrite any inherited value - a dispatcher that spawned
    # this spawn must not leak ITS name into the child. The pane lane
    # snapshots os.environ into pane_env below, and the bg/thread lanes hand
    # the detached session this process's environment, so one write covers
    # both. Registered in prov_prev so the finally restores the caller's env,
    # the same scoping contract FNO_NODE is pinned to.
    prov_prev["FNO_WORKER_NAME"] = os.environ.get("FNO_WORKER_NAME")
    os.environ["FNO_WORKER_NAME"] = name

    # The write policy resolves BEFORE the substrate branch so a pane spawn
    # refuses instead of silently dropping the enforcement it was handed.
    # Claude-only for the same reason: the sandbox block composes into the
    # claude --settings payload, so another harness would take the flag and
    # enforce nothing - the exact partial-jail this option exists to prevent.
    sandbox_settings: dict[str, object] | None = None
    if sandbox_write_policy:
        if substrate == "pane" and not once:
            print(
                "--sandbox-write-policy needs the thread substrate: a pane "
                "spawn's --settings slot is reserved by the mux hook server, "
                "so a pane cannot carry the OS write allowlist.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        if harness != "claude":
            print(
                f"--sandbox-write-policy composes into the claude --settings "
                f"payload; harness {harness!r} would carry the flag and "
                f"enforce nothing. Drop the flag or spawn claude.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        from fno.agents.model_routing import load_sandbox_write_policy

        sandbox_settings = load_sandbox_write_policy(sandbox_write_policy)

    # `--once` is the pre-substrate spelling of headless (the Rust client maps
    # it to --substrate headless): it always means a one-shot, never a pane.
    spawn_succeeded = False
    try:
        if substrate == "pane" and not once:
            from fno.agents.mux_spawn import dispatch_spawn_bounded_pane

            try:
                pane_dispatch = dispatch_spawn_bounded_pane
                pane_kwargs: dict[str, Any] = dict(
                    name=name,
                    message=message,
                    provider=harness,
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
                    split=split,
                    at=at,
                    tab=tab,
                    bounded_placement=bounded_placement,
                    crown_level=crown_level,
                    crown_scope=crown_scope,
                    succession=succeed,
                    provenance=prov_env,
                    account_env=account_env,
                    route_env=route_env,
                    monitor=monitor,
                    route_provider=route_provider,
                    provider_gate=gate,
                    route_provider_id=route_provider or recorded_provider,
                    model_name=model or route_model,
                    account_record_id=dispatch_account or account,
                    passthrough=passthrough,
                    launch_account=account or dispatch_account,
                    route_model=route_model,
                )
                pane_kwargs["workspace"] = squad
                pane_result = pane_dispatch(**pane_kwargs)
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
                "harness_source": harness_source,
                "status": pane_result.status,
                "mux_session": pane_result.session,
                "pane_id": pane_result.pane_id,
                # Two facts the receipt used to conflate. `bound` says the
                # worker reached its provider; `status` alone could not, so a
                # pane about to bind and one already dead read identically.
                "bound": pane_result.bound,
                # Independent delivery fact. Null means no seed was requested.
                # `unattempted` is printed before the command exits non-zero: no
                # send was made and the frame could not say otherwise, so the
                # seed is unverified. A genuine `unconfirmed` submit never
                # reaches this receipt - it fails the spawn inside
                # dispatch_spawn_pane.
                "seed": pane_result.seed,
            }
            if pane_result.seed_source is not None:
                receipt_obj["seed_source"] = pane_result.seed_source
            # The other half of the same question, and the discriminator for the
            # non-zero exit below. `seed: submitted` with
            # `pane_observation: unreadable` is a delivered payload on a pane
            # nobody could see; the two used to arrive fused as `unattempted`,
            # which said the opposite about the seed.
            if pane_result.pane_observation is not None:
                receipt_obj["pane_observation"] = pane_result.pane_observation
            if getattr(pane_result, "claim_store_writable", None) is False:
                from fno.claims.io import claims_dir, global_claims_root

                receipt_obj["claim_store_writable"] = False
                receipt_obj["claim_store_path"] = str(
                    claims_dir(global_claims_root())
                )
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
            if getattr(pane_result, "stamp_failure", None) is not None:
                # (x-b029, AC4-ERR) The stamp step reported its own failure: the
                # row is id-less and no retry is coming, so the receipt says so
                # instead of returning zero with a silent None.
                receipt_obj["stamp_failure"] = pane_result.stamp_failure
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
            if route_provider is not None or recorded_provider is not None:
                receipt_obj["provider"] = route_provider or recorded_provider
            receipt_model = model or route_model
            if receipt_model is not None:
                # v23 (x-2019): the label is the point. At receipt time this
                # token is the REQUEST - the pane's first turn has not landed,
                # so no observation exists yet to print instead. An unlabeled
                # token here is how the request passed for the effect.
                receipt_obj["model"] = receipt_model
                receipt_obj["model_basis"] = "requested"
            if pane_result.session_uuid is not None:
                receipt_obj["session_id"] = pane_result.session_uuid
            effective_message = getattr(pane_result, "effective_message", None)
            if effective_message is not None:
                receipt_obj.update(spawn_seed_receipt_fields(effective_message))
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
            # Locked Decision 5, renamed by x-74ea/x-d401: name the REQUESTED
            # mode so an audit of "why did this worker have edit rights" has a
            # durable answer - and only as a request, because fno cannot back
            # the outcome (a forced-default environment ignores the flag while
            # the old key read as an applied grant). Only when set, so the
            # unset receipt is unchanged.
            if permission_mode is not None:
                receipt_obj["permission_mode_requested"] = permission_mode
            # x-d012: name the pinned account so a mis-pin is visible at spawn
            # time, not at billing time. Only when set (receipt byte-stable else).
            # x-04ce: the account fact carries WHO chose it.
            _account, _source = launch_provenance.receipt_account_fields(
                pane_result.launch_account, pane_result.launch_account_source, account
            )
            if _account is not None:
                receipt_obj["account"] = _account
                if _source is not None:
                    receipt_obj["account_source"] = _source
            if dispatch_account is not None:
                receipt_obj["dispatch_account"] = dispatch_account
                # Name the credential provenance and the env keys actually
                # staged (never their values): a reader can tell a
                # canary-proven overlay from a record re-derivation, which is
                # the difference the outage handoff's proof rests on.
                if account_env is not None:
                    receipt_obj["credential_source"] = credential_source
                    receipt_obj["credential_env_keys"] = credential_env_keys
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
            # x-4342: the node this pane works gets its sessions row now, while
            # the receipt holds the worker identity. Runs before the exit-22
            # check on purpose: an unverified seed still left a live pane, and
            # that pane's provenance is real whether its payload landed or not.
            # node= is the ALREADY-resolved FNO_NODE from the provenance pass
            # (`or {}`: resolve_provenance is Optional, like every prov_env
            # consumer below treats it).
            _stamp_spawned_session_row(
                node=(prov_env or {}).get("FNO_NODE"), message=message, phase=stamp_phase,
                worker_name=pane_result.name, worker_harness=pane_result.provider,
                worker_session_uuid=pane_result.session_uuid,
                worker_effort=effort,
            )
            _stamp_launch_edge((prov_env or {}).get("FNO_NODE"))
            # Exit 22 means one thing: a receipt WAS written and something on it
            # is unverified - the seed, or the pane the seed was handed to. Both
            # leave a caller holding a row it cannot trust, which is what exit 22
            # has always told it, so the code is REUSED rather than split.
            # Splitting it would be the change that breaks readers: every script
            # branching on 22 today already does the right thing here, and a new
            # code reaches those scripts as an unrecognized failure. The receipt
            # carries `seed` and `pane_observation` for a reader that wants to
            # tell the two apart, which is the whole point of separating them.
            #
            # `unconfirmed` is deliberately absent, and not because it is
            # verified: it fails the spawn inside dispatch_spawn_pane and never
            # reaches a receipt at all, so keying on it here would be a dead
            # branch. `unattempted` is a live pane whose frame could not be
            # read. `unknown` is a live pane whose submit mux never answered.
            # Both leave a caller holding a row it cannot trust, which is what
            # exit 22 has always told it.
            # A seed that rode in the harness argv can no longer reach
            # `unattempted`: its delivery is settled at exec time and is
            # classified before the frame is consulted. So exit 22 no longer
            # fires for a late-painting pane on claude/codex/opencode, which
            # is the false alarm that had readers re-seeding or reaping live
            # workers. It still fires for the pane-send path, where an
            # unreadable frame really does leave the send unmade.
            #
            # `pane_observation` is the third key, and dropping it here would
            # make the whole split decorative: the argv arms that used to say
            # `unattempted` on an unreadable frame now say `submitted`, so
            # without this clause a pane that may already be gone would exit 0
            # and its row would hold a slot against max_live. That protection is
            # the reason those arms lied in the first place; it is kept, and
            # only the reason given for it changes.
            if (
                pane_result.seed in ("unattempted", "unknown")
                or pane_result.pane_observation == "unreadable"
            ):
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
                harness=harness,
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
                passthrough=list(passthrough) if passthrough else None,
                headless=substrate == "headless",
                output_format=output_format,
                resume_session_id=resume,
                account_env=account_env,
                launch_account=account or dispatch_account,
                route_provider_id=route_provider or recorded_provider,
                model_name=model or route_model,
                account_record_id=dispatch_account or account,
                crown_level=crown_level,
                crown_scope=crown_scope,
                succession=succeed,
                route_provider=route_provider,
                provider_gate=gate,
                sandbox_settings=sandbox_settings,
                # x-98ab: the ALREADY-resolved FNO_NODE from the provenance
                # pass - the node this spawn is FOR, not the ambient value.
                node=(prov_env or {}).get("FNO_NODE"),
                # The route's model token, recorded on the row (the receipt
                # already names it; the row now matches).
                route_model=route_model,
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
        # A one-shot's worker has already exited by the time `dispatch_spawn`
        # returns, so there is nobody left to inherit the reservation and
        # holding it to TTL wedges the node under a holder that is gone.
        # `spawn_succeeded` is the wrong discriminator here: it is only
        # evaluated after the worker exits on exactly these two substrates, so
        # a twenty-second `--once` run kept `dispatch:<id>` for the rest of its
        # TTL. `pane` and `bg` return while the worker lives on, so their
        # reservation is inherited and must stay. `--once` is the pre-substrate
        # spelling of headless, so both spellings answer the same.
        one_shot = once or substrate == "headless"
        if one_shot or not spawn_succeeded:
            _release_dispatch_claims(node_reservation, node_claim)
        for _k, _v in prov_prev.items():
            if _v is None:
                os.environ.pop(_k, None)
            else:
                os.environ[_k] = _v

    # x-4342: bg/headless lane - reached only on a successful dispatch (every
    # failure above exits). The registry row the dispatch minted carries the
    # worker's full harness session id; a one-shot whose row was torn down or
    # whose uuid never resolved skips with a named line, not a bad row.
    # getattr like the receipt above: a minimal once-lane result carries only
    # kind/reply, and the stamp must not add attribute requirements to it.
    _stamp_spawned_session_row(
        node=(prov_env or {}).get("FNO_NODE"), message=message, phase=stamp_phase,
        worker_name=getattr(result, "name", None),
        worker_harness=getattr(result, "provider", None),
        worker_session_uuid=None,
        worker_effort=effort,
    )
    _stamp_launch_edge((prov_env or {}).get("FNO_NODE"))

    if result.kind == "created":
        # claude plain spawn: compact hand-rolled JSON receipt on stdout.
        # Hand-rolled f-string (NOT json.dumps) for byte-parity with Rust Task 1.3.
        # Escape `"` in the name so the receipt stays valid JSON for jq
        # consumers (name validation blocks backslash already, so this is the
        # only escapable character; sigma-review hardening finding).
        safe_name = result.name.replace('"', '\\"')
        # Locked Decision 5 / Rust parity, renamed by x-74ea/x-d401: name the
        # REQUESTED mode (flag or the yolo-derived bypassPermissions) so an
        # audit can tell elevated permissions were REQUESTED on this fallback
        # path - fno can back the request, never the applied outcome. Only
        # when set, so the unset receipt is byte-identical. Full JSON-string
        # encode (not a bare `"`-escape): nothing validates this value against a
        # closed set before it lands here, so a config-sourced mode carrying a
        # backslash or control char would otherwise emit invalid JSON. Matches
        # Rust's json_string_ascii byte-for-byte for every ordinary mode.
        eff_mode = permission_mode or ("bypassPermissions" if yolo else None)
        perm_field = (
            f', "permission_mode_requested": {json.dumps(eff_mode)}'
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
        account_field = launch_provenance.bg_account_field(result, account)
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
        message_field = spawn_seed_receipt_fragment(effective_message)
        # provider/model appear only for an explicit route or model. provider is the
        # vendor, never a harness; `model` is the EFFECTIVE model (--model wins).
        receipt_provider = route_provider or recorded_provider
        provider_field = (
            f", \"provider\": {json.dumps(receipt_provider)}" if receipt_provider else ""
        )
        receipt_model = model or route_model
        # A receipt that prints `model` labels it: the request is not the effect.
        # A caught substitution carries the marker naming both values.
        substitution = getattr(result, "model_substituted", None)
        if receipt_model and substitution:
            model_field = (
                f", \"model\": {json.dumps(substitution['observed'])}"
                ', "model_basis": "verified"'
                f", \"model_substituted\": {json.dumps(substitution)}"
            )
        elif receipt_model:
            model_field = (
                f", \"model\": {json.dumps(receipt_model)}"
                ', "model_basis": "requested"'
            )
        else:
            model_field = ""
        seed = getattr(result, "seed_unverified", None)
        seed_field = (
            f', "seed": "unverified", "seed_unverified": {json.dumps(seed)}' if seed else ""
        )
        receipt = (
            f'{{"name": "{safe_name}", "short_id": "{result.short_id}", '
            f'"harness": "{result.provider}"{provider_field}{model_field}, '
            f'"status": "{"spawning" if seed else "live"}"'
            f"{perm_field}{cwd_field}{account_field}{cred_field}{message_field}{seed_field}}}"
        )
        sys.stdout.write(receipt + "\n")
        sys.stdout.flush()
        # QoS (x-c5cc): a bg worker is claude's child, so its exec can't be
        # wrapped, demote post-hoc via the roster, bounded and non-fatal.
        # After the receipt flush so line-parsing consumers never wait on it.
        if substrate == "bg" and result.provider == "claude" and result.short_id:
            from fno.agents.spawn_gate import qos_demote_bg_worker

            qos_demote_bg_worker(result.short_id)
    else:
        # once path: reply verbatim on stdout (no added newline per ask contract).
        sys.stdout.write(result.reply or "")
        sys.stdout.flush()

    pane_view = (
        defaulted and substrate == "bg" and spawn_succeeded
        and result.kind == "created" and os.environ.get("FNO_PANE")
    )
    if pane_view:
        # Post-receipt, best effort: a placement failure never recolors the verdict.
        from fno.agents.spawn_defaults import place_default_view

        place_default_view(result.name)


#: Exit status `fno agents name` uses for a naming refusal. Deliberately not 2:
#: Click already spends 2 on usage errors including "no such command", so a
#: shell caller cannot distinguish a refusal from a stale `fno` at exit 2.
NAME_REFUSED_EXIT = 3


@agents_app.command("name", hidden=True)
def cmd_name(
    prefix: Optional[str] = typer.Argument(None, help="Legacy operation prefix (target|think|...); omit with --verb."),
    node_id: Optional[str] = typer.Argument(None, help="Full backlog node id; never abbreviated."),
    slug: str = typer.Option("", "--slug", help="Human-readable tail; the only expendable part."),
    qualifier: str = typer.Option("", "--qualifier", help="Lifecycle reason, e.g. retro."),
    discriminator: str = typer.Option("", "--discriminator", help="Uniqueness token; never shaved."),
    source: str = typer.Option("", "--source", help="Dispatch source code (x-84b2); omit when attended."),
    verb: str = typer.Option("", "--verb", help="Verb code (t|bp|r|th|f) or a work verb the bridge maps."),
) -> None:
    """Mechanical bridge to the canonical agent-name owner, for shell dispatchers.

    Prints one name on stdout. Exit 3 (NOT 2) is the naming refusal; 2 is Click's usage
    error, which an `fno` too old to know this verb also returns - reading 2 as a refusal
    refuses the fleet on a stale install.
    """
    from fno.agents.naming import AgentNameError, BridgeUsageError, bridge_name

    # One positional binds to PREFIX by Click's left-to-right rule; read it as the node.
    if node_id is None:
        prefix, node_id = None, prefix
    if not node_id:
        typer.echo("error: a node id is required: fno agents name [prefix] <node-id>", err=True)
        raise typer.Exit(2)
    try:
        name = bridge_name(
            prefix or "",
            node_id,
            slug=slug or None,
            qualifier=qualifier or None,
            discriminator=discriminator or None,
            source=source or None,
            verb=verb or None,
        )
    except (BridgeUsageError, AgentNameError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(NAME_REFUSED_EXIT if isinstance(exc, AgentNameError) else 2)
    typer.echo(name)


# `rename` moved to the Rust client (`agent.rename` over the daemon RPC; rust_runtime's router
# entry resolves it). Python's rename_agent stays: the transaction library, not a command twin.


@agents_app.command("retask", hidden=True)
def cmd_retask(
    worker: str = typer.Argument(..., help="Blueprint worker registry label or session id."),
    node: str = typer.Option(..., "--node", help="Target backlog node."),
    model: Optional[str] = typer.Option(None, "--model"),
    effort: Optional[str] = typer.Option(None, "--effort"),
    json_out: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Retask one finished worker (pane or thread) onto the node's next verb."""
    from fno.agents.retask import run_retask
    from fno.config import load_settings

    try:
        receipt = run_retask(
            worker,
            node=node,
            settings=load_settings(),
            model=model,
            effort=effort,
        )
    except (OSError, RuntimeError, ValueError) as exc:
        typer.echo(f"agents retask: {exc}", err=True)
        raise typer.Exit(code=2)
    if json_out:
        typer.echo(json.dumps(receipt))
    else:
        # Same JSON either way (sorted keys are the only difference): the
        # receipt is machine-parsed by the king loop, so there is no human
        # rendering to switch to.
        typer.echo(json.dumps(receipt, sort_keys=True))
    if receipt.get("status") != "retasked":
        raise typer.Exit(code=1)


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
    probe-then-reserve window is no wider than the two ``fno agents claim`` shell-outs it
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
    - already-running a live ``node:<id>`` claim with a target init behind it
                      (reason=live-claim, holder=<owner>), a held claim with NO
                      target init behind it (reason=unproven-claim: a launch
                      window, or a hand ``claim acquire`` - a holder exists and
                      a worker is unproven),
                      a suspect claim (reason=suspect-claim: TTL-unexpired dead pid,
                      a respawned worker - the caller maps this to skipped-contested,
                      x-ba4b), a worker ROW on the node while the claim itself does
                      not hold it (reason=worker-row, worker=<names>: the receipt
                      names the row and no claim holder, because peeking and
                      stopping that worker is what frees the node - a release
                      frees nothing), OR a racing dispatcher already holds
                      ``dispatch:<id>`` (reason=reservation-held). No reservation
                      acquired.
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
            # Booleans render lowercase, matching the --json lane. Python's
            # str(True) is "True", and this text line is parsed with sed by
            # shell callers, so a capitalised token would be a second spelling
            # of the same fact for anything that learns to read it.
            if key == "detail":
                parts.append(f'{key}="{value}"')
            elif isinstance(value, bool):
                parts.append(f"{key}={'true' if value else 'false'}")
            else:
                parts.append(f"{key}={value}")
        line = " ".join(parts)
    sys.stdout.write(line + "\n")
    sys.stdout.flush()
    raise typer.Exit(code=exit_code)


@agents_app.command("list")
def cmd_list(
    cwd: str = typer.Option(None, "--cwd", help="Filter by working directory."),
    harness: str = typer.Option(
        None,
        "--harness",
        "-H",
        help="Filter by harness (claude | codex | gemini | cursor-agent).",
    ),
    _provider_tombstone: str = typer.Option(
        None,
        "--provider",
        hidden=True,
        help="Retired: filter by --harness.",
    ),
    status: AgentStatusFilter = typer.Option(
        None, "--status", help="Filter by served activity (writing | quiet | parked | refused | orphaned | unknown); process liveness is the liveness field on fno agents list --json."
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

    ``model`` is omitted from every row on purpose: the stored model is
    intended configuration, not observed truth. Read ``observed_model``
    (transcript-sampled, with a sample count) and ``requested_model``;
    ``fields_omitted`` names what was dropped.
    """
    from fno.agents.read import list_agents
    from fno._flag_aliases import refuse_retired_provider

    refuse_retired_provider(_provider_tombstone)

    status_value: str | None = status.value if status is not None else None
    progress_value: str | None = progress.value if progress is not None else None
    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())

    result = list_agents(
        cwd=cwd,
        # The CLI flag is the harness axis and now feeds the harness filter.
        # It used to ride the `provider` param, which compared the harness
        # pre-split and the vendor after, so `--harness claude` dropped every
        # claude-hosted row the moment the axes separated.
        harness=harness,
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
    """Internal: emit registry rows DAEMON-FREE, with the served liveness pair.

    Hooks need stored crown, spawn-edge, and origin fields plus the served
    liveness verdict, derived by the freshness rule. Output is
    ``{"agents": [...]}`` via a client-side registry read. There is no Python
    registry-json left: a missing binary is refused here.
    """
    from fno.agents.rust_runtime import refuse_without_binary

    refuse_without_binary("registry-json")


@agents_app.command("registry-repair", hidden=True)
def cmd_registry_repair(
    to: int = typer.Option(
        ..., "--to", help="Schema version to roll the file back DOWN to."
    ),
    path: str = typer.Option(
        None, "--path", help="Registry file. Defaults to the process-global one."
    ),
    apply: bool = typer.Option(
        False, "--apply", help="Perform the repair. Omit to preview it."
    ),
) -> None:
    """Internal: roll a poisoned registry's schema_version back down.

    Holds the registry lock, refuses unless the on-disk version is strictly
    above ``--to``, refuses if any row carries a newer-schema field with a real
    value, backs the file up, then drops the empty unknown keys and replaces
    atomically. Dry run by default, matching `fno agents watchdog` and
    `fno backlog maintain`.
    """
    from pathlib import Path as _Path

    from fno.agents.registry import RegistryRepairRefused, repair_registry_schema

    try:
        plan = repair_registry_schema(
            to, path=_Path(path) if path else None, apply=apply
        )
    except RegistryRepairRefused as exc:
        sys.stderr.write(f"{exc}\n")
        raise typer.Exit(1) from exc

    verb = "repaired" if apply else "would repair"
    sys.stdout.write(
        f"{verb} {plan.path}: schema_version {plan.on_disk} -> {plan.to_version}\n"
    )
    for name in sorted(plan.dropped):
        keys = ", ".join(plan.dropped[name])
        sys.stdout.write(f"  {name}: drop {keys} (empty)\n")
    if not plan.dropped:
        sys.stdout.write("  no rows carry keys this fno does not know\n")
    if plan.backup is not None:
        sys.stdout.write(f"  backup: {plan.backup}\n")
    if not apply:
        sys.stdout.write("  dry run; pass --apply to perform it\n")


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
    cross_project: bool = typer.Option(
        False,
        "--cross-project",
        hidden=True,
        help="Authorize machine-wide store selection after uniqueness checks.",
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
            resolved = resolve_agent(
                token,
                path=Path(registry) if registry else None,
                scope_cwd=os.getcwd(),
                cross_project=cross_project,
            )
            resolved_entry = resolved.entry
        except AgentResolutionError as exc:
            if exc.ambiguous:
                sys.stderr.write(f"{exc}\n")
                raise typer.Exit(code=HEAL_TOKEN_AMBIGUOUS_EXIT)
            if exc.unavailable:
                sys.stderr.write(f"{exc}\n")
                raise typer.Exit(code=HEAL_TOKEN_UNAVAILABLE_EXIT)
            raise typer.Exit(code=HEAL_TOKEN_MISS_EXIT)
        if cross_project and resolved.matched_by == "harness_store":
            sys.stderr.write("scope=cross-project\n")
        sys.stdout.write(_json.dumps(asdict(resolved_entry)))
        sys.stdout.write("\n")
        return

    try:
        entry = resolve_from_harness_store(
            token,
            registry_path=Path(registry) if registry else None,
            scope_cwd=os.getcwd(),
            cross_project=cross_project,
        )
    except AgentResolutionError as exc:
        sys.stderr.write(f"{exc}\n")
        raise typer.Exit(code=HEAL_TOKEN_AMBIGUOUS_EXIT)
    if entry is None:
        raise typer.Exit(code=HEAL_TOKEN_MISS_EXIT)
    if cross_project:
        sys.stderr.write("scope=cross-project\n")
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


@agents_app.command("whoami", hidden=True)
def cmd_whoami(
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit JSON regardless of TTY."),
) -> None:
    """Print THIS mesh worker's own registered name (+ registry enrichment).

    The derived-name peers use to address you via ``fno agents mail send <name>``.
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
    from fno.agents.self_stamp import identity_ambiguity_message, resolve_self_identity

    _ident = resolve_self_identity(os.environ)
    if not (os.environ.get("FNO_AGENT_SELF") or "").strip() and _ident.disposition == "ambiguous":
        print(f"error: {identity_ambiguity_message(_ident)}", file=sys.stderr)
        raise typer.Exit(code=4)
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
    """Join THIS session to the mesh roster so peers can `fno agents mail send` to it.

    The self-service seam behind ``/fno-me``: a session a human started by hand
    has no spawn-created roster row. This resolves the ambient harness identity
    (CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID / ...) and writes an ``idle`` row
    named by the canonical bare ``<shortid>`` handle, the same string the
    session self-stamps and drains, so a durable ``fno agents mail send`` to it lands.
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
    from fno.agents.self_stamp import IdentityAmbiguousError, require_self_identity

    if delivery_policy is not None and delivery_policy not in ("bus-only", "off"):
        sys.stderr.write(
            f"error: --delivery-policy accepts 'bus-only' or 'off', "
            f"got {delivery_policy!r}\n"
        )
        raise typer.Exit(code=2)

    try:
        ident = require_self_identity()
    except IdentityAmbiguousError as exc:
        sys.stderr.write(f"cannot register: {exc}\n")
        raise typer.Exit(code=4) from exc
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
            harness=harness, session_id=session_id, cwd=os.getcwd(),
            origin="operator",
            delivery_policy=delivery_policy,
        )
    except Exception as exc:  # a deliberate manual join reports failure (unlike the fail-open hook)
        sys.stderr.write(f"register failed: {exc}\n")
        raise typer.Exit(code=1) from exc

    # `origin` is write-once, so a human taking over a pane footnote spawned
    # keeps `spawned` and this call cannot change it. Silence there reads as
    # success: the operator believes they are registered as attended, while
    # mail still treats them as unattended. The refusal is deliberate - a
    # birth fact is not a claim a later caller gets to revise - so this says
    # it rather than hiding it.
    if entry.origin is not None and entry.origin != "operator":
        sys.stderr.write(
            f"note: origin stays {entry.origin!r}; it records what created this "
            "row and is written once. Mail escalation reads it, so this "
            f"session is still treated as {entry.origin!r}.\n"
        )

    # x-481e: record a clock saying "no expiry" beside a hand-stamped policy.
    # Not for enforcement - an absent clock already never lapses. This is what
    # lets the DND column on `fno agents list` say "held" for this row instead
    # of leaving the operator to guess from a blank cell.
    if delivery_policy is not None:
        from fno.mail import hold as _hold

        if delivery_policy == "bus-only":
            _hold.arm_permanent(entry.name)
        else:
            _hold.clear(entry.name)

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
            f"`fno agents mail send {entry.name} \"...\"`\n"
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
    show_pane_stats: bool = typer.Option(
        False,
        "--pane-stats",
        help="Append per-pane mux server counters, differenced over the last "
        "two 30s snapshots in the global events journal (bytes_in, "
        "grid_updates, frames_composited, frames_emitted, cpu_ns).",
    ),
) -> None:
    """Show every live worker process - fno-spawned and foreign claude bg
    alike - with pid, RSS (MB), and status (x-c5cc US4).

    The same union the spawn gate counts, so this is the audit surface every
    gate message points at. Python-only (RSS via psutil; not routed to the
    Rust client). ``--subagents`` (x-af92) appends a read-only sidechain
    section; each row also carries its node and whether it shipped (x-1379).
    """
    from fno.agents.top import render_top

    print(
        render_top(
            as_json=as_json,
            include_subagents=show_subagents,
            include_pane_stats=show_pane_stats,
        )
    )


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


# The pane-identity command moved to fno.agents.pane_identity (file
# budget); the composition stays on the agents app here.
from fno.agents.pane_identity import cmd_pane_identity  # noqa: E402

agents_app.command("pane-identity", hidden=True)(cmd_pane_identity)



def _registry_falsifiers(handles: list[str]) -> dict[str, str | None]:
    """One registry read for N handles. Same three-key match as the single form.

    A handle with no registry row (a discovered-but-unadopted session) carries
    no falsifier, which is absence of evidence and NOT a death sentence.

    Matched on name, session id, AND short id, because callers key on different
    ones: a human types the name, while the Rust list path passes
    ``harness_session_id`` (``registry_truth_handle`` in daemon.rs). A
    name-only lookup silently returns "no falsifier" for every row on that path,
    which reads exactly like a healthy process and is how a guard ends up
    decorative on one of two reachable paths.

    The batch is why ``--handles`` exists at all: ``load_registry`` was the
    per-handle cost inside a per-handle subprocess. First matching row wins per
    handle, exactly as the single-handle ``next()`` lookup did. Never raises.
    """
    from fno.agents.reachability import registry_falsifier

    out: dict[str, str | None] = dict.fromkeys(handles, None)
    wanted = {h for h in handles if h}
    if not wanted:
        return out
    try:
        from fno.agents.registry import load_registry

        rows = list(load_registry())
    except Exception:  # noqa: BLE001 -- an unreadable registry falsifies nothing
        return out
    # `registry_falsifier` stays OUTSIDE the except, as it was on the
    # single-handle path. A falsifier that raises is a malfunction, and
    # swallowing it here would report "no falsifier" - indistinguishable from a
    # healthy row, which is the decorative-guard shape this docstring warns
    # about.
    resolved: set[str] = set()
    for row in rows:
        # `harness_session_id` is Optional, so drop the empties before matching.
        # A None could never be in `wanted` anyway - that set already excludes
        # falsy handles - so this narrows the type without changing the match.
        row_keys = {
            key for key in (row.name, row.harness_session_id, row.short_id) if key
        }
        keys = (row_keys & wanted) - resolved
        if not keys:
            continue
        falsifier = registry_falsifier(row)
        for key in keys:
            out[key] = falsifier
        resolved |= keys
        if resolved == wanted:
            break
    return out


def _batch_resolver():
    """The ORDINARY resolver with its two expensive reads hoisted out of the
    per-handle path.

    ``resolve_or_suggest`` re-reads the registry (74 ms) on every call, and a
    NAME handle misses the registry fast path and falls through to a full
    discovery scan (483 ms of psutil sweep plus transcript globbing). Profiled
    on the live roster, twelve handles spent 5.8 of 8.3 seconds running that
    one scan twelve times. Hoisting both is what makes a batch actually cheap
    rather than merely co-located in one process.

    A hoist, never a second resolver. Every match stays inside
    ``resolve_or_suggest``, so a batch resolves a handle through the same code
    a single call does. Growing a second matcher here is the drift a Rust
    reimplementation of the probe was rejected for.

    Degrades to the per-call read if the hoisted one fails, so a batch is never
    less able to resolve a handle than a single call is.
    """
    from fno.agents.discover import _discover_from_registry, resolve_or_suggest

    try:
        rows = _discover_from_registry(None)
    except Exception:  # noqa: BLE001 -- fall back to the per-call read
        rows = None
    cache: dict = {}

    def resolve(handle: str):
        return resolve_or_suggest(
            handle,
            require_alive=False,
            registry_rows=rows,
            discovery_cache=cache,
        )

    return resolve


def _registry_falsifier(handle: str) -> str | None:
    """The falsifier for one ``handle``. A wrapper over
    [`_registry_falsifiers`] so the three-key match has ONE implementation and
    the single-handle path cannot drift from the batch one."""
    return _registry_falsifiers([handle])[handle]


def _truth_payload(result: dict, *, falsifier: str | None = None) -> dict:
    """The ``truth --json`` wire shape: the Python/Rust boundary.

    ``family1_truth_probe`` (crates/fno-agents/src/claude_ask.rs) reads this,
    and ``resume`` decides "is live" from it. The reachability verdict has to
    be ON this wire or Rust keeps re-deriving liveness from the raw transcript
    ``state`` and never sees the falsifier. ``state`` stays exactly as it was;
    overloading a field Rust already matches on is a silent contract break.
    """
    from fno.agents.reachability import classify_reachability

    reach = classify_reachability(
        truth_state=result.get("state"),
        age_s=result.get("last_activity_age_s"),
        falsifier=falsifier,
        observed_model=result.get("observed_model"),
    )
    payload = {
        k: result.get(k)
        for k in (
            "handle",
            "state",
            "reason",
            "last_activity_age_s",
            "last_event_at", "last_activity_basis",
            "last_message",
            "provider_refusal",
            "session_id",
            "observed_model",
            "harness_title",
        )
    }
    payload["reachability"] = reach.verdict
    payload["basis"] = reach.basis
    return payload


@agents_app.command("truth", hidden=True)
def cmd_truth(
    handle: str | None = typer.Argument(
        None, help="Worker handle / short id / session id (as in `fno agents list`)."
    ),
    handles: str | None = typer.Option(
        None,
        "--handles",
        help=(
            "BATCH MODE. Comma-separated handles, answered in ONE process. "
            "Emits a JSON object keyed by handle (with --json), else one "
            "handle-prefixed line each. Batch mode ALWAYS exits 0, even when a "
            "handle is unresolvable: a batch has no single exit code to carry, "
            "so every entry carries its own state and reason instead. A caller "
            "reading exit 0 as 'all resolved' is reading an absence; read the "
            "per-entry state."
        ),
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

    `--handles a,b,c` answers many in ONE process. The Rust daemon probes every
    roster row on every sweep, and one interpreter cold start (780 ms measured)
    per row against 0.83 ms of real work is a 940-to-1 overhead ratio. Batching
    pays that start once per sweep. It stays a batch rather than a Rust port
    because `state` and `observed_model` must come from the SAME reader (see
    `TruthProbe` in `crates/fno-agents/src/claude_ask.rs`); a Rust
    reimplementation would grow the second transcript reader that constraint
    exists to prevent.
    """
    import json as _json

    from fno.agents.session_truth import render_truth, resolve_session_truth

    # Split rather than one combined test, so the positional narrows to `str`
    # for the single-handle path below without an assert standing in for the
    # control flow.
    usage = "pass exactly one of: a positional handle, or --handles a,b,c"

    if handles is not None:
        if handle is not None:
            print(usage, file=sys.stderr)
            raise typer.Exit(code=2)
        # ponytail: handles ride argv. Ceiling is roughly 125 handles at 36
        # chars, under 5 KB and far below ARG_MAX; read the list from stdin if
        # a roster ever outgrows that.
        names = [h.strip() for h in handles.split(",") if h.strip()]
        falsifiers = _registry_falsifiers(names)
        resolver = _batch_resolver()
        answers = [
            (name, resolve_session_truth(name, resolve=resolver), falsifiers[name])
            for name in names
        ]
        if json_out:
            sys.stdout.write(
                _json.dumps(
                    {
                        name: _truth_payload(result, falsifier=falsifier)
                        for name, result, falsifier in answers
                    }
                )
                + "\n"
            )
        else:
            for name, result, falsifier in answers:
                payload = _truth_payload(result, falsifier=falsifier)
                sys.stdout.write(
                    f"{name}: {render_truth(result)} "
                    f"[{payload['reachability']}: {payload['basis']}]\n"
                )
        sys.stdout.flush()
        # Always 0: an unresolvable handle is reported in its own entry, never
        # in an exit code the whole batch would have to share.
        return

    if handle is None:
        print(usage, file=sys.stderr)
        raise typer.Exit(code=2)

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


def _run_unfinished_report(
    *,
    now: float,
    json_out: bool,
    mail_to: Optional[str] = None,
) -> None:
    """The default watchdog surface: build and publish the unfinished-work
    report. Recovery internals stay behind --apply/--only; this path never
    renders a session verdict as the operator's answer."""
    from fno.agents import unfinished_work as uw
    from fno.agents import watchdog as wd
    from fno.paths import resolve_repo_root

    roots = uw.report_roots() or [Path(resolve_repo_root())]
    snapshot = uw.build_report(roots, now_s=now)

    recipient = mail_to
    if recipient is None:
        try:
            from fno.config import load_settings

            recipient = str(load_settings().recovery.watchdog.mail_to or "")
        except Exception:  # noqa: BLE001 - config read miss means no mail
            recipient = ""

    def _note(line: str) -> None:
        print(line, file=sys.stderr)

    payload = uw.publish_report(
        snapshot, source="manual", now_s=now, mail_to=recipient or "", log=_note
    )
    # The keeper lane rides the default report: each finding names its own
    # clearing command, and the lane here is read-only whatever the flags -
    # collection is `--only keeper --apply-all`, and the row lanes below never
    # see a keeper.
    lane_lines: list[str] = []
    try:
        from fno.agents import keeper_lane

        lane_result = keeper_lane.discover()
        lane_lines = keeper_lane.render(lane_result).splitlines()
        payload["keepers"] = lane_result.to_json()
    except Exception as exc:  # noqa: BLE001 - the report outlives one lane's crash
        lane_lines = [f"keeper lane: crashed: {exc!r}"]
    # The outage instrument rides the default report: open breakers
    # and every refusal reason with its count are part of the operator's
    # answer, and a -J reader must not need --only to learn a lane is down.
    payload["provider_outages"] = wd.measure_provider_outages_safe(now)
    if json_out:
        sys.stdout.write(json.dumps(payload) + "\n")
        sys.stdout.flush()
        return
    typer.echo(uw.snapshot_digest(snapshot))
    for line in lane_lines:
        typer.echo(line)
    for line in wd.provider_outage_lines(payload.get("provider_outages")):
        typer.echo(line)
    for warning in payload["warnings"]:
        print(f"warning: {warning}", file=sys.stderr)


def _watchdog_only_help() -> str:
    from fno.agents import watchdog as wd

    return (
        "DIAGNOSTIC: filter the internal session-verdict table to one verdict "
        f"({'|'.join(sorted(wd.VERDICTS))}). Recovery internals, not the "
        "operator report."
    )


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
            "Execute every lane: wake plus reroute (which stops and respawns "
            "a session), plus keeper collection, which kills an orphaned "
            "keeper process and its hosted children. Row retirement is the "
            "daemon sweep's question (`fno agents reap`). Implies --apply."
        ),
    ),
    only: Optional[str] = typer.Option(
        None, "--only",
        help=_watchdog_only_help(),
    ),
    since: str = typer.Option(
        "24h",
        "--since",
        help="Codex recovery age for --only recoverable (1s through 30d).",
    ),
    cwd: Optional[str] = typer.Option(
        None,
        "--cwd",
        help="Exact checkout scope for --only recoverable. Defaults to cwd.",
    ),
    session_id: Optional[str] = typer.Option(
        None,
        "--session-id",
        help=(
            "For --only recoverable, select one canonical full Codex UUID "
            "before dry-run or apply."
        ),
    ),
    mail_to: Optional[str] = typer.Option(
        None,
        "--mail",
        help=(
            "Mail the digest to this handle (an agent name, short id, or "
            "project:<slug>). Defaults to config.recovery.watchdog.mail_to. "
            "Skipped when the non-leave verdict set is unchanged."
        ),
    ),
) -> None:
    """Report unfinished work: started nodes nobody holds, done branches
    ahead of origin/main, dirty ownerless worktrees, and ownerless PRs older
    than a day. Every finding names the one command that clears it.

    The transcript is the truth source (keyed by session id); the registry
    and claude's agent view are hints. The default output is the
    unfinished-work report; --apply/--only reach the internal recovery
    lanes, which stay explicit.
    """
    import time as _time

    from fno.agents import watchdog as wd

    if only is not None and only not in wd.VERDICTS:
        print(f"fno agents watchdog: unknown verdict {only!r}", file=sys.stderr)
        raise typer.Exit(code=2)

    now = _time.time()
    if only == wd.RECOVERABLE:
        selected_session_id = session_id if isinstance(session_id, str) else None
        if selected_session_id:
            from fno.agents.discover import _is_canonical_full_uuid

            if not _is_canonical_full_uuid(selected_session_id):
                print(
                    "fno agents watchdog: --session-id requires one canonical full UUID",
                    file=sys.stderr,
                )
                raise typer.Exit(code=2)
        try:
            recency_seconds = wd.parse_recovery_since(since)
            scope_cwd = wd.resolve_recovery_cwd(cwd)
        except ValueError as exc:
            print(f"fno agents watchdog: {exc}", file=sys.stderr)
            raise typer.Exit(code=2) from exc

        payload, rows, scan = wd.run_recoverable_sweep(
            cwd=scope_cwd,
            recency_seconds=recency_seconds,
            now_s=now,
            session_id=selected_session_id,
        )
        pairs = [
            (wd.Verdict(**data), row)
            for data, row in zip(payload["verdicts"], rows)
        ]

        def _print_verdicts():
            for verdict, row in pairs:
                typer.echo(
                    f"{verdict.verdict:11} {verdict.row_id} "
                    f"handle={verdict.name} cwd={row.cwd}"
                )

        def _apply_and_emit():
            # One move shared by both lanes: apply, then the JSON emit.
            results = wd.apply_recoverable(scan, scope_cwd=scope_cwd)
            if json_out:
                sys.stdout.write(
                    json.dumps(
                        {
                            **payload,
                            "results": results,
                            "result_counts": wd.recovery_result_counts(results),
                        }
                    )
                    + "\n"
                )
            return results

        if not scan.complete:
            if apply or apply_all:
                results = _apply_and_emit()
                print(results[0]["detail"], file=sys.stderr)
                raise typer.Exit(code=3)
            if json_out:
                sys.stdout.write(json.dumps(payload) + "\n")
            else:
                _print_verdicts()
                for warning in payload["warnings"]:
                    print(f"warning: {warning}", file=sys.stderr)
                typer.echo(
                    f"recoverable=unknown complete=false "
                    f"scanned={payload['scanned_count']}"
                )
            return

        if selected_session_id and not scan.recoverable:
            payload["selection_error"] = "selected session is not recoverable"
            if json_out:
                sys.stdout.write(json.dumps(payload) + "\n")
            else:
                print(
                    f"fno agents watchdog: session {selected_session_id} is not "
                    "one recoverable candidate",
                    file=sys.stderr,
                )
            raise typer.Exit(code=3)

        previous_events = wd._last_events_signature()
        signature = wd.verdict_signature(payload)
        wd.write_sweep_file(
            "manual",
            payload["counts"],
            now,
            signature,
            events_signature=signature,
            recoverable_count=payload["recoverable_count"],
        )
        for verdict, _row in pairs:
            if verdict.row_id in wd.fresh_non_leave(payload, previous_events):
                wd.emit_event(
                    "watchdog_verdict",
                    {
                        "row_id": verdict.row_id,
                        "name": verdict.name,
                        "verdict": verdict.verdict,
                        "basis": verdict.basis,
                    },
                )
        if not apply and not apply_all:
            if json_out:
                sys.stdout.write(json.dumps(payload) + "\n")
            else:
                _print_verdicts()
                typer.echo(
                    f"recoverable={payload['recoverable_count']} "
                    f"usable={payload['usable_recoverable_count']} "
                    f"unusable={payload['unusable_recoverable_count']} complete=true "
                    f"scanned={payload['scanned_count']}"
                )
            return

        results = _apply_and_emit()
        if not json_out:
            for result in results:
                line = f"{result['outcome']:9} {result['detail']}"
                print(line, file=sys.stderr if result["outcome"] != "applied" else sys.stdout)
            result_counts = wd.recovery_result_counts(results)
            typer.echo(
                f"applied={result_counts['applied']} "
                f"refused={result_counts['refused']} "
                f"deferred={result_counts['deferred']}"
            )
        return

    if only == wd.KEEPER:
        # The keeper lane's own surface: keepers have no registry row, so the
        # per-row sweep has nothing to classify. Dry run names every keeper
        # with its verdict and reason; ONLY --apply-all collects, because
        # killing a keeper destroys work - the bar --apply's help text
        # promises the wake lane never crosses.
        from fno.agents import keeper_lane

        try:
            lane_result = keeper_lane.discover()
        except Exception as exc:  # noqa: BLE001 - a crashed lane withholds, never half-acts
            print(f"fno agents watchdog: keeper lane crashed: {exc!r}", file=sys.stderr)
            raise typer.Exit(code=3) from exc
        if json_out:
            from datetime import datetime, timezone

            payload = {
                "generated_at": datetime.fromtimestamp(
                    now, tz=timezone.utc
                ).strftime("%Y-%m-%dT%H:%M:%SZ"),
                "lane": wd.KEEPER,
                **lane_result.to_json(),
            }
            if apply_all:
                keepers, children = keeper_lane.reap_keepers(lane_result)
                payload["reaped_keepers"] = keepers
                payload["reaped_children"] = children
            sys.stdout.write(json.dumps(payload) + "\n")
            sys.stdout.flush()
            raise typer.Exit(code=3 if lane_result.broken else 0)
        typer.echo(keeper_lane.render(lane_result))
        if lane_result.broken:
            raise typer.Exit(code=3)
        if apply and not apply_all:
            typer.echo(
                "--apply never kills a keeper (wake lane only); "
                "the collect flag is --apply-all"
            )
        elif apply_all:
            keepers, children = keeper_lane.reap_keepers(lane_result)
            typer.echo(
                f"reaped {len(keepers)} keeper(s), {len(children)} hosted child(ren)"
            )
        return

    if only is None and not apply and not apply_all:
        # The default surface: the unfinished-work report. Session verdicts
        # (and their counts) are recovery internals behind --apply/--only,
        # never the operator's answer.
        _run_unfinished_report(now=now, json_out=json_out, mail_to=mail_to)
        return

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

    # Push, not pull: mail before writing the sweep file, so only a delivered
    # digest advances the change gate - a transient send failure must not
    # permanently swallow the verdict behind an unchanged signature.
    recipient = mail_to
    if recipient is None:
        try:
            from fno.config import load_settings

            recipient = str(load_settings().recovery.watchdog.mail_to or "")
        except Exception:  # noqa: BLE001 - config read miss means no mail
            recipient = ""
    signature = ""
    try:
        ok, receipt, signature = wd.mail_gate(payload, recipient or "")
        if not ok:
            print(f"watchdog mail: {receipt}", file=sys.stderr)
    except Exception as exc:  # noqa: BLE001 - mail never breaks the sweep
        print(f"watchdog mail failed: {exc}", file=sys.stderr)
    # A filtered run publishes only its own rows: never stamp the whole
    # non-leave set, and stamp the UNION with what was already published,
    # or the next tick re-emits every filtered-out row.
    events_payload = (
        payload if only is None
        else {**payload, "verdicts": [v._asdict() for v, _ in pairs]}
    )
    prev_events_sig = wd._last_events_signature()
    signature_to_stamp = wd.union_signature(
        prev_events_sig, wd.verdict_signature(events_payload)
    ) if only is not None else wd.verdict_signature(events_payload)
    wd.write_sweep_file(
        "manual", payload["counts"], now, signature,
        events_signature=signature_to_stamp,
        terminal_harness_rows=payload.get("terminal_harness_rows", 0),
        provider_outages=payload.get("provider_outages") or wd._unknown_provider_report(
            "provider_outage_payload_missing"
        ),
    )

    # Classification events ride every mode (a dry-run-only verdict once left
    # apply modes with no event record), gated on fresh_non_leave so a filtered
    # hand-run neither diverges from the tick's record nor re-emits the fleet.
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
        for line in wd.provider_outage_lines(payload.get("provider_outages")):
            typer.echo(line)
        return

    lanes = "all" if apply_all else "wake"
    results = []
    # One global provider rotation per sweep, shared across every row.
    rotation = wd.RotationBudget()
    for v, row in pairs:
        try:
            outcome, detail = wd.apply_verdict(
                v, lanes=lanes, cwd=row.cwd, node=row.node, rotation=rotation
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
            wd.outcome_event(outcome),
            {"row_id": v.row_id, "verdict": v.verdict, "detail": detail,
             "outcome": outcome},
        )
    keeper_reaped: dict = {}
    if apply_all and only is None:
        # Keeper collection rides --apply-all, never --apply: killing a keeper
        # destroys work, the bar the flag help draws. It also rides the FULL
        # surface only: `--only <verdict>` is a diagnostic filter over the
        # session table, and a filtered run must not widen its destructive
        # scope to a lane the filter did not name. A crashed or broken lane is
        # named, never silently skipped.
        from fno.agents import keeper_lane

        try:
            lane_result = keeper_lane.discover()
            keepers, children = keeper_lane.reap_keepers(lane_result)
        except Exception as exc:  # noqa: BLE001 - one lane's crash never ends the sweep
            print(f"keeper lane crashed: {exc!r}", file=sys.stderr)
        else:
            keeper_reaped = {"keepers": keepers, "keeper_children": children}
            if lane_result.broken:
                print(f"keeper lane: {lane_result.broken_reason}", file=sys.stderr)
            elif keepers or children:
                typer.echo(
                    f"reaped {len(keepers)} keeper(s), "
                    f"{len(children)} hosted child(ren)"
                )
    if json_out:
        sys.stdout.write(json.dumps({"results": results, **keeper_reaped}) + "\n")
        sys.stdout.flush()


@agents_app.command("stale-escalate", hidden=True)
def cmd_stale_escalate(
    json_out: bool = typer.Option(False, "--json", "-J", help="Machine-readable output."),
) -> None:
    """Reconcile the durable stale-row question to the measured fleet."""
    from fno.agents import stale_lane as se

    se.run(json_out=json_out)


@agents_app.command("friction-escalate", hidden=True)
def cmd_friction_escalate(
    json_out: bool = typer.Option(False, "--json", "-J", help="Machine-readable output."),
) -> None:
    """Reconcile ONE [watchdog-friction:*] question to the measured fleet."""
    from fno.agents import friction_lane as fl

    fl.run(json_out=json_out)


@agents_app.command("ping", hidden=True)
def cmd_ping() -> None:
    """Health check (placeholder): exit 0, no verb surface grown."""
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

    Claude agents: shells out to ``claude stop`` on the short id and prints
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
            "but still refuses a live pane worker it cannot stop. A Claude "
            "row also removes its harness session and worktree under Claude's "
            "guards; non-Claude bg and headless processes survive. WARNING: leaves an orphan session record in that "
            "harness's own store, named on stderr, for you to clean manually."
        ),
    ),
    audit_actor: str | None = typer.Option(None, "--audit-actor", hidden=True),
    audit_reason: str = typer.Option("operator-requested", "--audit-reason", hidden=True),
    audit_request_id: str | None = typer.Option(None, "--audit-request-id", hidden=True),
    audit_worktree_touched: bool = typer.Option(
        False, "--audit-worktree-touched", hidden=True
    ),
    audit_reclaimed_bytes: int | None = typer.Option(
        None, "--audit-reclaimed-bytes", hidden=True
    ),
) -> None:
    """Remove an agent: harness or mux session first, registry row after.

    rm runs on the Rust runtime only: it requires the `fno-agents` binary,
    and `FNO_AGENTS_RUNTIME=python` cannot force it onto Python. With a
    binary installed, `auto` routing (the default) execs it directly, so
    this body is reached only when no binary resolved.

    Per-harness teardown (all on the Rust side):

    \b
      claude    bg session: drops the claude session record; pane session:
                kills the mux pane
      codex     drops the session's entry from ~/.codex/session_index.jsonl
      opencode  registry-only; `rm` will not delete an opencode session,
                because that also deletes its child sessions and its whole
                message history. Run `opencode session delete <id>` if you
                want the conversation gone.
      gemini    registry-only (no teardown arm for a deprecated provider)

    Your history is never removed here -- teardown drops the harness's
    index record, not the conversation. On teardown failure the registry
    row is kept so you can retry; ``--force`` drops it anyway and names
    the orphan in the receipt. A live row is refused until the row is
    provably gone: a non-claude pane worker is told to kill its pane and
    re-run rm, and a claude row names what its roster read showed.
    Terminal rows need
    no separate stop first. Do not tear a session down by hand: the
    harness session record IS the resume handle, and dropping it directly
    spends that handle for nothing this command has not already done. If one
    is already orphaned, use the retained full ``harness_session_id`` with
    ``fno agents adopt``. A linked worktree is removed only when the shared
    guarded predicate says it is clean, merged, and unowned; otherwise the
    row is removed and the worktree receipt names the refusal.
    """
    from fno import rust_binary
    from fno.agents.rust_runtime import refuse_without_binary, runtime_mode

    binary = rust_binary.resolve_installed_binary()
    if runtime_mode() == "python" or binary is None:
        # There is no Python rm to fall back to: the twin was deleted
        # (d-e11b2b3e: one verb, one implementation). Refuse by name.
        refuse_without_binary("rm")


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
    """Attach to a running agent session interactively.

    The Rust client verb owns attach; with an installed binary the runtime
    router execs it before this function runs. With a live mux server it
    drives the one dedicated thread pane, which routes every harness by
    capability. With no mux server: claude execs ``claude attach
    <short_id>``, a codex thread execs its declared attach form, pi joins
    its own session, and every other row is asked of the capability table.
    A row whose harness reads ``features.attach = native`` names the
    daemon-kept lane it needs (exit 24); any other state refuses by name
    with the key, the state and the probe that settles it (exit 13).

    There is no Python attach left: a missing binary is refused here.
    """
    from fno.agents.rust_runtime import refuse_without_binary

    _ = name
    refuse_without_binary("attach")


# Observability verbs: trace + resume (Tasks 3.3 / 3.4 / 3.5)
# Both commands live in their own modules so this CLI file stays focused
# on shape + wiring. The cmd_<verb> functions are re-bound here as
# Typer subcommands; tests can still monkeypatch cli.cmd_<verb> for
# spy injection.

from fno.agents.trace_cli import cmd_trace as _cmd_trace  # noqa: E402
from fno.agents.resume_cli import cmd_resume as _cmd_resume  # noqa: E402
from fno.agents.history import history_command  # noqa: E402

agents_app.command("trace", hidden=True)(_cmd_trace)
agents_app.command("resume")(_cmd_resume)
# Advertised (x-6db9): the one verb over live rows, reap receipts and the
# ledger. `fno whoami ledger` stays as its hidden alias.
agents_app.command("history")(history_command)


# Gate verb (Task 2.3): per-provider injection verification gate management


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


@agents_app.command(
    "yard",
    hidden=True,
    help=(
        "The yard identity fold: species, rarity tier, crown, and first-sighting "
        "per registry citizen.\n\n"
        "Read-only over the agent registry and the graph archive. Consumed by "
        "the mux yard overlay (fail-open shell-out); `--json` is the machine "
        "surface, the text mode is a one-line-per-citizen summary. Outcome "
        "only - never the rank, the frequency, or the boundary."
    ),
)
def yard(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the fold as JSON."),
) -> None:
    """Fold the fleet into yard citizens (the f[no]nimals)."""
    import json as _json

    from fno import paths
    from fno.agents.registry import load_registry
    from fno.graph.store import GraphUnreadableError, read_graph_strict
    from fno.yard import RARITY_TIERS, fold

    rows = load_registry()
    archive_path = paths.graph_archive_json()
    # Strict on purpose: this is a truth-bound fold, not a browse verb. The
    # lenient reader turns a corrupt archive into [], which would mark every
    # citizen a first-sighting - fabricated outcome on the machine surface the
    # mux renders. Unreadable history fails the fold so the overlay degrades.
    try:
        archive = read_graph_strict(archive_path) if archive_path.exists() else []
    except GraphUnreadableError as exc:
        typer.secho(f"agents yard: archive unreadable, fold refused: {exc}", err=True)
        raise typer.Exit(code=1) from exc
    citizens = fold(rows, archive)

    if as_json:
        typer.echo(_json.dumps({"citizens": citizens}, indent=2))
        return
    if not citizens:
        typer.echo("the yard is empty (no registry rows)")
        return
    for c in citizens:
        mark = " NEW" if c["first_sighting"] else ""
        crown = f" crown {c['crown_level']}" if c["crown_level"] else ""
        typer.echo(
            f"{c['name']:<24} species {c['species']:>2}  {c['rarity']:<9}{crown}{mark}"
        )
    n_new = sum(1 for c in citizens if c["first_sighting"])
    typer.echo(f"{len(citizens)} citizens, {n_new} first sighting(s); tiers: {'/'.join(RARITY_TIERS)}")


# x-244c: instruments over the capability table. `probe` measures the live
# harness against the (config-merged) row. Four verdicts and UNKNOWN never
# acts: a missing binary or a timeout is UNKNOWN with its reason, never a
# disagreement, because an absent instrument is not a measurement.
harness_app = typer.Typer(
    help="Instruments over the harness capability table.",
    no_args_is_help=True,
)


@harness_app.command("probe")
def harness_probe(
    harness: str = typer.Argument(..., help="Harness name to probe."),
    live: bool = typer.Option(
        False, "--live", help="Allow behavioral probes (they spawn a scratch session)."
    ),
    write: bool = typer.Option(
        False, "--write", help="Emit the config stanza for each disagreement, evidence + date beside it."
    ),
    as_json: bool = typer.Option(False, "--json", "-J", help="Machine-readable report."),
) -> None:
    import json as _json

    from fno.agents.capability_probe import probe_harness

    report = probe_harness(harness, live=live, write=write)
    if as_json:
        typer.echo(_json.dumps(report, indent=2))
    else:
        if "error" in report:
            typer.secho(f"probe refused: {report['error']}", err=True)
            raise typer.Exit(code=1)
        typer.echo(f"probe {harness} (map_version {report['map_version']})")
        for field in report["fields"]:
            typer.echo(
                f"{field['verdict']:<11} {field['field']}: {field['detail']}"
            )
        for warning in report["warnings"]:
            typer.secho(f"override warning: {warning}", fg=typer.colors.YELLOW, err=True)
        if report["stanza"]:
            typer.echo("")
            typer.echo(report["stanza"])
    if any(field["verdict"] == "DISAGREES" for field in report["fields"]):
        raise typer.Exit(code=1)


agents_app.add_typer(harness_app, name="harness", hidden=True)

from fno.agents import (  # noqa: E402,F401
    ask_cli,
    distress_reads,
    gate_reads,
    peek_cli,
    transcript_reads,
)


@agents_app.command(
    "incident",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def incident(ctx: typer.Context) -> None:
    """Durable fleet incident breaker (x-77db).

    stop --reason T [--by X] | clear --reason T [--by X] | status [--json].
    """
    import subprocess

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        typer.secho(
            "fno agents incident: fno-agents binary not found; `fno doctor update --rust` or set FNO_AGENTS_BIN",
            err=True,
        )
        raise typer.Exit(code=1)
    proc = subprocess.run([str(binary), "fleet-incident", *ctx.args])
    raise typer.Exit(code=proc.returncode if proc.returncode >= 0 else 128 - proc.returncode)
