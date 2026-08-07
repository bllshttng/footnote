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
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Mapping, Optional

from fno import paths
from fno.agents.dispatch import (
    DispatchAskError,
    _capture_parent_edge,
    validate_spawn_name,
)
from fno.agents.harness_map import DispatchResolveError, normalize_command
from fno.agents.lock import hold_agent_lock
from fno.agents.registry import (
    AgentEntry,
    AgentResolutionError,
    AgentStatus,
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
    # The addressable mail handle, derived from ``session_uuid`` by
    # ``harness_identity.canonical_handle`` (the single source for that string -
    # do not restate which slice it takes); "" for providers whose transport key
    # is not short_id (US8).
    short_id: str = ""
    # A Codex pane whose rollout has not appeared yet is created but not
    # addressable. Keep that transition explicit instead of calling it live.
    status: str = "live"
    effective_message: Optional[str] = None
    # Server-authored exact-placement receipt (x-6928): anchor/direction/fallback
    # + squad/tab the split landed in. None unless `--at` pinned the origin.
    placement: Optional[dict] = None


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
    file, so routed values must use its ``--claude-env`` interface instead. It
    consumes ``--session-id`` the same way: the session id belongs to happy on
    this route and is discovered after the spawn, never pinned before it.

    These refusals are on the ONLY reachable happy path, so the usual "a guard
    on one of N paths is decorative" audit does not apply here and does not need
    re-running: this function has exactly one production caller (the
    ``resolved_monitor == "happy"`` branch below), there is no happy launcher in
    the Rust crates or the shell dispatchers, and the ``--monitor happy`` flag
    itself already requires pane + claude + zai and refuses a separate
    ``--model``. Re-verify with an anchored sweep (``grep -rn happy_pane_argv``
    over explicit trees, not an ``rg`` glob exclude) if that branch ever grows a
    sibling.

    The carry below is lossless by construction -- it iterates the route, so it
    cannot drop a key -- and the test suite holds it to set equality against a
    full seven-key route, not just the endpoint pair.
    """
    if any(arg == "--settings" or arg.startswith("--settings=") for arg in argv):
        raise DispatchAskError(
            "refusing to launch a routed claude pane through happy with "
            "--settings: happy consumes that flag for its own hook server and "
            "discards the caller's file, so the route would be silently ignored "
            "and the worker would launch on the default account. Carry the route "
            "as --claude-env KEY=VALUE instead.",
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
    claude_env: list[str] = []
    for key, value in route_env.items():
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
    """
    from fno.agents.discover import codex_session_ids_started_in

    naptime = sleep or time.sleep
    if child_pid is not None:
        # Race-free primary path: the child's open rollout identifies its own
        # session. codex opens the rollout shortly after start, so retry until it
        # appears; never fall through to cwd/time guessing when we have a pid.
        for attempt in range(_CODEX_BACKFILL_ATTEMPTS):
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
    for attempt in range(_CODEX_BACKFILL_ATTEMPTS):
        if attempt:
            naptime(_CODEX_BACKFILL_DELAY_S)
        ids = codex_session_ids_started_in(cwd, since_ms, sessions_dir=sessions_dir)
        if len(ids) > 1:
            return None  # ambiguous; retrying cannot narrow it
        if len(ids) == 1 and ids[0] == prev:
            return ids[0]
        prev = ids[0] if ids else None
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
    if message.strip().startswith(("/", "$fno:")):
        message = normalize_command(message, provider)

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
        # Any sandboxed posture (including --full-auto and an explicit
        # <sandbox>:<approval>) inherits codex's read-only .git carveout and
        # cannot commit without the grant. Only the two bypass postures skip it.
        from fno.agents.providers.codex import git_writable_args

        bounded = (permission_mode != "yolo") if permission_mode else not yolo
        if bounded:
            argv += git_writable_args(cwd)
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
    from fno.harness_identity import AMBIENT_IDENTITY_ENV

    for _id_name in AMBIENT_IDENTITY_ENV:
        unset += ["-u", _id_name]
    if provider == "claude":
        # Worker parity: transcripts must persist for resume/adoption.
        pairs.append("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1")
        # Raise the harness Stop-hook block cap so fno's repeated-block loop is
        # not force-ended at the default 9 (x-1680). The helper honors an
        # operator-set value, so an explicit env wins over the fno default.
        from fno.agents.providers.claude import claude_stop_hook_block_cap

        pairs.append(f"CLAUDE_CODE_STOP_HOOK_BLOCK_CAP={claude_stop_hook_block_cap()}")
    # Per-spawn account overlay (x-d012): profile (CLAUDE_CONFIG_DIR) + the
    # account's own login. SCRUB inherited auth vars (env -u) so an ambient
    # ANTHROPIC_API_KEY/CLAUDE_CODE_OAUTH_TOKEN can't override the account's
    # login and bill the wrong account. Applied BEFORE the route so a route (when
    # both are present, x-5ed4) wins endpoint+auth+model atomically (x-2af5):
    # env(1) assignments are left-to-right last-wins, so the route pairs below
    # override the account's, while CLAUDE_CONFIG_DIR survives.
    if account_env:
        from fno.agents.account_env import SCRUB_AUTH_VARS

        for _k in SCRUB_AUTH_VARS:
            unset += ["-u", _k]
        pairs += [f"{k}={v}" for k, v in account_env.items()]
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
    if role or route_env:
        route = route_env
        if route is None:
            from fno.agents.model_routing import resolve_route

            route = resolve_route(role)
        if route:
            # Scrub the parent's Anthropic creds so the routed AUTH_TOKEN wins:
            # a lingering API key or subscription OAuth token would otherwise
            # override it and send the routed pane back to Anthropic. `env -u`
            # on an unset var is a harmless no-op. `unset +=` (not `=`) so the
            # account/provenance unsets above are preserved.
            if provider == "claude":
                unset += [
                    "-u",
                    "ANTHROPIC_API_KEY",
                    "-u",
                    "CLAUDE_CODE_OAUTH_TOKEN",
                ]
            pairs += [f"{k}={v}" for k, v in route.items()]
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


def _mux_pane_alive(mux: dict, runner=subprocess.run) -> Optional[bool]:
    """Return exact pane liveness, or ``None`` when the mux cannot answer."""
    try:
        proc = _run_mux(
            [
                "mux", "pane", "wait", "--session", str(mux["session"]),
                str(mux["pane_id"]), "--timeout", "0",
            ],
            runner,
        )
    except (KeyError, DispatchAskError):
        return None
    if proc.returncode == _WAIT_EXITED:
        return False
    if proc.returncode in {0, 11}:
        return True
    return None


#: Bounded window an id-less happy-claude pane row waits for its worker to
#: register the session id (the SessionStart restamp). happy owns the id, so the
#: row is created `spawning` and only becomes addressable once the worker names
#: itself; if that never lands the row strands `spawning` forever, an
#: unrecoverable pane whose receipt would read as a soft success. The working
#: case lands well under this; the ceiling is paid only on failure (x-1406).
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
                (e for e in load_registry(path=registry_path) if e.name == name),
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


def _await_interactive_readiness(
    session: str,
    pane_id: int,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
) -> tuple[str, str]:
    """Interactive readiness gate (x-6928).

    A painted first frame plus a still-live child after a 1s dwell is READY; an
    alive but unpainted child at the deadline is LIVE; an early child exit is
    FAILED. Probed through the mux CLI so the gate works across the subprocess
    boundary: ``pane wait`` (exit 12 == the child exited) for liveness, and
    ``pane read`` (non-empty == painted) for the ready/live label. Only FAILED
    stops the spawn (AC5-ERR); READY and LIVE both proceed to the registry row.
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
                str(pane_id), "--lines", "1",
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
    readiness = "ready" if (painted.stdout or "").strip() else "live"
    return readiness, ""


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
    runner: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
    codex_sessions_dir: Optional[Path] = None,
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
    # Launch-time headroom picking (x-7d45). `pane` is the DEFAULT substrate and
    # `cmd_spawn` routes it straight here, never through `dispatch_spawn` - so a
    # picker wired only there would cover bg/headless and leave every default
    # interactive spawn on the exhausted account while the option read enabled.
    # Same helper, same rules (explicit account wins, routed spawns are skipped).
    if account_env is None and provider == "claude":
        from fno.agents.dispatch import _pick_account_env

        picked = _pick_account_env(role=role, route_env=route_env)
        account_env = dict(picked) if picked is not None else None

    launch_role = role
    if provider == "claude" and (role is not None or route_env):
        from fno.agents.model_routing import (
            RouteCompositionError,
            resolve_spawn_route,
        )

        try:
            route_env = resolve_spawn_route(role, route_env, account_overlay=bool(account_env))
        except RouteCompositionError as exc:
            raise DispatchAskError(str(exc), exit_code=2) from exc
        launch_role = None

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
    # Every resolve_monitor input is available here, so the hoist leaves the
    # unmonitored route byte-identical.
    resolved_monitor = resolve_monitor(
        monitor,
        harness=provider,
        route_provider=route_provider,
        route_env=route_env,
        account_env=account_env,
        model=model,
    )
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
    )
    if codex_route is not None:
        argv = [argv[0], *codex_route.config_args, *argv[1:]]
    if provider == "claude" and not claude_argv_is_interactive(argv):
        raise DispatchAskError(
            "refusing to pane-host claude with -p/--print (that bills the "
            "Agent SDK pool); the mux spawns interactive subscription-billed "
            "claude",
            exit_code=2,
        )
    # Keep the outer env wrapper: it scrubs inherited Anthropic credentials,
    # while --claude-env reasserts the complete route in happy's claude child.
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
        proc = _run_mux(
            run_args,
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
        placement_receipt: Optional[dict] = None
        if exact:
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

        if exact:
            # Interactive readiness gate (x-6928): hold the registry row and the
            # success receipt until the provider proves it launched. An early
            # exit (AC5-ERR) reaps ONLY this pane - the mux's tree normalization
            # collapses the split, the viewer's focus and any later sibling split
            # survive (AC5-FR/AC6-FR), and no registry row is written.
            readiness, readiness_detail = _await_interactive_readiness(
                session, pane_id, runner
            )
            if readiness == "failed":
                reaped, cleanup_detail = _reap_spawned_pane(
                    session, pane_id, runner
                )
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
            # child_pid reads the id race-free from the pane's open rollout. A
            # pid-less row (_lookup_child_pid best-effort miss) is left id-less:
            # the id is what routes the row into reconcile's mux branch, and a
            # pid-less row there would be kept immortal (pid_live maps None to
            # true), so never stamp one without a pid to correlate (Codex P1/P2, #603).
            if child_pid is not None:
                session_uuid = _backfill_codex_session_id(
                    cwd,
                    spawn_started_ms,
                    sessions_dir=codex_sessions_dir,
                    child_pid=child_pid,
                )
            if session_uuid is None:
                from fno.agents import events as _events

                _events.emit(
                    "agent_session_id_uncaptured",
                    name=name,
                    harness=provider,
                    cwd=str(cwd),
                    reason="no unique codex rollout for this cwd after spawn",
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

        # Crown stamp (US9): the grantor is the spawning session (the parent edge
        # captured above), or "human" for a direct human spawn with no session
        # env - never a caller-supplied value. Only stamped when a crown was
        # actually requested (crown_level is not None).
        crown_grantor_val = (spawned_by_session or "human") if crown_level is not None else None

        # x-ae2d: record WHICH route this pane launched with, so a later relaunch
        # (which re-launches a process rather than attaching to this live one) can
        # re-apply it or refuse. A happy pane carries its route as --claude-env
        # rather than --settings, so nothing has materialized the file yet;
        # materializing here is what makes the route recoverable at all. The
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

        def _append(rows: list[AgentEntry]) -> list[AgentEntry]:
            nonlocal stored_session_uuid, row_status, crown_level, crown_scope, crown_grantor_val, crown_declined
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
            # same rows, same idiom as the claim check above. spawn --crown was
            # made reachable by the _is_crown_bearing_spawn routing fix; without
            # this guard it stamps a duplicate crown. A worker without a crown is
            # recoverable; a duplicate crown over one scope is not.
            if crown_level is not None and crown_scope:
                _terminal = {"exited", "orphaned", "failed", "permanent_dead"}
                if any(r.crown_scope == crown_scope and r.status not in _terminal for r in rows):
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
                "spawning"
                if stored_session_uuid is None
                and (provider == "codex" or (provider == "claude" and not pin_session))
                else "live"
            )
            rows.append(
                AgentEntry(
                    name=name,
                    harness=provider,
                    cwd=str(cwd),
                    log_path="",
                    harness_session_id=stored_session_uuid,
                    status=row_status,
                    pid=child_pid,
                    pid_start_time=pid_start_time,
                    mux={"session": session, "pane_id": pane_id},
                    spawned_by_session=spawned_by_session,
                    spawned_by_harness=spawned_by_harness,
                    spawned_by_cwd=spawned_by_cwd,
                    crown_level=crown_level,
                    crown_scope=crown_scope,
                    crown_grantor=crown_grantor_val,
                    route_settings_path=route_settings_path,
                )
            )
            return rows

        try:
            if provider == "claude":
                route_settings_path = route_settings_path_for(route_env)
            _declined_scope = crown_scope if crown_level is not None else None
            update_registry(_append, path=registry_path)
            if crown_declined and _declined_scope:
                import sys
                print(
                    f"spawn: crown declined (scope {_declined_scope!r} already held "
                    "by a live row); spawned uncrowned. The worker launched without a crown.",
                    file=sys.stderr,
                )
        except (AgentResolutionError, OSError, ValueError, RegistryVersionError) as exc:
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

        # x-1406: a happy-hosted claude row is created id-less (`spawning`)
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
                    try:
                        update_registry(
                            lambda rows: [r for r in rows if r.name != name],
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

    return MuxSpawnResult(
        name=name,
        provider=provider,
        session=session,
        pane_id=pane_id,
        child_pid=child_pid,
        session_uuid=session_uuid,
        short_id=short_id_val,
        status=row_status,
        effective_message=effective_message,
        placement=placement_receipt,
    )
