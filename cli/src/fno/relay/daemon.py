"""Group 3 of the cross-session agent relay (x-908b / x-a2c9): the ALWAYS-ON
relay DAEMON. Tails the durable bus, routes each peer-to-peer relay message to
its recipient's inject handle, and re-injects replies -- with NO human in the
routing path (US3 / AC3). The human-out-of-loop guarantee is the whole point.

Three safety properties are structural, not best-effort:

- **Dedup (Invariant).** Every message is delivered at most once per recipient,
  keyed on ``msg_id`` -- a retry storm on the bus cannot double-inject.
- **Cycle termination (US4 / AC4).** Each hop increments ``hop_count``; a message
  whose count reaches ``ttl`` is dropped with ``relay_ttl_exhausted`` -- a cyclic
  route ``a -> b -> a -> b`` terminates by the hop guard, never by chance timing.
- **Provenance (US5 / AC5).** Every injected hop is framed with the
  ``<fno_mail ...>`` tag (the recipient must know it is a peer, not its user). A
  cross-provider
  recipient whose message cannot be framed is REFUSED
  (``relay_dropped{unframed-cross-provider}``) -- the spike's Alice-rejection
  failure made impossible by construction.

The vehicle (``deliver``) is injected: the real claude vehicle
(:func:`daemon_deliver`) routes through the daemon's interactive-claude worker
(the ``worker.submit`` RPC, behind the daemon-held ``session:`` claim); tests pass
a fake recorder. This keeps the routing core a pure, deterministic function over
the real jsonl bus -- AC3/AC4/AC5 are testable with no claude spawn.

Singleton via :mod:`fno.claims` (``relay:daemon`` key): two daemon instances must
not both route the same bus message (Concurrency). The internal "delivery queue"
is the bus cursor itself -- one daemon is one consumer of one log, so a
cursor-bounded poll IS the queue (no hand-rolled seq/semaphore machinery needed).
"""
from __future__ import annotations

import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

from fno.agents.events import emit
from fno.bus import cursor as _cursor
from fno.bus.log import Envelope, append, iter_messages
from fno.relay import envelope as _env
from fno.relay import registry as _registry
from fno.relay import router as _router
from fno.relay.registry import RegistryEntry
from fno.relay.router import Resolution, Unroutable

CLAIM_KEY = "relay:daemon"
CURSOR_NAME = "relay-daemon"
CLAUDE = "claude"

# A reply read from a recipient's pane. ``None`` means "delivered, no reply
# captured this hop" (still a successful route).
Deliver = Callable[[Resolution, str], Optional[str]]


@dataclass(frozen=True)
class RouteResult:
    status: str  # "routed" | "dropped" | "skipped"
    reason: Optional[str] = None
    framed: Optional[str] = None
    reply_envelope: Optional[Envelope] = None


# ---------------------------------------------------------------------------
# Routing core (pure; deliver injected) -- the unit AC3/AC4/AC5 test against.
# ---------------------------------------------------------------------------

