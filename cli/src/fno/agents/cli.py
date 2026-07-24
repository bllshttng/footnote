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

# `mcp` re-homed under `fno agents` (x-71b6): the MCP sidecar client is
# agent-mesh machinery (mail live-inject), so it belongs beside the other
# agents verbs, not at the top level. Registered hidden - it is plumbing, not a
# human menu verb. The top-level `fno mcp` stays as a hidden alias for one
# release (its sole cross-language caller, the Rust daemon's deliver_envelope,
# keeps shelling `fno mcp send` until the alias is retired in a later pass).
from fno.mcp.cli import mcp_app as _mcp_app  # noqa: E402

agents_app.add_typer(_mcp_app, name="mcp", hidden=True)


class AgentStatusFilter(str, enum.Enum):
    """Rendered family-1 liveness values accepted by ``list --status``."""

    live = "live"
    orphaned = "orphaned"
    unknown = "unknown"


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


# The crown ladder is exactly three altitudes: VP (0, project) -> Director
# (1, epic) -> IC (2, node). A level outside 0..2 is not a real crown, so it is
# refused - this both enforces the ladder and keeps crown_level far inside the
# Rust registry row's u32 (a fat-fingered arbitrary-precision Python int can't
# overflow it and poison the shared store).
_MAX_CROWN_LEVEL = 2


