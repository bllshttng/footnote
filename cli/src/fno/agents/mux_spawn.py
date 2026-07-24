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

import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time
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

#: Per-harness default model, keyed by provider (mux_spawn owns this alongside
#: _EFFORT_ALLOWED). A harness appears ONLY when fno must supply a model the
#: harness will not self-default: opencode's providerID/modelID pair needs one,
#: so fno injects the z.ai GLM secondary. claude and codex are omitted ON
#: PURPOSE - each reads its own harness config and self-defaults better than fno
#: can guess, so injecting nothing is correct. An explicit --model always
#: overrides. ponytail: a code table, not a config knob, until the set outgrows
#: a literal.
_PER_HARNESS_DEFAULT_MODEL = {
    "opencode": "z-ai/glm-5.2",
}


@dataclass
class MuxSpawnResult:
    name: str
    provider: str
    session: str
    pane_id: int
    child_pid: Optional[int]
    session_uuid: Optional[str]
    # Claude's 8-hex jobId (``session_uuid[:8]``), the addressable mail handle;
    # "" for providers whose transport key is not short_id (US8).
    short_id: str = ""


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

        return load_settings().mux.shell_integration
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


def permission_pane_tokens(provider: str, mode: str) -> list[str]:
    """Map a ``--permission-mode`` value to provider-native pane argv tokens.

    Fail-closed (Locked Decision 1): an unmappable (provider, value) pair raises
    before any spawn - permissions are a trust boundary, never a silent
    downgrade. agy ``skip`` returns ``[]`` because its argv already carries
    ``--dangerously-skip-permissions`` unconditionally."""
    if not mode:
        raise DispatchAskError("--permission-mode requires a value", exit_code=2)
    if provider == "claude":
        # Exact passthrough; claude's own CLI validates the vocabulary.
        return ["--permission-mode", mode]
    if provider == "gemini":
        return ["--yolo"] if mode == "yolo" else ["--approval-mode", mode]
    if provider == "codex":
        if mode == "full-auto":
            return ["--full-auto"]
        if mode == "yolo":
            return ["--dangerously-bypass-approvals-and-sandbox"]
        sandbox, sep, approval = mode.partition(":")
        if sep and sandbox and approval:
            return ["--sandbox", sandbox, "--ask-for-approval", approval]
        raise DispatchAskError(
            f"codex --permission-mode {mode!r} unmappable; use a shortcut "
            "(full-auto, yolo) or the <sandbox>:<approval> form "
            "(e.g. workspace-write:on-request)",
            exit_code=2,
        )
    if provider == "opencode":
        if mode == "auto":
            return ["--auto"]
        raise DispatchAskError(
            f"opencode --permission-mode {mode!r} unmappable; only 'auto' maps "
            "(--auto). Per-tool permissions are config-only (permission table).",
            exit_code=2,
        )
    if provider == "agy":
        if mode == "skip":
            return []
        raise DispatchAskError(
            f"agy --permission-mode {mode!r} unmappable; only 'skip' maps "
            "(--dangerously-skip-permissions). Finer control is config-only "
            "(toolPermission).",
            exit_code=2,
        )
    raise DispatchAskError(f"provider {provider!r} has no permission-mode mapping", exit_code=2)