def route_message(
    env: Envelope,
    *,
    deliver: Deliver,
    index: Optional[dict[str, RegistryEntry]] = None,
    node_resolver=None,
    events_path: Optional[Path] = None,
    seen: Optional[set[str]] = None,
) -> RouteResult:
    """Route ONE relay bus envelope to its recipient. Emits exactly one decision
    event (``relay_routed`` / ``relay_dropped{reason}``); routing is never silent
    (Invariant). A re-injected reply is returned as ``reply_envelope`` for the
    caller to append back to the bus (the cycle's next hop)."""
    # Dedup: at most once per msg_id (Invariant). A re-seen id is skipped without
    # an event (it was already decided on its first pass). The id is recorded as
    # seen ONLY at a terminal decision below -- NOT here -- so a delivery that
    # raises is retried on the next pass rather than silently swallowed.
    if seen is not None and env.id in seen:
        return RouteResult(status="skipped", reason="duplicate")

    def _decide(result: RouteResult) -> RouteResult:
        if seen is not None:
            seen.add(env.id)
        return result

    # TTL / cycle guard (US4 / AC4): a message that has used up its hops is
    # dropped, never re-injected.
    hops, ttl = _env.hop_count(env), _env.ttl(env)
    if hops >= ttl:
        _emit(events_path, "relay_ttl_exhausted", msg_id=env.id, hop_count=hops, ttl=ttl, to=env.to)
        return _decide(RouteResult(status="dropped", reason="ttl-exhausted"))

    # Resolve the recipient (Boundary: an unknown target is surfaced, not eaten).
    idx = index if index is not None else _registry.index()
    try:
        res = _router.resolve(env.to, index=idx, node_resolver=node_resolver)
    except (Unroutable, ValueError) as exc:
        _emit(events_path, "relay_dropped", reason="unroutable", msg_id=env.id, to=env.to, detail=str(exc))
        return _decide(RouteResult(status="dropped", reason="unroutable"))

    # A recipient footnote owns no inject handle for cannot be delivered to
    # (design Failure Mode: refuse a hand-opened terminal at send time).
    if not res.inject_handle:
        _emit(events_path, "relay_dropped", reason="no-inject-handle", msg_id=env.id, to=env.to)
        return _decide(RouteResult(status="dropped", reason="no-inject-handle"))

    # Provenance framing (US5 / AC5). Every hop is framed; a message that cannot
    # be framed (missing provenance) is refused for a cross-provider recipient
    # (AC5-FR -- unframed cross-provider injection is a bug by construction).
    framed = _env.frame_envelope(env)
    if framed is None:
        if res.provider != CLAUDE:
            _emit(events_path, "relay_dropped", reason="unframed-cross-provider", msg_id=env.id, to=env.to)
            return _decide(RouteResult(status="dropped", reason="unframed-cross-provider"))
        _emit(events_path, "relay_dropped", reason="unframable", msg_id=env.id, to=env.to)
        return _decide(RouteResult(status="dropped", reason="unframable"))

    # Deliver (inject) and capture any reply. A deliver that RAISES is surfaced as
    # relay_deliver_failed and never crashes the daemon (design Failure Mode
    # "Errors"). The vehicle (daemon_deliver -> deliver_session) owns the bounded
    # worker.submit retry for recoverable drops; an exception escaping it is
    # unrecoverable (no daemon-held handle / dead worker), so the daemon surfaces
    # it and moves on -- a single
    # decision event, no head-of-line block, no per-pass event flood. (A pre-peer
    # arrival race is a peer-manager concern: register before announcing ready.)
    try:
        reply = deliver(res, framed)
    except Exception as exc:  # noqa: BLE001 - a vehicle failure must never crash the daemon
        _emit(events_path, "relay_deliver_failed", msg_id=env.id, to=env.to,
              provider=res.provider, error=str(exc))
        return _decide(RouteResult(status="dropped", reason="deliver-failed"))
    _emit(events_path, "relay_routed", msg_id=env.id, to=env.to, provider=res.provider, hop_count=hops)

    reply_env: Optional[Envelope] = None
    if reply:
        # The reply is the cycle's next hop: addressed back to the original
        # sender, hop_count incremented (so a -> b -> a terminates at ttl).
        reply_env = _env.make_relay_envelope(
            from_session=res.session_id,
            # Address the reply back by session id (the routable form); the
            # bare id would be parsed as a name lookup and miss.
            to=f"session:{env.from_session or env.from_}",
            body=reply,
            provider_from=res.provider,
            hop_count=hops + 1,
            ttl=ttl,
            thread=env.thread,
            in_reply_to=env.id,
        )
    return _decide(RouteResult(status="routed", framed=framed, reply_envelope=reply_env))


# ---------------------------------------------------------------------------
# Bus tail (one drain pass) + forever loop
# ---------------------------------------------------------------------------

def run_once(
    *,
    deliver: Deliver,
    index: Optional[dict[str, RegistryEntry]] = None,
    node_resolver=None,
    events_path: Optional[Path] = None,
    seen: Optional[set[str]] = None,
    append_replies: bool = True,
) -> list[RouteResult]:
    """Drain unread ``kind=relay`` messages once: route each, advance the cursor,
    and (by default) append captured replies back to the bus as the next hop.

    The cursor (``relay-daemon``) makes this the daemon's delivery queue: only
    messages after the last-acked id are processed, so a restart resumes cleanly.
    """
    results: list[RouteResult] = []
    unread, resync_to = _tail_after_cursor()
    if resync_to is not None:
        # Cursor rotated out of retention (daemon was down past rotation). The
        # relay sink is keystroke injection -- NOT idempotent -- so re-injecting a
        # stale backlog is worse than skipping it. Resync to head: advance past
        # the gap, route nothing, emit one observable event. (An inbox would
        # fail-open and rescan; a router must not replay stale hops.)
        _cursor.advance_cursor(CURSOR_NAME, resync_to)
        emit("relay_cursor_resync", path=events_path, advanced_to=resync_to)
        return results
    last_id: Optional[str] = None
    # Resolve the registry ONCE per pass: index() discovers live sessions (a
    # ~/.claude/sessions scan), too costly to repeat per message.
    idx = index if index is not None else _registry.index()
    for env in unread:
        last_id = env.id
        if env.kind != _env.RELAY_KIND:
            continue  # not relay traffic (human mail etc.) -- leave it for its consumer
        res = route_message(
            env, deliver=deliver, index=idx, node_resolver=node_resolver,
            events_path=events_path, seen=seen,
        )
        results.append(res)
        if res.reply_envelope is not None and append_replies:
            append(res.reply_envelope)
    if last_id is not None:
        _cursor.advance_cursor(CURSOR_NAME, last_id)
    return results


