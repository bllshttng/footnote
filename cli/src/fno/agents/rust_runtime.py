"""Rust runtime routing for ``fno agents`` (Phase 6 W6 / cv-d28b266a).

The Rust daemon is the **default** runtime for the daemon-native verbs: by
default ``fno agents <verb> [args]`` execs the compiled ``fno-agents`` client
binary for the verbs that exist only on the Rust side (``spawn``, ``status``,
``drive``, the ``*-channel`` verbs) whenever an *installed* binary is present.
Following the full thin-wrapper rewire (ab-d82655d7) and the client-side
``ask`` ports (claude ab-cc926b4e, codex ab-0429c6e1, gemini ab-73da4ac2),
EVERY dispatchable verb auto-routes to the binary — including ``ask`` for all
providers. ``PYTHON_AGENT_VERBS`` is now empty, so ``AUTO_ROUTE_VERBS`` equals
``RUST_CLIENT_VERBS`` and that identity is the whole routing contract. The
Python implementations all stay registered as the ``FNO_AGENTS_RUNTIME=python``
fallback (and serve when no installed binary is present). See
:data:`AUTO_ROUTE_VERBS`.

``FNO_AGENTS_RUNTIME`` selects the runtime explicitly (see :func:`runtime_mode`):

- ``rust``   -- force the binary for every verb; a missing binary is a hard 127.
- ``python`` -- force the Python dispatch; never touch the binary.
- unset / anything else -- ``auto`` (the default described above).

To keep the default from surprising a *development* checkout, ``auto`` resolves
only *installed* binaries (bundled wheel dir / launcher sibling / ``PATH``) and
ignores the cargo dev target; a dev opts into the local build with
``FNO_AGENTS_RUNTIME=rust``. This makes the change reversible per-invocation.

Design: ``internal/fno/design/2026-05-22-fno-pty-supervisor-and-drive.md``
(line 136 — "Python ``fno agents <verb>`` is a thin Typer wrapper that execs
``fno-agents <verb>``"). Plan: ``plans/2026-05-25-phase6-w6-distribution.md``.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import IO, TYPE_CHECKING, Callable, Mapping, NoReturn, Optional, Sequence

# The binary lookup itself is stdlib-only and has callers below this layer, so
# it lives at fno.rust_binary; this module keeps the dispatch/routing half.
from fno import rust_binary

if TYPE_CHECKING:
    import click

#: Env var that selects the runtime. Recognized values: ``rust`` (force the
#: binary), ``python`` (force Python dispatch); anything else (incl. unset)
#: means ``auto`` -- the default, which prefers an installed binary per-verb.
RUNTIME_ENV = "FNO_AGENTS_RUNTIME"
#: Exit code when the runtime is requested but the binary is absent. Distinct
#: from the daemon's own codes (1/2/13/14/15/18) so the failure is legible.
BIN_NOT_FOUND_EXIT = 127

#: Verbs the bundled ``fno-agents`` client implements end-to-end: the daemon
#: request verbs in ``client.rs`` ``build_request`` plus the directly-dispatched
#: verbs (``drive``, ``status``, and the client-side ``drive-authority``,
#: ``trace``, ``ping``, ``resume``, ``attach``, ``logs`` ported in ab-d82655d7).
#: The ``auto`` (default) runtime routes all of these to Rust except the verbs
#: Python still owns (see :data:`PYTHON_AGENT_VERBS`). Kept in sync with
#: ``crates/fno-agents/src/bin/client.rs`` by a test that parses that file, so
#: drift fails CI rather than silently mis-routing.
RUST_CLIENT_VERBS = frozenset(
    {
        "spawn",
        "ask",
        "list",
        "status",
        "stop",
        "rm",
        "reconcile",
        # Daemon binary-version drift restart (ab-1891cdff): a Rust-only verb
        # dispatched directly in client.rs before build_request (no daemon RPC).
        # SIGTERMs a stale daemon and lazy-starts a fresh one from the current
        # binary; PTY workers survive (Outcome B).
        "restart",
        # Manual dead-row GC (x-b1aa): the same sweep the daemon runs on its idle
        # tick, on demand. Dispatched directly in client.rs before build_request
        # (operates on the registry under the shared flock; no daemon RPC).
        "reap",
        # `drive` and `grid` (the WebSocket drive surface + the TUI compositor)
        # were retired at G4 (x-f54c) when the mux became the agent-PTY
        # substrate; the binary intercepts them with a mux pointer.
        "register-channel",
        "unregister-channel",
        "push-channel",
        # Python-only verbs ported to the Rust client (full thin-wrapper rewire).
        # These dispatch client-side in client.rs before build_request (no daemon
        # RPC, except `logs --follow` which upgrades to the agent.logs WS stream).
        "drive-authority",
        "trace",
        "ping",
        "resume",
        "adopt",
        "attach",
        "logs",
        # `host`/`promote` (interactive daemon PTY hosting) were retired at G4
        # (x-f54c); spawn a mux-hosted pane with `spawn --substrate pane`.
        # Stop-hook decision verb (control-plane collapse wedge, ab-d0337fbc).
        # The bash shim in hooks/target-stop-hook.sh calls the binary DIRECTLY
        # (explicit resolution order, no Python routing); this entry exists so
        # `fno agents loop-check` also works for manual/diagnostic invocation
        # and so the client.rs<->router parity test stays in sync.
        "loop-check",
        # Unified driver loop verb (step 5, ab-781b6d17). Dispatched directly
        # in client.rs before build_request (no daemon RPC); this entry keeps
        # the client.rs<->router parity test in sync and lets `fno agents loop
        # run ...` route for manual invocation.
        "loop",
        # Terminal-only side-effect WRITER (control-plane step 6, ab-f8e5f214).
        # Like loop-check, the bash stop-hook shim calls the binary DIRECTLY on a
        # terminal-allow decision (no Python routing); this entry exists so the
        # client.rs<->router parity test stays in sync.
        "finalize",
        # Eliminate-don't-vendor folds (packaging EPIC ab-8bdb4642, US1
        # ab-58645f63): Rust ports of the deleted scripts/lib/kill-criteria.sh
        # and scripts/lib/verify-event-evidence.sh. Both dispatch DIRECTLY in
        # client.rs before build_request (no daemon RPC). The Python `fno phase
        # kill-check` wrapper resolves the binary and invokes its verb
        # explicitly (not via `fno agents` routing); these entries exist so
        # the client.rs<->router parity test stays in sync.
        "kill-check",
        "verify-evidence",
        # Inside-leg state push (inside-out E3.2): a per-turn hook calls
        # `fno agents report --session-id <uuid> --seq <n> --state <s>` and the
        # Rust client sends the agent.report RPC to an already-running daemon
        # (never lazy-starts). Dispatched directly in client.rs before
        # build_request (no Python impl); this entry keeps the
        # client.rs<->router parity test in sync and provides the help line.
        "report",
        # Agent-state wait + event subscription (mux roadmap wave 2).
        # Both dispatch DIRECTLY in client.rs before build_request (no daemon
        # RPC): `wait` polls registry.json until a row reaches idle|blocked|done;
        # `subscribe` follows the daemon's events.jsonl and streams state
        # transitions + pane exits as NDJSON. These entries keep the
        # client.rs<->router parity test in sync and provide the help lines.
        "wait",
        "subscribe",
        # Catch-up digest (x-4e2d): read-only "while you were gone" fold over
        # events.jsonl + ledger.json for a session. Dispatched directly in
        # client.rs before build_request (no daemon RPC, no Python impl); this
        # entry keeps the client.rs<->router parity test in sync.
        "digest",
        # Needs-me queue (x-feec): read-only fold over events.jsonl + ledger for
        # ALL sessions, emitting review_wedged / budget_stop items. Dispatched in
        # client.rs before build_request (no daemon RPC, no Python impl).
        "needs",
    }
)

#: Verbs the Python ``agents`` app implements that do NOT auto-route to the
#: Rust client.
#:
#: ``send`` (G2 Task 2.1): async durable-first delivery verb. Python owns it
#: in Group 2; Rust port deferred to Group 4. The verb is NOT in
#: ``RUST_CLIENT_VERBS`` so it never auto-routes to the daemon.
#:
#: History: ``stop``/``rm`` (Task 2.1), ``list``/``reconcile`` (Task 3.1), and
#: the six former Python-only verbs (``logs``/``ping``/``drive-authority``/
#: ``attach``/``resume``/``trace``, ab-d82655d7) all reached Rust stdout/JSON
#: parity and left this set. ``ask`` was the last holdout: claude shipped
#: client-side in ab-cc926b4e (PR #366), codex + the provider-conditional flip
#: in ab-0429c6e1 (PR #371), and gemini + this UNCONDITIONAL flip in ab-73da4ac2.
#: The provider-conditional special case (``RUST_CLIENT_ASK_PROVIDERS`` +
#: ``_resolve_ask_provider``) is gone; the ``AUTO_ROUTE_VERBS`` identity below is
#: now the whole routing contract except for ``send``.
PYTHON_AGENT_VERBS: frozenset[str] = frozenset({
    # G2 Task 2.3: injection gate management; uses Python _daemon_rpc; no Rust port planned.
    "gate",
    # Messaging (send/inbox/ack) moved OUT of `fno agents` into the dedicated
    # `fno mail` namespace (ab-cee91152); the agents group is lifecycle-only.
    # Epic ab-d3a1ae3e G2 Task 4.3: the stream-json observe surface. Pure Python;
    # polls the worker's stream.read_frames directly. No Rust client port (the
    # `--watch` worker-binary surface noted in client.rs is a separate lane), so
    # it must never auto-route to the daemon.
    "watch",
    # ab-098967b4 P1: internal helper the Rust `list` render path shells out to
    # for the discovered-live-sessions lane. Pure Python (reads
    # ~/.claude/sessions via fno.agents.discover); no Rust port, so it
    # must never auto-route — Rust invokes it with FNO_AGENTS_RUNTIME=python.
    "discovered-json",
    # ab-098967b4 P2: internal helper the Rust loop-check shells out to on a
    # `block` decision for the loop-boundary inbox nudge. Pure Python (reads the
    # bus via fno.agents.nudge); no Rust port.
    "nudge-peek",
    # x-73cc: the shared bg-dispatch guard verb. Pure-Python orchestration of
    # `fno claim` (Guard 1 node-claim probe + Guard 2 dispatch:<id> reservation)
    # called by both dispatch-node.sh and spawn.sh. There is NO `spawn-guard` on
    # the Rust client, so it must never auto-route to the daemon (it would 404 /
    # be shadowed for installed users). Python owns it.
    "spawn-guard",
    # x-3218: the canonical agent-name bridge, the shell twin of the Python
    # dispatchers' direct `fno.agents.naming` import. Pure Python and purely
    # computational (no daemon state); there is NO `name` on the Rust client, so
    # it must never auto-route. The daemon stays the name VALIDATOR at the spawn
    # boundary and must not become the generator - truncating there would make
    # the name a caller reasons about differ from the one the runtime registers.
    "name",
    # x-da8c: the registry-miss healer the Rust lifecycle verbs shell out to.
    # Pure Python (fno.agents.store_fallback); no Rust port. Staying out of
    # RUST_CLIENT_VERBS is the recursion guard for that shellout, so listing it
    # here is documentary — AUTO_ROUTE_VERBS already excludes it.
    "heal-token",
    # x-301a: "what is MY registered mesh name?" — reads FNO_AGENT_SELF + the
    # registry, read-only. Pure Python (fno.agents.whoami); there is NO
    # `whoami` on the Rust client, so it must never auto-route to the daemon.
    # Listing it here is defensive/documentary: whoami is not in
    # RUST_CLIENT_VERBS, so AUTO_ROUTE_VERBS already excludes it.
    "whoami",
    # x-c5cc: the spawn-gate audit surface — every live worker process with
    # RSS via psutil, over the same union the gate counts. Python-only by
    # design (LD8): no daemon involvement, no Rust port, never auto-routes.
    "top",
    # x-05da: the read-only observe leg (twin of `fno mail send`). Reads a
    # peer's on-disk transcript / status events via fno.agents.peek. No Rust
    # client port, so it must never auto-route to the daemon.
    "peek",
    # The /fno-me self-service join verb: resolves ambient harness identity and
    # writes an idle roster row (register_existing_session). Pure Python, no Rust
    # client port, so it must never auto-route to the daemon.
    "register",
    # x-a472: the transcript-tail supervision classifier (`fno agents truth`).
    # Read-only, pure Python (fno.agents.session_truth reads the transcript via
    # peek); no Rust client port, so it must never auto-route to the daemon.
    "truth",
    # x-7685: daemon-free registry read for hooks (`fno agents registry-json`).
    # Pure Python (load_registry, a file read) - deliberately NOT the Rust-routed
    # `fno agents list`, which lazy-starts the daemon a Stop hook must never wait
    # on. No Rust client port, so it must never auto-route to the daemon.
    "registry-json",
})

#: Verbs the ``auto`` (default) runtime routes to Rust: the Rust client verbs
#: MINUS the verbs Python still owns. Since ab-73da4ac2 ``PYTHON_AGENT_VERBS`` is
#: empty, so this equals :data:`RUST_CLIENT_VERBS` exactly — every dispatchable
#: verb (incl. ``ask`` for all providers) auto-routes when an *installed* binary
#: is present. A forced ``FNO_AGENTS_RUNTIME=rust`` still routes every verb; a
#: forced ``=python`` (or no installed binary) keeps the mature Python dispatch.
AUTO_ROUTE_VERBS = RUST_CLIENT_VERBS - PYTHON_AGENT_VERBS

#: Short help for the verbs that exist ONLY on the Rust client (no
#: ``@agents_app.command`` registration). Without these, ``fno agents --help``
#: -- which always renders the Python group help (a bare ``--help`` never execs
#: the binary) -- silently omits every Rust-only verb, so an agent reading the
#: help has no way to discover ``host``/``grid``/``drive``/``spawn``/... This
#: dict is the discoverability source: :class:`_AgentsRuntimeGroup` injects each
#: entry into ``list_commands``/``get_command`` so the group help lists them with
#: a description, even though the actual dispatch happens in ``make_context``
#: (which execs the binary before Click ever resolves the sub-command).
#:
#: Insertion order is the help display order. The keys MUST equal
#: ``RUST_CLIENT_VERBS`` minus the Python-registered command names -- a test
#: (``test_rust_only_verb_help_covers_unregistered_verbs``) enforces that, so a
#: future Rust-only verb cannot land without a help entry and re-introduce the
#: gap.
RUST_ONLY_VERB_HELP: dict[str, str] = {
    # "spawn" is now Python-registered (Task 1.2): a Python cmd_spawn command
    # provides the --once / ephemeral lifecycle path and the claude plain-spawn
    # path. The daemon PTY worker path (codex/gemini without --once) still
    # auto-routes to Rust via RUST_CLIENT_VERBS + AUTO_ROUTE_VERBS, but because
    # the verb has a Python @agents_app.command it is no longer "Rust-only" and
    # must not appear here (test_rust_only_verb_help_covers_unregistered_verbs
    # enforces the invariant).
    "status": "Report daemon liveness and per-agent state.",
    "restart": "Restart a stale daemon (pick up a new build; PTY workers survive).",
    "reap": "Garbage-collect finished agent-view rows (terminal, past grace, clean worktree); --json for machine output.",
    "register-channel": "Register a Claude Code session as an agent channel.",
    "unregister-channel": "Unregister an agent channel by id.",
    "push-channel": "Push a message to a registered agent channel.",
    "loop-check": "Stop-hook decision: external-truth done()/backstop check (read-only).",
    "loop": "Unified driver loop: run --driver target [options] (step 5).",
    "finalize": "Terminal-only side-effect writer: ledger record + (ship) plan stamp/handoff (step 6).",
    "kill-check": "Evaluate a plan's kill_criteria (folded from kill-criteria.sh); usually via `fno phase kill-check`.",
    "verify-evidence": "Verify child-promise event evidence and non-Claude agent presence (folded from verify-event-evidence.sh).",
    "report": "Inside-leg state push (E3.2): store working|blocked|done on a claude row; called by the per-turn hook.",
    "wait": "Block until an agent's registry row reaches idle|blocked|done: --agent <name> --state <s> [--timeout-ms N] [--json].",
    "subscribe": "Stream registry state transitions + pane exits as NDJSON (follows events.jsonl): [--agent <name>] [--kinds state,exit] [--json].",
    "digest": "Catch-up 'while you were gone' fold over events + ledger for a session: --session <s> --since <ts> [--json].",
    "needs": "Needs-me queue fold over events + ledger across all sessions (review_wedged/budget_stop): [--since-epoch <secs>] [--fires-floor <n>] [--json].",
    "adopt": "Register an orphaned session by its session id so it is addressable (peek/ask/resume/mail); resolves the registry, .fno/target-state.md, then harness stores.",
}

#: The only Rust-only verb the In-N-Out menu advertises (x-71b6). Every other
#: :data:`RUST_ONLY_VERB_HELP` verb is display-hidden - stop-hook / runtime
#: plumbing (``loop-check``/``finalize``/``kill-check``/...) and daemon channel
#: verbs a human never types - but stays invocable, listed by ``fno help --all``
#: and each verb's own ``--help``. Membership is display-only; it never changes
#: dispatch or the RUST_CLIENT_VERBS routing set.
RUST_ONLY_ADVERTISED: frozenset[str] = frozenset({"status"})

#: Verbs retired at G4 (x-f54c): the grid, the WebSocket ``drive`` surface, and
#: the interactive daemon PTY hosting behind ``host``/``promote`` moved to the
#: mux. They are NOT in :data:`RUST_CLIENT_VERBS` (no routable client verb) and
#: NOT in :data:`RUST_ONLY_VERB_HELP` (not advertised in ``--help``), but
#: ``get_command`` still resolves them to a one-line mux pointer that exits
#: non-zero, so a script hitting a retired verb gets a helpful error instead of a
#: bare "No such command" no-op (AC5-EDGE). The Rust binary carries the same
#: pointers for a raw ``fno-agents <verb>`` / forced-rust call.
RETIRED_VERB_POINTERS: dict[str, str] = {
    "grid": "agent panes now live in the mux. Open `fno mux`, or script panes with `fno mux pane ls|read|run|send|wait|kill`.",
    "drive": "drive an agent pane in the mux. Use `fno mux pane send <pane> ...`, or open `fno mux` and type into the pane.",
    "host": "spawn a mux-hosted agent pane with `fno agents spawn --name <n> --substrate pane`.",
    "promote": "the mux hosts agent panes; spawn one with `fno agents spawn --name <n> --substrate pane`.",
}


def runtime_mode() -> str:
    """Resolve the runtime selection from ``FNO_AGENTS_RUNTIME``.

    Returns one of:

    - ``"rust"``   -- the caller forced the Rust binary (hard error if absent).
    - ``"python"`` -- the caller forced the Python dispatch (binary untouched).
    - ``"auto"``   -- the default (unset or any unrecognized value): Rust is the
      runtime for the verbs it implements when an *installed* binary is present,
      and Python serves every other case.
    """
    val = os.environ.get(RUNTIME_ENV, "").strip().lower()
    if val == "rust":
        return "rust"
    if val == "python":
        return "python"
    return "auto"


def rust_runtime_enabled() -> bool:
    """True iff the caller *forced* the Rust runtime via ``FNO_AGENTS_RUNTIME=rust``.

    Note this is narrower than "the Rust binary will run": under the default
    ``auto`` mode an installed binary also runs, but only for supported verbs.
    """
    return runtime_mode() == "rust"


def _is_pane_substrate_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` targeting the ``pane`` substrate (4a-G2).

    The pane substrate is mux-hosted now: the Python back half owns the
    ``fno mux pane run`` spawn (front-half reuse + registry mux ref), so a
    pane spawn must never route to the Rust client's daemon RPC (the daemon
    PTY host retires at G4; a silent fallback there is exactly what AC1-ERR
    forbids). ``pane`` is the default, so an absent ``--substrate`` counts.
    The scan stops at ``--argv`` like the other raw-args scans so a payload
    token can never masquerade as our flag.
    """
    if verb != "spawn":
        return False
    substrate = "pane"
    it = iter(args)
    for a in it:
        if a == "--argv":
            break
        if a in ("--once", "-o", "--headless", "-p"):
            # The headless spellings (--once/-o and --headless/-p): a one-shot,
            # never a pane. Must be honored here or a headless spawn would route
            # to the pane back half. `-H` (harness) and `-P` (vendor) are NOT
            # here: they are value flags that do not pick the lane.
            return False
        if a == "--substrate":
            substrate = next(it, "")
        elif a.startswith("--substrate="):
            substrate = a.split("=", 1)[1]
    return substrate == "pane"


