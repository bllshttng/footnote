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
from typing import NoReturn

import typer

from fno.agents.rust_runtime import make_agents_group_cls

agents_app = typer.Typer(
    name="agents",
    help=(
        "Cross-CLI agent lifecycle (claude / codex / gemini): "
        "spawn / chat / watch / list / logs / stop. "
        "To message a peer, use `fno mail send <name>` (or the `/mail` skill)."
    ),
    no_args_is_help=True,
    # Default Rust runtime (Phase 6 W6 / cv-d28b266a): by default this group
    # execs the installed `fno-agents` binary for the verbs it implements, and
    # falls back to the Python dispatch below otherwise. FNO_AGENTS_RUNTIME=rust
    # forces the binary; =python forces this Python path. See rust_runtime.py.
    cls=make_agents_group_cls(),
)


class AgentStatusFilter(str, enum.Enum):
    """Enum of registry status values accepted by ``list --status``.

    Mirrors :data:`fno.agents.registry.KNOWN_STATUSES` exactly so
    Typer rejects unknown values at parse time with an allowed-values
    list (AC3-ERR). registry.status is a projection of state.status, so
    this is the full snake_case AgentStatus vocabulary (mirrors the Rust
    ``AgentStatus`` enum). The import-time assertion below converts that
    invariant into mechanical enforcement so the two definitions cannot
    drift.
    """

    spawning = "spawning"
    ready = "ready"
    idle = "idle"
    busy = "busy"
    live = "live"
    restarting = "restarting"
    orphaned = "orphaned"
    failed = "failed"
    exited = "exited"
    permanent_dead = "permanent_dead"


# Defense against silent drift: KNOWN_STATUSES is the registry's truth
# and AgentStatusFilter is the CLI's mirror. If a future schema bump
# adds a status without updating the enum, this assertion crashes at
# import time with an actionable message rather than letting the CLI
# silently reject filters that the registry now accepts.
from fno.agents.registry import KNOWN_STATUSES as _KNOWN_STATUSES  # noqa: E402

assert {member.value for member in AgentStatusFilter} == set(_KNOWN_STATUSES), (
    "AgentStatusFilter is out of sync with registry.KNOWN_STATUSES; "
    "update both when adding a new agent status."
)


def _resolve_dispatch_workdir(cwd: str | None, fresh: bool, here: bool) -> Path:
    """Worker launch dir honoring --cwd > --fresh > caller cwd.

    Mirrors the Rust client's ``effective_worker_cwd`` precedence (ab-77b691dc,
    AC6): an explicit ``--cwd`` always wins; ``--fresh`` resolves the canonical
    (main) checkout so a worker dispatched from a linked worktree starts from
    canonical; ``--here``/``--in-place`` suppresses ``--fresh``. A canonical that
    lands on the caller's own dir is a no-op (no redirect note). Only the Python
    fallback runtime reaches this -- when an installed binary auto-routes the
    verb, the Rust client owns the identical precedence.
    """
    if cwd:
        return Path(cwd).resolve()
    caller = Path(os.getcwd()).resolve()
    if fresh and not here:
        from fno.paths import resolve_canonical_repo_root

        # Best-effort: any resolution error (missing git, odd environment) falls
        # back to the caller cwd, the safe side, rather than crashing the dispatch
        # (review MEDIUM).
        try:
            canonical = resolve_canonical_repo_root().resolve()
        except Exception:
            return caller
        if canonical != caller:
            print(
                f"fno agents: --fresh: dispatching from canonical main ({canonical}); "
                "pass --here to stay in this worktree",
                file=sys.stderr,
            )
        return canonical
    return caller


# ---------------------------------------------------------------------------
# Group 2, Task 4.3: `fno agents watch` — observe a held stream-json thread
# ---------------------------------------------------------------------------


def _agents_home_dir() -> Path:
    """The agents home (mirrors dispatch._daemon_rpc resolution)."""
    env = os.environ.get("FNO_AGENTS_HOME")
    if env:
        return Path(env)
    return Path(os.path.expanduser("~")) / ".fno" / "agents"