def _parse_crown(spec: str) -> tuple[int, str]:
    """Parse a ``--crown 'level=N,scope=X'`` spec into (level, scope); exit 2 on
    any malformed part. ``level`` must be a non-negative int, ``scope`` nonblank.
    Order-free, both keys required. The grantor is deliberately NOT here: it is
    stamped ambiently at spawn from the spawning session, never caller-supplied
    (US9, the never-self-declared rule).

    ponytail: splits scope on ``,`` - a scope with a literal comma is not a real
    node/epic/project id, so the simple split holds.
    """
    parts: dict[str, str] = {}
    for chunk in spec.split(","):
        key, sep, val = chunk.partition("=")
        if not sep:
            print(f"--crown expects 'level=N,scope=X'; got {chunk!r}", file=sys.stderr)
            raise typer.Exit(code=2)
        parts[key.strip()] = val.strip()
    missing = {"level", "scope"} - parts.keys()
    if missing:
        print(
            f"--crown missing {', '.join(sorted(missing))}; need 'level=N,scope=X'",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    try:
        level = int(parts["level"])
    except ValueError:
        print(f"--crown level must be an int >= 0; got {parts['level']!r}", file=sys.stderr)
        raise typer.Exit(code=2)
    if level < 0:
        print(f"--crown level must be >= 0; got {level}", file=sys.stderr)
        raise typer.Exit(code=2)
    if level > _MAX_CROWN_LEVEL:
        print(
            f"--crown level must be <= {_MAX_CROWN_LEVEL}; got {level}",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if not parts["scope"]:
        print("--crown scope must be nonblank", file=sys.stderr)
        raise typer.Exit(code=2)
    return level, parts["scope"]


def _scope_is_subset(target_scope: str, grantor_scope: str | None) -> bool:
    """Is ``target_scope`` a subset of the grantor's crown scope (US10)?

    Scopes are opaque ids/names, so structural project>epic>node containment is
    not derivable in code. The enforceable rule: a grantor may grant a DIFFERENT
    (narrower) scope, never re-grant its own - a same-scope grant would be a peer
    crown, already refused by the one-live-crown-per-scope rule. Deeper
    containment is the grantor's good-faith responsibility, the same trust the
    crown model places in a crowning brief.
    """
    return bool(grantor_scope) and target_scope != grantor_scope


@agents_app.command("crown", hidden=True)
def cmd_crown(
    handle: str = typer.Argument(
        ..., help="Existing agent handle (name / 8-hex / session id) to coronate in place."
    ),
    scope: str = typer.Option(
        ...,
        "--scope",
        help="The epic / project / node id the crown rules over (e.g. --scope x-d92e).",
    ),
    level: int | None = typer.Option(
        None,
        "--level",
        help=(
            "Ladder altitude 0..2 (VP=0 project, Director=1 epic, IC=2 node). "
            "Default: the grantor's level+1 (superset-king), else 0."
        ),
    ),
) -> None:
    """Coronate an EXISTING session in place (US10): write the US9 crown fields
    onto ``handle``'s registry row.

    The crown is GRANTED, never self-declared - the grantor class is stamped as
    provenance: a live superset-crown holder (scope validated against its own
    row), an attended human (a shell with no agent identity in env), or a
    standing config grant (``config.agents.crown_config_grant``, DEFAULT OFF).
    Refuses a self-grant and a second live crown over the same scope.
    """
    from fno.agents.registry import (
        AgentResolutionError,
        load_registry,
        resolve_agent,
        update_registry,
    )
    from fno.config import load_settings

    scope = scope.strip()
    if not scope:
        print("--scope must be nonblank", file=sys.stderr)
        raise typer.Exit(code=2)
    if level is not None and (level < 0 or level > _MAX_CROWN_LEVEL):
        print(
            f"--level must be between 0 and {_MAX_CROWN_LEVEL}; got {level}",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    try:
        target = resolve_agent(handle).entry
    except AgentResolutionError as exc:
        print(f"no agent for handle {handle!r}: {exc}", file=sys.stderr)
        raise typer.Exit(code=2)

    # The caller's own identity: a spawned agent carries FNO_AGENT_SELF, a human
    # shell does not. That absence IS the attended-human signal.
    caller_self = (os.environ.get("FNO_AGENT_SELF") or "").strip()
    caller_row = None
    if caller_self:
        try:
            caller_row = resolve_agent(caller_self).entry
        except AgentResolutionError:
            caller_row = None

    # Refusal: never self-declared - a session cannot crown its own row.
    if caller_row is not None and caller_row.name == target.name:
        print(
            "refusing a self-grant: a crown is bestowed, never self-declared",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # Refusal: one live crown per scope.
    for e in load_registry():
        if e.name != target.name and e.crown_scope == scope and e.status == "live":
            print(
                f"refusing: {e.name!r} already holds a live crown over scope "
                f"{scope!r} (one live crown per scope)",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)

    # Authorize + stamp the grantor provenance (first match wins).
    caller_has_crown = caller_row is not None and caller_row.crown_level is not None
    resolved_level = level
    if caller_row is not None and caller_has_crown and _scope_is_subset(scope, caller_row.crown_scope):
        grantor = caller_row.session_id or caller_row.name  # superset-king
        if resolved_level is None:
            resolved_level = (caller_row.crown_level or 0) + 1
    elif load_settings().agents.crown_config_grant:
        grantor = "config-grant"
    elif not caller_self:
        grantor = "human"  # an attended human shell (no agent identity)
    else:
        print(
            f"refusing: this session holds no superset crown over {scope!r}, is "
            "not an attended human, and config.agents.crown_config_grant is off. "
            "Grant from a superset-king, a human shell, or enable the config grant.",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)
    if resolved_level is None:
        resolved_level = 0
    if resolved_level > _MAX_CROWN_LEVEL:
        print(
            f"refusing: derived crown level {resolved_level} exceeds the ceiling "
            f"{_MAX_CROWN_LEVEL}",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    def _crown(rows: list) -> list:
        for r in rows:
            if r.name == target.name:
                r.crown_level = resolved_level
                r.crown_scope = scope
                r.crown_grantor = grantor
        return rows

    update_registry(_crown)
    print(
        json.dumps(
            {
                "crowned": target.name,
                "level": resolved_level,
                "scope": scope,
                "grantor": grantor,
            }
        )
    )


@agents_app.command("spawn")
def cmd_spawn(
    message: str = typer.Argument("", help="The prompt to seed the worker with."),
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
            "(--provider zai --model glm-5.2 == --route zai,glm-5.2). This is NOT "
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
            "zai/glm-5.2; legacy comma zai,glm-5.2 also accepted). Bypasses the "
            "--role table and guard (explicit intent "
            "is not auto-routing) and wins over any configured lane. FAILS CLOSED: "
            "an unknown provider, non-anthropic protocol, or missing key refuses "
            "the spawn - never a silent primary-model launch. claude only."
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
    crown: str | None = typer.Option(
        None,
        "--crown",
        help=(
            "Bestow an orchestrator crown on the spawned worker (US9): "
            "'level=N,scope=X'. scope = the epic / project / node id the crown "
            "rules over (e.g. scope=x-d92e); level = the ladder altitude 0..2 "
            "(VP=0 project, Director=1 epic, IC=2 node). Stamped on the child's "
            "row with the grantor derived from THIS session - never self-declared."
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
    # --squad is a hidden back-compat alias for --workspace (US2); --workspace wins.
    squad = squad if squad is not None else squad_compat

    from fno.agents.dispatch import DispatchAskError, SpawnResult, dispatch_spawn
    from fno.agents.provider_resolve import (
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
    _moved_cwd = (
        str(workdir)
        if not cwd and workdir != Path(os.getcwd()).resolve()
        else None
    )

    # Three orthogonal axes: --harness names the CLI binary, --provider the model
    # vendor that binary talks to, --model the model at that vendor. `provider` is
    # the local name for the HARNESS axis all the way down -- it is the
    # dispatch_spawn kwarg and the receipt key every consumer parses, so the wire
    # name outranks the tidier local one.
    provider = harness
    if vendor is not None:
        vendor = vendor.strip()
        # The historical confusion, refused by name rather than silently launching
        # the wrong thing: `--provider claude` used to select the CLI binary.
        from fno.agents.providers import READABLE_PROVIDERS

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
                "(the vendor's own model id, e.g. --model glm-5.2)",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        # The model belongs to the route from here: it reaches the worker as the
        # routed ANTHROPIC_MODEL, never as a `claude --model` token (which would
        # hand the claude CLI a vendor model id it cannot resolve).
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
    # Provenance rides the pane receipt's provider_source field below (the
    # default substrate). The bg/once stdout receipts stay byte-parity-locked
    # with the Rust client, so they don't carry it.

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
    if once and route is not None and substrate == "pane":
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
        print("--workspace/-s needs a nonblank workspace name", file=sys.stderr)
        raise typer.Exit(code=2)
    if placement_requested and (substrate != "pane" or once):
        print(
            "--workspace/-s and --split/-x apply only to --substrate pane (bg/headless have "
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

    # --crown level=N,scope=X (US9): parse + validate now; the grantor is stamped
    # ambiently at spawn from this session, so the child's row records who
    # actually bestowed the crown, never a value it could forge. Scoped to the
    # pane substrate for now (the court's own substrate); a bg/headless crown is
    # refused fail-closed rather than silently dropped.
    crown_level: int | None = None
    crown_scope: str | None = None
    if crown is not None:
        if substrate != "pane" or once:
            print(
                "--crown applies only to --substrate pane (the court's substrate); "
                "bg/headless crowns are not yet supported",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        crown_level, crown_scope = _parse_crown(crown)

    # --account contradictions (x-d012), refused BEFORE route resolution so a
    # keyless route never masks this receipt. --account bills a specific claude
    # account; a non-claude provider, a --route, or an auto-routing --role sends
    # the worker to a different provider - the route's ANTHROPIC_* would override
    # the account's CLAUDE_CONFIG_DIR and silently mis-bill. Refuse all three.
    if account is not None:
        if provider != "claude":
            print(f"--account is claude-only; got provider {provider!r}", file=sys.stderr)
            raise typer.Exit(code=2)
        if route is not None or role is not None:
            other = "--route" if route is not None else "--role"
            print(
                f"--account cannot combine with {other}: --account bills a claude "
                "account, provider routing sends the worker elsewhere. Pass one.",
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
        # Explicit routes remain limited to the bg/headless contract; role routing
        # is the pane-capable path.
        if provider != "claude" or substrate not in ("bg", "headless"):
            print(
                "--route is claude on --substrate bg or headless only; "
                f"got provider {provider!r} substrate {substrate!r}.",
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

    # Resolve a role once before pane/bg/headless fan out so every substrate
    # receives the same endpoint, auth, and model mapping.
    if route is None and role is not None and provider == "claude":
        from fno.agents.model_routing import resolve_route

        route_env = resolve_route(role, notice=lambda note: print(note, file=sys.stderr))

    # Provider rotation stamps the selected account in FNO_*; a managed OAuth
    # account shares the default Claude slot and cannot compose atomically with
    # a separate role/route endpoint. Refuse before the spawn gate.
    if route_env and os.environ.get("FNO_PROVIDER_AUTH", "").strip().lower() == "managed":
        overlay_id = os.environ.get("FNO_PROVIDER_ID", "").strip() or "unknown"
        intent = f"routed role {role!r}" if role is not None else f"route {route!r}"
        print(
            f"refusing {intent} over managed OAuth provider {overlay_id!r}: "
            "endpoint, auth, and model must be selected as one provider route; "
            "no worker launched.",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    # Per-spawn account overlay (x-d012). Resolve + FAIL CLOSED here, BEFORE the
    # gate, like --route: a refusal spawns nothing, takes no gate slot, and
    # leaves the node dispatchable. Contradictions (non-claude provider, --route,
    # --role) were already refused above, before route resolution.
    account_env: dict[str, str] | None = None
    if account is not None:
        from fno.agents.account_env import resolve_account_overlay_or_exit

        overlay = resolve_account_overlay_or_exit(account)
        account_env = overlay.env if overlay else None

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

    # Prior values of the provenance keys the bg/headless arm exports below, so
    # the finally can put the process env back.
    prov_prev: dict[str, "str | None"] = {}

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
                    crown_level=crown_level,
                    crown_scope=crown_scope,
                    provenance=resolve_provenance(node, slug, plan),
                    account_env=account_env,
                    route_env=route_env,
                )
            except DispatchAskError as exc:
                print(str(exc), file=sys.stderr)
                raise typer.Exit(code=exc.exit_code) from exc
            # Compact one-line receipt, superset of the daemon-spawn receipt shape
            # ({"name","short_id","provider","status"}) so line-parsing consumers
            # keep working. short_id carries claude's 8-hex jobId so the caller can
            # mail the pane straight from the receipt (US8); "" for providers that
            # resume off harness_session_id instead.
            receipt_obj = {
                "name": pane_result.name,
                "short_id": pane_result.short_id,
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
            # x-d012: name the pinned account so a mis-pin is visible at spawn
            # time, not at billing time. Only when set (receipt byte-stable else).
            if account is not None:
                receipt_obj["account"] = account
            if _moved_cwd is not None:
                receipt_obj["cwd"] = _moved_cwd
            receipt = json.dumps(receipt_obj)
            sys.stdout.write(receipt + "\n")
            sys.stdout.flush()
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
        from fno.agents.mux_spawn import PROVENANCE_KEYS, resolve_provenance

        prov_env = resolve_provenance(node, slug, plan)
        prov_prev.update({k: os.environ.get(k) for k in PROVENANCE_KEYS})
        for _k in PROVENANCE_KEYS:
            os.environ.pop(_k, None)
        os.environ.update(prov_env)

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
                account_env=account_env,
            )
        except DispatchAskError as exc:
            print(str(exc), file=sys.stderr)
            raise typer.Exit(code=exc.exit_code) from exc
    finally:
        # Release the gate's claims once the dispatch result exists (or the
        # spawn failed): registry/roster rows carry the count from here.
        gate.release()
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
        cwd_field = (
            f", \"cwd\": {json.dumps(_moved_cwd)}"
            if _moved_cwd is not None
            else ""
        )
        # x-d012: name the pinned account. Only when set, so a non-account bg
        # receipt stays byte-identical to the Rust client's (which never emits
        # it - an --account spawn always re-execs into this Python path).
        account_field = f", \"account\": {json.dumps(account)}" if account else ""
        receipt = (
            f'{{"name": "{safe_name}", "short_id": "{result.short_id}", '
            f'"provider": "{result.provider}", "status": "live"'
            f'{perm_field}{cwd_field}{account_field}}}'
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
    # Visibility barrier (x-a7ab 1.2 / x-b44e): re-read dispatch:<id> AFTER the
    # acquire to confirm THIS holder is the one on disk before launching. The
    # create-only acquire already serializes two callers, but a peer whose
    # exclusive create won a visibility-lagged race (or an FS where O_EXCL is
    # not fully atomic across the acquire + launch) surfaces as a different
    # holder here; that peer launches, this dispatcher skips with duplicate-
    # claim so exactly one worker is born. Acquisition-before-observable-work
    # is now a re-verified fact, not an assumption.
    try:
        post = claim_status(res_key, root=_root_for(res_key))
    except Exception:  # pragma: no cover - claim_status never raises today
        post = {}
    if post.get("holder") != holder:
        _emit(
            "already-running",
            reason="duplicate-claim",
            holder=post.get("holder") or "unknown",
        )
    _emit("dispatchable", reservation_key=res_key, reservation_holder=holder)


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
        "only at spawn. Removed at 0.4.0.",
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
        help="Retired: filter by --harness. Removed at 0.4.0.",
    ),
    status: AgentStatusFilter = typer.Option(
        None, "--status", help="Filter by liveness (live | orphaned | unknown)."
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
    is_tty = bool(getattr(sys.stdout, "isatty", lambda: False)())

    result = list_agents(
        cwd=cwd,
        provider=harness,
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
    harness: str = typer.Option(
        None, "--harness", help="Filter discovered rows by harness."
    ),
    _provider_tombstone: str = typer.Option(
        None,
        "--provider",
        hidden=True,
        help="Retired: filter by --harness. Removed at 0.4.0.",
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


#: `heal-token` exit codes. 13 mirrors the lifecycle verbs' not-found code; the
#: ambiguity code is distinct from BOTH that and typer's internal-error 1 so the
#: Rust caller can tell "refuse loudly with these candidates" from "degrade to
#: the original not-found error" (x-da8c AC4 vs AC5).
HEAL_TOKEN_MISS_EXIT = 13
HEAL_TOKEN_AMBIGUOUS_EXIT = 3


@agents_app.command("heal-token", hidden=True)
def cmd_heal_token(
    token: str = typer.Argument(..., help="Session-shaped token (8-hex, UUID, ses_...)."),
    registry: str = typer.Option(
        None,
        "--registry",
        help="Adopt into THIS registry file (default: the configured one).",
    ),
) -> None:
    """Internal: adopt the session TOKEN names from its harness store, as JSON.

    The one x-9cc5 healer behind ``registry.resolve_agent``, exposed so the Rust
    lifecycle verbs (logs/attach/resume) heal a registry miss through the SAME
    probe rather than growing a second one. Exit 0 with the adopted row on
    stdout; 13 on a miss or a non-session-shaped token; 3 with the candidate
    list on stderr when the token is ambiguous.

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


@agents_app.command("whoami", hidden=True)
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
    # Resolve THIS process's session id from whichever harness marker is set
    # (x-ec59): a codex/gemini worker resolves its own row via harness_session_id,
    # not just CLAUDE_CODE_SESSION_ID. Falls back to the claude marker when the
    # shared resolver finds nothing so today's claude behavior is unchanged.
    from fno.harness_identity import resolve_harness_identity

    _ident = resolve_harness_identity()
    session_uuid = _ident.session_id or os.environ.get("CLAUDE_CODE_SESSION_ID")
    # Scope registry matching to this process's harness so a provider-local session
    # id can't match a same-id row of another harness (x-ec59). The env fallback is
    # CLAUDE_CODE_SESSION_ID, so an unresolved marker means claude.
    session_harness = _ident.harness or ("claude" if session_uuid else None)
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

    ident = resolve_harness_identity()
    session_id = ident.session_id or os.environ.get("CLAUDE_CODE_SESSION_ID")
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
            _json.dumps({"registered": True, "name": entry.name, "provider": entry.harness}) + "\n"
        )
    else:
        sys.stdout.write(
            f"joined the mesh as {entry.name} - peers can now reach you with "
            f"`fno mail send {entry.name} \"...\"`\n"
        )


@agents_app.command("top", hidden=True)
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
    """
    import json as _json

    from fno.agents.session_truth import render_truth, resolve_session_truth

    result = resolve_session_truth(handle)
    if json_out:
        payload = {
            k: result.get(k)
            for k in ("handle", "state", "reason", "last_activity_age_s", "session_id")
        }
        sys.stdout.write(_json.dumps(payload) + "\n")
    else:
        sys.stdout.write(render_truth(result) + "\n")
    sys.stdout.flush()
    if result.get("state") == "unknown" and result.get("reason") == "not-found":
        raise typer.Exit(code=13)


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


@agents_app.command("rm", hidden=True)
def cmd_rm(
    name: str = typer.Argument(..., help="Agent name (from `fno agents list`)."),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help=(
            "Drop the registry entry even when the harness teardown fails "
            "or refuses (e.g. uncommitted worktree changes). WARNING: "
            "leaves an orphan session record in that harness's own store, "
            "named on stderr, for you to clean manually."
        ),
    ),
) -> None:
    """Remove an agent: harness session record first, registry row after.

    Per-harness teardown:

    \b
      claude    `claude rm <short_id>` (session record + worktree, via
                claude's own delegation contract)
      codex     drops the session's entry from ~/.codex/session_index.jsonl
      opencode  registry-only; `rm` will not delete an opencode session,
                because that also deletes its child sessions and its whole
                message history. Run `opencode session delete <id>` if you
                want the conversation gone.
      gemini    registry-only (no teardown arm for a deprecated provider)

    Your history is never removed here -- teardown drops the harness's
    index record, not the conversation. On teardown failure the registry
    row is kept so you can retry; ``--force`` drops it anyway and names
    the orphan. Removing an agent does not stop a running session; use
    ``fno agents stop`` for that.

    Worktrees are NOT removed here for non-claude harnesses (nothing on
    the registry row marks a cwd as an isolated worktree). Reap them with
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