def _args_before_argv(args: Sequence[str]) -> Sequence[str]:
    """The fno-arg head, stopping at the ``--argv`` provider-payload boundary.

    A payload token (e.g. the child command's own ``--resume``/``--role``) must
    never be read as one of our routing flags, so every spawn-flag scan operates
    on this slice. Mirrors the ``--argv`` break in :func:`_is_pane_substrate_spawn`.
    """
    if "--argv" in args:
        return args[: list(args).index("--argv")]
    return args


def _is_role_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` carrying ``--role`` (x-d2fe).

    Role-based model routing is implemented only in the Python spawn path
    (``cmd_spawn`` -> ``bg_create`` resolves the per-spawn env). The Rust
    client does not parse ``--role``, so a ``spawn ... --role <r>`` that
    auto-routed to the binary would exit with ``unknown flag: --role``.
    Detecting it here lets the call fall through to the Python runtime, which
    owns the single source of truth for the routing policy.

    ``-r`` is NOT a role alias anymore (x-f76e reassigned it to ``--resume``);
    role is long-form only here.
    """
    if verb != "spawn":
        return False
    return any(
        a == "--role" or a.startswith("--role=") for a in _args_before_argv(args)
    )


def _is_crown_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` carrying ``--crown`` (bestow-at-spawn).

    ``--crown``/``-k`` is implemented only in the Python spawn path (``cmd_spawn``
    derives the rung from the scope and stamps the crown onto the spawned row).
    The Rust client parses neither spelling, so a crown-bearing spawn that
    auto-routed to the binary would exit with ``unknown flag`` - the documented
    grammar reachable only from the path the default route never reaches. Same
    shape and reason as ``--role`` above. Detected here so the call falls through
    to the Python runtime that owns the implementation.

    BOTH spellings must be listed. The short form is not cosmetic: it is the one
    the docs teach for a portfolio (``-k etl -k web``), so a detector that knew
    only the long form would route exactly the multi-scope case into a binary
    that cannot parse it.

    Load-bearing on ``--substrate bg``, where it is what makes the crown land at
    all: bg spawns otherwise exec the binary. The pane substrate diverts on its
    own via ``_is_pane_substrate_spawn``.
    """
    if verb != "spawn":
        return False
    return any(
        a in ("--crown", "-k") or a.startswith(("--crown=", "-k="))
        for a in _args_before_argv(args)
    )


def _is_monitor_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """Keep ``--monitor`` on the Python path that owns its pane-only guards."""
    if verb != "spawn":
        return False
    return any(
        a == "--monitor" or a.startswith("--monitor=")
        for a in _args_before_argv(args)
    )


def _is_dispatch_account_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` carrying ``--dispatch-account`` (the cutover carrier).

    The Rust spawn parser does not know the flag and exits ``unknown flag:
    --dispatch-account``, killing the launch. Python owns it: it resolves the
    destination record's env overlay and refuses a harness mismatch. Keeping the
    decision HERE rather than at one caller is the point - a shell dispatcher
    pinning its own runtime protects only itself, and the next caller to reach
    for the flag would hit the binary.
    """
    if verb != "spawn":
        return False
    return any(
        a == "--dispatch-account" or a.startswith("--dispatch-account=")
        for a in args
    )