def tier3_pane_tokens(
    provider: str,
    *,
    add_dir: Optional[str] = None,
    agent: Optional[str] = None,
    tools: Optional[str] = None,
    deny_tools: Optional[str] = None,
) -> list[str]:
    """Map the Tier-3 harness-native passthrough flags to provider-native pane
    argv tokens (x-b6e2), in a fixed order (add-dir, agent, allowedTools,
    disallowedTools). Fail-closed per cell: a set flag with no equivalent for
    ``provider`` raises before spawn - never a silent drop. An empty/None value
    is unset (no token). Mirrors the Rust HarnessFlags mapping + the client.rs
    guard, so pane and bg/headless agree on which cells exist."""

    def unsupported(flag: str) -> "list[str]":
        raise DispatchAskError(
            f"{flag} is not supported for provider {provider!r}; drop it or pick "
            "a provider that maps it",
            exit_code=2,
        )

    out: list[str] = []
    # --add-dir: claude/codex/agy grant extra write access. opencode --dir SETS
    # cwd (not additive) and gemini is unverified, so both fail closed.
    if add_dir:
        if provider in ("claude", "codex", "agy"):
            out += ["--add-dir", add_dir]
        else:
            unsupported("--add-dir")
    # --agent: claude and opencode select a sub-agent by name.
    if agent:
        if provider in ("claude", "opencode"):
            out += ["--agent", agent]
        else:
            unsupported("--agent")
    # --tools / --deny-tools: claude only (--allowedTools / --disallowedTools).
    # codex/opencode tool scope is a different axis (sandbox / config presets).
    if tools:
        if provider == "claude":
            out += ["--allowedTools", tools]
        else:
            unsupported("--tools")
    if deny_tools:
        if provider == "claude":
            out += ["--disallowedTools", deny_tools]
        else:
            unsupported("--deny-tools")
    return out


_EFFORT_SUPERSET = frozenset({"minimal", "low", "medium", "high", "xhigh", "max"})
_EFFORT_ALLOWED = {
    "claude": frozenset({"low", "medium", "high", "xhigh", "max"}),
    "codex": frozenset({"minimal", "low", "medium", "high", "xhigh"}),
    "opencode": _EFFORT_SUPERSET,
}


def effort_tokens(provider: str, value: str) -> list[str]:
    """Validate effort and return the provider-native argv tokens."""
    if not value:
        raise DispatchAskError("--effort requires a value", exit_code=2)
    if value not in _EFFORT_SUPERSET:
        raise DispatchAskError(
            f"--effort {value!r} unknown; valid: {', '.join(sorted(_EFFORT_SUPERSET))}",
            exit_code=2,
        )
    allowed = _EFFORT_ALLOWED.get(provider)
    if allowed is None:
        raise DispatchAskError(
            f"provider {provider!r} has no reasoning-effort surface; omit --effort",
            exit_code=2,
        )
    if value not in allowed:
        raise DispatchAskError(
            f"{provider} --effort {value!r} unmappable; {provider} supports "
            f"{', '.join(sorted(allowed))}",
            exit_code=2,
        )
    if provider == "claude":
        return ["--effort", value]
    if provider == "codex":
        return ["-c", f"model_reasoning_effort={value}"]
    return []


def apply_opencode_variant(model: str, effort: str, *, state_path: Optional[Path] = None) -> None:
    """Best-effort atomic update of opencode's persisted model variant."""
    path = state_path or Path.home() / ".local" / "state" / "opencode" / "model.json"
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        locks_dir = path.parent / "locks"
        locks_dir.mkdir(parents=True, exist_ok=True)
        with (locks_dir / "model.json.lock").open("a") as lock:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            data = json.loads(path.read_text()) if path.exists() else {}
            if not isinstance(data, dict):
                raise ValueError("model state is not an object")
            variants = data.setdefault("variant", {})
            if not isinstance(variants, dict):
                raise ValueError("variant is not an object")
            variants[model] = effort
            tmp_path = None
            try:
                with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as tmp:
                    json.dump(data, tmp, separators=(",", ":"))
                    tmp.flush()
                    os.fsync(tmp.fileno())
                    tmp_path = Path(tmp.name)
                os.replace(tmp_path, path)
                tmp_path = None
            finally:
                if tmp_path is not None:
                    try:
                        tmp_path.unlink(missing_ok=True)
                    except OSError:
                        pass
    except (OSError, ValueError, TypeError, json.JSONDecodeError) as exc:
        print(f"warning: could not set opencode effort variant: {exc}", file=sys.stderr)


#: Bounds the post-spawn session-id poll. opencode writes its session row some
#: time after the TUI starts, so the capture is best-effort by construction: two
#: cheap tries keep the added spawn latency near zero and a miss is recorded, not
#: retried into a stall.
_OPENCODE_BACKFILL_ATTEMPTS = 2
_OPENCODE_BACKFILL_DELAY_S = 0.4
_OPENCODE_DB_TIMEOUT_S = 5.0