def _tail_after_cursor() -> tuple[list[Envelope], Optional[str]]:
    """Bus envelopes AFTER the daemon's cursor, oldest -> newest.

    Unlike :func:`bus.cursor.scan_unread` (which filters to ``to == name``, an
    inbox view), the daemon is a ROUTER: it tails the whole log and routes by
    each message's ``to``.

    Returns ``(unread, resync_to)``. ``resync_to`` is non-None ONLY when the
    cursor is set but its id has rotated out of retention: it carries the newest
    id so the caller can resync to head WITHOUT replaying a stale backlog (the
    sink is not idempotent). An absent cursor (never run) returns all retained
    with no resync (first-run catch-up is the intended behavior)."""
    cur = _cursor.read_cursor(CURSOR_NAME)
    msgs = list(iter_messages())
    if cur is None:
        return msgs, None
    if all(m.id != cur for m in msgs):
        return [], (msgs[-1].id if msgs else None)  # rotated out -> resync to head
    after: list[Envelope] = []
    passed = False
    for m in msgs:
        if passed:
            after.append(m)
        if m.id == cur:
            passed = True  # cur is guaranteed present (rotated-out handled above)
    return after, None


def run_forever(
    *,
    deliver: Deliver,
    poll_interval: float = 1.0,
    events_path: Optional[Path] = None,
    holder: Optional[str] = None,
    max_passes: Optional[int] = None,
) -> None:
    """Run the daemon: hold the singleton claim, poll the bus, route forever.

    Singleton via ``relay:daemon`` (Concurrency: two daemons must not both route).
    ``max_passes`` bounds the loop for tests; ``None`` runs until interrupted.

    ponytail: a poll loop, not a hand-rolled semaphore queue. One daemon is the
    sole consumer of one append-only log, so the cursor IS the queue; a
    select/inotify wakeup is a latency optimization to add only if a profiler
    asks for it.
    """
    from fno.claims.core import ClaimHeldByOther, acquire_claim, release_claim

    holder = holder or f"relay-daemon:{_pid()}"
    try:
        acquire_claim(CLAIM_KEY, holder, reason="cross-session relay daemon")
    except ClaimHeldByOther:
        emit("relay_daemon_already_running", path=events_path, holder=holder)
        return

    # Durable dedup is the bus cursor: each message is tailed once and the cursor
    # advances past it; a rotated-out cursor resyncs to head (run_once) rather
    # than replaying. ``seen`` is the in-process belt-and-suspenders that also
    # makes a deliver-failed message retry exactly once per pass (it is recorded
    # seen only at a terminal decision). ponytail: it grows for the process
    # lifetime; cap to an LRU only if a very long-lived daemon's memory matters.
    seen: set[str] = set()
    passes = 0
    try:
        emit("relay_daemon_started", path=events_path, holder=holder)
        while max_passes is None or passes < max_passes:
            try:
                run_once(deliver=deliver, events_path=events_path, seen=seen)
            except Exception as exc:  # a route failure must never crash the daemon
                emit("relay_daemon_error", path=events_path, error=str(exc))
            passes += 1
            time.sleep(poll_interval)
    finally:
        release_claim(CLAIM_KEY, holder)
        emit("relay_daemon_stopped", path=events_path, holder=holder)


# ---------------------------------------------------------------------------
# Real claude vehicle: route through the daemon-held session: handle (E4.3).
# ---------------------------------------------------------------------------

