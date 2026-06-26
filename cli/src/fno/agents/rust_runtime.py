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

Design: ``internal/fno/design/2026-05-22-abi-pty-supervisor-and-drive.md``
(line 136 — "Python ``fno agents <verb>`` is a thin Typer wrapper that execs
``fno-agents <verb>``"). Plan: ``plans/2026-05-25-phase6-w6-distribution.md``.
"""
from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path
from typing import IO, TYPE_CHECKING, Callable, NoReturn, Optional, Sequence

if TYPE_CHECKING:
    import click

#: Name of the Rust client binary on PATH / in the bundled wheel dir. Windows
#: appends .exe (the release matrix stages + bundles `fno-agents.exe` there).
BINARY_NAME = "fno-agents.exe" if os.name == "nt" else "fno-agents"
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
        "drive",
        # Client-side TUI compositor (ab-3c063856). Like ``drive``, it owns
        # the terminal and does not fit the one-shot RPC path; the verb is
        # dispatched directly in ``bin/client.rs`` before ``build_request``.
        # Auto-routes through :data:`AUTO_ROUTE_VERBS` (not in the Python
        # PYTHON_AGENT_VERBS carve-out).
        "grid",
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
        "attach",
        "logs",
        # Interactive-drive agent type (ab-26b5fe82): both call agent.spawn with
        # host_mode=interactive. `host` = fresh interactive codex/gemini; `promote
        # --from <uuid>` = resume an existing session into a drivable TUI.
        "host",
        "promote",
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
        # kill-check` / `fno event verify-evidence` wrappers resolve the binary
        # and invoke these verbs explicitly (not via `fno agents` routing);
        # these entries exist so the client.rs<->router parity test stays in sync.
        "kill-check",
        "verify-evidence",
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
    # G3 (ab-0b16d65c): the `/agents chat` live escalation verb. A pure-Python
    # SYNCHRONOUS orchestrator over the shipped daemon RPCs (agent.spawn adopt +
    # agent.switchboard drive); there is NO `chat` verb on the Rust client, so it
    # must never auto-route to the daemon (a 404 for installed users). Sibling of
    # `send`: Python owns it.
    "chat",
    # x-73cc: the shared bg-dispatch guard verb. Pure-Python orchestration of
    # `fno claim` (Guard 1 node-claim probe + Guard 2 dispatch:<id> reservation)
    # called by both dispatch-node.sh and spawn.sh. There is NO `spawn-guard` on
    # the Rust client, so it must never auto-route to the daemon (it would 404 /
    # be shadowed for installed users). Python owns it.
    "spawn-guard",
    # x-301a: "what is MY registered mesh name?" — reads FNO_AGENT_SELF + the
    # registry, read-only. Pure Python (fno.agents.whoami); there is NO
    # `whoami` on the Rust client, so it must never auto-route to the daemon.
    # Listing it here is defensive/documentary: whoami is not in
    # RUST_CLIENT_VERBS, so AUTO_ROUTE_VERBS already excludes it.
    "whoami",
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
    "drive": "Drive a spawned agent's TUI (--watch / --step / --paranoid).",
    "grid": "Compose several agents into one multi-panel terminal view.",
    "host": "Host a fresh interactive codex/gemini TUI in a drivable worker.",
    "promote": "Promote an existing session (--from <uuid>) into a drivable codex/gemini TUI, or adopt a claude session as a stream-json thread (add --provider claude).",
    "register-channel": "Register a Claude Code session as an agent channel.",
    "unregister-channel": "Unregister an agent channel by id.",
    "push-channel": "Push a message to a registered agent channel.",
    "loop-check": "Stop-hook decision: external-truth done()/backstop check (read-only).",
    "loop": "Unified driver loop: run --driver target|megawalk [options] (step 5).",
    "finalize": "Terminal-only side-effect writer: ledger record + (ship) plan stamp/handoff (step 6).",
    "kill-check": "Evaluate a plan's kill_criteria (folded from kill-criteria.sh); usually via `fno phase kill-check`.",
    "verify-evidence": "Verify subagent/child-promise event evidence (folded from verify-event-evidence.sh); usually via `fno event verify-evidence`.",
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


def _bundled_binary() -> Optional[Path]:
    """The wheel-bundled binary at ``<package>/_bin/fno-agents`` (W6 Wave 3)."""
    bundled = Path(__file__).resolve().parent.parent / "_bin" / BINARY_NAME
    return bundled if bundled.is_file() and os.access(bundled, os.X_OK) else None


def _sibling_binary() -> Optional[Path]:
    """The binary installed next to the running launcher (the wheel scripts dir).

    pip installs both the ``fno`` console script and the bundled ``fno-agents``
    wheel-script into the same bin/ (Scripts/ on Windows). When ``fno`` is invoked
    by absolute path without that dir on ``PATH`` (common in CI / cron wrappers),
    ``shutil.which`` misses the binary even though it sits right beside the
    launcher; this finder catches that case (codex P2 on PR #351).
    """
    launcher = sys.argv[0] if sys.argv else ""
    if not launcher:
        return None
    sibling = Path(launcher).resolve().parent / BINARY_NAME
    return sibling if sibling.is_file() and os.access(sibling, os.X_OK) else None


def _path_binary() -> Optional[Path]:
    """The binary as resolved on ``PATH`` (``cargo install`` / GH release / wheel script)."""
    found = shutil.which(BINARY_NAME)
    return Path(found) if found else None


def _cargo_dev_binary() -> Optional[Path]:
    """Dev fallback: a ``cargo build --release`` artifact under the repo tree.

    ``__file__`` is ``cli/src/fno/agents/rust_runtime.py`` so the repo root
    is ``parents[4]``. Checks both a crate-local ``target/`` and a workspace
    ``target/`` so it works whether or not a workspace is introduced later.
    """
    here = Path(__file__).resolve()
    try:
        repo_root = here.parents[4]
    except IndexError:  # installed shallower than a dev checkout
        return None
    # Only meaningful in a development checkout. When the package is installed
    # into site-packages, parents[4] is some unrelated ancestor; refuse to
    # traverse it so we never return a coincidental wrong binary.
    if not (repo_root / "Cargo.toml").exists() and not (repo_root / "crates").is_dir():
        return None
    candidates = (
        repo_root / "crates" / "fno-agents" / "target" / "release" / BINARY_NAME,
        repo_root / "target" / "release" / BINARY_NAME,
    )
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def resolve_binary() -> Optional[Path]:
    """Locate ``fno-agents``: bundled wheel dir -> PATH -> cargo dev target.

    Bundled wins so a ``pip install fno`` wheel is self-contained even when
    a different (older) ``fno-agents`` happens to be on PATH. The launcher-sibling
    lookup sits ahead of PATH so an abs-path ``fno`` invocation still resolves the
    co-installed binary.
    """
    for finder in (_bundled_binary, _sibling_binary, _path_binary, _cargo_dev_binary):
        found = finder()
        if found is not None:
            return found
    return None


def resolve_installed_binary() -> Optional[Path]:
    """Locate an *installed* ``fno-agents`` (bundled wheel dir -> launcher sibling
    -> PATH), deliberately excluding the cargo dev target.

    The ``auto`` (default) runtime uses this narrower set so a *development*
    checkout -- where only ``crates/fno-agents/target/release`` exists -- stays on
    the Python dispatch by default, and the in-process test suite never execs the
    binary. A dev who wants Rust opts in explicitly with ``FNO_AGENTS_RUNTIME=rust``,
    which routes through the full :func:`resolve_binary` (cargo dev included).
    """
    for finder in (_bundled_binary, _sibling_binary, _path_binary):
        found = finder()
        if found is not None:
            return found
    return None


def _is_role_bearing_spawn(verb: str, args: Sequence[str]) -> bool:
    """True for a ``spawn`` carrying ``--role`` (x-d2fe).

    Role-based model routing is implemented only in the Python spawn path
    (``cmd_spawn`` -> ``bg_create`` resolves the per-spawn env). The Rust
    client does not parse ``--role``, so a ``spawn ... --role <r>`` that
    auto-routed to the binary would exit with ``unknown flag: --role``.
    Detecting it here lets the call fall through to the Python runtime, which
    owns the single source of truth for the routing policy.
    """
    if verb != "spawn":
        return False
    return any(a == "--role" or a.startswith("--role=") for a in args)


def route_to_rust(
    args: Sequence[str],
    *,
    binary: Optional[Path] = None,
    _exec: Callable[..., None] = os.execv,
    _resolve: Callable[[], Optional[Path]] = resolve_binary,
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
            f"fno agents: {RUNTIME_ENV}=rust is set but the '{BINARY_NAME}' binary "
            "was not found (looked in the bundled wheel dir, on PATH, and in the "
            "cargo dev target; a file present but not executable is also skipped - "
            "try `chmod +x`). Get it via `pip install fno` (bundled wheel), "
            "`cargo install fno-agents`, or `cargo build --release -p fno-agents`.",
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


def _make_rust_only_command(verb: str, help_text: str) -> "click.Command":
    """A placeholder Click command for a Rust-only verb, used for help + fallback.

    The happy path never runs this body: :meth:`_AgentsRuntimeGroup.make_context`
    execs the ``fno-agents`` binary before Click resolves the sub-command whenever
    the verb auto-routes and an installed binary is present. This command exists
    so the verb (a) appears in ``fno agents --help`` with a description and (b)
    degrades to a legible message instead of a bare "No such command" when it is
    reached -- i.e. under ``FNO_AGENTS_RUNTIME=python`` (no Python implementation
    exists for these verbs) or in a checkout with no *installed* binary.
    """
    import click

    @click.command(
        name=verb,
        help=f"{help_text} (Rust runtime).",
        # Don't choke on the verb's real flags before we print the message --
        # we never act on them here, but a bare "no such option" would bury it.
        context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
        add_help_option=True,
    )
    def _placeholder() -> NoReturn:
        if runtime_mode() == "python":
            print(
                f"fno agents {verb}: no Python implementation -- this verb runs only on "
                f"the '{BINARY_NAME}' Rust runtime. Unset {RUNTIME_ENV} (auto) with the "
                f"binary installed, or set {RUNTIME_ENV}=rust to use a local cargo build.",
                file=sys.stderr,
            )
        else:
            print(
                f"fno agents {verb}: requires the '{BINARY_NAME}' Rust runtime, which was "
                "not found (bundled wheel dir, launcher sibling, PATH). Get it via "
                f"`pip install fno` (bundled), `cargo install fno-agents`, or set "
                f"{RUNTIME_ENV}=rust to use a local `cargo build --release -p fno-agents`.",
                file=sys.stderr,
            )
        raise SystemExit(BIN_NOT_FOUND_EXIT)

    return _placeholder


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
        verb (``spawn``/``status``/``drive``/``grid``/``host``/``promote``/the
        ``*-channel`` verbs). :meth:`list_commands` and :meth:`get_command` close
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
                return _make_rust_only_command(name, RUST_ONLY_VERB_HELP[name])
            return None

        # Click's make_context signature carries precise Context types we do not
        # need here; the override just intercepts then delegates verbatim.
        def make_context(self, info_name, args, parent=None, **extra):  # type: ignore[no-untyped-def]
            if args and args[0] not in ("-h", "--help"):
                verb = args[0]
                mode = runtime_mode()
                # A role-bearing spawn (x-d2fe) is Python-only: the Rust client
                # cannot parse --role, so never route it to the binary in any
                # mode; fall through to the Python dispatch that implements it.
                role_spawn = _is_role_bearing_spawn(verb, args)
                if mode == "rust" and not role_spawn:
                    route_to_rust(list(args))  # execs; does not return
                elif mode == "auto" and verb in AUTO_ROUTE_VERBS and not role_spawn:
                    # Since ab-73da4ac2 this includes ``ask`` for every provider
                    # (the unconditional flip): the Rust client owns the full
                    # create/resume decision and surfaces the unresolvable-create
                    # exit-2 error itself, so there is no provider-conditional
                    # branch anymore.
                    binary = resolve_installed_binary()
                    if binary is not None:
                        route_to_rust(list(args), binary=binary)  # execs
                    # else: no installed binary -> Python dispatch below.
                # mode == "python", or no installed binary -> Python dispatch below.
            return super().make_context(info_name, args, parent=parent, **extra)

    return _AgentsRuntimeGroup