def _resolve_stream_short_id(name: str) -> "str | None":
    """Resolve an agent name to its worker short_id via the RAW registry.json.

    The parsed Python ``AgentEntry`` drops ``short_id`` (the worker-socket key),
    so read the on-disk registry directly. Returns None when absent/unreadable.
    """
    reg = _agents_home_dir() / "registry.json"
    try:
        data = json.loads(reg.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    # The on-disk top-level key is `agents` (the Rust `entries` field serializes
    # under that name); accept `entries` too for forward/backward safety.
    rows = data.get("agents") or data.get("entries") or []
    for e in rows:
        if isinstance(e, dict) and e.get("name") == name:
            return e.get("short_id")
    return None


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
    short_id = _resolve_stream_short_id(name)
    if short_id is None:
        print(
            f"fno agents watch: no agent named {name!r} in the registry",
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


@agents_app.command("spawn")
def cmd_spawn(
    name: str = typer.Argument(..., help="Agent name."),
    message: str = typer.Argument("", help="Initial message (optional; empty string if omitted)."),
    provider: str | None = typer.Option(
        None,
        "--provider",
        "-p",
        help=(
            "claude | codex | gemini (optional). Defaults to the invoking "
            "harness, then claude. An explicit value wins."
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
        "-H",
        help=(
            "Shortcut for --substrate headless: a one-shot (-p/--exec) worker. "
            "Mobile-friendly (no '--substrate' to type; -H is one hyphen). "
            "Wins over --substrate; equivalent to the legacy --once/-o."
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
            "For claude: no-op with a stderr note."
        ),
    ),
    fresh: bool = typer.Option(
        False,
        "--fresh",
        help=(
            "Resolve the worker cwd to the canonical (main) repo root regardless "
            "of caller cwd. Opt-in; an explicit --cwd still wins."
        ),
    ),
    here: bool = typer.Option(
        False,
        "--here",
        "--in-place",
        help="Opt out of --fresh: keep the worker in the caller's cwd.",
    ),
    role: str | None = typer.Option(
        None,
        "--role",
        "-r",
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
            "zai/glm-5.2; legacy comma zai,glm-5.2 also accepted). Bypasses the "
            "--role table and guard (explicit intent "
            "is not auto-routing) and wins over any configured lane. FAILS CLOSED: "
            "an unknown provider, non-anthropic protocol, or missing key refuses "
            "the spawn - never a silent primary-model launch. claude only."
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
            "Unset = provider default; opencode defaults to z-ai/glm-5.2."
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
        help=(
            "Resume an existing claude session UUID instead of starting fresh: "
            "the new --bg supervisor continues that transcript (US4 bg-thread "
            "revival). claude + --substrate bg only; forwarded as --resume <uuid>."
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
    squad: str | None = typer.Option(
        None,
        "--squad",
        "-s",
        help=(
            "Pane placement (x-3e38): send the new pane to a squad by its visible "
            "workspace name instead of the cwd-derived default. --substrate pane only."
        ),
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
    \"provider\": \"claude\", \"status\": \"live\"}.

    codex/gemini --once: creates + exchanges + tears down the registry
    row. stdout = provider reply verbatim. stderr = teardown receipt.

    Plain spawn for codex/gemini (no --once) requires the fno-agents daemon
    (Rust runtime); this Python path exits 13 with guidance.
    """
    from fno.agents.dispatch import DispatchAskError, SpawnResult, dispatch_spawn
    from fno.agents.provider_resolve import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )

    workdir = _resolve_dispatch_workdir(cwd, fresh, here)

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
    # Provenance rides the pane receipt's provider_source field below (the
    # default substrate). The bg/once stdout receipts stay byte-parity-locked
    # with the Rust client, so they don't carry it.

    # x-2c27 named the substrate axis; 4a-G2 retargeted its default: `pane`
    # is mux-hosted and Python OWNS that back half (rust_runtime carves pane
    # spawns out of the binary route), `bg`/`headless` keep their existing
    # lanes. Validate to parity with the Rust client (exit 2 on a bad value);
    # headless still maps onto the `once` lever.
    # --headless / -H is the ergonomic shortcut for --substrate headless (x-c772,
    # mobile: no '--substrate' to type). It wins over an explicit --substrate so
    # `-H` alone always resolves to the one-shot lane.
    if headless:
        substrate = "headless"
    if substrate not in ("pane", "bg", "headless"):
        print(
            f"--substrate must be one of: pane, bg, headless (got {substrate})",
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
    placement_requested = squad is not None or split is not None
    if squad is not None and not squad.strip():
        print("--squad/-s needs a nonblank squad name", file=sys.stderr)
        raise typer.Exit(code=2)
    if placement_requested and (substrate != "pane" or once):
        print(
            "--squad/-s and --split/-x apply only to --substrate pane (bg/headless have "
            "no pane geometry)",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if split is not None and split not in ("left", "right", "up", "down"):
        print(
            f"--split/-x must be left, right, up, or down (got {split!r})",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # Explicit --route override (x-b0b4). Resolve + FAIL CLOSED here, BEFORE the
    # gate, so a refusal spawns nothing, acquires no gate slot, and leaves the
    # node dispatchable. resolve_explicit_route bypasses the role table + guard
    # (explicit intent) and returns None for unknown/non-anthropic/keyless - which
    # for --route is a hard refusal, not the role lane's silent fallback.
    route_env: dict[str, str] | None = None
    if route is not None:
        if provider != "claude" or substrate != "bg":
            print(
                "--route is claude + --substrate bg only (the delivery-dispatch "
                f"lane); got provider {provider!r} substrate {substrate!r}.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        from fno.agents.model_routing import _parse_target, resolve_explicit_route

        parsed = _parse_target(route)
        if parsed is None:
            print(
                f"--route must be 'provider,model' with a non-empty model token; "
                f"got {route!r}",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        notes: list[str] = []
        route_env = resolve_explicit_route(
            parsed[0], parsed[1], notice=notes.append
        )
        if not route_env:
            reason = "; ".join(notes) or "provider unknown, non-anthropic, or keyless"
            print(
                f"--route {route!r} refused ({reason}); no worker launched, node "
                "stays dispatchable.",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)

    # Spawn gate (x-c5cc): cap + RAM floor at the top of the primitive, before
    # the substrate fan-out. This Python gate is the SOLE gate on every path
    # that reaches cmd_spawn (the front door execs the binary for bg/headless,
    # so those normally gate in Rust; the Rust pane arm re-execs back here) —
    # exactly one gate evaluation per spawn (LD1). `--once` is the
    # pre-substrate spelling of a headless one-shot, so it gates as headless.
    from fno.agents.spawn_gate import run_gate

    gate = run_gate(
        name,
        "headless" if (once or substrate == "headless") else substrate,
        force=force,
        no_wait=no_wait,
    )

    # `--once` is the pre-substrate spelling of headless (the Rust client maps
    # it to --substrate headless): it always means a one-shot, never a pane.
    try:
        if substrate == "pane" and not once:
            from fno.agents.mux_spawn import dispatch_spawn_pane, resolve_provenance

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
                    provenance=resolve_provenance(node, slug, plan),
                )
            except DispatchAskError as exc:
                print(str(exc), file=sys.stderr)
                raise typer.Exit(code=exc.exit_code) from exc
            # Compact one-line receipt, superset of the daemon-spawn receipt shape
            # ({"name","short_id","provider","status"}) so line-parsing consumers
            # keep working; short_id is empty (a mux row has no worker socket).
            receipt_obj = {
                "name": pane_result.name,
                "short_id": "",
                "provider": pane_result.provider,
                "provider_source": provider_source,
                "status": "live",
                "mux_session": pane_result.session,
                "pane_id": pane_result.pane_id,
            }
            # Locked Decision 5: name the applied mode so an audit of "why did
            # this worker have edit rights" has a durable answer. Only when set,
            # so the unset receipt is unchanged.
            if permission_mode is not None:
                receipt_obj["permission_mode"] = permission_mode
            receipt = json.dumps(receipt_obj)
            sys.stdout.write(receipt + "\n")
            sys.stdout.flush()
            return
        if substrate == "headless":
            once = True

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
                resume_session_id=resume,
            )
        except DispatchAskError as exc:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc
    finally:
        # Release the gate's claims once the dispatch result exists (or the
        # spawn failed): registry/roster rows carry the count from here.
        gate.release()

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
        receipt = (
            f'{{"name": "{safe_name}", "short_id": "{result.short_id}", '
            f'"provider": "{result.provider}", "status": "live"{perm_field}}}'
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
    object) in ``{dispatchable, already-running, corrupted, error}``:

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
    - corrupted       the ``node:<id>`` claim is corrupted; launch nothing.
    - error           the claim probe failed or the reservation could not be
                      acquired (fail-closed); launch nothing.

    Exit 0 for every clean verdict (incl. already-running and corrupted). Exit
    non-zero ONLY for a usage error or a fail-closed guard error (verdict=error),
    so a stale ``fno`` without this verb (Typer "No such command") also fails
    closed in the caller.
    """
    from fno.claims.cli import _parse_ttl
    from fno.claims.core import ClaimHeldByOther, acquire_claim, claim_status

    def _root_for(key: str):
        # Delegate to the shared routing rule (fno.claims.io.claims_root_for):
        # node:/dispatch:/reconcile: (global-id kinds) live in the global root,
        # so spawn-guard dedups dispatch:<id> against advance/reconcile across
        # repos; repo-local keys keep the cwd/env default.
        from fno.claims.io import claims_root_for

        return claims_root_for(key)

    node_key = f"node:{node_id}"
    res_key = f"dispatch:{node_id}"

    def _emit(verdict: str, *, exit_code: int = 0, **fields: "str | None") -> "NoReturn":
        obj: dict[str, str] = {"verdict": verdict}
        for k, v in fields.items():
            if v is not None:
                obj[k] = v
        if json_output:
            line = json.dumps(obj)
        else:
            parts = [f"verdict={verdict}"]
            for k, v in fields.items():
                if v is None:
                    continue
                # detail is free text (spaces/punctuation) -> quote it; the rest
                # are barewords the callers split on whitespace.
                parts.append(f'{k}="{v}"' if k == "detail" else f"{k}={v}")
            line = " ".join(parts)
        sys.stdout.write(line + "\n")
        sys.stdout.flush()
        raise typer.Exit(code=exit_code)

    # ---- Guard 1: node-claim probe (fail CLOSED on any probe failure) -------
    # claim_status is documented to never raise, but a crashing probe must never
    # collapse to "free" and double-launch, so wrap defensively.
    try:
        info = claim_status(node_key, root=_root_for(node_key))
    except Exception as exc:  # pragma: no cover - claim_status never raises today
        _emit(
            "error",
            exit_code=3,
            detail=(f"claim probe failed ({exc}); not dispatching to avoid a double-launch"),
        )
    state = info.get("state")
    if not state:
        _emit(
            "error",
            exit_code=3,
            detail="claim status returned no parseable state; not dispatching",
        )
    if state == "live":
        _emit("already-running", reason="live-claim", holder=info.get("holder") or "unknown")
    if state == "suspect":
        # x-ba4b: TTL-unexpired, dead pid (respawned worker). The TTL still
        # protects the slot, so dispatch must skip-not-steal. The caller maps
        # reason=suspect-claim to a `skipped-contested` outcome and advances.
        _emit(
            "already-running",
            reason="suspect-claim",
            holder=info.get("holder") or "unknown",
        )
    if state == "corrupted":
        _emit(
            "corrupted",
            detail=(
                f"node:{node_id} claim is corrupted; force-release or repair before dispatching"
            ),
        )
    # state in {free, stale} -> a dispatchable candidate. stale = dead holder; the
    # worker's atomic init-acquire reclaims it (recovery-via-redispatch preserved).

    if no_reserve:
        _emit("dispatchable")

    # ---- Guard 2: dispatcher reservation (closes the boot-window race) ------
    # A short-TTL create-only dispatch:<id> claim serializes two dispatchers with
    # DIFFERENT worker names that both passed Guard 1 before either spawned. A
    # racing peer gets held-by-other -> already-running; the caller releases on a
    # spawn failure and lets it TTL-expire on success.
    try:
        acquire_claim(
            res_key,
            holder,
            reason=f"bg-dispatch reservation for {node_id}",
            ttl_ms=_parse_ttl(ttl),
            root=_root_for(res_key),
        )
    except ClaimHeldByOther:
        _emit("already-running", reason="reservation-held")
    except Exception as exc:
        # Any other failure - a malformed --ttl (ValueError from _parse_ttl),
        # a claim validation/corruption/gone-away, or a filesystem error
        # (OSError/PermissionError) - fails CLOSED as verdict=error rather than
        # tracing out, so the caller still refuses to launch (gemini review).
        _emit(
            "error",
            exit_code=3,
            detail=f"could not acquire dispatch reservation {res_key} ({exc})",
        )
    _emit("dispatchable", reservation_key=res_key, reservation_holder=holder)


@agents_app.command("ask")
def cmd_ask(
    name: str | None = typer.Argument(None, help="Agent name. Omit when using --to-project."),
    message: str | None = typer.Argument(None, help="Message to send."),
    provider: str | None = typer.Option(
        None, "--provider", "-p", help="claude | codex | gemini (required on first ask)."
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
            "Resolve the worker cwd to the canonical (main) repo root regardless "
            "of caller cwd. Opt-in; an explicit --cwd still wins."
        ),
    ),
    here: bool = typer.Option(
        False,
        "--here",
        "--in-place",
        help="Opt out of --fresh: keep the worker in the caller's cwd.",
    ),
) -> None:
    """Send a message to a registered agent (follow-up only).

    ``ask`` requires the agent to already exist. Unknown names exit 16
    with a hint pointing at ``fno agents spawn <name> -p <provider>``.
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

    workdir = _resolve_dispatch_workdir(cwd, fresh, here)

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
            provider=provider,
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


@agents_app.command("chat")
def cmd_chat(
    a: str = typer.Argument(..., help="First peer (the 'from' side of the seed)."),
    b: str = typer.Argument(..., help="Second peer (the 'to' side of the seed)."),
    seed: str = typer.Argument(..., help="Opening message that seeds the channel."),
    cwd: str | None = typer.Option(
        None, "--cwd", "-c", help="Working directory context for a fresh thread."
    ),
    yes: bool = typer.Option(
        False, "--yes", "-y", help="Skip the [y/N] confirm (the caveat is still shown)."
    ),
) -> None:
    """Open a live stream-json channel between two claude peers (G3, ab-0b16d65c).

    Adopts BOTH peers onto the stream-json lane and drives a bounded A<->B relay.
    ALWAYS billed: every hop spends Agent SDK plan credit, so the exact command
    and that caveat are shown before the gate regardless of confirm posture
    (AC3-UI). v1 is claude<->claude only.

    Stdout contract: one terminal-state line. Exit 0 when a channel ran; nonzero
    on a refusal (a peer was a busy running loop) or a failure (unknown peer or a
    dead adopt child).
    """
    import shlex

    from fno.agents.dispatch import _CHAT_PLAN_CREDIT_CAVEAT, dispatch_chat

    workdir = Path(cwd).resolve() if cwd else Path(os.getcwd())

    # AC3-UI: ALWAYS echo the exact command + the plan-credit caveat, even on the
    # auto-skip (--yes) path, so a billed launch is never invisible.
    exact = f"fno agents chat {shlex.quote(a)} {shlex.quote(b)} {shlex.quote(seed)}"
    print(f"$ {exact}", file=sys.stderr)
    print(f"note: {_CHAT_PLAN_CREDIT_CAVEAT}.", file=sys.stderr)

    no_confirm = yes or os.environ.get("FNO_CHAT_NO_CONFIRM")
    if not no_confirm:
        interactive = sys.stdin.isatty() and sys.stderr.isatty()
        if not interactive:
            print(
                "chat needs a [y/N] confirm (billed) but there is no TTY; re-run "
                "with --yes to launch. Nothing was adopted.",
                file=sys.stderr,
            )
            raise typer.Exit(code=3)
        print("Open this billed live channel? [y/N] ", end="", file=sys.stderr, flush=True)
        try:
            answer = sys.stdin.readline().strip().lower()
        except Exception:
            answer = ""
        if answer not in ("y", "yes"):
            print("aborted; nothing adopted.", file=sys.stderr)
            raise typer.Exit(code=3)

    res = dispatch_chat(a, b, seed, cwd=workdir)

    for n in res.notes:
        print(f"note: {n}", file=sys.stderr)

    if res.status == "ok":
        adopted = ", ".join(res.adopted)
        # res.adopted carries the stream-lane HOST names (the watch targets), in
        # [host_a, host_b] order; observe B's host.
        watch_target = res.adopted[-1] if res.adopted else b
        print(
            f"chat {a}<->{b}: {res.turns}/{res.ceiling} turns over [{adopted}] "
            f"(observe: fno agents watch {watch_target})"
        )
        return
    if res.status == "refused":
        print(f"chat refused: {res.reason}", file=sys.stderr)
        raise typer.Exit(code=1)
    # failed
    print(f"chat failed: {res.reason}", file=sys.stderr)
    raise typer.Exit(code=1)


@agents_app.command("list")
def cmd_list(
    cwd: str = typer.Option(None, "--cwd", help="Filter by working directory."),
    provider: str = typer.Option(
        None, "--provider", help="Filter by provider (claude | codex | gemini)."
    ),
    status: AgentStatusFilter = typer.Option(
        None, "--status", help="Filter by registry status (live | orphaned)."
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

    status_value: str | None = status.value if status is not None else None
    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())

    result = list_agents(
        cwd=cwd,
        provider=provider,
        status=status_value,
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


@agents_app.command("discovered-json", hidden=True)
def cmd_discovered_json(
    cwd: str = typer.Option(None, "--cwd", help="Filter discovered rows by cwd."),
) -> None:
    """Internal: emit the discovered-live-sessions lane as JSON.

    The real ``fno agents list`` auto-routes to the Rust client, which owns
    the rendered surface; that path shells out to THIS verb to merge the P1
    host-local live-session lane (ab-098967b4). Output is
    ``{"discovered_sessions": [...]}``. Fail-open: any error prints an empty
    lane and exits 0 so ``agents list`` is never broken by discovery.
    """
    import json as _json

    out: dict = {"discovered_sessions": []}
    try:
        from pathlib import Path as _Path

        from fno.agents import discover as discover_mod
        from fno.agents.registry import load_registry

        try:
            entries = load_registry()
            exclude = {e.claude_short_id for e in entries if e.claude_short_id}
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
        ]
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


@agents_app.command("peek")
def cmd_peek(
    handle: str = typer.Argument(
        ...,
        help="Peer handle (same as `fno mail send`: alias, hex short-id, or <harness>-<short8>).",
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
    """Observe a peer read-only — the twin of `fno mail send`.

    Resolves ``<handle>`` through the same union resolver mail send uses, so any
    peer you can message you can observe. Prefers normalized status events when
    present, else tails the peer's on-disk transcript (claude/codex). Never
    writes anything the peer reads. Exit 13 = unknown peer, 1 = known peer whose
    harness has no reader yet, 0 = observed (or "no activity yet").
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


@agents_app.command("whoami")
def cmd_whoami(
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit JSON regardless of TTY."),
) -> None:
    """Print THIS mesh worker's own registered name (+ registry enrichment).

    The derived-name peers use to address you via ``fno mail send <name>``.
    Resolves identity from ``FNO_AGENT_SELF`` (the env the spawn path
    injects), falling back to a registry row matching
    ``CLAUDE_CODE_SESSION_ID`` when the env is absent. Read-only: it never
    mutates the registry, emits an event, or writes state.

    Exit 0 when a name is resolved; exit 3 ("not a registered mesh agent")
    for a human / top-level session with no mesh identity. Distinct from
    ``fno whoami`` (top-level), which reports operating CONTEXT
    (fleet -> walker -> session -> provider), not the mesh name.
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
    session_uuid = os.environ.get("CLAUDE_CODE_SESSION_ID")
    live_warnings: list[str] = []

    def _live_status_fn(short_id: str) -> str | None:
        from fno.agents.providers import claude as claude_mod

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


@agents_app.command("top")
def cmd_top(
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit the same rows as JSON (script parity)."
    ),
) -> None:
    """Show every live worker process — fno-spawned and foreign claude bg
    alike — with pid, RSS (MB), and status (x-c5cc US4).

    The same union the spawn gate counts, so this is the audit surface every
    gate message points at. Python-only (RSS via psutil; not routed to the
    Rust client).
    """
    from fno.agents.top import render_top

    print(render_top(as_json=as_json))


@agents_app.command("ping")
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

    from fno.agents.drive_authority import active_drive_sessions

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


@agents_app.command("rm")
def cmd_rm(
    name: str = typer.Argument(..., help="Agent name (from `fno agents list`)."),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help=(
            "Override claude's refusal (e.g. uncommitted worktree changes) "
            "and drop the registry entry regardless. WARNING: leaves an "
            "orphan supervisor session that you must clean via 'claude rm "
            "<short_id>' manually."
        ),
    ),
) -> None:
    """Remove an agent.

    Claude agents: ``claude rm <short_id>`` first, then drop the registry
    row on success. ``--force`` keeps removing the registry row even when
    claude rm fails. Codex / gemini agents: registry-only removal (the
    on-disk session files stay; clean manually if desired).
    """
    from fno.agents.dispatch import DispatchAskError, rm_agent

    try:
        rm_agent(name, force=force)
    except DispatchAskError as exc:
        print(str(exc), file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc


@agents_app.command("reconcile")
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

    out.write(
        f"{result.scanned} entries scanned: "
        f"{len(result.orphaned)} orphaned, "
        f"{len(result.recovered)} recovered, "
        f"{len(result.skipped)} skipped"
    )
    if result.errors:
        out.write(f", {len(result.errors)} errors")
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
    (the abi-owned supervisor) as the planned landing for cross-provider
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

agents_app.command("trace")(_cmd_trace)
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