def _is_resume_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` carrying ``--resume`` / ``-r`` (x-f76e / x-9844).

    The front-door normalizer rewrites ``-r <id>`` into ``--resume <full-uuid>``,
    and the Rust spawn parser does not (yet) know ``--resume``, so a resume-bearing
    spawn that auto-routed to the binary would exit ``unknown flag: --resume``.
    Keeping it Python routes it to ``cmd_spawn``, which owns the bg-thread revival
    lane. (``-r`` is matched too for a pre-normalization raw argv.)
    """
    if verb != "spawn":
        return False
    return any(
        a in ("--resume", "-r")
        or a.startswith("--resume=")
        or a.startswith("-r=")
        for a in _args_before_argv(args)
    )


def _is_route_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` carrying ``--route`` (x-b0b4) or its decomposed
    spelling ``--provider``/``-P`` (the model-vendor axis, which ``cmd_spawn``
    folds into the same route with ``--model``).

    Routing is parsed only by the Python spawn path (``cmd_spawn`` resolves +
    fail-closes it via ``resolve_explicit_route``). The Rust client does not
    materialize a route, so a routed spawn auto-routed to the binary would either
    exit ``unknown flag: --route`` or silently launch on the primary model."""
    if verb != "spawn":
        return False
    return any(
        a in ("--route", "--provider", "-P")
        or a.startswith(("--route=", "--provider="))
        for a in _args_before_argv(args)
    )


#: Flags that compose a COMPLETE route (endpoint + auth + model) before any
#: worker is born, so an inherited tier remap is no longer ambiguous.
#: ``-P``/``--provider``/``--route`` are fail-CLOSED at the CLI (an unresolvable
#: route exits 2 before spawning) and ``--account`` is resolved and scrubbed by
#: :func:`_scrub_account_auth_at_seam`.
#:
#: ``--role`` is deliberately NOT here: `resolve_route` is fail-SAFE, returning
#: None for a protected role, a disabled block, an unconfigured provider, or a
#: missing key, which leaves the ambient environment untouched. A role is
#: exempted below only once it resolves to a real route.
_ROUTE_NAMING_FLAGS = ("--route", "--provider", "-P", "--account")


def _spawn_flag_value(args: Sequence[str], *names: str) -> Optional[str]:
    """The value of the first of ``names`` present in ``args`` (``--f v`` or
    ``--f=v``), or None. Scans only the fno-arg head, like every other flag scan
    here, so a payload token can never masquerade as our flag."""
    head = list(_args_before_argv(args))
    for i, a in enumerate(head):
        if a in names:
            return head[i + 1] if i + 1 < len(head) else ""
        for n in names:
            if a.startswith(f"{n}="):
                return a.split("=", 1)[1]
    return None


def inherited_tier_remap(
    args: Sequence[str], env: Optional[Mapping[str, str]] = None
) -> Optional[tuple[str, str]]:
    """Argv adapter over :func:`model_routing.tier_remap_conflict`, returning
    ``(alias, remapped_model)`` for a spawn whose ``--model`` tier alias the
    ambient env silently redefines, or None.

    The shared checker owns the invariant; this only maps CLI flags onto it so
    the same rule is enforced at the seam and at the in-process spawn APIs.
    """
    from fno.agents.model_routing import resolve_route, tier_remap_conflict

    head = list(_args_before_argv(args))
    if any(
        a in _ROUTE_NAMING_FLAGS
        or a.startswith(("--route=", "--provider=", "--account="))
        for a in head
    ):
        return None
    # The remap vars are claude-only; an explicit non-claude harness is unaffected.
    harness = (_spawn_flag_value(args, "--harness", "-H") or "claude").strip().lower()
    if harness != "claude":
        return None
    found = tier_remap_conflict(_spawn_flag_value(args, "--model", "-m"), env)
    if found is None:
        return None
    role = _spawn_flag_value(args, "--role")
    if role and resolve_route(role, env=env):
        return None
    return found


def _pick_account_at_seam(args: Sequence[str]) -> list[str]:
    """Inject a headroom-picked ``--account`` for a spawn that named none.

    Runs at the SAME seam as the auth scrub below, for the same reason: a
    bg/headless spawn auto-routes to the Rust client through an ``exec``, so
    neither Python spawn seam is ever reached. A picker wired only there would
    leave every such worker on the ambient - possibly exhausted - account while
    ``pick_on_launch`` read enabled, which is the decorative-guard shape this
    repo's corpus names. Injecting the flag here is the one edit BOTH runtimes
    see, and the scrub below then applies the account's overlay exactly as it
    does for an explicit ``--account``.

    Six spawns are left alone: one that already named an account (explicit
    intent always wins), one carrying ``--role`` or ``--route``/``--provider``
    (the CLI refuses ``--account`` alongside either, because the route's
    ANTHROPIC_* would override the account's CLAUDE_CONFIG_DIR and mis-bill),
    one carrying ``--dispatch-account`` (a quota cutover already selected the
    destination account, and a second pick merges two overlays the same way),
    one carrying ``--resume`` (see below), and one pinned to a non-claude
    harness (``--account`` is claude-only).
    """
    out = list(args)
    if _spawn_flag_value(out, "--account") is not None:
        return out
    if _is_role_bearing_spawn("spawn", out) or _is_route_bearing_spawn("spawn", out):
        return out
    if _is_resume_bearing_spawn("spawn", out):
        # A revive continues an EXISTING transcript, and that transcript lives
        # under the config dir it was created in - so an injected --account
        # points CLAUDE_CONFIG_DIR at a directory where the uuid does not exist.
        # It also collides with the recorded-route restore in `dispatch_spawn`,
        # which reads any --account as operator intent and refuses the pair:
        # the operator would be told to drop a flag they never typed. Nothing to
        # pick - the transcript already decided where it lives.
        return out
    if _is_dispatch_account_bearing_spawn("spawn", out):
        # A cutover already SELECTED its account, and picking a second one here
        # merges two overlays: the destination's config_dir with the picked
        # account's api key, which is the mis-bill this function already refuses
        # for --route. Nothing to pick - the selector decided.
        return out
    harness = _spawn_flag_value(out, "--harness", "-H")
    if harness not in (None, "", "claude"):
        return out
    try:
        from fno.agents.dispatch import pick_account_id

        picked = pick_account_id()
    except Exception:  # noqa: BLE001 - picking is advisory; never block a spawn
        return out
    if not picked:
        return out
    return [*out, "--account", picked]


def _scrub_account_auth_at_seam(args: Sequence[str]) -> None:
    """Drop inherited vendor auth/model vars from ``os.environ`` for an
    ``--account`` spawn, at the seam, so the Rust client inherits the scrub too.

    The three Python substrate seams already scrub ``SCRUB_AUTH_VARS`` before
    layering the account overlay, but an ``--account`` spawn on ``--substrate
    bg`` auto-routes to the Rust client, which has no ``ANTHROPIC_*`` handling
    at all -- so the Python scrub never ran for it and the pinned account
    inherited the parent's vendor endpoint and tier remaps. ``route_to_rust``
    execs with ``os.environ``, so scrubbing here is the one edit both runtimes
    see. The account overlay is still layered per-substrate afterwards, so an
    account record that legitimately pins any of these still wins. A
    route-bearing ``--account`` spawn skips this scrub entirely: routing is
    Python-only and the create lane scrubs + applies route-wins itself, and a
    vendor may name a scrubbed var (e.g. ``ANTHROPIC_AUTH_TOKEN``) as its
    ``api_key_env``, so scrubbing first would strip the key before
    ``resolve_explicit_route`` reads it.

    Order is load-bearing: the overlay is resolved BEFORE the scrub. An api_key
    record may reference the ambient environment (``ANTHROPIC_API_KEY =
    "${ENV:ANTHROPIC_API_KEY}"``) and ``resolve_env_value`` reads
    ``os.environ``, so scrubbing first would delete the source value and make a
    previously valid ``--account`` spawn unresolvable. Resolve, then scrub, then
    re-apply the resolved values.

    A resolution failure leaves the environment untouched and returns: the
    downstream ``resolve_account_overlay_or_exit`` owns that refusal receipt,
    and this seam must not pre-empt it with a different error.
    """
    account = _spawn_flag_value(args, "--account")
    if not account:
        return
    if _is_route_bearing_spawn("spawn", args):
        return
    from fno.agents import account_env as _account_env

    try:
        overlay = _account_env.resolve_account_overlay(account)
    except Exception:
        return
    if overlay is None:
        return
    for var in _account_env.SCRUB_AUTH_VARS:
        os.environ.pop(var, None)
    os.environ.update(overlay.env)


def _refuse_inherited_tier_remap(args: Sequence[str]) -> None:
    """Exit 2 rather than launch a worker whose model alias the ambient env
    redefines. Runs at the routing seam, BEFORE the Rust/Python fork, so this
    one call covers both runtimes (the Rust client has no ANTHROPIC_* handling
    at all, so a Python-only guard would be decorative). The in-process spawn
    APIs enforce the same invariant via ``check_spawn_tier_remap``."""
    from fno.agents.model_routing import RouteCompositionError

    try:
        found = inherited_tier_remap(args)
    except RouteCompositionError as exc:
        print(f"fno agents spawn: {exc}", file=sys.stderr)
        raise SystemExit(2) from exc
    if found is None:
        return
    from fno.agents.model_routing import remap_conflict_message

    print(f"fno agents spawn: {remap_conflict_message(*found)}", file=sys.stderr)
    raise SystemExit(2)


def env_scrub_spawn_warning(
    args: Sequence[str], env: Optional[Mapping[str, str]] = None
) -> Optional[str]:
    """Argv adapter over :func:`model_routing.env_scrub_warning` for the CLI
    spawn seam, returning the warning when the spawn pins a permission mode
    under CLAUDE_CODE_SUBPROCESS_ENV_SCRUB, else None.

    Resolves the harness the way ``spawn`` does: an explicit ``-H`` wins, else
    the invoking harness (``CODEX_THREAD_ID`` -> codex, ...), else claude. The
    seam runs BEFORE Typer resolves the option default, so resolving it here
    avoids a false positive on a non-claude invoking session. Reuses
    ``spawn_defaults._has_permission_mode`` (the one knob the spawn parser
    treats as mutually exclusive, including ``--yolo``) rather than a parallel
    flag scan.
    """
    explicit = _spawn_flag_value(args, "--harness", "-H")
    if explicit:
        provider = explicit
    else:
        from fno.harness_identity import resolve_harness_identity

        provider = resolve_harness_identity(env).harness or "claude"
    from fno.agents.model_routing import env_scrub_warning
    from fno.agents.spawn_defaults import _has_permission_mode

    return env_scrub_warning(
        provider,
        permission_pinned=_has_permission_mode(_args_before_argv(args)),
        env=env,
    )


def _warn_env_scrub_spawn(args: Sequence[str]) -> None:
    """Surface the env-scrub warning at the spawn seam, never a refusal.

    Pairs with _refuse_inherited_tier_remap at the same seam (both runtimes see
    this one edit) and runs AFTER it, so a refused spawn stays quiet."""
    msg = env_scrub_spawn_warning(args)
    if msg is not None:
        print(msg, file=sys.stderr)


def _is_provenance_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` carrying ``--node``/``--slug``/``--plan`` (x-84a8).

    Provenance flags are parsed only by the Python spawn verb (``cmd_spawn``
    resolves them into the pane env). The Rust client does not know them, so a
    ``spawn ... --node <id>`` auto-routed to the binary would exit ``unknown
    flag: --node``. Keeping it Python covers every caller (direct CLI and the
    /agent spawn.sh forward) in one place, not just the pane substrate. The
    flags are unused on the Python bg/headless path (provenance rides the pane
    wrapper only), so forcing Python is harmless there."""
    if verb != "spawn":
        return False
    prov = ("--node", "--slug", "--plan")
    return any(
        a in prov or a.startswith(tuple(f"{p}=" for p in prov))
        for a in _args_before_argv(args)
    )


def _is_output_format_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """Keep the internal Claude JSON-envelope flag on the Python spawn path.

    The Rust client does not parse ``--output-format``. PR-watch uses this
    internal option to preserve ``claude -p``'s result envelope across the
    canonical headless spawn, so auto-routing it to Rust would reject the flag
    before any worker starts.
    """
    if verb != "spawn":
        return False
    return any(
        a == "--output-format" or a.startswith("--output-format=")
        for a in _args_before_argv(args)
    )


def route_to_rust(
    args: Sequence[str],
    *,
    binary: Optional[Path] = None,
    _exec: Callable[..., None] = os.execv,
    _resolve: Callable[[], Optional[Path]] = rust_binary.resolve_binary,
    _stderr: Optional[IO[str]] = None,
) -> NoReturn:
    """Exec ``fno-agents`` with ``args`` (the verb + everything after ``fno agents``).

    On success ``os.execv`` replaces the current process, so this never returns.
    Both failure modes raise ``SystemExit(127)`` with an actionable message
    rather than letting a raw "binary not found" / exec error surface as a
    misleading spawn failure (design open-question #11):

    - the binary is absent (``_resolve`` returns ``None``); or
    - the binary resolves but ``os.execv`` fails (``OSError``: TOCTOU delete,
      lost execute bit, incompatible arch, ``ETXTBSY``, ...).

    When ``binary`` is supplied (the ``auto`` path has already resolved an
    installed binary), it is used directly and ``_resolve`` is skipped -- so the
    happy default path never double-resolves and never spuriously hits the
    missing-binary exit. When ``binary`` is ``None`` (the forced ``=rust`` path),
    ``_resolve`` runs and a missing binary is the hard 127 error.

    The ``_exec`` / ``_resolve`` / ``_stderr`` hooks exist purely so the decision
    logic is unit-testable without actually replacing the test process.
    """
    err = _stderr if _stderr is not None else sys.stderr
    if binary is None:
        binary = _resolve()
    if binary is None:
        print(
            f"fno agents: {RUNTIME_ENV}=rust is set but the '{rust_binary.BINARY_NAME}' binary "
            f"was not found (looked at ${rust_binary.BINARY_ENV}, in the bundled "
            "wheel dir, beside the launcher, on PATH, and in the cargo dev target; "
            "a file present but not executable is also skipped - try `chmod +x`). "
            "Get it via `pip install fno` (bundled wheel), `cargo install fno-agents`, "
            f"or `cargo build --release -p fno-agents` plus "
            f"`export {rust_binary.BINARY_ENV}=<path>`.",
            file=err,
        )
        raise SystemExit(BIN_NOT_FOUND_EXIT)
    argv = [str(binary), *args]
    try:
        _exec(str(binary), argv)
    except OSError as exc:
        # execv raises (never returns) on failure. Convert to the same legible
        # surface as the missing-binary case instead of a raw traceback.
        print(f"fno agents: failed to exec '{binary}': {exc}", file=err)
        raise SystemExit(BIN_NOT_FOUND_EXIT) from exc
    # Only reached if a stubbed _exec returns (real os.execv never does).
    raise SystemExit(1)  # pragma: no cover


def _make_rust_only_command(
    verb: str, help_text: str, *, hidden: bool = False
) -> "click.Command":
    """A placeholder Click command for a Rust-only verb, used for help + fallback.

    The happy path never runs this body: :meth:`_AgentsRuntimeGroup.make_context`
    execs the ``fno-agents`` binary before Click resolves the sub-command whenever
    the verb auto-routes and an installed binary is present. This command exists
    so the verb (a) appears in ``fno agents --help`` with a description and (b)
    degrades to a legible message instead of a bare "No such command" when it is
    reached -- i.e. under ``FNO_AGENTS_RUNTIME=python`` (no Python implementation
    exists for these verbs) or in a checkout with no *installed* binary.

    ``hidden`` (x-71b6 tiering) keeps the verb invocable but off the advertised
    ``fno agents --help`` listing - the display-only counterpart of the Python
    commands' ``@agents_app.command(..., hidden=True)``.
    """
    import click

    @click.command(
        name=verb,
        help=f"{help_text} (Rust runtime).",
        hidden=hidden,
        # Don't choke on the verb's real flags before we print the message --
        # we never act on them here, but a bare "no such option" would bury it.
        context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
        add_help_option=True,
    )
    def _placeholder() -> NoReturn:
        if runtime_mode() == "python":
            print(
                f"fno agents {verb}: no Python implementation -- this verb runs only on "
                f"the '{rust_binary.BINARY_NAME}' Rust runtime. Unset {RUNTIME_ENV} (auto) with the "
                f"binary installed, or set {RUNTIME_ENV}=rust to use a local cargo build.",
                file=sys.stderr,
            )
        else:
            print(
                f"fno agents {verb}: requires the '{rust_binary.BINARY_NAME}' Rust runtime, which was "
                "not found (bundled wheel dir, launcher sibling, PATH -- the default "
                f"lookup deliberately ignores ${rust_binary.BINARY_ENV}). Get it via "
                f"`pip install fno` (bundled), `cargo install fno-agents`, or set "
                f"{RUNTIME_ENV}=rust (which does honor ${rust_binary.BINARY_ENV}) to use "
                "a local `cargo build --release -p fno-agents`.",
                file=sys.stderr,
            )
        raise SystemExit(BIN_NOT_FOUND_EXIT)

    return _placeholder


def _make_retired_command(verb: str, pointer: str) -> "click.Command":
    """A Click command for a verb retired at G4 (x-f54c): print a one-line mux
    pointer to stderr and exit non-zero, never a silent no-op (AC5-EDGE)."""
    import click

    @click.command(
        name=verb,
        help=f"(retired at G4) {pointer}",
        context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
        add_help_option=True,
    )
    def _retired() -> NoReturn:
        print(f"fno agents {verb} was retired at G4: {pointer}", file=sys.stderr)
        raise SystemExit(2)

    return _retired


def make_agents_group_cls() -> type:
    """Build the TyperGroup subclass that short-circuits to the Rust binary.

    Returned (not module-level) so importing this module never imports
    ``typer.core`` unless the agents sub-app is actually constructed — keeps the
    lazy-import startup budget intact.
    """
    import typer.core

    class _AgentsRuntimeGroup(typer.core.TyperGroup):
        """Intercept ``fno agents <verb>`` before Typer parses the sub-command.

        Routing follows :func:`runtime_mode`:

        - ``rust``   -- force the binary for every verb (missing binary -> 127).
        - ``python`` -- never touch the binary; defer to Python dispatch.
        - ``auto`` (default) -- Rust is the runtime for the daemon-native verbs
          (:data:`AUTO_ROUTE_VERBS`) when an *installed* binary is present;
          otherwise (a verb with a Python contract, or no installed binary) fall
          through to the mature Python dispatch.

        A bare ``fno agents -h`` / ``--help`` (help as the first token) always
        falls through to the Python group help so the wrapper stays discoverable;
        ``fno agents <verb> --help`` forwards to the binary, which owns that
        verb's help.

        Because that bare ``--help`` renders the *Python* group, it would list
        only the ``@agents_app.command`` verbs and silently omit every Rust-only
        verb (``spawn``/``status``/the ``*-channel`` verbs). :meth:`list_commands` and :meth:`get_command` close
        that gap by injecting the :data:`RUST_ONLY_VERB_HELP` entries into the
        help listing (and into command resolution, for a legible fallback) without
        touching the routing decision in :meth:`make_context`.
        """

        def list_commands(self, ctx):  # type: ignore[no-untyped-def]
            """Python-registered verbs first, then the Rust-only verbs.

            Keeps ``fno agents --help`` complete. The Rust-only names are appended
            (not merged into the Typer registry) so ``agents_app.registered_commands``
            -- the source of truth for "has a Python implementation" -- is unchanged.
            """
            base = list(super().list_commands(ctx))
            seen = set(base)
            return base + [v for v in RUST_ONLY_VERB_HELP if v not in seen]

        def get_command(self, ctx, name):  # type: ignore[no-untyped-def]
            """Resolve Python verbs normally; synthesize the Rust-only ones.

            Only matters for help rendering and the no-route fallback: when a
            Rust-only verb auto-routes with an installed binary, ``make_context``
            execs the binary before Click ever calls this.
            """
            cmd = super().get_command(ctx, name)
            if cmd is not None:
                return cmd
            if name in RUST_ONLY_VERB_HELP:
                return _make_rust_only_command(
                    name,
                    RUST_ONLY_VERB_HELP[name],
                    hidden=name not in RUST_ONLY_ADVERTISED,
                )
            if name in RETIRED_VERB_POINTERS:
                return _make_retired_command(name, RETIRED_VERB_POINTERS[name])
            return None

        # Click's make_context signature carries precise Context types we do not
        # need here; the override just intercepts then delegates verbatim.
        def make_context(self, info_name, args, parent=None, **extra):  # type: ignore[no-untyped-def]
            if args and args[0] not in ("-h", "--help"):
                verb = args[0]
                # config.agents.defaults injection runs at the seam, BEFORE the
                # route/fork, so a bare `spawn` inherits the operator's defaults
                # on both the Rust route and the Python dispatch (x-de9d US8).
                # A bad config never bricks spawning: the helper returns args
                # unchanged on a load failure (an unknown config provider still
                # exits 2 by design).
                if verb == "spawn":
                    from fno.agents.spawn_defaults import inject_spawn_defaults

                    args = inject_spawn_defaults(args)
                    # Same seam, same reason: these must see the post-defaults
                    # args and must cover BOTH runtimes, so they run here rather
                    # than in either spawn implementation. The pick runs FIRST so
                    # the scrub below sees the account it chose and applies that
                    # overlay, exactly as it would for an explicit --account.
                    args = _pick_account_at_seam(args)
                    _scrub_account_auth_at_seam(args)
                    _refuse_inherited_tier_remap(args)
                    # The env-scrub warning is NOT emitted here: a Python-route
                    # spawn falls through to dispatch_spawn / dispatch_spawn_pane,
                    # which emit it, so warning at the seam too would print it
                    # twice. The Rust-exec branches below emit it before they
                    # exec, since the binary never reaches dispatch.
                mode = runtime_mode()
                # A role-bearing spawn (x-d2fe) is Python-only: the Rust client
                # cannot parse --role, so never route it to the binary in any
                # mode; fall through to the Python dispatch that implements it.
                # A pane-substrate spawn (4a-G2, the default) is Python-only the
                # same way: the mux-hosted back half lives in cmd_spawn, and the
                # binary would route it to the retiring daemon PTY host.
                # A provenance-bearing spawn (x-84a8, --node/--slug/--plan) is
                # Python-only for the same reason as --role: the binary cannot
                # parse those flags. A --resume-bearing spawn (x-9844 revive-in-
                # place) is Python-only for the same reason: the Rust spawn
                # parser has no --resume flag, and Python owns the revival.
                py_spawn = (
                    _is_role_bearing_spawn(verb, args)
                    or _is_crown_bearing_spawn(verb, args)
                    or _is_monitor_bearing_spawn(verb, args)
                    or _is_route_bearing_spawn(verb, args)
                    or _is_pane_substrate_spawn(verb, args)
                    or _is_provenance_bearing_spawn(verb, args)
                    or _is_resume_bearing_spawn(verb, args)
                    or _is_output_format_bearing_spawn(verb, args)
                    or _is_dispatch_account_bearing_spawn(verb, args)
                )
                if mode == "rust" and not py_spawn:
                    _warn_env_scrub_spawn(args)  # Rust exec: Python dispatch never runs
                    route_to_rust(list(args))  # execs; does not return
                elif mode == "auto" and verb in AUTO_ROUTE_VERBS and not py_spawn:
                    # Since ab-73da4ac2 this includes ``ask`` for every provider
                    # (the unconditional flip): the Rust client owns the full
                    # create/resume decision and surfaces the unresolvable-create
                    # exit-2 error itself, so there is no provider-conditional
                    # branch anymore.
                    binary = rust_binary.resolve_installed_binary()
                    if binary is not None:
                        _warn_env_scrub_spawn(args)  # Rust exec: Python dispatch never runs
                        route_to_rust(list(args), binary=binary)  # execs
                    # else: no installed binary -> Python dispatch below.
                # mode == "python", or no installed binary -> Python dispatch below.
            return super().make_context(info_name, args, parent=parent, **extra)

    return _AgentsRuntimeGroup
