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
(crates/fno pty.rs); the mesh identity (``FNO_AGENT_SELF``/``FNO_AGENT_HARNESS``)
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
import shutil
import subprocess
import sys
import tempfile
import time
import uuid as _uuid
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Callable, Mapping, Optional, Sequence

from fno import paths
from fno.agents.dispatch import (
    DispatchAskError,
    _capture_parent_edge,
    _capture_spawn_trigger,
    _touch_log_path,
    validate_spawn_name,
)
from fno.agents.harness_map import DispatchResolveError, normalize_command
from fno.agents.lock import hold_agent_lock
from fno.agents.registry import (
    AgentEntry,
    AgentResolutionError,
    AgentStatus,
    RegistryVersionError,
    TERMINAL_STATUSES,
    _has_resolvable_handle,
    load_registry,
    update_registry,
)
from fno.agents.crown import (
    calling_agent_row,
    crown_validation_error,
    grant_error,
)
from fno.agents.whoami import is_caller_row

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
    "opencode": "z-ai/glm-5.3",
}


@dataclass
class MuxSpawnResult:
    name: str
    provider: str
    session: str
    pane_id: int
    child_pid: Optional[int]
    session_uuid: Optional[str]
    # The addressable mail handle, derived from ``session_uuid`` by
    # ``harness_identity.canonical_handle`` (the single source for that string -
    # do not restate which slice it takes); "" for providers whose transport key
    # is not short_id (US8).
    short_id: str = ""
    # A Codex pane whose rollout has not appeared yet is created but not
    # addressable. Keep that transition explicit instead of calling it live.
    status: str = "live"
    # Two facts that must never collapse into one field. `bound` answers "did
    # the worker bind a session identity", true only when one was actually
    # obtained. `pane_alive` answers "does the pane exist", probed and never
    # assumed (None = the mux could not answer).
    # Before these existed, a pane that would bind in 4s and one that had
    # already died produced byte-identical receipts, so a caller could not tell
    # a slow worker from a corpse and would re-prompt the corpse.
    #
    # THREE values, not two. None means this harness has no spawn-time session
    # identity at all (see _SESSION_BINDING_HARNESSES), so the spawn asserts
    # nothing rather than inventing an answer - claiming True there would be the
    # same unverified "it's live" this change exists to remove. A consumer
    # treats False as doubt and None as "no better than before".
    #
    # It defaults to None for the same reason: a field whose job is to carry
    # doubt must not default to certainty.
    #
    # For the harnesses where short_id IS the handle, `short_id` is a PROJECTION
    # of `bound`, never an independent claim: `bound == bool(short_id)` is an
    # invariant with a test on it, which is what makes an empty short_id a
    # signal rather than a formatting detail.
    bound: Optional[bool] = None
    pane_alive: Optional[bool] = None
    # Why this spawn is unbound, in the receipt whenever `bound` is False. Never
    # set when bound.
    unbound_reason: Optional[str] = None
    # Captured pane output for a worker that died before binding. The pane's
    # scrollback dies with the pane (the mux drops the pane registry entry in
    # close_pane), so this file is the only evidence the death ever leaves.
    log_path: str = ""
    effective_message: Optional[str] = None
    # Server-authored exact-placement receipt (x-6928): anchor/direction/fallback
    # + squad/tab the split landed in. None unless `--at` pinned the origin.
    placement: Optional[dict] = None
    # LD5: true only when this pane was adopted after `pane run`'s
    # control read went unanswered. A painted frame plus a captured session id
    # proves the provider booted with the argv it was given; it does NOT prove
    # the prompt was consumed (x-7ebd is the sibling failure for that). Callers
    # must render this receipt as "recovered", never "spawned".
    recovered: bool = False
    readiness: Optional[str] = None
    readiness_rule: Optional[str] = None


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


def happy_routed_panes_enabled() -> bool:
    """Whether routed claude panes launch through ``happy``.

    This defaults off because installing and pairing happy is machine-local.
    """
    try:
        from fno.config import _candidate_paths, _load_raw, load_settings

        for candidate in _candidate_paths():
            if candidate.is_file() and not _load_raw(candidate)[1]:
                raise ValueError(f"could not parse {candidate}")

        return load_settings().agents.happy_routed_panes
    except Exception as exc:
        raise DispatchAskError(
            "could not read config.agents.happy_routed_panes; refusing the "
            "routed pane rather than silently launching an unmonitored worker",
            exit_code=2,
        ) from exc


def resolve_monitor(
    explicit: Optional[str],
    *,
    harness: str,
    route_provider: Optional[str],
    route_env: Optional[Mapping[str, str]],
    account_env: Optional[Mapping[str, str]] = None,
    model: Optional[str] = None,
) -> str:
    """Resolve the one supported monitor without widening legacy routing."""
    if explicit is not None:
        if explicit != "happy":
            raise DispatchAskError(
                f"--monitor must be 'happy' (got {explicit!r})",
                exit_code=2,
            )
        if harness != "claude":
            raise DispatchAskError(
                f"--monitor happy requires the claude harness; got {harness!r}",
                exit_code=2,
            )
        if model is not None:
            raise DispatchAskError(
                "--monitor happy refuses a separate --model override; the model "
                "must come from the resolved zai route",
                exit_code=2,
            )
        required_route_keys = (
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_MODEL",
        )
        explicit_route = route_env or {}
        missing_route_keys = [
            key
            for key in required_route_keys
            if not str(explicit_route.get(key, "")).strip()
        ]
        if route_provider != "zai" or missing_route_keys:
            detail = (
                f"; missing {', '.join(missing_route_keys)}"
                if missing_route_keys
                else ""
            )
            raise DispatchAskError(
                "--monitor happy currently requires a complete resolved zai route"
                f"{detail}",
                exit_code=2,
            )
        conflicting_credentials = [
            key
            for key in ("ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN")
            if any(
                str(source.get(key, "")).strip()
                for source in (explicit_route, account_env or {})
            )
        ]
        if conflicting_credentials:
            raise DispatchAskError(
                "--monitor happy refuses a resolved zai route with a conflicting "
                f"Anthropic credential: {', '.join(conflicting_credentials)}",
                exit_code=2,
            )
        from fno.agents.model_routing import resolve_explicit_route

        expected_route = resolve_explicit_route(
            "zai", str(explicit_route["ANTHROPIC_MODEL"])
        )
        if expected_route is None or dict(explicit_route) != expected_route:
            raise DispatchAskError(
                "--monitor happy currently requires a complete resolved zai route",
                exit_code=2,
            )
        return "happy"
    if harness == "claude" and route_env and happy_routed_panes_enabled():
        return "happy"
    return "none"


def happy_pane_argv(
    argv: list[str],
    route_env: Mapping[str, str],
    *,
    explicit: bool = False,
) -> list[str]:
    """Carry a routed claude pane through happy without losing its endpoint.

    happy reserves ``--settings`` for its hook server and discards a caller's
    file, so a routed value cannot reach the child through a settings file. It
    consumes ``--session-id`` the same way: the session id belongs to happy on
    this route and is discovered after the spawn, never pinned before it.

    ``--settings`` being closed does NOT make ``--claude-env`` the only channel,
    which is what this docstring used to claim. happy's ``claudeLocal`` builds
    its child env as ``{...process.env, ...claudeEnvVars}`` -- an overlay on
    inheritance, not a replacement for it -- and the bundle never reads, sets,
    or scrubs any ``ANTHROPIC_*`` var (its sole occurrence of that string is a
    usage example in ``--help``). So happy's OWN environment reaches claude
    intact, and :func:`_mesh_env_wrapper` already puts every ``route_env`` key
    there via ``env(1)``, since it wraps the argv this function returns. Each
    route key therefore reaches the child twice, and the ``--claude-env`` copy
    is the redundant one.

    That redundancy is not free: ``env(1)`` execs, so its assignments vanish
    from the process image, while happy is a long-lived node parent whose argv
    survives for the whole session. A credential on ``--claude-env`` is a
    world-readable ``ps`` token for as long as the worker lives, and from there
    it reaches transcripts and screen capture -- durable stores that leave the
    box. So the carry below drops :data:`SECRET_ROUTE_VARS` and keeps the rest.
    Dropping them is behavior-preserving by happy's merge semantics above; it is
    not a tradeoff, and the credential is not "lost" but carried by the wrapper
    that was always carrying it.

    What this buys is precisely "no LONG-LIVED argv token", NOT "no argv token".
    Be exact here, because the next reader will treat this as the security
    contract: the wrapper is handed to ``mux pane run`` as ``-- env
    ANTHROPIC_AUTH_TOKEN=... happy ...``, so the credential is still on that
    client process's command line for the ``pane run`` RPC alone, and on the
    pane's own ``env`` command line until it execs. The readiness gate is NOT
    part of that window: ``_run_mux`` has returned (and the credential-bearing
    process exited) before :func:`_await_interactive_readiness` runs, and that
    gate probes through separate ``pane wait`` / ``pane read`` subprocesses
    carrying no route at all -- a non-exact spawn does not run it. Both windows
    are bounded by one subprocess each, against a whole worker session before
    this change; neither duration is independently measured, so do not restate
    them as a number. Closing the client-side window needs the assignments to
    travel over the mux IPC instead of the command line, which is a Rust
    protocol change and is tracked separately.

    These refusals are on the ONLY reachable happy path, so the usual "a guard
    on one of N paths is decorative" audit does not apply here and does not need
    re-running: this function has exactly one production caller (the
    ``resolved_monitor == "happy"`` branch below), there is no happy launcher in
    the Rust crates or the shell dispatchers, and the ``--monitor happy`` flag
    itself already requires pane + claude + zai and refuses a separate
    ``--model``. Re-verify with an anchored sweep (``grep -rn happy_pane_argv``
    over explicit trees, not an ``rg`` glob exclude) if that branch ever grows a
    sibling.

    The carry below is lossless at the CHILD, not at this function: the suite
    holds the fully wrapped argv to a union invariant (every route key rides
    either the ``env(1)`` run or ``--claude-env``; no secret rides
    ``--claude-env``; every secret rides the ``env(1)`` run). Asserting on this
    function's output alone cannot tell "the secret moved to the wrapper" from
    "the secret was dropped", and those two differ by a 401 in production and by
    nothing in a test.
    """
    if any(arg == "--settings" or arg.startswith("--settings=") for arg in argv):
        raise DispatchAskError(
            "refusing to launch a routed claude pane through happy with "
            "--settings: happy consumes that flag for its own hook server and "
            "discards the caller's file, so the route would be silently ignored "
            "and the worker would launch on the default account. Carry the route "
            "in the env(1) wrapper (happy merges its own environment into the "
            "claude child), with the non-secret remainder as --claude-env "
            "KEY=VALUE. Never put a credential on --claude-env: happy is a "
            "long-lived parent, so its argv is a world-readable `ps` token.",
            exit_code=2,
        )
    if any(arg == "--session-id" or arg.startswith("--session-id=") for arg in argv):
        raise DispatchAskError(
            "refusing to launch a claude pane through happy with --session-id: "
            "happy extracts that flag out of the argv and never re-adds it under "
            "its hook server, so claude mints a different id and the spawn receipt "
            "would name a session that never exists (unpeekable worker, id-less "
            "registry row). Let happy own the id; fno discovers it afterwards.",
            exit_code=2,
        )
    if shutil.which("happy") is None:
        if explicit:
            raise DispatchAskError(
                "--monitor happy was requested, but 'happy' is not on PATH; "
                "install it with npm install -g happy",
                exit_code=127,
            )
        raise DispatchAskError(
            "config.agents.happy_routed_panes is on, but 'happy' is not on PATH; "
            "a routed claude pane is invisible to the Claude app's remote view "
            "without it. Install it (npm install -g happy) or set "
            "config.agents.happy_routed_panes = false to accept a local-only pane.",
            exit_code=127,
        )
    from fno.agents.account_env import SECRET_ROUTE_VARS

    claude_env: list[str] = []
    for key, value in route_env.items():
        if key in SECRET_ROUTE_VARS:
            continue
        claude_env += ["--claude-env", f"{key}={value}"]
    return ["happy", *claude_env, *argv[1:]]


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


# Codex writes its rollout at session start, but the TUI has to boot first, so
# the first look can legitimately land before the file exists. More attempts and
# a longer nap than opencode's SQLite read for exactly that reason. One extra
# attempt buys the stability gate in _backfill_codex_session_id its confirmation
# probe without shrinking the capture window (Codex P2, #603).
_CODEX_BACKFILL_ATTEMPTS = 5
_CODEX_BACKFILL_DELAY_S = 0.75


