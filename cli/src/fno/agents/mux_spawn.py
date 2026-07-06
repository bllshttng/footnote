"""Mux-pane spawn back half (4a-G2): host an agent's PTY as a mux pane.

``fno agents spawn --substrate pane`` (the default substrate) lands here: the
front half - name validation, provider selection, per-agent flock, collision
check, role routing, billing guard - is the same machinery the daemon/bg paths
use; only the HOSTING call differs. Instead of the fno-agents daemon spawning
a PTY worker, this subprocesses ``fno mux pane run --session <s> --cwd <cwd>
-- env <mesh env> <provider argv>`` (the G1 script API), parses the
machine-readable pane id off stdout, and writes the registry row with the
``mux: {session, pane_id}`` ref (create-after-spawn: a failed spawn writes NO
row, and there is never a silent daemon-PTY fallback - AC1-ERR).

The mux server itself sets ``FNO_SESSION``/``FNO_PANE`` in the pane child env
(crates/fno pty.rs); the mesh identity (``FNO_AGENT_SELF``/``FNO_AGENT_PROVIDER``)
rides an ``env(1)`` wrapper because ``pane run`` carries argv, not env.

Interactive argv per provider mirrors the Rust daemon providers
(crates/fno-agents/src/provider.rs) - the subscription-billed interactive
forms, never ``-p``/``--print`` (D2 billing guard, re-checked here before any
pane exists).
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import uuid as _uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

from fno import paths
from fno.agents.dispatch import (
    DispatchAskError,
    _capture_parent_edge,
    validate_spawn_name,
)
from fno.agents.lock import hold_agent_lock
from fno.agents.registry import (
    AgentEntry,
    RegistryVersionError,
    load_registry,
    update_registry,
)

#: Bound on the `pane run` / `pane ls` subprocesses. `pane run` includes a
#: possible server self-spawn + squad git resolve (~2s worst case), so this is
#: generous next to reality, tight next to a wedged mux.
_MUX_SUBPROCESS_TIMEOUT_S = 30

#: The default mux session when neither --session nor FNO_SESSION names one
#: (mirrors crates/fno proto::DEFAULT_SESSION).
_DEFAULT_SESSION = "main"

#: opencode is spawned with a model by default (its provider/model form). The
#: chosen default is the z.ai GLM (the secondary provider); an explicit --model
#: overrides it. ponytail: a plain constant, not a config knob, until a second
#: opencode default is actually wanted.
_OPENCODE_DEFAULT_MODEL = "z-ai/glm-5.2"


@dataclass
class MuxSpawnResult:
    name: str
    provider: str
    session: str
    pane_id: int
    child_pid: Optional[int]
    session_uuid: Optional[str]


def _fno_bin() -> str:
    """The `fno` front-door binary (the Rust mux owner). ``FNO_BIN`` overrides
    for tests and non-PATH installs."""
    return os.environ.get("FNO_BIN") or "fno"


def _shell_integration() -> str:
    """``config.mux.shell_integration`` -> the value the Rust mux reads from
    ``FNO_MUX_SHELL_INTEGRATION``. The settings loader is Python-only,
    so the spawn front-half is the config->env bridge: set on the ``pane run``
    subprocess env, which self-spawns the mux server, so the server (which reads
    the knob when it wraps pane shells) inherits it. Fail-safe to the default
    (never break a spawn on a config read); the Rust side treats absent/anything
    but ``off`` as on regardless.

    ponytail: an interactive `fno mux` server (born from the Rust client, no
    Python) reads the default (on) unless the user exports the env - the plan
    de-scoped Rust reading settings.yaml.
    """
    try:
        from fno.config import load_settings

        return load_settings().config.mux.shell_integration
    except Exception:
        return "mux-panes"


def resolve_mux_session(explicit: Optional[str] = None) -> str:
    """flag > FNO_SESSION > "main" (Locked 7, mirrors mux_cli resolve_session).

    An in-pane spawn inherits its own session via FNO_SESSION, so
    agents-spawn-agents lands siblings in the same session by default.
    """
    if explicit:
        return explicit
    env = os.environ.get("FNO_SESSION", "")
    return env if env else _DEFAULT_SESSION


def claude_argv_is_interactive(argv: list[str]) -> bool:
    """D2 billing guard predicate (mirrors the daemon's
    ``claude_argv_is_interactive``): a mux-hosted claude must be the
    interactive subscription-billed form - any ``-p``/``--print`` token means
    the Agent-SDK-credit lane and is refused before a pane exists (AC1-FR)."""
    return not any(tok in ("-p", "--print") for tok in argv)


# Providers with an interactive-pane form below. This is the pane-hostable set -
# a DISTINCT invariant from READABLE_PROVIDERS (which only means "the registry
# loader tolerates this string in a row"). The two coincide today (opencode
# graduated from staged-manifest-only to hosted at x-51f6) but diverge the
# moment the next readable-but-argvless provider is staged. Gate the pane path
# on THIS, so a staged provider is refused with an honest message rather than
# slipping to build_pane_argv's backstop raise.
# Keep in sync with the branches in build_pane_argv (the round-trip test enforces it).
PANE_HOSTABLE_PROVIDERS: tuple[str, ...] = (
    "claude",
    "codex",
    "gemini",
    "agy",
    "opencode",
)


def build_pane_argv(
    provider: str,
    message: str,
    cwd: Path,
    yolo: bool,
    session_uuid: Optional[str],
    model: Optional[str] = None,
) -> list[str]:
    """The interactive PANE argv for ``provider`` - the bare-TUI form a mux
    pane hosts. This is DISTINCT from each provider's Rust ``create_argv``
    (crates/fno-agents/src/provider.rs), which builds the HEADLESS one-shot
    form for the `--substrate headless` lane; the two intentionally differ
    (e.g. opencode: bare ``opencode --prompt <msg>`` here vs
    ``opencode run --auto <msg>`` there) and there is no cross-language
    parity contract between them - don't go looking for one.

    ``model`` (x-c772): an explicit ``--model`` forwarded to the provider's own
    TUI flag (claude/codex/gemini/agy ``--model <m>``; opencode
    ``--model <provider/model>``). Exact passthrough, no fuzzy resolution;
    empty/None = provider default. A CLI ``--model`` arg beats any role-routing
    model set via env (``resolve_route``), so explicit intent wins."""
    if provider == "claude":
        # `claude --session-id <uuid> [message]`: the pinned session id makes
        # the transcript discoverable and keys the inside-leg reports
        # (handle_report matches claude_session_uuid).
        argv = ["claude"]
        if session_uuid:
            argv += ["--session-id", session_uuid]
        if model:
            argv += ["--model", model]
        if message:
            argv.append(message)
        return argv
    if provider == "codex":
        # `codex [OPTIONS] [PROMPT]` with no subcommand is the interactive CLI.
        argv = ["codex", "-C", str(cwd)]
        argv += (
            ["--dangerously-bypass-approvals-and-sandbox"]
            if yolo
            else ["--sandbox", "workspace-write"]
        )
        if model:
            argv += ["--model", model]
        if message:
            argv.append(message)
        return argv
    if provider == "gemini":
        # `-i` executes the prompt then stays interactive; --skip-trust avoids
        # the workspace-trust modal blocking the TUI.
        argv = ["gemini", "--skip-trust"]
        if model:
            argv += ["--model", model]
        if message:
            argv += ["-i", message]
        argv += ["--yolo"] if yolo else ["--approval-mode", "default"]
        return argv
    if provider == "agy":
        # agy (Antigravity) interactive pane (x-8f7f US1). Mirrors AgyProvider in
        # provider.rs: `--dangerously-skip-permissions` is the never-prompt lane
        # so an unattended pane can't wedge on its first approval. agy is
        # stateless (no session id, no JSON envelope), so no --session-id pin;
        # `-p`/`--print` is agy's HEADLESS form (exits after printing) and must
        # NOT be used for a pane. A message rides as the trailing positional,
        # matching claude's interactive form.
        # ponytail: argv unvalidated against a live agy TUI (agy is closed-source);
        # pin it via capture-readiness-grid.sh when the manifest is validated.
        argv = ["agy", "--dangerously-skip-permissions"]
        if model:
            argv += ["--model", model]
        if message:
            argv.append(message)
        return argv
    if provider == "opencode":
        # Bare `opencode` is the TUI (x-51f6); `opencode run` is the HEADLESS
        # form and must not be pane-hosted. The positional is a PROJECT PATH,
        # not a prompt, so the message rides --prompt (argv pinned from
        # opencode source, packages/opencode/src/cli/cmd/tui.ts). --auto is
        # the never-prompt lane (visible spelling of the hidden
        # --yolo/--dangerously-skip-permissions aliases); non-yolo keeps
        # opencode's default permission prompting for the answer queue.
        argv = ["opencode"]
        if message:
            argv += ["--prompt", message]
        # opencode expects the provider/model form and is always launched with a
        # model: an explicit --model wins, else the z-ai/glm-5.2 default.
        argv += ["--model", model or _OPENCODE_DEFAULT_MODEL]
        if yolo:
            argv.append("--auto")
        return argv
    raise DispatchAskError(
        f"provider {provider!r} has no interactive pane form", exit_code=2
    )


def _mesh_env_wrapper(
    name: str,
    provider: str,
    role: Optional[str],
    argv: list[str],
    provenance: Optional[dict[str, str]] = None,
) -> list[str]:
    """Prefix ``argv`` with ``env(1)`` carrying the mesh identity the daemon
    worker used to set on its PTY child (worker.rs), plus any role-routing env
    (x-d2fe) and node provenance (x-84a8). ``pane run`` transports argv only, so
    env rides the wrapper; the spawn-name validation already forbids
    ``=``/newlines in ``name``.

    ``provenance`` is an already-resolved map of provenance env vars (e.g.
    ``FNO_NODE``/``FNO_SLUG``/``FNO_PLAN``) for a node-driven spawn; empty values
    are dropped so an ad-hoc pane exports nothing new (the starship module hides
    absent vars via ``when``)."""
    pairs = [f"FNO_AGENT_SELF={name}", f"FNO_AGENT_PROVIDER={provider}"]
    unset: list[str] = []
    if provider == "claude":
        # Worker parity: transcripts must persist for resume/adoption.
        pairs.append("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1")
    if role:
        from fno.agents.model_routing import resolve_route

        route = resolve_route(role)
        if route:
            # Scrub the parent's Anthropic creds so the routed AUTH_TOKEN wins:
            # a lingering API key or subscription OAuth token would otherwise
            # override it and send the routed pane back to Anthropic. `env -u`
            # on an unset var is a harmless no-op.
            unset = ["-u", "ANTHROPIC_API_KEY", "-u", "CLAUDE_CODE_OAUTH_TOKEN"]
            pairs += [f"{k}={v}" for k, v in route.items()]
    if provenance:
        pairs += [f"{k}={v}" for k, v in provenance.items() if v]
    return ["env", *unset, *pairs, *argv]


def resolve_provenance(
    node: Optional[str],
    slug: Optional[str] = None,
    plan: Optional[str] = None,
) -> dict[str, str]:
    """Build the ``FNO_NODE``/``FNO_SLUG``/``FNO_PLAN`` provenance map for a
    node-driven pane spawn (x-84a8).

    ``node`` is the only required input (a node id or slug). ``slug``/``plan``
    fill from the graph node record when absent - a single graph read that a
    caller can skip by passing both. An unresolvable node keeps just
    ``FNO_NODE``; no node at all yields ``{}`` so an ad-hoc pane exports nothing
    (edge AC: no empty-string exports). ``FNO_PLAN`` is omitted when the node has
    no linked plan; ``FNO_PR`` is intentionally absent (unknown at spawn)."""
    if not node:
        return {}
    if slug is None or plan is None:
        try:
            from fno.graph.load import load_graph

            for rec in load_graph():
                if rec.get("id") == node or rec.get("slug") == node:
                    node = rec.get("id") or node  # normalize a slug input to id
                    if slug is None:
                        slug = rec.get("slug") or ""
                    if plan is None:
                        plan = rec.get("plan_path") or ""
                    break
        except Exception:
            # A missing/corrupt graph must not block the spawn; degrade to the
            # node id alone rather than raising in the pane path.
            pass
    prov = {"FNO_NODE": node, "FNO_SLUG": slug or "", "FNO_PLAN": plan or ""}
    return {k: v for k, v in prov.items() if v}


def _run_mux(
    args: list[str],
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    env: Optional[dict[str, str]] = None,
) -> "subprocess.CompletedProcess[str]":
    try:
        return runner(
            [_fno_bin(), *args],
            capture_output=True,
            text=True,
            timeout=_MUX_SUBPROCESS_TIMEOUT_S,
            **({"env": env} if env is not None else {}),
        )
    except FileNotFoundError as exc:
        raise DispatchAskError(
            f"the '{_fno_bin()}' binary was not found on PATH; the pane "
            "substrate is hosted by the fno mux (set FNO_BIN or install fno)",
            exit_code=127,
        ) from exc
    except subprocess.TimeoutExpired as exc:
        raise DispatchAskError(
            f"fno mux did not answer within {_MUX_SUBPROCESS_TIMEOUT_S}s "
            f"({' '.join(args[:3])}...)",
            exit_code=1,
        ) from exc


def _lookup_child_pid(
    session: str,
    pane_id: int,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
) -> Optional[int]:
    """Best-effort child-pid fetch via ``pane ls --json`` (feeds the registry
    row's ``pid`` so reconcile/GC can probe liveness). ``None`` on any miss -
    the pane is live regardless."""
    try:
        proc = _run_mux(
            ["mux", "pane", "ls", "--session", session, "--json"], runner
        )
        if proc.returncode != 0:
            return None
        for row in json.loads(proc.stdout or "[]"):
            if row.get("pane_id") == pane_id:
                pid = row.get("child_pid")
                return int(pid) if pid is not None else None
    except (ValueError, DispatchAskError):
        return None
    return None


def dispatch_spawn_pane(
    name: str,
    message: str,
    provider: str,
    cwd: Path,
    *,
    yolo: bool = False,
    role: Optional[str] = None,
    model: Optional[str] = None,
    session: Optional[str] = None,
    provenance: Optional[dict[str, str]] = None,
    runner: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
) -> MuxSpawnResult:
    """Spawn ``name`` as a mux-hosted agent pane (AC1-HP).

    Ordering (Failure Modes: no half-created row):

    1. validate name + provider (front-half reuse).
    2. build the interactive argv; billing guard for claude (AC1-FR).
    3. per-agent flock -> collision check.
    4. ``fno mux pane run`` (self-spawns the server when absent - AC1-EDGE,
       the G1 bind-is-the-lock path). Non-zero exit -> NO pane, NO row, error
       names the mux session; never a daemon-PTY fallback (AC1-ERR).
    5. registry row with ``mux: {session, pane_id}`` (create-after-spawn).
    """
    validate_spawn_name(name)
    # x-8f7f: gate the PANE path on PANE_HOSTABLE_PROVIDERS, not KNOWN_PROVIDERS.
    # A pane host only needs an interactive argv (build_pane_argv) - not a full
    # Python dispatch adapter. agy is exactly that case (Rust-only provider, no
    # Python adapter, but pane-hostable), so widening the global KNOWN_PROVIDERS
    # would leak it into headless/bg Python dispatch that has no agy codepath.
    if provider not in PANE_HOSTABLE_PROVIDERS:
        raise DispatchAskError(
            f"unknown provider {provider!r}; pane-hostable providers: "
            f"{', '.join(PANE_HOSTABLE_PROVIDERS)}",
            exit_code=2,
        )

    session = resolve_mux_session(session)
    session_uuid = str(_uuid.uuid4()) if provider == "claude" else None
    argv = build_pane_argv(provider, message, cwd, yolo, session_uuid, model)
    if provider == "claude" and not claude_argv_is_interactive(argv):
        raise DispatchAskError(
            "refusing to pane-host claude with -p/--print (that bills the "
            "Agent SDK pool); the mux spawns interactive subscription-billed "
            "claude",
            exit_code=2,
        )
    # QoS (x-c5cc): demote the provider command INSIDE the env wrapper —
    # wrapping outermost would break the mux server's FNO_NODE provenance
    # parse, which is anchored on argv[0] == "env" (server.rs node_from_argv).
    # env(1) applies its assignments and then execs taskpolicy/nice -> provider.
    from fno.agents.spawn_gate import qos_wrap

    wrapped = _mesh_env_wrapper(name, provider, role, qos_wrap(argv), provenance)

    registry_path = paths.agents_registry_path()

    def _on_wait() -> None:
        print(f"Waiting for agent {name!r} lock...", file=sys.stderr, flush=True)

    with hold_agent_lock(name, registry_path, on_wait=_on_wait):
        try:
            entries = load_registry()
        except (OSError, ValueError, RegistryVersionError) as exc:
            raise DispatchAskError(
                f"registry read failed: {exc}", exit_code=12
            ) from exc
        if any(e.name == name for e in entries):
            raise DispatchAskError(
                f"agent {name!r} already exists; "
                f"use 'fno agents rm {name}' first or pick another name",
                exit_code=2,
            )

        # --claim marks the pane writer-claim eligible (agent panes only);
        # mail's live inject holds it around each burst.
        #
        # FNO_MUX_SHELL_INTEGRATION rides the pane-run ENV: the mux server that
        # spawns pane shells reads it, and this pane-run process is
        # what self-spawns the server when absent (client.rs), so the server
        # inherits the config-derived knob. Latched at server birth - an
        # already-running server keeps its value.
        proc = _run_mux(
            [
                "mux",
                "pane",
                "run",
                "--claim",
                "--session",
                session,
                "--cwd",
                str(cwd),
                "--",
                *wrapped,
            ],
            runner,
            env={**os.environ, "FNO_MUX_SHELL_INTEGRATION": _shell_integration()},
        )
        if proc.returncode != 0:
            # G1 contract: non-zero exit == no pane was created, so refusing
            # here leaves no half-created state anywhere (AC1-ERR).
            detail = (proc.stderr or proc.stdout or "").strip()
            raise DispatchAskError(
                f"mux pane spawn failed in session {session!r}: "
                f"{detail or 'no output'} (no registry row written; "
                "there is no daemon-PTY fallback)",
                exit_code=1,
            )
        try:
            pane_id = int((proc.stdout or "").strip().splitlines()[-1])
        except (ValueError, IndexError) as exc:
            raise DispatchAskError(
                f"mux pane run returned unparseable output {proc.stdout!r} "
                f"for session {session!r}; a pane may exist without a "
                f"registry row - inspect with 'fno mux pane ls --session "
                f"{session}'",
                exit_code=1,
            ) from exc

        child_pid = _lookup_child_pid(session, pane_id, runner)
        spawned_by_session, spawned_by_harness, spawned_by_cwd = _capture_parent_edge()

        def _append(rows: list[AgentEntry]) -> list[AgentEntry]:
            rows.append(
                AgentEntry(
                    name=name,
                    provider=provider,
                    cwd=str(cwd),
                    log_path="",
                    claude_session_uuid=session_uuid,
                    status="live",
                    pid=child_pid,
                    mux={"session": session, "pane_id": pane_id},
                    spawned_by_session=spawned_by_session,
                    spawned_by_harness=spawned_by_harness,
                    spawned_by_cwd=spawned_by_cwd,
                )
            )
            return rows

        update_registry(_append, path=registry_path)

    return MuxSpawnResult(
        name=name,
        provider=provider,
        session=session,
        pane_id=pane_id,
        child_pid=child_pid,
        session_uuid=session_uuid,
    )