#: A bare opencode session id on its own output line. Matching this (rather than
#: reading the first line) skips both the `id` column header and the plugin
#: banners opencode prints to stdout ahead of real output.
_SES_ID_RE = re.compile(r"^ses_[A-Za-z0-9]+$")


def _query_opencode_sessions(sql: str, runner: Optional[Callable] = None) -> Optional[list[str]]:
    """Run one read-only store query, returning the session ids it printed.

    ``None`` means the query could not be run at all (binary missing, timeout,
    nonzero exit) as distinct from ``[]`` (ran clean, matched nothing) - the
    caller treats both as "do not stamp", but only the latter is a real answer.
    """
    run = runner or subprocess.run
    try:
        proc = run(
            ["opencode", "db", sql],
            capture_output=True,
            text=True,
            timeout=_OPENCODE_DB_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    return [ln.strip() for ln in (proc.stdout or "").splitlines() if _SES_ID_RE.match(ln.strip())]


def _backfill_opencode_session_id(
    cwd: Path,
    since_ms: int,
    *,
    runner: Optional[Callable] = None,
    sleep: Optional[Callable] = None,
) -> Optional[str]:
    """Best-effort capture of a freshly spawned pane's opencode session id.

    opencode's ``--session`` only continues an EXISTING session, so an id cannot
    be minted ahead of the spawn the way claude's uuid is; it has to be
    discovered afterwards. A session is ours only if it was created after we
    spawned AND its directory is exactly our pane cwd - matched on the directory
    string, never opencode's project id, which several worktrees of one repo
    share.

    Returns the id only on an unambiguous match. Zero candidates (or two, from a
    same-cwd race) return ``None`` so the row stays live-only rather than
    carrying a session id that may belong to another pane.
    """
    naptime = sleep or time.sleep
    escaped = str(cwd).replace("'", "''")
    sql = (
        "select id from session "
        f"where directory='{escaped}' and time_created >= {int(since_ms)}"
    )
    for attempt in range(_OPENCODE_BACKFILL_ATTEMPTS):
        if attempt:
            naptime(_OPENCODE_BACKFILL_DELAY_S)
        ids = _query_opencode_sessions(sql, runner)
        if ids and len(ids) == 1:
            return ids[0]
        if ids and len(ids) > 1:
            return None  # ambiguous; retrying cannot narrow it
    return None


def build_pane_argv(
    provider: str,
    message: str,
    cwd: Path,
    yolo: bool,
    session_uuid: Optional[str],
    model: Optional[str] = None,
    permission_mode: Optional[str] = None,
    effort: Optional[str] = None,
    add_dir: Optional[str] = None,
    agent: Optional[str] = None,
    tools: Optional[str] = None,
    deny_tools: Optional[str] = None,
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
    # x-b6e2: resolve the Tier-3 passthrough tokens once, up front, so an
    # unmappable (provider, flag) cell fails closed BEFORE any provider arm builds
    # an argv. Supported cells return the tokens; every arm splices them in below.
    tier3 = tier3_pane_tokens(
        provider, add_dir=add_dir, agent=agent, tools=tools, deny_tools=deny_tools
    )
    if provider == "claude":
        # `claude --session-id <uuid> [message]`: the pinned session id makes
        # the transcript discoverable and keys the inside-leg reports
        # (handle_report matches claude_session_uuid).
        argv = ["claude"]
        if session_uuid:
            argv += ["--session-id", session_uuid]
        if model:
            argv += ["--model", model]
        if permission_mode:
            argv += permission_pane_tokens("claude", permission_mode)
        elif yolo:
            # AC4-HP: claude --yolo now means bypassPermissions (was a no-op).
            argv += ["--permission-mode", "bypassPermissions"]
        if effort:
            argv += effort_tokens("claude", effort)
        argv += tier3
        if message:
            argv.append(message)
        return argv
    if provider == "codex":
        # `codex [OPTIONS] [PROMPT]` with no subcommand is the interactive CLI.
        argv = ["codex", "-C", str(cwd)]
        if permission_mode:
            argv += permission_pane_tokens("codex", permission_mode)
        else:
            argv += (
                ["--dangerously-bypass-approvals-and-sandbox"]
                if yolo
                else ["--sandbox", "workspace-write"]
            )
        if model:
            argv += ["--model", model]
        if effort:
            argv += effort_tokens("codex", effort)
        argv += tier3
        if message:
            argv.append(message)
        return argv
    if provider == "gemini":
        if effort:
            effort_tokens("gemini", effort)
        # `-i` executes the prompt then stays interactive; --skip-trust avoids
        # the workspace-trust modal blocking the TUI.
        argv = ["gemini", "--skip-trust"]
        if model:
            argv += ["--model", model]
        if message:
            argv += ["-i", message]
        if permission_mode:
            argv += permission_pane_tokens("gemini", permission_mode)
        else:
            argv += ["--yolo"] if yolo else ["--approval-mode", "default"]
        return argv
    if provider == "agy":
        if effort:
            effort_tokens("agy", effort)
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
        if permission_mode:
            # skip -> [] (argv already carries the flag); anything else raises.
            argv += permission_pane_tokens("agy", permission_mode)
        if model:
            argv += ["--model", model]
        argv += tier3
        if message:
            argv.append(message)
        return argv
    if provider == "opencode":
        if effort:
            effort_tokens("opencode", effort)
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
        # opencode expects the provider/model form. An explicit --model wins,
        # else the per-harness default table (opencode is the only entry);
        # inject nothing if the table has no entry for this provider.
        _default_model = model or _PER_HARNESS_DEFAULT_MODEL.get(provider)
        if _default_model:
            argv += ["--model", _default_model]
        argv += tier3
        if permission_mode:
            argv += permission_pane_tokens("opencode", permission_mode)
        elif yolo:
            argv.append("--auto")
        return argv
    raise DispatchAskError(f"provider {provider!r} has no interactive pane form", exit_code=2)


def _mesh_env_wrapper(
    name: str,
    provider: str,
    role: Optional[str],
    argv: list[str],
    provenance: Optional[dict[str, str]] = None,
    account_env: Optional[dict[str, str]] = None,
    route_env: Optional[dict[str, str]] = None,
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
    if role or route_env:
        route = route_env
        if route is None:
            from fno.agents.model_routing import resolve_route

            route = resolve_route(role)
        if route:
            # Scrub the parent's Anthropic creds so the routed AUTH_TOKEN wins:
            # a lingering API key or subscription OAuth token would otherwise
            # override it and send the routed pane back to Anthropic. `env -u`
            # on an unset var is a harmless no-op.
            unset = ["-u", "ANTHROPIC_API_KEY", "-u", "CLAUDE_CODE_OAUTH_TOKEN"]
            pairs += [f"{k}={v}" for k, v in route.items()]
    # Set-or-clear the whole triple, never merge. A pane spawned from a
    # node-bound worker inherits that worker's env, so adding only what this
    # spawn resolved would leave an ad-hoc pane carrying the parent's FNO_NODE
    # and a plan-less child carrying the parent's FNO_PLAN - which ambient
    # origin capture would then persist into every node the pane files.
    # `env -u` on an unset var is a harmless no-op.
    resolved_prov = {k: v for k, v in (provenance or {}).items() if v}
    for _k in PROVENANCE_KEYS:
        if _k not in resolved_prov:
            unset += ["-u", _k]
    pairs += [f"{k}={v}" for k, v in resolved_prov.items()]
    # Per-spawn account overlay (x-d012), applied LAST so an explicit --account
    # beats a stale parent CLAUDE_CONFIG_DIR (env(1) assignments are
    # left-to-right, last wins). --account + --route/--role is refused at the
    # CLI (contradictory axes), so the role-route pairs above are never present
    # on an --account spawn - the two never co-occur here. SCRUB inherited auth
    # vars (env -u) so an ambient ANTHROPIC_API_KEY/CLAUDE_CODE_OAUTH_TOKEN can't
    # override the account's own login and bill the wrong account.
    if account_env:
        from fno.agents.account_env import SCRUB_AUTH_VARS

        for _k in SCRUB_AUTH_VARS:
            unset += ["-u", _k]
        pairs += [f"{k}={v}" for k, v in account_env.items()]
    return ["env", *unset, *pairs, *argv]


#: The provenance env keys, as one set. Callers that export them must set or
#: clear the whole triple together so a child never sees a mix of its own node
#: and its parent's slug/plan.
PROVENANCE_KEYS: tuple[str, ...] = ("FNO_NODE", "FNO_SLUG", "FNO_PLAN")


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
    # The graph read also NORMALIZES a slug input to an id. Skipping it when the
    # caller supplied slug+plan would export FNO_NODE=<slug>, and the ambient
    # origin-capture consumer matches ids exactly - so a slug-driven spawn would
    # silently file its nodes with no origin at all.
    from fno.graph._constants import has_node_id_prefix, is_wellformed_node_id

    if slug is None or plan is None or not has_node_id_prefix(node):
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
        except Exception as e:
            # A graph read failure must not block the spawn -- but it must not
            # degrade a SLUG into FNO_NODE=<slug> either. The origin-capture
            # consumer matches ids exactly and would drop a slug as an unknown
            # node, blaming a bad id for what was a read failure. Keep `node`
            # only when it is a STRICTLY well-formed id (hex suffix); the liberal
            # has_node_id_prefix admits a title-derived slug like `x-marks-the-
            # spot`, which would leak right back into FNO_NODE. An absent-but-
            # well-formed id is still dropped downstream by the capture side's
            # known-ids check, so strict-here is safe. Never re-raise: the pane
            # path degrades. Log under FNO_DEBUG so a missing-origin node is
            # traceable to the read failure rather than being silently invisible.
            if not is_wellformed_node_id(node):
                if os.environ.get("FNO_DEBUG"):
                    print(
                        f"resolve_provenance: graph read failed ({type(e).__name__}); "
                        f"dropping unresolved node '{node}' from provenance",
                        file=sys.stderr,
                    )
                node = None
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
            f"fno mux did not answer within {_MUX_SUBPROCESS_TIMEOUT_S}s ({' '.join(args[:3])}...)",
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
        proc = _run_mux(["mux", "pane", "ls", "--session", session, "--json"], runner)
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
    permission_mode: Optional[str] = None,
    effort: Optional[str] = None,
    add_dir: Optional[str] = None,
    agent: Optional[str] = None,
    tools: Optional[str] = None,
    deny_tools: Optional[str] = None,
    session: Optional[str] = None,
    squad: Optional[str] = None,
    split: Optional[str] = None,
    crown_level: Optional[int] = None,
    crown_scope: Optional[str] = None,
    provenance: Optional[dict[str, str]] = None,
    account_env: Optional[dict[str, str]] = None,
    route_env: Optional[dict[str, str]] = None,
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

    if provider == "claude" and (role is not None or route_env):
        from fno.agents.model_routing import (
            RouteCompositionError,
            resolve_spawn_route,
        )

        try:
            route_env = resolve_spawn_route(role, route_env)
        except RouteCompositionError as exc:
            raise DispatchAskError(str(exc), exit_code=2) from exc

    session = resolve_mux_session(session)
    session_uuid = str(_uuid.uuid4()) if provider == "claude" else None
    # Read before the pane exists so the opencode backfill can only ever match a
    # session this spawn created, never one already open in the same cwd.
    spawn_started_ms = int(time.time() * 1000)
    argv = build_pane_argv(
        provider,
        message,
        cwd,
        yolo,
        session_uuid,
        model,
        permission_mode,
        effort,
        add_dir=add_dir,
        agent=agent,
        tools=tools,
        deny_tools=deny_tools,
    )
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

    wrapped = _mesh_env_wrapper(
        name, provider, role, qos_wrap(argv), provenance, account_env, route_env
    )

    registry_path = paths.agents_registry_path()

    def _on_wait() -> None:
        print(f"Waiting for agent {name!r} lock...", file=sys.stderr, flush=True)

    with hold_agent_lock(name, registry_path, on_wait=_on_wait):
        try:
            entries = load_registry()
        except (OSError, ValueError, RegistryVersionError) as exc:
            raise DispatchAskError(f"registry read failed: {exc}", exit_code=12) from exc
        if any(e.name == name for e in entries):
            raise DispatchAskError(
                f"agent {name!r} already exists; "
                f"use 'fno agents rm {name}' first or pick another name",
                exit_code=2,
            )
        if provider == "opencode" and effort:
            _variant_model = model or _PER_HARNESS_DEFAULT_MODEL.get(provider)
            if _variant_model:
                apply_opencode_variant(_variant_model, effort)

        # --claim marks the pane writer-claim eligible (agent panes only);
        # mail's live inject holds it around each burst.
        #
        # FNO_MUX_SHELL_INTEGRATION rides the pane-run ENV: the mux server that
        # spawns pane shells reads it, and this pane-run process is
        # what self-spawns the server when absent (client.rs), so the server
        # inherits the config-derived knob. Latched at server birth - an
        # already-running server keeps its value.
        # Placement directives ride the OUTER pane-run transport, before the `--`
        # that fences the provider argv (x-3e38). build_pane_argv stays
        # placement-blind so provider-native commands are never contaminated.
        placement_args: list[str] = []
        if squad:
            placement_args += ["squad", squad]
        if split:
            placement_args += ["split", split]
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
                *placement_args,
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

        # opencode ids are discovered, not minted (see _backfill_opencode_session_id).
        # A miss leaves the row exactly as live-only as before capture existed,
        # so it is logged rather than raised - the pane is already running and a
        # missing id costs resume, not the spawn.
        if provider == "opencode":
            # Reuses the spawn `runner` seam, so the store read is stubbed by
            # the same fake every spawn test already installs and the suite
            # never touches the real ~/.local/share/opencode.
            session_uuid = _backfill_opencode_session_id(cwd, spawn_started_ms, runner=runner)
            if session_uuid is None:
                from fno.agents import events as _events

                _events.emit(
                    "agent_session_id_uncaptured",
                    name=name,
                    harness=provider,
                    cwd=str(cwd),
                    reason="no unique opencode session for this cwd after spawn",
                )

        # Claude addresses a pane by its 8-hex jobId (the first block of the
        # session UUID). The row does NOT store it in short_id - that field is
        # the worker/bg transport slot, and a mux row must hold exactly one live
        # ref (validate_single_live_ref). The jobId resolves back to this row via
        # resolve_agent's derived_short rule (harness_session_id[:8]), so the
        # receipt can hand the king a usable mail handle without touching the row
        # (US8). Empty for providers that resume off harness_session_id.
        short_id_val = session_uuid[:8] if provider == "claude" and session_uuid else ""

        # Crown stamp (US9): the grantor is the spawning session (the parent edge
        # captured above), or "human" for a direct human spawn with no session
        # env - never a caller-supplied value. Only stamped when a crown was
        # actually requested (crown_level is not None).
        crown_grantor_val = (spawned_by_session or "human") if crown_level is not None else None

        def _append(rows: list[AgentEntry]) -> list[AgentEntry]:
            # Claim check, inside the registry write lock so it is atomic with
            # the stamp. Two panes racing in one cwd can each see the SAME lone
            # candidate (the second pane's session may not exist yet when both
            # query), and the ambiguity rule cannot catch that - it only sees one
            # row. Whichever append lands first owns the id; the loser drops to
            # live-only rather than pointing resume at another pane's session.
            claimed = session_uuid is not None and any(
                r.harness_session_id == session_uuid for r in rows
            )
            rows.append(
                AgentEntry(
                    name=name,
                    harness=provider,
                    cwd=str(cwd),
                    log_path="",
                    harness_session_id=None if claimed else session_uuid,
                    status="live",
                    pid=child_pid,
                    mux={"session": session, "pane_id": pane_id},
                    spawned_by_session=spawned_by_session,
                    spawned_by_harness=spawned_by_harness,
                    spawned_by_cwd=spawned_by_cwd,
                    crown_level=crown_level,
                    crown_scope=crown_scope,
                    crown_grantor=crown_grantor_val,
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
        short_id=short_id_val,
    )