def _codex_session_id_for_pid(pid: int, *, psutil_mod=None) -> Optional[str]:
    """The codex TUI's session id, read race-free from an open rollout in the
    pane's own process tree.

    codex holds its rollout fd open and the first-line session_meta carries the
    id, so the pane's process identifies its session deterministically: each
    pane's tree holds a distinct rollout, so a same-cwd sibling can never be
    mis-identified (Codex P1, #603). The pane pid AND its descendants are
    inspected: a wrapper launcher (the @openai/codex Node shim) holds the pane
    pid while its native child opens the rollout (Codex P1, #603 r5). The id
    comes from session_meta.payload.id, not the filename UUID, which is not
    always the session id in older turn-id layouts (Codex P2, #603 r5).

    Returns None when psutil is unavailable, the process is gone, no rollout is
    open yet, or the tree holds more than one distinct session (ambiguous).
    """
    psu = psutil_mod
    if psu is None:
        try:
            import psutil
        except ImportError:
            return None
        psu = psutil
    try:
        procs = [psu.Process(pid)]
    except Exception:  # noqa: BLE001 -- NoSuchProcess / AccessDenied / ZombieProcess
        return None
    try:
        procs += psu.Process(pid).children(recursive=True)
    except Exception:  # noqa: BLE001 -- a child dying mid-walk yields a partial tree
        pass
    rollout_paths = []
    for proc in procs:
        try:
            files = proc.open_files()
        except Exception:  # noqa: BLE001 -- NoSuchProcess / AccessDenied per proc
            continue
        for f in files:
            base = os.path.basename(f.path)
            if base.startswith("rollout-") and base.endswith(".jsonl"):
                rollout_paths.append(f.path)
    if not rollout_paths:
        return None
    from fno.agents.discover import _codex_session_meta

    ids = set()
    for path in rollout_paths:
        payload = _codex_session_meta(Path(path))
        sid = payload.get("id") if payload else None
        if isinstance(sid, str) and sid:
            ids.add(sid)
    if len(ids) == 1:
        return next(iter(ids))
    return None


def _backfill_codex_session_id(
    cwd: Path,
    since_ms: int,
    *,
    sessions_dir: Optional[Path] = None,
    child_pid: Optional[int] = None,
    sleep: Optional[Callable] = None,
    psutil_mod=None,
    attempts: Optional[int] = None,
) -> Optional[str]:
    """Best-effort capture of a freshly spawned codex pane's session id.

    With the pane's child pid, the id is read race-free from the child's open
    rollout (see :func:`_codex_session_id_for_pid`): codex mints no ``--session``
    id, but its process holds the rollout open and the filename embeds the id, so
    each pane resolves its own session even when two spawn in one cwd at once.
    Without a child pid (direct/test callers only) this falls back to cwd +
    started-after-spawn discovery, stability-gated -- that path cannot fully
    separate a same-cwd sibling and is not used by production spawns.

    A miss leaves the row id-less so ``_discover_from_registry`` drops it before
    name matching, which is what makes a live codex worker unreachable by truth,
    peek, and the mail name lane -- a miss costs addressability, never the spawn.

    ``attempts`` overrides the retry count. The spawn passes 1 because
    :func:`_await_pane_binding` owns the retry there and watches for the pane's
    death between probes, which a bare retry loop cannot see. Direct callers keep
    the default.
    """
    from fno.agents.discover import codex_session_ids_started_in

    naptime = sleep or time.sleep
    tries = _CODEX_BACKFILL_ATTEMPTS if attempts is None else max(int(attempts), 1)
    if child_pid is not None:
        # Race-free primary path: the child's open rollout identifies its own
        # session. codex opens the rollout shortly after start, so retry until it
        # appears; never fall through to cwd/time guessing when we have a pid.
        for attempt in range(tries):
            if attempt:
                naptime(_CODEX_BACKFILL_DELAY_S)
            sid = _codex_session_id_for_pid(child_pid, psutil_mod=psutil_mod)
            if sid is not None:
                return sid
        return None

    # No child pid: direct/test caller only. Two same-cwd panes can surface
    # rollouts out of order, so accept only once the same single id repeats on
    # the next probe -- a transiently-unique sibling then grows to an ambiguous
    # pair (-> None below) or is displaced first (Codex P1 residual, #603).
    prev: Optional[str] = None
    # Floored at 2: the stability gate below accepts an id only when the SAME
    # single id repeats, and `prev` is None on the first pass, so one attempt
    # can never return anything. A caller passing attempts=1 here would get a
    # guaranteed silent miss rather than one honest probe.
    for attempt in range(max(tries, 2)):
        if attempt:
            naptime(_CODEX_BACKFILL_DELAY_S)
        ids = codex_session_ids_started_in(cwd, since_ms, sessions_dir=sessions_dir)
        if len(ids) > 1:
            return None  # ambiguous; retrying cannot narrow it
        if len(ids) == 1 and ids[0] == prev:
            return ids[0]
        prev = ids[0] if ids else None
    return None


def pane_passthrough_tokens(
    passthrough: Optional[Sequence[str]],
    emitted: Sequence[str],
) -> list[str]:
    """Validate `--` passthrough tokens against what fno itself emitted, for
    splicing into a provider arm (x-1caa).

    A passthrough token naming a flag the arm already carries is a named
    refusal, never a silent last-wins: two sources for one value make the
    spawn receipt disagree with the process. ``emitted`` is every option token
    the arm will carry, INCLUDING the tail spliced after the passthrough
    position (gemini/opencode permission tokens), minus the message. The
    ``--`` fence itself and value tokens are not flags; an ``=``-joined flag
    compares by its name.
    """
    if not passthrough:
        return []
    emitted_flags = {
        tok.split("=", 1)[0] for tok in emitted if tok.startswith("-") and tok != "--"
    }
    for tok in passthrough:
        if not tok.startswith("-"):
            continue
        flag = tok.split("=", 1)[0]
        if flag in emitted_flags:
            raise DispatchAskError(
                f"refusing {flag} on both sides: fno emitted it from its own "
                "flag and the passthrough carries it too. Two sources for one "
                "value is the defect, not the collision. Drop one.",
                exit_code=2,
            )
    return list(passthrough)


#: x-1caa: provider tokens that turn a pane into a dead one-shot, promoted from
#: the comments on the arms below into guards now that passthrough hands the
#: operator the exact token each comment warned about. Bare tokens match too -
#: opencode's `run` is a subcommand, not a flag. claude is deliberately absent:
#: its row stays enforced by :func:`claude_argv_is_interactive` so the D2
#: billing guard keeps its name and its existing test. A provider with no row
#: forwards everything on purpose; enumerating each CLI's flag surface is the
#: maintenance burden passthrough exists to remove.
PANE_HEADLESS_FORM_TOKENS: Mapping[str, tuple[str, ...]] = {
    "agy": ("-p", "--print"),  # agy's headless form: prints, exits, kills the pane
    "codex": ("exec",),  # headless subcommand (the --headless help pairs it with claude -p)
    "opencode": ("run",),  # headless subcommand; the positional is a project path
}