def daemon_deliver(
    *,
    holder: Optional[str] = None,
    settle_ms: int = 1000,
    timeout: float = 180.0,
) -> Deliver:
    """Build a ``deliver`` that routes a hop through a live claude session footnote
    holds the ``session:<uuid>`` claim on, replacing the retired footnote-owned PTY
    peer. Two lanes share one claim guard:

    - **interactive** (E4.3): a footnote-spawned PTY worker -> the daemon
      ``worker.submit`` RPC (:func:`fno.relay.roundtrip.deliver_session`).
    - **attached** (G3, node x-e027): an adopted ``claude --bg`` session with no
      worker socket -> the ``control.sock`` op:'reply' inject
      (:func:`fno.relay.roundtrip.deliver_attached`, the ``mail-inject`` verb).

    Single-writer claim guard (Locked Decision #1 / AC-E4-3): before injecting, the
    relay ACQUIRES ``session:<uuid>``. Finding it held by the lane's holder
    (``pty:<short>`` -- the same tag for both lanes: the daemon writes it at E1
    spawn, the adopt path at G1) is the signal that a live handle exists to route
    THROUGH. A SUCCESSFUL acquire means the claim is FREE -- no host owns the
    session -- so there is no handle; the relay RELEASES the probe claim and refuses
    rather than spawning a second ``--session-id X`` writer (two processes pinned to
    one session id corrupt the transcript). The relay never ends up holding a
    ``session:`` claim itself. Both lanes flowing through this one claim is the
    single-writer serialize: grid-drive and relay-inject contend on it, footnote
    serializes (Claude's own remote bridge is an out-of-band writer, advisory).

    A vehicle exception is surfaced by :func:`route_message` as
    ``relay_deliver_failed`` (it never crashes the daemon). Not unit-tested end to
    end (the live substrate is the ``FNO_LIVE_RELAY`` gate, AC-E4-5); the claim
    guard and the resolution/capture primitives are unit-tested.
    """
    from fno.claims.core import ClaimHeldByOther, acquire_claim, release_claim  # noqa: PLC0415
    from fno.relay.roundtrip import (  # noqa: PLC0415
        deliver_attached, deliver_session, interactive_claim_holder,
        resolve_attached_short_id, resolve_worker_short_id,
    )

    holder = holder or f"relay-daemon:{_pid()}"

    def _deliver(res: Resolution, framed: str) -> Optional[str]:
        sid = res.session_id
        if not sid:
            raise RuntimeError("relay_deliver_failed: resolution carries no session_id")
        # Gather BOTH candidate lanes for this session, each with the claim holder
        # it would be held under (``pty:<short>``) and its delivery vehicle. A
        # session normally resolves to exactly one lane, but a stale live-looking
        # interactive row can coexist with the valid adopted row -- so we dispatch by
        # which candidate actually HOLDS the claim, not by resolution precedence
        # (codex peer P2: precedence would refuse when the wrong lane wins the order).
        candidates: list[tuple[str, Callable[[], Optional[str]]]] = []
        short_id = resolve_worker_short_id(sid)
        if short_id is not None:
            candidates.append((
                interactive_claim_holder(short_id),
                lambda: deliver_session(sid, framed, settle_ms=settle_ms, timeout=timeout, short_id=short_id),
            ))
        attached_short = resolve_attached_short_id(sid)
        if attached_short is not None:
            candidates.append((
                interactive_claim_holder(attached_short),
                lambda: deliver_attached(sid, framed, timeout=timeout),
            ))
        if not candidates:
            raise RuntimeError(
                f"relay_deliver_failed: no live daemon worker or adopted session for session {sid}"
            )

        # Probe the single-writer claim ONCE (AC-E4-3). Held by a candidate's lane
        # holder -> route through THAT lane. A free claim (no host) or a foreign
        # holder (a ``stream:`` lane / external writer matching no candidate) refuses
        # rather than injecting into a session footnote does not PTY-host.
        key = f"session:{sid}"
        try:
            acquire_claim(key, holder, reason="relay routing probe")
        except ClaimHeldByOther as exc:
            for expected, deliver_fn in candidates:
                if exc.holder == expected:
                    return deliver_fn()
            holders = " or ".join(e for e, _ in candidates)
            raise RuntimeError(
                f"relay_deliver_failed: session:{sid} held by {exc.holder!r}, not the daemon "
                f"interactive lane ({holders}); refusing to route"
            )
        # Free (or stale-reclaimed): no host -> no handle. Drop the probe claim so
        # the relay is never a second holder, then refuse.
        release_claim(key, holder)
        raise RuntimeError(
            f"relay_deliver_failed: session:{sid} not daemon-held (no live handle to route through)"
        )

    return _deliver


def _emit(events_path: Optional[Path], kind: str, **data) -> None:
    emit(kind, path=events_path, **data)


def _pid() -> int:
    import os
    return os.getpid()


def _main(argv: Optional[list[str]] = None) -> int:
    """Run the relay router daemon: ``python -m fno.relay.daemon``.

    Tails the bus and routes peer-to-peer with the singleton claim held. This is
    the operator/launchd entrypoint. Each hop is routed through the daemon's
    interactive-claude worker via :func:`daemon_deliver`: the relay finds the
    recipient's ``session:<uuid>`` claim daemon-held and injects through the
    ``worker.submit`` RPC (E4.3). A recipient with no live daemon worker surfaces
    ``relay_deliver_failed`` rather than silently dropping. The retired
    peer-lifecycle manager (the old in-process PTY ``peers`` dict) is gone -- the
    Rust daemon's worker model owns the PTYs now.
    """
    import argparse

    parser = argparse.ArgumentParser(prog="python -m fno.relay.daemon")
    parser.add_argument("--poll-interval", type=float, default=1.0)
    parser.add_argument("--max-passes", type=int, default=None,
                        help="bound the loop (default: run until interrupted)")
    args = parser.parse_args(argv)

    run_forever(
        deliver=daemon_deliver(),
        poll_interval=args.poll_interval,
        max_passes=args.max_passes,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