def refuse_pane_headless_form(provider: str, argv: Sequence[str]) -> None:
    """Refuse a composed pane argv carrying a provider's headless-form token
    (x-1caa). Checked on the COMPOSED argv next to the claude billing guard, so
    a token reaches this check whichever side of ``--`` it came from."""
    refused = PANE_HEADLESS_FORM_TOKENS.get(provider)
    if not refused:
        return
    for tok in argv:
        if tok in refused:
            raise DispatchAskError(
                f"refusing to pane-host {provider} with {tok!r}: that is "
                f"{provider}'s headless form - it answers once and exits, "
                "killing the pane at birth. Use --substrate headless for a "
                "one-shot.",
                exit_code=2,
            )


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
    name: Optional[str] = None,
    passthrough: Optional[Sequence[str]] = None,
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
    model set via env (``resolve_route``), so explicit intent wins.

    ``name`` is the worker's registry name, forwarded to claude's ``--name``
    (its session DISPLAY name). Parity with the bg lane, which has always
    hardcoded it (``claude_ask.rs`` ``build_argv``). Without it claude falls
    back to a name inherited from the launching session's lineage, so every
    pane worker on one box shows the SAME string in any session list that reads
    it - N distinct workers collapse onto one row and the list cannot route.
    Only claude is wired: the other pane arms have no verified equivalent flag,
    and guessing one fails the spawn rather than degrading.

    ``passthrough`` (x-1caa): tokens after a ``--`` on the spawn command line,
    spliced INSIDE the arm - upstream of the composed-argv refusals in
    :func:`dispatch_spawn_pane` - so they inherit the same guards fno's own
    flags pass through, rather than appending past them. Absent/empty composes
    a byte-identical argv."""
    if message.strip().startswith(("/", "$fno:")):
        message = normalize_command(message, provider)

    from fno.agents.harness_map import render_session_argv

    identity = render_session_argv(provider, "interactive_create", session_uuid)

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
        argv = identity
        if name:
            argv += ["--name", name]
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
        argv += pane_passthrough_tokens(passthrough, argv)
        if message:
            # The seed rides behind `--` so a leading-flag seed ("--model x
            # ...") reaches claude as the prompt positional instead of dying
            # in its flag parser (verified against the real CLI).
            argv += ["--", message]
        return argv
    if provider == "codex":
        # `codex [OPTIONS] [PROMPT]` with no subcommand is the interactive CLI.
        argv = [*identity, "-C", str(cwd)]
        if permission_mode:
            argv += permission_pane_tokens("codex", permission_mode)
        else:
            argv += (
                ["--dangerously-bypass-approvals-and-sandbox"]
                if yolo
                else ["--sandbox", "workspace-write"]
            )
        # Any sandboxed posture (including --full-auto and an explicit
        # <sandbox>:<approval>) inherits codex's read-only .git carveout and
        # cannot commit without the grant. Only the two bypass postures skip it.
        from fno.agents.harnesses.codex import git_writable_args

        bounded = (permission_mode != "yolo") if permission_mode else not yolo
        if bounded:
            argv += git_writable_args(cwd)
        if model:
            argv += ["--model", model]
        if effort:
            argv += effort_tokens("codex", effort)
        argv += tier3
        argv += pane_passthrough_tokens(passthrough, argv)
        if message:
            # Same fence as the claude arm: clap itself prescribes `--` ("to
            # pass ... as a value, use '-- ...'"), so a leading-flag seed is
            # the PROMPT, not a flag.
            argv += ["--", message]
        return argv
    if provider == "gemini":
        if effort:
            effort_tokens("gemini", effort)
        # `-i` executes the prompt then stays interactive; --skip-trust avoids
        # the workspace-trust modal blocking the TUI.
        argv = [*identity, "--skip-trust"]
        if model:
            argv += ["--model", model]
        # Permission tokens are computed before the passthrough splice so the
        # duplicate-flag refusal sees them (they append after the -i pair, past
        # the splice position). Output order is unchanged. The emitted set also
        # carries the -i flag itself (it rides after the splice) and BOTH
        # permission spellings: the axis is always materialized here under one
        # of the two names, so an alias-shaped passthrough (--yolo against an
        # --approval-mode arm or the reverse) is a named refusal, never a
        # silent last-wins.
        perm = (
            permission_pane_tokens("gemini", permission_mode)
            if permission_mode
            else (["--yolo"] if yolo else ["--approval-mode", "default"])
        )
        emitted = [*argv, *perm, "--yolo", "--approval-mode"]
        if message:
            emitted.append("-i")
        argv += pane_passthrough_tokens(passthrough, emitted)
        if message:
            # argv-fence: exempt (gemini CLI deprecated 2026-07-27; the -i
            # value form is pinned by tests and left as-is).
            argv += ["-i", message]
        argv += perm
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
        argv = [*identity, "--dangerously-skip-permissions"]
        if permission_mode:
            # skip -> [] (argv already carries the flag); anything else raises.
            argv += permission_pane_tokens("agy", permission_mode)
        if model:
            argv += ["--model", model]
        argv += tier3
        argv += pane_passthrough_tokens(passthrough, argv)
        if message:
            # Deliberately unfenced: agy has no clean end-of-options. Probed
            # 2026-08-15, `agy -p -- "<prompt>"` folds the flag text AND the
            # fence itself into the prompt, so fencing corrupts the seed; an
            # unfenced leading-flag seed rides into the prompt mangled, not
            # dead. argv-fence: exempt (test_argv_fence_gate honors this marker)
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
        argv = identity
        if message:
            # Equal-form binds the value even when it is flag-shaped; yargs
            # misparses the split form there (probed: usage error on a
            # leading-flag value), so never spell this "--prompt", message.
            argv += [f"--prompt={message}"]
        # opencode expects the provider/model form. An explicit --model wins,
        # else the per-harness default table (opencode is the only entry);
        # inject nothing if the table has no entry for this provider.
        _default_model = model or _PER_HARNESS_DEFAULT_MODEL.get(provider)
        if _default_model:
            argv += ["--model", _default_model]
        argv += tier3
        # Computed before the splice (like gemini) so the duplicate refusal sees
        # the permission tokens that append after the splice position. When the
        # axis is set, --auto's hidden aliases (--yolo /
        # --dangerously-skip-permissions, per the arm comment above) count as
        # the same flag: a passthrough carrying one is a second source.
        perm = (
            permission_pane_tokens("opencode", permission_mode)
            if permission_mode
            else (["--auto"] if yolo else [])
        )
        emitted = [*argv, *perm]
        if perm:
            emitted += ["--yolo", "--dangerously-skip-permissions"]
        argv += pane_passthrough_tokens(passthrough, emitted)
        argv += perm
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
    # FNO_AGENT_ROW_PENDING marks the one substrate whose row is written AFTER
    # the child starts (the pane id does not exist until `mux pane run` returns),
    # so only here may a worker's SessionStart hook wait for its own row. Headless
    # one-shots set FNO_AGENT_SELF too and deliberately never get a row, so
    # keying the wait on FNO_AGENT_SELF alone would make every one-shot sit out
    # the full deadline before its first prompt.
    #
    # It carries the NAME, not a bare flag, and the hook waits only when the two
    # agree. A pane worker that itself launches a headless one-shot passes this
    # whole environment down; every spawn path overwrites FNO_AGENT_SELF but none
    # of them clears an inherited marker, so a bare `=1` would silently re-enable
    # the wait for a nested child that will never have a row. Scoping it to the
    # identity makes the stale value self-invalidating instead of requiring every
    # present and future spawn path to remember to unset it.
    pairs = [
        f"FNO_AGENT_SELF={name}",
        f"FNO_AGENT_HARNESS={provider}",
        f"FNO_AGENT_ROW_PENDING={name}",
    ]
    from fno.setup.github_cli import worker_environment

    proxy_env = worker_environment(os.environ)
    if proxy_env.get("PATH") != os.environ.get("PATH"):
        pairs.append(f"PATH={proxy_env['PATH']}")
    unset: list[str] = []
    # A spawned child inherits its parent's ROUTE (account, model, node
    # provenance below) but never its parent's IDENTITY. An ambient session
    # marker riding through this seam is exactly how a claude worker spawned
    # from a codex parent comes to carry a foreign CODEX_THREAD_ID and resolve
    # as the wrong harness; each harness re-mints its own marker for the child
    # process, so scrubbing the inherited set is lossless. AMBIENT_IDENTITY_ENV
    # is identity-only (the resolver tuples plus the direct-read legacy markers
    # like CLAUDECODE_SESSION_ID), never routing, so an account or auth var
    # cannot be swept here. `env -u` on an unset var is a harmless no-op.
    from fno.harness_identity import ambient_identity_env_unset_args

    unset += ambient_identity_env_unset_args()
    if provider == "claude":
        # Worker parity: transcripts must persist for resume/adoption.
        pairs.append("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1")
        # Raise the harness Stop-hook block cap so fno's repeated-block loop is
        # not force-ended at the default 9 (x-1680). The helper honors an
        # operator-set value, so an explicit env wins over the fno default.
        from fno.agents.harnesses.claude import claude_stop_hook_block_cap

        pairs.append(f"CLAUDE_CODE_STOP_HOOK_BLOCK_CAP={claude_stop_hook_block_cap()}")
    # Per-spawn account overlay (x-d012) and any route compose through ONE
    # function (x-8552), same as bg/headless: scrub inherited auth vars
    # (env -u), layer the account (profile + its own login), layer the route
    # last so it wins endpoint+auth+model as one unit (x-2af5). env(1)
    # assignments are left-to-right last-wins, so rendering the composed
    # overlay as pairs below expresses that order without re-deriving it.
    resolved_route = route_env
    if resolved_route is None and role:
        from fno.agents.model_routing import resolve_spawn_route

        # Through the guarded door, not bare resolve_route: this internal lane
        # is the one place a seam could hand-roll an unguarded resolution, and
        # the completeness refusals must fire on it like every other path.
        resolved_route = resolve_spawn_route(role, None)
    if account_env or resolved_route:
        from fno.agents.account_env import SCRUB_AUTH_VARS, compose_worker_credentials

        composed, _decision = compose_worker_credentials(
            account_env, resolved_route, {}
        )
        # The scrub floor is claude-shaped (SCRUB_AUTH_VARS are all
        # ANTHROPIC_*/CLAUDE_* vars): an OpenAI-protocol route on another
        # harness must not strip unrelated inherited Claude auth. `env -u` on
        # an unset var is a harmless no-op; `unset +=` (not `=`) so the
        # identity/provenance unsets above are preserved.
        if provider == "claude":
            for _k in SCRUB_AUTH_VARS:
                unset += ["-u", _k]
        pairs += [f"{k}={v}" for k, v in composed.items()]
    # The inherited-model scrub runs OUTSIDE the account/route block above: the
    # hole was the unrouted default path, which is most spawns. Claude-shaped
    # like SCRUB_AUTH_VARS (these are all ANTHROPIC_* vars, so stripping them
    # from a non-claude pane would be unrelated); env(1) is left-to-right
    # last-wins and `unset` renders before `pairs`, so a composed route's own
    # model vars still win. `env -u` on an unset var is a harmless no-op.
    if provider == "claude":
        from fno.agents.model_routing import (
            incoherent_model_env_notice,
            incoherent_model_env_unset_args,
            overlay_restores_model_env,
        )

        _flags = incoherent_model_env_unset_args()
        if _flags:
            unset += _flags
            # The names ride the flag pairs (-u NAME), so the notice reads
            # them back rather than scanning the env a second time.
            print(
                incoherent_model_env_notice(
                    _flags[1::2],
                    routed=overlay_restores_model_env(account_env, resolved_route),
                ),
                file=sys.stderr,
            )
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
    timeout: Optional[float] = None,
) -> "subprocess.CompletedProcess[str]":
    try:
        return runner(
            [_fno_bin(), *args],
            capture_output=True,
            text=True,
            timeout=_MUX_SUBPROCESS_TIMEOUT_S if timeout is None else timeout,
            **({"env": env} if env is not None else {}),
        )
    except FileNotFoundError as exc:
        raise DispatchAskError(
            f"the '{_fno_bin()}' binary was not found on PATH; the pane "
            "substrate is hosted by the fno mux (set FNO_BIN or install fno)",
            exit_code=127,
        ) from exc
    except subprocess.TimeoutExpired as exc:
        # The EFFECTIVE timeout, not the module default: the binding-loop probes
        # pass a 2s bound, so naming 30s here would be a diagnostics lie in the
        # one subsystem this change exists to make truthful.
        effective = _MUX_SUBPROCESS_TIMEOUT_S if timeout is None else timeout
        raise DispatchAskError(
            f"fno mux did not answer within {effective}s ({' '.join(args[:3])}...)",
            exit_code=1,
        ) from exc


def _reap_spawned_pane(
    session: str,
    pane_id: int,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
) -> tuple[bool, str]:
    """Attempt exact pane cleanup and return confirmed status plus failure detail."""
    try:
        cleanup = _run_mux(
            ["mux", "pane", "kill", "--session", session, str(pane_id)],
            runner,
        )
    except DispatchAskError as exc:
        return False, str(exc)
    if cleanup.returncode == 0:
        return True, ""
    return False, (cleanup.stderr or cleanup.stdout or "no output").strip()


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


#: `fno mux pane wait` exit code when the pane's child exited mid-wait
#: (crates/fno EXIT_WAIT_EXITED). The readiness gate treats it as launch failure.
_WAIT_EXITED = 12

#: `fno mux pane run` exit code when the mux never answered the control read
#: (crates/fno EXIT_CONTROL_UNANSWERED). The verb REACHED the server, so
#: unlike every other non-zero code this does not prove the pane is absent.
_MUX_CONTROL_UNANSWERED = 20


def _pid_started_at_or_after(pid: int, since_s: float) -> bool:
    """True iff `pid`'s wall-clock start time is at/after `since_s` (both real
    Unix epoch seconds). ``spawn_gate._process_start_time`` is NOT usable here:
    it is documented as "the incarnation token in the Rust registry's units",
    an opaque value meant only for equality against a previously recorded
    token (Linux: `/proc/<pid>/stat` clock ticks since boot; macOS: epoch
    microseconds) - comparing either against an epoch-seconds bound is
    nonsense. False on any read failure: a process this cannot date is not a
    provable match."""
    try:
        import psutil

        return psutil.Process(pid).create_time() >= since_s
    except Exception:
        return False


def _reconcile_unanswered_run(
    session: str,
    cwd: Path,
    spawn_started_ms: int,
    claimed_pane_ids: set,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
) -> int:
    """After ``mux pane run`` exits ``_MUX_CONTROL_UNANSWERED``, decide whether a
    pane exists rather than asserting it does not (LD2/LD3). Never retries the
    run (LD1): the verb already reached the server, and re-sending it risks a
    second pane and a second worker.

    Enumerates the session's panes and looks for exactly one candidate that
    matches this spawn: same cwd, a live child pid whose start time is at or
    after ``spawn_started_ms``, and not already claimed by a registry row.
    Zero, one, or many candidates get three different, honest answers. An
    empty or unparseable listing is UNKNOWN, never proof of absence - see
    ``_pane_absent_from_listing``'s docstring for why ``pane ls`` prints ``[]``
    and exits 0 when the session socket is refused or absent.

    Returns the adopted ``pane_id`` on exactly one candidate; raises
    ``DispatchAskError`` (never a registry row) on every other outcome.
    """
    proc: Optional["subprocess.CompletedProcess[str]"] = None
    detail = ""
    try:
        proc = _run_mux(["mux", "pane", "ls", "--session", session, "--json"], runner)
    except DispatchAskError as exc:
        detail = str(exc)
    else:
        if proc.returncode != 0:
            detail = (proc.stderr or "").strip()

    rows: Optional[list] = None
    if proc is not None and proc.returncode == 0:
        try:
            parsed = json.loads(proc.stdout or "")
        except (ValueError, TypeError):
            detail = f"unparseable pane ls output: {(proc.stdout or '').strip()!r}"
        else:
            if isinstance(parsed, list):
                rows = parsed
            else:
                detail = f"unexpected pane ls output: {(proc.stdout or '').strip()!r}"

    if not rows:
        # Covers the ls call failing outright, a non-zero exit, unparseable
        # output, and a genuinely empty listing - all four read the same way
        # here: the mux never answered the run AND could not be asked
        # afterward, so whether a pane exists stays UNKNOWN, not disproven.
        suffix = f" ({detail})" if detail else ""
        raise DispatchAskError(
            f"the mux never answered 'pane run' for session {session!r}, and "
            f"'pane ls' could not confirm whether a pane exists{suffix}; a "
            f"pane may be live - inspect with 'fno mux pane ls --session "
            f"{session}' before retrying.",
            exit_code=1,
        )

    since_s = spawn_started_ms / 1000
    candidates = []
    for row in rows:
        if not isinstance(row, dict) or row.get("cwd") != str(cwd):
            continue
        pid = row.get("child_pid")
        if pid is None:
            continue
        if not _pid_started_at_or_after(int(pid), since_s):
            continue
        if row.get("pane_id") in claimed_pane_ids:
            continue
        candidates.append(row)

    if not candidates:
        raise DispatchAskError(
            f"no pane was created; the mux never answered and no pane in "
            f"{session!r} matches this spawn. Retry, or use --substrate bg.",
            exit_code=1,
        )
    if len(candidates) > 1:
        ids = ", ".join(str(c.get("pane_id")) for c in candidates)
        raise DispatchAskError(
            f"the mux never answered and {len(candidates)} panes in "
            f"{session!r} match this spawn ({ids}); cannot tell them apart - "
            f"inspect with 'fno mux pane ls --session {session}' before "
            "retrying.",
            exit_code=1,
        )
    return int(candidates[0]["pane_id"])


def _pane_absent_from_listing(
    mux: dict, runner, timeout: Optional[float] = None
) -> Optional[bool]:
    """True when the mux SUCCESSFULLY enumerated its panes and ours is not there.

    This is the reaped-pane case, and it is the normal one: when a pane's child
    exits the mux drops the pane outright (``close_pane``), so a later
    ``pane wait`` finds no pane to watch and answers with the generic
    ``EXIT_ERROR`` (1) that also covers io failures, version skew, and server
    errors. Exit 12 only ever fires when the child dies while a watcher is
    already subscribed, which a ``--timeout 0`` probe almost never is. Without
    this fallback the death branch would be near-unreachable and a corpse would
    sit out the whole window and land as `spawning` - the original bug.

    The positive control is a NON-EMPTY listing, and the empty case is the whole
    reason: `fno mux pane ls --json` deliberately prints `[]` and exits 0 when the
    session socket is refused or absent (crates/fno/src/mux_cli.rs, the
    `is_ls && no_server` branch). So exit 0 plus parseable JSON does NOT prove
    the instrument ran - "I could not reach the session" and "the session has no
    panes" are the same bytes. Only a listing that names at least one OTHER pane
    proves the server answered with real content; absence from that is the mux
    saying our pane is gone.

    An empty list therefore returns None, not True. That costs death detection
    when the dying pane was the session's last, and it is the right trade: this
    helper's False is the condemn signal for reconcile and
    ``reachability.pane_falsifier``, so a wrong False marks a LIVE worker exited
    whenever the mux socket is briefly unreachable. Failing to unknown loses a
    retry; failing to condemned loses a worker.
    """
    try:
        proc = _run_mux(
            ["mux", "pane", "ls", "--session", str(mux["session"]), "--json"],
            runner,
            timeout=timeout,
        )
    except Exception:  # noqa: BLE001 -- a probe never fails a spawn
        return None
    if getattr(proc, "returncode", 1) != 0:
        return None
    try:
        rows = json.loads(proc.stdout or "")
    except (ValueError, TypeError):
        return None
    if not isinstance(rows, list) or not rows:
        # Empty is indistinguishable from an unreachable session (see docstring).
        return None
    return not any(
        isinstance(r, dict) and r.get("pane_id") == mux["pane_id"] for r in rows
    )


def _mux_pane_alive(
    mux: dict, runner=subprocess.run, timeout: Optional[float] = None
) -> Optional[bool]:
    """Return exact pane liveness, or ``None`` when the mux cannot answer.

    ``timeout`` defaults to the shared module bound. The binding loop passes a
    tight one; reconcile and ``reachability.pane_falsifier`` must NOT inherit it,
    because a mux answering in 3s under load would turn into "unavailable" there
    and skip a heal that used to succeed. Tight bounds belong to the caller that
    needs them, not baked into shared code.
    """
    try:
        proc = _run_mux(
            [
                "mux", "pane", "wait", "--session", str(mux["session"]),
                str(mux["pane_id"]), "--timeout", "0",
            ],
            runner,
            timeout=timeout,
        )
    except Exception:  # noqa: BLE001 -- see below: a probe never fails a spawn
        # Deliberately as broad as _read_pane_tail's. This runs at the same
        # point, after the pane exists and under the registry lock, and its whole
        # contract is "None = the mux could not answer" - so an unhandled
        # PermissionError or a fork failure escaping here would orphan a live
        # pane with no registry row, which is a worse outcome than every reason
        # the probe might have failed. A narrow catch held the liveness probe to
        # a weaker standard than the tail read it sits next to.
        return None
    if proc.returncode == _WAIT_EXITED:
        return False
    if proc.returncode in {0, 11}:
        return True
    # Every other code is ambiguous by construction (EXIT_ERROR = 1 covers a
    # dead pane AND io failure AND version skew AND server error), so it is not
    # a death signal on its own. Ask the authoritative enumeration instead.
    absent = _pane_absent_from_listing(mux, runner, timeout=timeout)
    if absent is None:
        return None
    return not absent


#: How long a pane may take to bind its session before the spawn stops waiting.
#: The codex backfill's old ceiling was 3.0s (5 probes x 0.75s), which cannot
#: separate "codex is slow to open its rollout" from "codex died on an argv
#: error". A healthy spawn pays none of this: the loop returns the instant
#: either signal fires, so only the genuinely ambiguous case waits it out.
#:
#: CEILING, and it is not arbitrary: `run_dispatch_one` in crates/fno/src/
#: server.rs kills the whole `fno dispatch one` subprocess after 20s, and that
#: budget also has to cover process start, node selection, and pane creation. A
#: window at or near 20s would get the subprocess killed BEFORE the registry
#: append, leaving a live pane with no row - the very orphan this change exists
#: to prevent. Keep this comfortably under that budget; if the dispatch timeout
#: moves, this moves with it.
#:
#: Nothing is lost by the shorter window: a death is detected when it happens
#: (see _pane_absent_from_listing), not at expiry. The window only bounds how
#: long a SLOW BIND is waited out.
_BINDING_WINDOW_S = 8.0
_BINDING_WINDOW_ENV = "FNO_PANE_BINDING_WINDOW_S"
_BINDING_POLL_S = 0.75
#: Per-probe subprocess ceiling inside the binding loop. The shared
#: `_MUX_SUBPROCESS_TIMEOUT_S` is 30s and one tick issues up to three probes, so
#: a wedged mux could make a SINGLE tick run ~90s and blow the window ceiling
#: this whole design depends on - straight past the 20s dispatch kill, into the
#: live-pane-with-no-registry-row orphan. These probes are all cheap reads with
#: an honest "could not answer" result, so a tight bound costs nothing.
_PROBE_TIMEOUT_S = 2.0
#: How long the wait may stay silent before it says what it is doing. A spawn
#: that used to return in 3s can now take the full window, and a silent block on
#: an interactive verb reads as a hang, so the wait announces itself once.
_BINDING_ANNOUNCE_S = 3.0
#: Pane scrollback retained per tick, and the slice echoed to stderr on death.
_PANE_TAIL_LINES = 200
_PANE_TAIL_ECHO_LINES = 10


def _binding_window_s() -> float:
    """The binding window (env-overridable for CI, positive only)."""
    raw = os.environ.get(_BINDING_WINDOW_ENV)
    if raw:
        try:
            v = float(raw)
        except ValueError:
            v = 0.0
        if v > 0:
            return v
    return _BINDING_WINDOW_S


@dataclass
class PaneBinding:
    """Which of three worlds a spawn is in once its binding window closes."""

    #: The worker's session id; None unless it bound.
    session_id: Optional[str]
    #: Probed pane liveness. None means the mux could not answer.
    pane_alive: Optional[bool]
    #: Why it is unbound; "" when bound.
    reason: str
    #: Retained pane scrollback, "" when nothing was captured.
    tail: str


def _read_pane_tail(
    mux: dict,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
) -> str:
    """Best-effort pane scrollback, "" on any failure.

    Evidence is best-effort; binding is not. Nothing in here may raise into the
    spawn, so the catch is deliberately broad: a spawn must never fail because
    its flight recorder did.
    """
    try:
        proc = _run_mux(
            [
                "mux", "pane", "read", "--session", str(mux["session"]),
                str(mux["pane_id"]), "--lines", str(_PANE_TAIL_LINES),
            ],
            runner,
            timeout=_PROBE_TIMEOUT_S,
        )
    except Exception:  # noqa: BLE001 -- see docstring: evidence never fails a spawn
        return ""
    if getattr(proc, "returncode", 1) != 0:
        return ""
    return proc.stdout or ""


def _await_pane_binding(
    mux: dict,
    bind_probe: Callable[[], Optional[str]],
    *,
    window_s: Optional[float] = None,
    runner: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
    sleep: Optional[Callable] = None,
    label: str = "the worker",
) -> PaneBinding:
    """Wait out the gap between "a pane exists" and "a worker reached its provider".

    These are two different facts, and reporting the first as the second is the
    defect this exists to end: before it, a pane that would bind in 4s and one
    that had already died produced byte-identical receipts.

    Three outcomes, and the caller must be able to tell them apart:

    * bound - ``session_id`` is set; the worker named itself.
    * died - the pane's child is CONFIRMED gone and never bound.
    * still booting - the window expired with the pane up, or unprovable.

    Ambiguity always resolves to still-booting. The asymmetry is the whole
    reason: a false "died" kills a working worker, a false "still booting"
    costs a retry. Anyone tightening this branch later should promote a signal
    from unknown to confirmed, never lower the bar for calling a worker dead.

    ``_mux_pane_alive`` is the death oracle, and its two signals are ranked by
    how much they prove. ``pane wait --timeout 0`` exiting 12 is definitive: the
    child exited. But that only fires when the child dies while a watcher is
    already subscribed, which a reaped pane never is, so it is not enough on its
    own. The fallback is absence from a NON-EMPTY ``pane ls``, which is evidence
    only because another pane in the listing proves the server answered with
    real content; an empty listing is what an unreachable session also produces,
    so it resolves to unknown. See ``_pane_absent_from_listing``.

    Each tick also retains the pane's scrollback, because the mux drops a dead
    pane's buffer in ``close_pane``: the capture has to happen BEFORE the death
    or there is nothing left to capture. That is why both prior occurrences of
    this bug left zero evidence.

    Provider-agnostic by construction (``bind_probe`` is the only provider-aware
    part), though only the codex route is wired onto it today.
    """
    naptime = sleep or time.sleep
    window = max(window_s if window_s is not None else _binding_window_s(), 0.0)
    started = time.monotonic()
    deadline = started + window
    tail = ""
    announced = False
    first_pass = True
    last_alive: Optional[bool] = None
    while True:
        sid = bind_probe()
        if sid:
            # pane_alive True is OBSERVED, not assumed: the probe read the id
            # from a rollout fd held open by a live process in the pane's tree,
            # so a dead pane cannot produce one.
            return PaneBinding(sid, True, "", tail)
        # Deadline before the tail read as well: evidence is best-effort, and a
        # slow mux must not buy itself another bounded probe past the ceiling.
        # The first pass is exempt for the same reason as below - one full look.
        if not first_pass and time.monotonic() >= deadline:
            return PaneBinding(None, last_alive, "binding-window-expired", tail)
        # Keep the newest NON-EMPTY read: a TUI that has not painted yet reads
        # empty, and a later empty read must not erase real earlier evidence.
        fresh = _read_pane_tail(mux, runner)
        if fresh.strip():
            tail = fresh
        # Checked BETWEEN probes, not only at the tick boundary: one tick issues
        # three subprocesses, so a slow mux would otherwise carry the loop far
        # past the ceiling even with each probe individually bounded.
        #
        # NOT on the first pass, though: the whole loop owes one complete look
        # even when the window is zero or already spent, and bailing here would
        # skip the liveness probe entirely - so a pane that was already dead
        # would report "still booting" instead of dead.
        if not first_pass and time.monotonic() >= deadline:
            # last_alive, not None: the previous tick may have OBSERVED the pane
            # up 0.75s ago, and reporting "the mux could not answer" would throw
            # away a real observation.
            return PaneBinding(None, last_alive, "binding-window-expired", tail)
        first_pass = False
        alive = _mux_pane_alive(mux, runner, timeout=_PROBE_TIMEOUT_S)
        last_alive = alive if alive is not None else last_alive
        if alive is False:
            return PaneBinding(None, False, "pane-died-before-binding", tail)
        # Checked AFTER a full probe, so a zero or negative window still buys
        # exactly one look rather than skipping the loop entirely.
        if time.monotonic() >= deadline:
            return PaneBinding(None, alive, "binding-window-expired", tail)
        if not announced and time.monotonic() - started >= _BINDING_ANNOUNCE_S:
            announced = True
            print(
                f"spawn: pane {mux.get('pane_id')} is up; waiting up to "
                f"{window:.0f}s for {label} to bind its session",
                file=sys.stderr,
            )
        naptime(_BINDING_POLL_S)


#: The statuses whose death was PROVED by a probe, and therefore the only ones
#: whose row may be reclaimed by a same-name respawn.
#:
#: Deliberately NOT `TERMINAL_STATUSES`, and `orphaned` is why: that word means
#: "live but not currently routable" (`dispatch.py` stamps it on a delivery
#: failure with the message "agent is live but not currently routable"), and
#: reconcile keeps the pid on an orphaned row precisely because the process is
#: still running. Reclaiming one would delete a LIVE worker's registry row,
#: leaving it invisible to `fno agents list`, reconcile, and lane cleanup while
#: a second pane starts on the same node. A control-socket hiccup is not a death.
_RECLAIMABLE_STATUSES = frozenset({"exited", "failed", "permanent_dead"})

#: Harnesses whose pane spawn binds a session identity, so `bound` is a claim
#: the spawn can actually make: claude pins one up front, codex and opencode
#: discover one afterwards.
#:
#: gemini and agy are pane-hostable but expose no spawn-time session id at all,
#: so the spawn asserts NOTHING for them (`bound: None`). Reporting False would
#: call a healthy worker a failure; reporting True would be the same unverified
#: "it's live" this change exists to remove. Their receipts are exactly as
#: trustworthy as before, and now say so.
_SESSION_BINDING_HARNESSES: tuple[str, ...] = ("claude", "codex", "opencode")


def _resolve_bound(session_uuid: Optional[str], harness: str) -> Optional[bool]:
    """Tri-state: bound / not bound / this harness binds no session at all."""
    if harness not in _SESSION_BINDING_HARNESSES:
        return None
    return session_uuid is not None


def _resolve_unbound_reason(
    bound: Optional[bool], reason: Optional[str], harness: str
) -> Optional[str]:
    """The reason a receipt carries; None unless ``bound`` is exactly False.

    Structural, not per-branch: the codex path names its reason precisely, but
    every OTHER way to end up unbound (an opencode backfill miss, a happy-claude
    route) would otherwise emit ``unbound_reason: null`` - the same empty signal
    as an empty ``short_id``, which is what this whole change exists to remove.
    Defaulting here means a new unbound branch cannot reintroduce a null by
    forgetting, and a bound spawn can never carry a stale reason.

    A harness that binds no session (``bound is None``) carries no reason
    either: there is no failure to explain, only a claim never made.

    ``harness`` is the axis word on purpose: the thing that failed to bind is the
    BINARY (claude, codex, opencode), not a model vendor. `dispatch_spawn_pane`
    still calls its local `provider`, which is the older spelling.
    """
    if bound is not False:
        return None
    # Generic on purpose: a reason naming the harness reads as "this harness
    # binds no session", which is exactly what `bound is None` means and the
    # opposite of what a MISS means. An opencode backfill miss has a real cause
    # ("no unique opencode session for this cwd after spawn", already emitted as
    # an event); a receipt field whose job is to explain must not misdirect when
    # the branch forgot to name one.
    return reason or "unbound-reason-unrecorded"


def _write_pane_death_log(name: str, tail: str, pane_id: Optional[int] = None) -> str:
    """Persist a dead pane's captured scrollback; return its path, "" on failure.

    An unwritable log dir must not fail the spawn: the caller still reports the
    death and still echoes the tail to stderr, it just cannot point at a file.

    The filename carries the pane id and a timestamp because the deaths worth
    reading are usually REPEATS of one another: a name-stable path would let the
    respawn's corpse overwrite the first, destroying the very run you wanted to
    compare against.
    """
    if not tail.strip():
        return ""
    try:
        from fno import paths as _paths

        log_dir = _paths.state_dir() / "logs"
        log_dir.mkdir(parents=True, exist_ok=True)
        stamp = time.strftime("%Y%m%dT%H%M%S", time.gmtime())
        suffix = f"pane{pane_id}-{stamp}" if pane_id is not None else stamp
        path = log_dir / f"{name}.{suffix}.pane.log"
        path.write_text(tail, encoding="utf-8")
        return str(path)
    except Exception:  # noqa: BLE001 -- see docstring: evidence never fails a spawn
        return ""


#: Bounded window an id-less happy-claude pane row waits for its worker to
#: register the session id (the SessionStart restamp). happy owns the id, so the
#: row is created `spawning` and only becomes addressable once the worker names
#: itself; if that never lands the row strands `spawning` forever, an
#: unrecoverable pane whose receipt would read as a soft success. The working
#: case lands well under this; the ceiling is paid only on failure.
_PANE_REGISTRATION_DEADLINE_S = 60
_PANE_REGISTRATION_POLL_S = 1.0


def _await_pane_registration(
    name: str,
    mux: dict,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    registry_path: Optional[Path] = None,
) -> tuple[Optional[str], str]:
    """Wait for an id-less happy-claude row to be restamped with its session id.

    Polls the registry for ``name`` until ``harness_session_id`` is filled (the
    worker named itself via its SessionStart hook) or the deadline passes. Reads
    the same ``registry_path`` the caller wrote the row through, not a
    re-resolved default, so the poll never watches a different file than the one
    it is waiting on. A confirmed-dead pane child short-circuits, so the common
    breakage (monitor binary missing on the pane's PATH, so the pane shell
    errors out) fails in seconds rather than the full window. Returns
    ``(session_id, "")`` on success or ``(None, reason)`` on timeout / early death.
    """
    deadline = time.monotonic() + _PANE_REGISTRATION_DEADLINE_S
    while time.monotonic() < deadline:
        try:
            entry = next(
                (
                    e
                    for e in load_registry(path=registry_path)
                    if e.name == name and e.mux == mux
                ),
                None,
            )
        except (OSError, ValueError, RegistryVersionError):
            entry = None
        if entry is not None and entry.harness_session_id:
            return entry.harness_session_id, ""
        # Fast-fail a dead pane: no restamp is ever coming, so don't sit out the
        # full window. `None` (mux could not answer) is NOT a death signal.
        if _mux_pane_alive(mux, runner) is False:
            return None, "pane process exited before registering"
        time.sleep(_PANE_REGISTRATION_POLL_S)
    return None, f"no session id within {_PANE_REGISTRATION_DEADLINE_S}s"


def _evaluate_manifest_screen(
    harness: str,
    screen: str,
    runner: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
    *,
    osc_title: Optional[str] = None,
    osc_progress: Optional[str] = None,
) -> dict:
    """Ask the Rust manifest engine for the winning rule on this exact screen."""
    from fno import rust_binary

    binary = rust_binary.resolve_installed_binary()
    if binary is None:
        return {"matched": False, "error": "manifest-eval binary unavailable"}
    try:
        argv = [str(binary), "manifest-eval", "--harness", harness]
        if osc_title is not None:
            argv += ["--osc-title", osc_title]
        if osc_progress is not None:
            argv += ["--osc-progress", osc_progress]
        proc = runner(
            argv,
            input=screen,
            capture_output=True,
            text=True,
            timeout=_PROBE_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return {"matched": False, "error": str(exc)}
    if proc.returncode != 0:
        return {
            "matched": False,
            "error": (proc.stderr or proc.stdout or "manifest-eval failed").strip(),
        }
    try:
        value = json.loads(proc.stdout)
    except (TypeError, json.JSONDecodeError):
        return {"matched": False, "error": "manifest-eval returned unreadable JSON"}
    return value if isinstance(value, dict) else {"matched": False, "error": "bad result"}


def _pane_osc_title(
    session: str,
    pane_id: int,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
) -> Optional[str]:
    """Read the pane title carried by mux metadata; unreadable means unknown."""
    try:
        proc = _run_mux(
            ["mux", "pane", "ls", "--session", session, "--json"], runner
        )
        if proc.returncode != 0:
            return None
        rows = json.loads(proc.stdout or "[]")
    except (DispatchAskError, TypeError, json.JSONDecodeError):
        return None
    for row in rows if isinstance(rows, list) else []:
        if row.get("pane_id") == pane_id and isinstance(row.get("title"), str):
            return row["title"]
    return None


def _await_interactive_readiness(
    provider: str,
    session: str,
    pane_id: int,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    *,
    manifest_evaluator: Optional[Callable[[str, str], dict]] = None,
    permission_action: Optional[str] = None,
    permission_sender: Optional[Callable[..., bool]] = None,
) -> tuple[str, str]:
    """Interactive readiness gate (x-6928).

    Liveness is necessary but never sufficient. A pane is READY only when the
    Rust manifest engine returns this harness's configured positive rule id.
    """
    try:
        probe = _run_mux(
            [
                "mux", "pane", "wait", "--session", session,
                str(pane_id), "--timeout", "1",
            ],
            runner,
        )
    except DispatchAskError as exc:
        return "failed", f"readiness probe failed: {exc}"
    if probe.returncode == _WAIT_EXITED:
        return "failed", "provider exited before readiness"
    if probe.returncode not in {0, 11}:
        detail = (probe.stderr or probe.stdout or "").strip()
        suffix = f": {detail}" if detail else ""
        return (
            "failed",
            f"readiness probe failed: pane wait exited {probe.returncode}{suffix}",
        )
    try:
        painted = _run_mux(
            [
                "mux", "pane", "read", "--session", session,
                str(pane_id), "--lines", "20",
            ],
            runner,
        )
    except DispatchAskError as exc:
        return "failed", f"readiness probe failed: {exc}"
    if painted.returncode != 0:
        detail = (painted.stderr or painted.stdout or "").strip()
        suffix = f": {detail}" if detail else ""
        return (
            "failed",
            f"readiness probe failed: pane read exited {painted.returncode}{suffix}",
        )
    screen = painted.stdout or ""
    if not screen.strip():
        return "live", "ready marker not observed: screen is unpainted"
    from fno.agents.harness_map import capabilities

    expected = capabilities(provider)["ready_marker"]
    if expected == "unsupported":
        return "live", f"harness {provider!r} has no pinned ready marker"
    osc_title = _pane_osc_title(session, pane_id, runner)
    evaluate = manifest_evaluator or (
        lambda harness, text: _evaluate_manifest_screen(
            harness, text, runner, osc_title=osc_title
        )
    )
    verdict = evaluate(provider, screen)
    observed = verdict.get("rule_id") if verdict.get("matched") else None
    if observed == expected:
        return "ready", f"ready-marker={expected}"
    if verdict.get("state") == "blocked" and observed:
        if permission_action is None:
            return "blocked", f"blocked-rule={observed}"
        from fno.agents.harness_map import DispatchResolveError, permission_response_keys

        try:
            keys = permission_response_keys(provider, permission_action, observed)
        except DispatchResolveError as exc:
            return "blocked", f"blocked-rule={observed}; response refused: {exc}"
        sender = permission_sender or _send_permission_response
        if not sender(
            provider, session, pane_id, observed, keys, verdict, runner, evaluate
        ):
            return "blocked", f"blocked-rule={observed}; fingerprinted response not sent"
        return _await_interactive_readiness(
            provider,
            session,
            pane_id,
            runner,
            manifest_evaluator=manifest_evaluator,
        )
    detail = f"expected {expected}; observed {observed or 'no matching manifest rule'}"
    if verdict.get("error"):
        detail += f" ({verdict['error']})"
    return "live", detail


def _send_permission_response(
    provider: str,
    session: str,
    pane_id: int,
    rule_id: str,
    keys: list[str],
    verdict: dict,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    evaluator: Callable[[str, str], dict],
) -> bool:
    """Re-read the fingerprinted prompt under the pane writer claim, then answer."""
    answerable = verdict.get("answerable")
    if not isinstance(answerable, dict):
        return False
    fingerprint = answerable.get("fingerprint")
    token_bytes = {
        "enter": b"\r", "tab": b"\t", "left": b"\x1b[D", "right": b"\x1b[C",
        "up": b"\x1b[A", "down": b"\x1b[B", "esc": b"\x1b",
    }
    desired = b"".join(token_bytes.get(key, key.encode("ascii")) for key in keys)
    options = answerable.get("options") or []
    if not any(bytes(option.get("keystroke") or []) == desired for option in options):
        return False
    pane = str(pane_id)
    claim = _run_mux(
        ["mux", "pane", "claim", pane, "--pid", str(os.getpid()), "--session", session],
        runner,
    )
    if claim.returncode != 0:
        return False
    try:
        fresh = _run_mux(
            ["mux", "pane", "read", pane, "--lines", "20", "--session", session],
            runner,
        )
        if fresh.returncode != 0:
            return False
        fresh_verdict = evaluator(provider, fresh.stdout or "")
        fresh_answerable = fresh_verdict.get("answerable") or {}
        if (
            fresh_verdict.get("rule_id") != rule_id
            or fresh_answerable.get("fingerprint") != fingerprint
        ):
            return False
        for key in keys:
            raw = token_bytes.get(key, key.encode("ascii")).decode("latin1")
            sent = _run_mux(
                ["mux", "pane", "send", pane, "--text", raw, "--session", session],
                runner,
            )
            if sent.returncode != 0:
                return False
        return True
    finally:
        _run_mux(
            ["mux", "pane", "release", pane, "--pid", str(os.getpid()), "--session", session],
            runner,
        )


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
    at: Optional[str] = None,
    crown_level: Optional[int] = None,
    crown_scope: Optional[str] = None,
    provenance: Optional[dict[str, str]] = None,
    account_env: Optional[dict[str, str]] = None,
    route_env: Optional[dict[str, str]] = None,
    monitor: Optional[str] = None,
    route_provider: Optional[str] = None,
    provider_gate: object | None = None,
    runner: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
    codex_sessions_dir: Optional[Path] = None,
    passthrough: Optional[Sequence[str]] = None,
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
    # Crown values, validated on the way in for the same reason the tier-remap
    # check below is: `cmd_spawn` parses `--crown` before it gets here, but an
    # in-process caller hands (level, scope) straight to this signature, and a
    # value that skipped validation is written to the SHARED registry. Fail
    # closed before the pane exists, so a refusal leaves no worker behind.
    crown_problem = crown_validation_error(crown_level, crown_scope)
    if crown_problem is not None:
        raise DispatchAskError(crown_problem, exit_code=2)
    if crown_level is not None:
        # Same authorization rule as the bg seam: a grant must be a strict subset
        # of what the grantor holds. Both paths check, because either is a door.
        grant_problem = grant_error(crown_scope or "", calling_agent_row())
        if grant_problem is not None:
            raise DispatchAskError(f"--crown: {grant_problem}", exit_code=2)

    # Launch-time headroom picking (x-7d45). `pane` is the DEFAULT substrate and
    # `cmd_spawn` routes it straight here, never through `dispatch_spawn` - so a
    # picker wired only there would cover bg/headless and leave every default
    # interactive spawn on the exhausted account while the option read enabled.
    # Same helper, same rules (explicit account wins, routed spawns are skipped).
    if account_env is None and provider == "claude":
        from fno.agents.dispatch import _pick_account_env

        picked = _pick_account_env(role=role, route_env=route_env)
        account_env = dict(picked) if picked is not None else None

    # The monitor contract is judged BEFORE the generic route guard: an
    # explicit --monitor happy refusal names the zai-shaped gap it found, and
    # letting resolve_spawn_route's completeness refusal fire first would
    # replace that diagnosis with a generic one. resolve_monitor's inputs are
    # all settled here (account picked, flags parsed); the unmonitored path
    # refuses nothing and stays byte-identical.
    resolved_monitor = resolve_monitor(
        monitor,
        harness=provider,
        route_provider=route_provider,
        route_env=route_env,
        account_env=account_env,
        model=model,
    )
    launch_role = role
    resolved_providers: list[str] = []
    if provider == "claude" and (role is not None or route_env):
        from fno.agents.model_routing import (
            RouteCompositionError,
            resolve_spawn_route,
        )

        try:
            route_env = resolve_spawn_route(
                role,
                route_env,
                resolved_provider=resolved_providers.append,
            )
        except RouteCompositionError as exc:
            raise DispatchAskError(str(exc), exit_code=2) from exc
        if resolved_providers:
            resolved_provider = resolved_providers[-1]
            if route_provider is not None and route_provider != resolved_provider:
                raise DispatchAskError(
                    f"resolved provider {resolved_provider!r} does not match supplied "
                    f"provider {route_provider!r}; refusing before dispatch",
                    exit_code=2,
                )
            route_provider = resolved_provider
        launch_role = None

    if provider == "claude" and route_env and not resolved_providers:
        raise DispatchAskError(
            "pre-resolved route has no bound model-provider identity; refusing "
            "because its provider cap cannot be evaluated; no worker launched",
            exit_code=2,
        )
    if provider == "claude" and route_env and route_provider is None:
        raise DispatchAskError(
            "resolved route has no model-provider axis; refusing to launch because "
            "its provider cap cannot be evaluated; no worker launched",
            exit_code=2,
        )
    from fno.agents.spawn_gate import consume_provider_admission

    if route_provider is not None and not (
        provider_gate is not None
        and consume_provider_admission(provider_gate, route_provider, name, "pane")
    ):
        raise DispatchAskError(
            f"provider {route_provider!r} has no matching admission token; "
            "refusing before dispatch; no worker launched",
            exit_code=2,
        )

    # x: the tier-remap invariant must hold on every reachable spawn path, not
    # just the CLI seam -- an in-process caller passing model="opus" under a
    # foreign ANTHROPIC_DEFAULT_OPUS_MODEL would otherwise still launch a worker
    # that dies on its first turn. Fail closed before anything is created.
    from fno.agents.model_routing import (
        RouteCompositionError,
        check_spawn_tier_remap,
        emit_env_scrub_warning,
    )

    try:
        check_spawn_tier_remap(
            provider,
            model,
            role=launch_role,
            route_env=route_env,
            account_env=account_env,
        )
    except RouteCompositionError as exc:
        raise DispatchAskError(str(exc), exit_code=2) from exc
    # Same seam, same reason as the tier-remap check above: a permission-pinned
    # claude worker launched under CLAUDE_CODE_SUBPROCESS_ENV_SCRUB stalls on
    # approvals, so warn on every reachable path, not just the CLI seam.
    emit_env_scrub_warning(provider, permission_pinned=bool(permission_mode or yolo))
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

    codex_route = None
    if provider == "codex" and role is not None:
        from fno.agents.model_routing import resolve_codex_route

        try:
            codex_route = resolve_codex_route(role)
        except RouteCompositionError as exc:
            raise DispatchAskError(str(exc), exit_code=2) from exc
        # An empty mapping marks the Codex lane as deliberately unrouted and
        # prevents the generic env wrapper from resolving a Claude route.
        route_env = dict(codex_route.env) if codex_route is not None else {}
        launch_role = None

    effective_message: Optional[str] = None
    if message.strip().startswith(("/", "$fno:")):
        try:
            message = normalize_command(message, provider)
        except DispatchResolveError as exc:
            raise DispatchAskError(str(exc), exit_code=2) from exc
        effective_message = message

    session = resolve_mux_session(session)
    # Resolve the monitor BEFORE the argv build. happy OWNS the claude session
    # id: claudeLocal() extracts `--session-id` out of the caller's argv and only
    # re-adds it on its `!hookSettingsPath` branch, which a normal `happy` launch
    # never takes (the start path assigns hookSettingsPath unconditionally). A
    # pinned uuid is therefore discarded, claude mints its own, and the receipt
    # names a session that never exists - which is what makes a happy pane
    # unpeekable and its registry row id-less whether it is healthy or a corpse.
    # resolved_monitor was settled above, before the route guard.
    pin_session = provider == "claude" and resolved_monitor != "happy"
    session_uuid = str(_uuid.uuid4()) if pin_session else None
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
        name=name,
        passthrough=passthrough,
    )
    if codex_route is not None:
        argv = [argv[0], *codex_route.config_args, *argv[1:]]
        # x-1caa: the route's config args splice AFTER build_pane_argv, so the
        # in-arm duplicate-flag check never saw them; re-run it against the
        # route tokens or a passthrough `-c`/`--model` flag is a silent
        # last-wins against the route's own setting.
        pane_passthrough_tokens(passthrough, codex_route.config_args)
    if provider == "claude" and not claude_argv_is_interactive(argv):
        raise DispatchAskError(
            "refusing to pane-host claude with -p/--print (that bills the "
            "Agent SDK pool); the mux spawns interactive subscription-billed "
            "claude",
            exit_code=2,
        )
    # x-1caa: same choke point as the billing guard above, reading the composed
    # argv so a passthrough token faces the identical check an fno-emitted one
    # would (a splice inside build_pane_argv, never an append past the guards).
    refuse_pane_headless_form(provider, argv)
    # The outer env wrapper is not merely a scrub: it SETS the whole route in
    # happy's own environment, and happy merges that into its claude child. So
    # the wrapper is what delivers the credential, and --claude-env carries only
    # the non-secret remainder (see happy_pane_argv). Order matters -- this runs
    # BEFORE _mesh_env_wrapper below, so the wrapper's assignments land outside
    # `happy` and reach it as env rather than as argv.
    if resolved_monitor == "happy":
        assert route_env is not None
        argv = happy_pane_argv(argv, route_env, explicit=monitor is not None)
    # QoS (x-c5cc): demote the provider command INSIDE the env wrapper —
    # wrapping outermost would break the mux server's FNO_NODE provenance
    # parse, which is anchored on argv[0] == "env" (server.rs node_from_argv).
    # env(1) applies its assignments and then execs taskpolicy/nice -> provider.
    from fno.agents.spawn_gate import qos_wrap

    wrapped = _mesh_env_wrapper(
        name, provider, launch_role, qos_wrap(argv), provenance, account_env, route_env
    )

    registry_path = paths.agents_registry_path()

    def _on_wait() -> None:
        print(f"Waiting for agent {name!r} lock...", file=sys.stderr, flush=True)

    with hold_agent_lock(name, registry_path, on_wait=_on_wait):
        try:
            entries = load_registry()
        except (OSError, ValueError, RegistryVersionError) as exc:
            raise DispatchAskError(f"registry read failed: {exc}", exit_code=12) from exc
        #: Set to the old status when this spawn is reclaiming a dead row's name,
        #: so the append can drop the corpse in the same transaction.
        replaced_terminal: Optional[str] = None
        # A PROVED-DEAD row does not own the name. It will never act again, so
        # holding the name hostage only deadlocks the caller: `fno dispatch one`
        # releases its claim and lane on failure and retries under the SAME
        # deterministic worker name, so a status-blind guard turns one dead pane
        # into a permanently failed node until a human runs `fno agents rm`.
        # The evidence survives the row: the captured pane output is a
        # timestamped file on disk, not a registry field.
        clash = next((e for e in entries if e.name == name), None)
        if clash is not None and clash.status in _RECLAIMABLE_STATUSES:
            replaced_terminal = clash.status
        elif clash is not None:
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
        if at:
            # `--at current` (or a numeric anchor) rides the outer pane-run
            # transport before the `--` fence. Python forwards the token
            # verbatim; the mux CLI resolves `current` from FNO_PANE so one Rust
            # parser owns env identity for every reachable caller (no Python
            # env drift, AC1-ERR).
            placement_args += ["at", at]
        # Stamp the spawn clock immediately before the pane runs, not at function
        # entry. A sibling pane starting a same-cwd session during the lock-wait
        # or argv-build above would otherwise clear the since_ms lower bound and
        # become the sole backfill candidate, stamping this row with the
        # sibling's id. Sampling here keeps the bound as tight as the pane run.
        spawn_started_ms = int(time.time() * 1000)
        run_args = [
            "mux",
            "pane",
            "run",
            "--claim",
            "--session",
            session,
            "--cwd",
            str(cwd),
            *placement_args,
        ]
        # Exact placement answers --json so the server authors the receipt
        # (anchor/direction/fallback); Python never synthesizes those from the
        # requested flags (AC1-UI). Legacy spawns keep the plain pane-id stdout.
        exact = bool(at)
        if exact:
            run_args.append("--json")
        run_args += ["--", *wrapped]
        # x-42c5: pop FNO_SPAWN_TRIGGER BEFORE this env snapshot, mirroring the
        # bg_create ordering fix in dispatch.py. `{**os.environ, ...}` here
        # seeds the pane-run transport (and, at server birth, the mux server
        # that spawns pane shells) - popped after this snapshot is too late.
        spawn_trigger = _capture_spawn_trigger()
        proc = _run_mux(
            run_args,
            runner,
            env={**os.environ, "FNO_MUX_SHELL_INTEGRATION": _shell_integration()},
        )
        placement_receipt: Optional[dict] = None
        recovered = False
        if proc.returncode == _MUX_CONTROL_UNANSWERED:
            # The verb reached the server; only the reply did not come back
            # (LD2). Reconcile instead of asserting no pane was created - the
            # reconcile itself never retries the run (LD1).
            #
            # Scoped to THIS mux session: pane ids are allocated independently
            # per server starting at 1, and registry identity is the
            # (session, pane_id) pair, so an unscoped set would wrongly treat
            # a different session's pane 1 as already claiming this session's
            # pane 1.
            claimed_pane_ids = {
                e.mux.get("pane_id")
                for e in entries
                if isinstance(e.mux, dict) and e.mux.get("session") == session
            }
            pane_id = _reconcile_unanswered_run(
                session, cwd, spawn_started_ms, claimed_pane_ids, runner
            )
            recovered = True
        elif proc.returncode != 0:
            # G1 contract: non-zero exit == no pane was created, so refusing
            # here leaves no half-created state anywhere (AC1-ERR).
            detail = (proc.stderr or proc.stdout or "").strip()
            raise DispatchAskError(
                f"mux pane spawn failed in session {session!r}: "
                f"{detail or 'no output'} (no registry row written; "
                "there is no daemon-PTY fallback)",
                exit_code=1,
            )
        elif exact:
            try:
                payload = json.loads((proc.stdout or "").strip().splitlines()[-1])
                pane_id = int(payload["pane_id"])
                placement_receipt = payload.get("placement")
            except (ValueError, KeyError, IndexError, json.JSONDecodeError) as exc:
                raise DispatchAskError(
                    f"mux pane run returned unparseable --json {proc.stdout!r} "
                    f"for session {session!r}; a pane may exist without a "
                    f"registry row - inspect with 'fno mux pane ls --session "
                    f"{session}'",
                    exit_code=1,
                ) from exc
        else:
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
        from fno.agents.spawn_gate import _process_start_time

        pid_start_time = _process_start_time(child_pid) if child_pid is not None else None

        # Interactive readiness is per harness and runs on every pane spawn;
        # process liveness alone never earns a ready receipt.
        readiness, readiness_detail = _await_interactive_readiness(
            provider,
            session,
            pane_id,
            runner,
            permission_action=(
                "allow_always"
                if yolo or permission_mode in {"yolo", "bypassPermissions"}
                else None
            ),
        )
        if readiness == "failed" or (recovered and readiness != "ready"):
            if recovered and readiness != "failed":
                readiness_detail = (
                    f"recovered pane never proved it started (readiness "
                    f"{readiness!r}, not ready)"
                )
            reaped, cleanup_detail = _reap_spawned_pane(session, pane_id, runner)
            if reaped:
                raise DispatchAskError(
                    f"agent {name!r} {readiness_detail} in session "
                    f"{session!r}; pane {pane_id} reaped, no registry row written",
                    exit_code=1,
                )
            raise DispatchAskError(
                f"agent {name!r} {readiness_detail}; pane "
                f"{pane_id} may still exist in session {session!r} because "
                f"exact cleanup failed: {cleanup_detail}",
                exit_code=1,
            )
        spawned_by_session, spawned_by_harness, spawned_by_cwd = _capture_parent_edge()
        # spawn_trigger was already popped before the pane-run env snapshot above.

        # The receipt's two independent facts (see MuxSpawnResult.bound), plus
        # the death-branch evidence. Only the codex route fills them in today;
        # every other route leaves the pane unprobed, which reads as None
        # ("not answered") rather than a fabricated True.
        pane_alive: Optional[bool] = None
        unbound_reason: Optional[str] = None
        death_log_path = ""
        death_tail = ""
        pane_died = False
        #: Set only when the spawn already knows the row's terminal status and
        #: must not let the id-less heuristic below relabel it.
        forced_row_status: Optional[AgentStatus] = None

        # opencode and codex ids are discovered, not minted (see the two
        # backfills). A miss leaves the row exactly as live-only as before
        # capture existed, so it is logged rather than raised - the pane is
        # already running and a missing id costs resume, not the spawn.
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
        elif provider == "codex":
            from fno.agents.harness_map import capabilities

            binding_caps = capabilities(provider)["session_binding"]
            # child_pid reads the id race-free from the pane's open rollout. A
            # pid-less row (_lookup_child_pid best-effort miss) is left id-less:
            # the id is what routes the row into reconcile's mux branch, and a
            # pid-less row there would be kept immortal (pid_live maps None to
            # true), so never stamp one without a pid to correlate (Codex P1/P2, #603).
            if child_pid is not None:
                _pid = child_pid
                binding_window_s = binding_caps["timeout_ms"] / 1000
                if _BINDING_WINDOW_ENV in os.environ:
                    binding_window_s = _binding_window_s()
                binding = _await_pane_binding(
                    {"session": session, "pane_id": pane_id},
                    # attempts=1: the binding loop owns the retry, because it
                    # also watches for the pane dying BETWEEN probes - which a
                    # bare retry loop cannot see, and which is the whole bug.
                    lambda: _backfill_codex_session_id(
                        cwd,
                        spawn_started_ms,
                        sessions_dir=codex_sessions_dir,
                        child_pid=_pid,
                        attempts=1,
                    ),
                    runner=runner,
                    window_s=binding_window_s,
                    label="codex",
                )
                session_uuid = binding.session_id
                pane_alive = binding.pane_alive
                if session_uuid is None:
                    unbound_reason = binding.reason
                    if binding.pane_alive is False:
                        # The pane is confirmed gone and never reached codex.
                        # Write the row anyway, terminal and carrying its log:
                        # the row plus its evidence IS the finding, and dropping
                        # it reproduces the information loss that left this bug
                        # unexplained for weeks.
                        pane_died = True
                        forced_row_status = "failed"
                        death_log_path = _write_pane_death_log(
                            name, binding.tail, pane_id=pane_id
                        )
                        death_tail = binding.tail
            else:
                # No pid means nothing to correlate the rollout against, so the
                # row is deliberately left id-less (stamping one without a pid
                # would make it immortal in reconcile - Codex P1/P2, #603). It
                # is still an unbound receipt and still owes a reason: an
                # unbound receipt whose reason is null is the same empty signal
                # this whole change exists to remove.
                unbound_reason = "no-child-pid-to-correlate"
            if session_uuid is None:
                from fno.agents import events as _events

                _events.emit(
                    "agent_session_id_uncaptured",
                    name=name,
                    harness=provider,
                    cwd=str(cwd),
                    reason="no unique codex rollout for this cwd after spawn",
                )
                if binding_caps["required"] and not pane_died:
                    reaped, cleanup_detail = _reap_spawned_pane(session, pane_id, runner)
                    if reaped:
                        raise DispatchAskError(
                            f"agent {name!r} required {provider} session binding "
                            f"({unbound_reason or 'not-captured'}); pane {pane_id} reaped, "
                            "no registry row written",
                            exit_code=1,
                        )
                    raise DispatchAskError(
                        f"agent {name!r} required {provider} session binding but cleanup "
                        f"failed: {cleanup_detail}; pane {pane_id} may still exist",
                        exit_code=1,
                    )
        elif provider == "claude" and not pin_session:
            # happy owns the id on this route, so the spawn CANNOT know it and
            # deliberately does not try. Guessing from the transcript store was
            # the obvious move and it is unsound: two panes starting in one cwd
            # snapshot the same baseline, and whichever writes its row first can
            # claim the other's session - binding the row to a healthy stranger,
            # which is worse than binding it to nothing.
            #
            # The worker names itself instead. Its SessionStart hook restamps
            # this row (keyed on FNO_AGENT_SELF, the one identity a harness
            # cannot re-mint) with the id it is actually using, which is proof
            # rather than inference. Until that lands the row stays id-less and
            # `spawning` - created, not yet addressable.
            from fno.agents import events as _events

            _events.emit(
                "agent_session_id_uncaptured",
                name=name,
                harness=provider,
                cwd=str(cwd),
                reason="happy owns the claude session id; awaiting SessionStart restamp",
            )

        # LD4: a recovered pane earns its row only by proving it started. The
        # readiness gate above already required a painted frame (not merely a
        # live child); this closes the other half. A non-happy claude spawn
        # already has session_uuid minted up front (pin_session) and the happy
        # route reaps on its own registration-wait failure below regardless of
        # `recovered` - so only opencode/codex, whose ids are DISCOVERED, can
        # reach here with a live pane and no proof of identity. The normal
        # (non-recovered) path tolerates that miss and logs
        # `agent_session_id_uncaptured` because the pane is known-good from its
        # own launch receipt; a recovered pane has no such receipt, so the same
        # miss here is the exact orphan this node exists to prevent.
        if (
            recovered
            and not pane_died
            and provider in ("codex", "opencode")
            and session_uuid is None
        ):
            reaped, cleanup_detail = _reap_spawned_pane(session, pane_id, runner)
            if reaped:
                raise DispatchAskError(
                    f"agent {name!r} recovered pane {pane_id} in session "
                    f"{session!r} never proved a {provider} session id; pane "
                    "reaped, no registry row written",
                    exit_code=1,
                )
            raise DispatchAskError(
                f"agent {name!r} recovered pane {pane_id} never proved a "
                f"{provider} session id; pane may still exist in session "
                f"{session!r} because cleanup failed: {cleanup_detail}",
                exit_code=1,
            )

        # Crown stamp (US9): the grantor is the spawning session (the parent edge
        # captured above), or "human" for a direct human spawn with no session
        # env - never a caller-supplied value. Only stamped when a crown was
        # actually requested (crown_level is not None).
        crown_grantor_val = (spawned_by_session or "human") if crown_level is not None else None

        # x-ae2d: record WHICH route this pane launched with, so a later relaunch
        # (which re-launches a process rather than attaching to this live one) can
        # re-apply it or refuse. A happy pane carries its route as env(1) plus
        # --claude-env rather than --settings, so nothing has materialized the
        # file yet; materializing here is what makes the route recoverable at
        # all -- including the credential, which by design rides no LONG-LIVED
        # argv token (it is still on the short-lived `mux pane run` client and
        # pre-exec `env` command lines; see happy_pane_argv). The
        # writer is content-addressed, so this costs one 0600 file per distinct
        # route, not one per spawn.
        #
        # ROUTE only, never an account overlay: the account settings writer drops
        # CLAUDE_CONFIG_DIR by construction, so recording one would promise a
        # restore that silently leaves the account behind.
        #
        # CLAUDE only, for the same reason. A codex route lives in `-c` config
        # args (`model_providers.<name>` + `model_provider`), not in the env -
        # `CodexRoute.env` carries only the API key. Recording that env would let
        # a relaunch "restore the route" into codex's DEFAULT provider holding the
        # route's key: half a restore, reported as a whole one. Codex route
        # survival needs an artifact this design does not model, so it stays
        # unrecorded and codex relaunch behavior is unchanged.
        # Materialized inside the reap guard below, NOT here: it does mkdir +
        # open + replace under the state dir, the pane is already running by this
        # point, and an OSError escaping uncaught would leave a live pane with no
        # registry row - the exact orphan `_reap_spawned_pane` exists to prevent.
        from fno.agents.model_routing import route_settings_path_for

        route_settings_path: Optional[str] = None

        stored_session_uuid: Optional[str] = None
        row_status: AgentStatus = "live"
        crown_declined = False
        crown_succeeded = False

        def _append(rows: list[AgentEntry]) -> list[AgentEntry]:
            nonlocal stored_session_uuid, row_status, crown_level, crown_scope, crown_grantor_val, crown_declined, crown_succeeded
            # Reclaiming a dead row's name: drop the corpse in the SAME
            # transaction that appends its replacement, so the registry never
            # holds two rows under one name. Re-checked here, under the write
            # lock, against the rows actually being written - the pre-lock read
            # above decided, this enforces.
            if replaced_terminal is not None:
                kept = [
                    r
                    for r in rows
                    if not (r.name == name and r.status in _RECLAIMABLE_STATUSES)
                ]
                # REFUSE rather than fall through. `replaced_terminal` was decided
                # from a pre-lock read, and `_stamp_status` writes under the
                # registry lock without holding the agent lock, so a concurrent
                # writer can flip that row off a reclaimable status in between -
                # e.g. an `exited` row revived. Falling through would append a
                # SECOND row under one name, breaking the very invariant this
                # block exists to hold. A refusal is recoverable; a duplicate is
                # not.
                if any(r.name == name for r in kept):
                    raise DispatchAskError(
                        f"agent {name!r} was reclaimable when checked but is not "
                        "now (a concurrent writer changed its status); nothing "
                        f"was written - re-run, or 'fno agents rm {name}' first",
                        exit_code=2,
                    )
                rows = kept
            # Claim check, inside the registry write lock so it is atomic with
            # the stamp. Two panes racing in one cwd can each see the SAME lone
            # candidate (the second pane's session may not exist yet when both
            # query), and the ambiguity rule cannot catch that - it only sees one
            # row. Whichever append lands first owns the id; the loser drops to
            # live-only rather than pointing resume at another pane's session.
            claimed = session_uuid is not None and any(
                r.harness_session_id == session_uuid for r in rows
            )
            stored_session_uuid = None if claimed else session_uuid
            # One-live-crown guard (x-7685): if another non-terminal row already
            # holds this scope, decline the crown and spawn uncrowned. Same lock,
            # same rows, same idiom as the claim check above. A worker without a
            # crown is recoverable; a duplicate crown over one scope is not.
            #
            # UNLESS the holder is the caller: that is SUCCESSION, and it is the
            # only way an abdicating king hands off, since a session that has
            # already exited cannot spawn its heir. The vacate and the stamp land
            # in this one write, so the scope is never doubly nor un-ruled.
            # A spawn that already knows it is writing a TERMINAL row must not
            # touch the crown at all. Succession vacates the caller's own row in
            # this same transaction, so crowning a corpse would move the scope
            # off a live king onto a row that will never act, leaving the scope
            # unruled and the caller silently stripped - and the codex death
            # path raises exit 13 immediately afterwards, so nothing would put
            # it back.
            if forced_row_status in TERMINAL_STATUSES:
                crown_level = None
                crown_scope = None
                crown_grantor_val = None
            if crown_level is not None and crown_scope:
                holders = [
                    r
                    for r in rows
                    if r.crown_scope == crown_scope
                    and r.status not in TERMINAL_STATUSES
                ]

                if holders and all(is_caller_row(h, spawned_by_session, spawned_by_harness) for h in holders):
                    for idx, r in enumerate(rows):
                        if r.crown_scope == crown_scope and is_caller_row(r, spawned_by_session, spawned_by_harness):
                            rows[idx] = replace(
                                r,
                                crown_level=None,
                                crown_scope=None,
                                crown_grantor=None,
                            )
                    crown_succeeded = True
                elif holders:
                    crown_level = None
                    crown_scope = None
                    crown_grantor_val = None
                    crown_declined = True
            # A pane with no identified session is created but not addressable.
            # Keep that transition explicit instead of calling it live - for the
            # happy-hosted claude route as much as for codex, where an id-less
            # row reporting "live" is exactly the corpse that passes every
            # liveness check the tooling offers.
            row_status = (
                forced_row_status
                if forced_row_status is not None
                else (
                    "spawning"
                    if stored_session_uuid is None
                    and (
                        provider == "codex"
                        or (provider == "claude" and not pin_session)
                    )
                    else "live"
                )
            )
            # Resolvable-handle fallback (x-7bcd): the pane itself is a live
            # ref (the `mux` field below), but that is not one of the three
            # legs the write-choke-point guard checks. Evaluated HERE, with
            # the FINAL stored_session_uuid (post claim-dedup above) rather
            # than the outer session_uuid, because a claim race can null it
            # out after this row already looked resolvable. The pid leg
            # needs BOTH child_pid AND a readable pid_start_time (a fake,
            # exited, or unreadable pid still leaves pid_start_time None) -
            # when none of the three will resolve, touch a stable per-name
            # log file so the row always has a resolvable handle and the
            # guard never refuses a live pane's registry write.
            final_log_path = death_log_path
            if not _has_resolvable_handle(
                pid=child_pid,
                pid_start_time=pid_start_time,
                log_path=final_log_path,
                harness=provider,
                harness_session_id=stored_session_uuid,
            ):
                touched_log_path = _touch_log_path(name)
                final_log_path = str(touched_log_path) if touched_log_path is not None else ""
            rows.append(
                AgentEntry(
                    name=name,
                    harness=provider,
                    provider=route_provider,
                    cwd=str(cwd),
                    # Written in the SAME registry transaction as the status, so
                    # a concurrent reconcile cannot land one without the other
                    # and lose the only evidence the death left.
                    log_path=final_log_path,
                    harness_session_id=stored_session_uuid,
                    status=row_status,
                    pid=child_pid,
                    pid_start_time=pid_start_time,
                    mux={"session": session, "pane_id": pane_id},
                    spawned_by_session=spawned_by_session,
                    spawned_by_harness=spawned_by_harness,
                    spawned_by_cwd=spawned_by_cwd,
                    spawn_trigger=spawn_trigger,
                    crown_level=crown_level,
                    crown_scope=crown_scope,
                    crown_grantor=crown_grantor_val,
                    route_settings_path=route_settings_path,
                )
            )
            return rows

        try:
            # Route-bearing rows only: the path is the restore contract, and an
            # account-only file (scrub floor + non-profile env) restores as "no
            # route" or as an incomplete unit the composition guard refuses,
            # which turned every --account worker's revive into an exit-2
            # refusal. account_env is included whenever a route IS present so
            # the row names the same composed file the wrapper rendered; a
            # route-only stamp would make a later restore silently drop the
            # account's pinned env.
            if provider == "claude" and route_env:
                route_settings_path = route_settings_path_for(route_env, account_env)
            _declined_scope = crown_scope if crown_level is not None else None
            update_registry(_append, path=registry_path)
            if crown_declined and _declined_scope:
                print(
                    f"spawn: crown declined (scope {_declined_scope!r} already held "
                    "by a live row); spawned uncrowned. The worker launched without a crown.",
                    file=sys.stderr,
                )
            elif crown_succeeded and _declined_scope:
                print(
                    f"spawn: crown over {_declined_scope!r} transferred from this "
                    f"session to {name} (succession). You no longer hold it.",
                    file=sys.stderr,
                )
            # Birth (x-8cd5 Wave 6): the row is written, so the pane worker now
            # exists in the registry. Emit to the daemon lifecycle log so this
            # birth joins the daemon's death events. The registry row leaves
            # short_id empty (mux is its one live transport ref) and carries
            # pid=child_pid, and the daemon death reads that row, so the durable
            # birth<->death key is name (always) and pid (the child pid) - not
            # the mux pane_id, which no death event carries. short_id stays None
            # to match the row the death reads, rather than naming an id
            # (pane_id) the death will never repeat.
            from fno.agents import events as _spawn_events

            _spawn_events.emit_spawned(
                name=name,
                short_id=None,
                pid=child_pid,
                provider=provider,
                spawned_by_session=spawned_by_session,
                spawned_by_harness=spawned_by_harness,
                spawned_by_cwd=spawned_by_cwd,
            )
        except (AgentResolutionError, OSError, ValueError, RegistryVersionError) as exc:
            # No row was written, so the orphan's later death would join no
            # birth. Record the failed start in the daemon log (x-8cd5 Wave 6).
            from fno.agents import events as _spawn_events

            _spawn_events.emit_spawn_failed(
                name=name, provider=provider, short_id=None, reason=f"registry-write: {exc}"
            )
            reaped, cleanup_detail = _reap_spawned_pane(session, pane_id, runner)
            if reaped:
                raise DispatchAskError(
                    f"registry write failed: {exc}; pane {pane_id} reaped, no registry row written",
                    exit_code=12,
                ) from exc
            raise DispatchAskError(
                f"registry write failed: {exc}; pane {pane_id} may still exist in "
                f"session {session!r} because exact cleanup failed: {cleanup_detail}",
                exit_code=12,
            ) from exc

        if pane_died:
            # The pane existed and the worker never reached its provider. Fail
            # LOUD and non-zero: an exit-0 receipt here is exactly the lie -
            # the caller cannot tell a corpse from a slow starter, and re-prompts
            # the corpse. The row survives (status `failed`, terminal, so
            # `fno agents rm` needs no --force) and carries the captured output.
            # "" has two causes and they need different words: nothing was
            # captured at all, or capture worked and the write failed. Telling
            # an operator to read a tail that was never printed is worst on the
            # exact path where they have least to go on.
            if death_log_path:
                where = f"captured output: {death_log_path}"
            elif death_tail.strip():
                where = "the captured tail could not be written to disk; it is above"
            else:
                where = "NO pane output was captured, so the cause is not recorded"
            if death_tail.strip():
                echo = "\n".join(death_tail.splitlines()[-_PANE_TAIL_ECHO_LINES:])
                print(
                    f"spawn: {name} pane {pane_id} died before binding; last output:\n{echo}",
                    file=sys.stderr,
                )
            raise DispatchAskError(
                f"agent {name!r} never reached codex: the pane was created and its "
                f"child exited before a session was bound, so nothing is running. "
                f"The registry row is `failed` (not live) and {where}. "
                "Read that output for the cause, then retry - the row is kept as "
                "evidence but it is terminal, so a respawn under the same name "
                "reclaims it rather than colliding. Or spawn with --substrate bg.",
                exit_code=13,
            )

        # A happy-hosted claude row is created id-less (`spawning`)
        # because happy owns the session id and restamps the row via the worker's
        # SessionStart hook. If that restamp never lands the row strands
        # `spawning` forever, an unrecoverable pane (unreachable on control.sock,
        # so mail-inject cannot rescue it) whose receipt would read as a soft
        # success. Wait for the restamp within a bounded window; on timeout reap
        # the pane, drop the stranded row, and fail loud so the receipt earns its
        # status. Scoped to the happy route: a codex id-less row is a different
        # mechanism (spawn-time backfill, not a worker hook) and its pane is
        # alive, so reaping it would be a regression.
        if provider == "claude" and resolved_monitor == "happy":
            registered_id, reg_reason = _await_pane_registration(
                name, {"session": session, "pane_id": pane_id}, runner, registry_path
            )
            if registered_id is None:
                reaped, cleanup_detail = _reap_spawned_pane(session, pane_id, runner)
                # Only drop the row once the pane is actually gone: removing it
                # while the pane may still live orphans a worker fno can no
                # longer point the operator at. Track removal success so the
                # error never claims "row removed" when it was not.
                row_removed = False
                if reaped:
                    this_mux = {"session": session, "pane_id": pane_id}
                    try:
                        update_registry(
                            lambda rows: [
                                r
                                for r in rows
                                if not (r.name == name and r.mux == this_mux)
                            ],
                            path=registry_path,
                        )
                        row_removed = True
                    except (
                        OSError,
                        ValueError,
                        AgentResolutionError,
                        RegistryVersionError,
                    ):
                        row_removed = False
                if reaped and row_removed:
                    tail = "pane reaped, registry row removed"
                elif reaped:
                    tail = (
                        "pane reaped, but registry row removal failed; a "
                        f"`spawning` row for {name!r} may linger - remove "
                        f"with 'fno agents rm {name}'"
                    )
                else:
                    tail = (
                        f"pane kill failed ({cleanup_detail}); it may still "
                        f"exist in session {session!r} - remove with "
                        f"'fno mux pane kill --session {session} {pane_id}'"
                    )
                raise DispatchAskError(
                    f"agent {name!r} did not register within "
                    f"{_PANE_REGISTRATION_DEADLINE_S}s ({reg_reason}); {tail}. "
                    "The worker's session id never arrived, so the pane was "
                    "neither addressable nor seeded - retry, or spawn with "
                    "--substrate bg.",
                    exit_code=1,
                )
            stored_session_uuid = registered_id
            row_status = "live"

        # Claude and Codex both resolve the canonical full harness id through the
        # generated mailbox handle. The row keeps short_id empty because mux is
        # its one live transport ref; the receipt may still hand out the derived
        # handle. Derive it via canonical_handle, the Python source for that
        # string; the send path, registry name fallback, and drain all read that
        # same function (see fno.harness_identity).
        from fno.harness_identity import canonical_handle

        session_uuid = stored_session_uuid
        short_id_val = (
            canonical_handle(session_uuid)
            if provider in ("claude", "codex") and session_uuid
            else ""
        )

    # `bound` is keyed on the session uuid, NOT on short_id: opencode's
    # transport key is not short_id (US8), so it leaves short_id empty while
    # being perfectly bound. For claude and codex, where short_id IS the handle,
    # the two agree, and a test pins that. gemini and agy bind no session at all
    # and resolve to None rather than to either lie.
    bound_val = _resolve_bound(session_uuid, provider)
    return MuxSpawnResult(
        name=name,
        provider=provider,
        session=session,
        pane_id=pane_id,
        child_pid=child_pid,
        session_uuid=session_uuid,
        short_id=short_id_val,
        status=row_status,
        bound=bound_val,
        pane_alive=pane_alive,
        unbound_reason=_resolve_unbound_reason(bound_val, unbound_reason, provider),
        log_path=death_log_path,
        effective_message=effective_message,
        placement=placement_receipt,
        recovered=recovered,
        readiness=readiness,
        readiness_rule=(
            readiness_detail.split("=", 1)[1].split(";", 1)[0]
            if readiness_detail.startswith(("ready-marker=", "blocked-rule="))
            else None
        ),
    )
