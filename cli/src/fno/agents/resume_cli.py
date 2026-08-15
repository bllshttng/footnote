"""fno.agents.resume_cli — ``fno agents resume`` subcommand.

Task 3.4 from 2026-05-22-fno-agents-observability.md.

Resolves an agent name to its provider + session id + cwd from the
registry and resumes it in the recorded cwd. ``--print-command`` dumps a
shell-pasteable one-liner instead, useful inside Claude Code (which
can't host an interactive TUI from inside a subprocess).

Provider resume substrates (Locked Decision #6, claude arm reworked to wake
instead of exec):

- ``codex`` → ``codex resume <codex_session_id>`` (bypasses the
  exec-source picker filter via direct UUID argument), via
  ``os.execvp``: hands the terminal to the provider CLI.
- ``claude`` → woken headlessly: a pty (``script -q /dev/null``), the
  row's own routed env restored from ``route_settings_path``, the
  message injected as three separate bracketed-paste-safe writes
  (clear / text / submit), and the live state verified to have moved to
  Working. Never execs: ``fno agents attach`` is the interactive
  hand-off; this is the unattended counterpart, and every step in the
  recipe can exit 0 having done nothing, so verification is what makes
  a no-op detectable instead of a lie. Up to two wake attempts, each
  bounded by a fixed ~19s send sequence plus a 60s subprocess timeout, so
  a full failure takes up to roughly two minutes wall-clock, not instant.
  ``--print-command`` still prints the OLD interactive form
  (``claude attach <short_id>``) for a claude row: that is the manual
  escape hatch for a human who wants to type into the session directly,
  distinct from (and no longer equivalent to) what a direct
  ``fno agents resume <name>`` now does.
- ``gemini`` / ``opencode`` → exec into the provider's own resume CLI,
  same as codex.

Exit codes:
- 0   - success (``--print-command``; a non-claude direct resume, where
  ``os.execvp`` replaces the process and the Python interpreter is
  gone; or a claude resume that verified Working).
- 2   - a claude row's recorded route could not be restored; refused
  rather than waking it onto the default account.
- 13  - name not in registry / missing cwd / missing session_id /
  unsupported provider.
- 14  - provider CLI not on ``$PATH``.
- 16  - claude wake attempts ran but the live state never reached
  Working.
"""
from __future__ import annotations

import os
import shlex
import shutil
import signal
import subprocess
import sys
from dataclasses import dataclass
from typing import Any, Optional

import typer


@dataclass(frozen=True)
class ResumeResult:
    """Return shape for the testable resume pipeline (no Typer dep)."""

    exit_code: int
    output: str = ""
    stderr: str = ""
    exec_argv: Optional[list[str]] = None
    exec_cwd: Optional[str] = None


def _session_id_for(entry: Any) -> Optional[str]:
    """Pick the harness-specific session id from an AgentEntry.

    Reads the harness -> field mapping from the shared
    :data:`fno.agents.registry.HARNESS_SESSION_ID_FIELDS` so this
    duck-typed resolver and ``AgentEntry.session_id`` stay in sync. Keyed on
    ``harness`` (x-8dfc) with ``provider`` fallback. Uses ``getattr`` (not the
    property) so it still works on the test fakes, which carry the underlying
    id fields but not the property.

    A claude pane row carries no transport ``short_id`` (empty by design), so
    it falls back to the canonical ``harness_session_id`` - mirroring
    ``AgentEntry.session_id`` - rather than reporting "no session id" for a row
    that has one (x-b84f).
    """
    from fno.agents.registry import HARNESS_SESSION_ID_FIELDS

    key = getattr(entry, "harness", None)
    field_name = HARNESS_SESSION_ID_FIELDS.get(key) if key else None
    transport = getattr(entry, field_name, None) if field_name else None
    if transport:
        return transport
    return getattr(entry, "harness_session_id", None)


def _build_resume_argv(
    provider: str, session_id: str, cwd: Optional[str] = None
) -> Optional[list[str]]:
    """Provider-specific resume argv. Returns None for unsupported providers."""
    if provider == "codex":
        # A bounded codex cannot write git metadata without an explicit grant,
        # and in a linked worktree that metadata sits outside the workspace
        # entirely. `codex resume` takes no --add-dir, so the grant rides -c,
        # which is global and must precede the subcommand.
        from pathlib import Path

        from fno.agents.harnesses.codex import git_writable_config_args

        grant = git_writable_config_args(Path(cwd)) if cwd else []
        return ["codex", *grant, "resume", session_id]
    if provider == "claude":
        # Spec: reuse fno's attach surface. claude's attach is
        # `claude attach <short_id>`.
        return ["claude", "attach", session_id]
    if provider == "opencode":
        # Bare `opencode --session <id>` is the interactive TUI attach (the
        # `codex resume <id>` precedent). The Rust provider's headless
        # `opencode run ... --session <id>` argv is a separate lane.
        return ["opencode", "--session", session_id]
    if provider == "gemini":
        return ["gemini", "--resume", session_id]
    return None


_DEFAULT_WAKE_MESSAGE = "continue"
_WAKE_ATTEMPTS = 2
_WAKE_ATTEMPT_TIMEOUT_SEC = 60.0
_WAKE_TARGET_STATUS = "Working"
# A row already in one of these needs no wake: Working is the target itself,
# Idle is a live, reachable session that may hold unsubmitted composer
# text - injecting keystrokes into it risks destroying that text for no
# benefit, since the row isn't blocked or stopped in the first place - and
# Done is a terminal row that was never going to reach Working no matter how
# many attempts run (visible here for the first time now that
# `claude_agents_json` passes `--all`). Defined lower-case directly (not
# derived from a Title-Case set) since `_state_of()` compares
# case-insensitively: a live_map producer that skips claude.py's own
# Title-Case normalization has slipped an un-normalized status through
# before (see the matching check in read.py's live_status fill-in, ~line
# 168 - both independently enumerate a subset of claude.py's
# `KNOWN_LIVE_STATUSES` and must be kept in sync by hand).
_WAKE_SKIP_STATUSES_LOWER = frozenset({"working", "idle", "done"})


def _default_agents_state_fn() -> dict[str, dict]:
    """Live-status map keyed by short_id, `--all` included.

    `claude agents --json` alone omits stopped/completed rows, so a
    verification read against that narrower view can never observe a
    blocked session (state "Needs input") land on "Working" -- the same
    blind spot that made every reap sweep report success over an empty set.
    """
    from fno.agents.harnesses.claude import claude_agents_json

    live_map, _warnings = claude_agents_json()
    return live_map


def _script_wrapped_attach(short_id: str) -> str:
    """The pty-allocating shell fragment that runs ``claude attach <id>``.

    BSD ``script`` (macOS, the verified environment) takes the command as
    trailing argv: ``script -q /dev/null claude attach <id>``. GNU/util-linux
    ``script`` (Linux) has no such form -- the command rides ``-c`` instead:
    ``script -qc "claude attach <id>" /dev/null``. Branching here picks the
    right syntax up front rather than guessing one and failing silently on
    the other (the wake would report exit 16 with no clue the real cause was
    a platform mismatch).

    A real BSD ``sys.platform`` carries a trailing version number (e.g.
    ``freebsd13``, ``openbsd7``) -- it never ends in the literal substring
    ``"bsd"``, so the check matches on the OS name prefix instead.
    """
    attach_cmd = f"claude attach {shlex.quote(short_id)}"
    _BSD_PREFIXES = ("freebsd", "openbsd", "netbsd", "dragonfly")
    if sys.platform == "darwin" or sys.platform.startswith(_BSD_PREFIXES):
        return f"script -q /dev/null {attach_cmd}"
    return f"script -qc {shlex.quote(attach_cmd)} /dev/null"


def _default_wake_fn(
    short_id: str,
    *,
    message: str,
    route_env: Optional[dict[str, str]],
    cwd: str,
    timeout: float = _WAKE_ATTEMPT_TIMEOUT_SEC,
) -> None:
    """One wake attempt: pty + routed env + clear/send/submit.

    ``claude attach`` allocates no pty of its own; invoked non-interactively
    it prints "Attaching..." and exits having done nothing, which reads as a
    no-op rather than a refusal. ``script`` supplies the pty.

    The attached session runs with bracketed paste on, so a trailing ``\\r``
    sent in the SAME write as the message inserts a newline into the input
    instead of submitting it -- three stacked, unsent wake messages were the
    visible proof of this on a real blocked fleet. The clear (``\\x15``),
    the message, and the submit (``\\r``) are three separate timed writes.

    A row launched on a secondary route (``route_settings_path``) must be
    woken with that SAME env: ``claude attach`` takes no options, so only
    the child process environment can carry ``ANTHROPIC_BASE_URL`` and the
    auth token. The caller resolves ``route_env`` via
    :func:`fno.agents.model_routing.read_route_settings` and hands it here
    already resolved -- this function never reads the settings file itself,
    so a credential never has to round-trip through a shell echo. The
    scrub-then-overlay order matches ``bg_create``/``headless_create``: an
    operator's own ambient ``ANTHROPIC_API_KEY`` is cleared first, so it can
    never sit alongside the routed row's credential in the attaching
    subprocess.

    Runs in its own process group (``start_new_session=True``) so a timeout
    kills the whole ``script``/``claude attach`` tree, not just the
    top-level ``bash``: an orphaned grandchild left holding the pty is
    exactly what would make a retry's second ``claude attach`` race the
    first for the same session.

    Runs ``script``/``claude attach`` from the agent's own recorded ``cwd``,
    matching every other resume arm (the non-claude ``os.chdir(cwd)`` before
    exec, and the Rust exec fallback's ``set_current_dir(cwd)``): ``claude
    attach <short_id>`` looks the session up by id, not by directory, so this
    is not load-bearing for finding the right session, but a wrong cwd would
    still leak into anything the attaching process reads project-locally.

    Does not reuse ``dispatch.py``'s ``_mux_pane_send``/``_paste_then_submit``
    (a guarded send plus content-based transcript confirmation): that
    primitive addresses a mux-hosted PANE, a different substrate from the
    ``claude --bg`` supervisor a blocked/stopped row actually is here, and
    has no ``claude attach`` arm. The fixed sleep sequence below is the
    exact recipe verified against a real blocked fleet; the caller's
    post-attempt state read (``_resume_claude_wake``) is the readiness
    signal this function itself does not have.
    """
    env = dict(os.environ)
    # Scrub only when there is something to restore, matching
    # bg_create/headless_create (harnesses/claude.py): a route-less row (the
    # common default-account case) keeps its ambient auth untouched rather
    # than losing it with nothing put back.
    if route_env:
        from fno.agents.account_env import SCRUB_AUTH_VARS

        for key in SCRUB_AUTH_VARS:
            env.pop(key, None)
        env.update(route_env)
    env["FNO_WAKE_MSG"] = message
    script_cmd = (
        "{ sleep 7; printf '\\x15'; sleep 1; printf '%s' \"$FNO_WAKE_MSG\"; "
        "sleep 2; printf '\\r'; sleep 9; } | "
    ) + _script_wrapped_attach(short_id)
    proc = subprocess.Popen(
        ["bash", "-c", script_cmd],
        env=env,
        cwd=cwd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except OSError:
            pass
        proc.wait()
        # `script`'s pty-attached child (the process that actually execs
        # into `claude attach`) calls login_tty(), which setsid()s it into
        # a BRAND NEW session before exec -- so it is never a member of the
        # bash pgid above, and the killpg call just above does not reach
        # it. Finish the job by matching the unique short_id in its command
        # line instead of by process-group membership; best-effort (no
        # match is the common case when the process already exited on its
        # own after stdin closed). Best-effort in full: a missing `pkill`
        # binary must not swallow the pending re-raise below, which is what
        # tells the caller this attempt timed out.
        try:
            subprocess.run(
                ["pkill", "-f", f"claude attach {short_id}"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except OSError:
            pass
        raise


def _resume_claude_wake(
    *,
    name: str,
    short_id: str,
    session_id: Optional[str],
    cwd: str,
    harness: str,
    route_settings_path: Optional[str],
    message: str,
    emit_event: Any,
    wake_fn: Any,
    agents_state_fn: Any,
) -> ResumeResult:
    """Wake a blocked/stopped claude session and verify it actually moved.

    ``fno agents attach`` already owns the interactive hand-off (exec into
    the TUI, hand the terminal to the operator); this is the headless
    counterpart -- allocate a pty, restore the row's own route, inject the
    message, and confirm the live state moved to Working. Every step here
    can exit 0 having done nothing; the verification read is what makes
    that detectable instead of a lie.
    """
    route_env: Optional[dict[str, str]] = None
    if route_settings_path:
        from fno.agents.model_routing import RouteRestoreError, read_route_settings

        try:
            route_env = read_route_settings(route_settings_path)
        except RouteRestoreError as exc:
            return ResumeResult(
                exit_code=2,
                stderr=(
                    f"fno agents resume: agent {name!r} was launched on the "
                    f"route recorded at {route_settings_path}, and it cannot "
                    f"be restored ({exc}). Refusing to wake it onto the "
                    f"default account.\n"
                ),
            )

    def _state_of() -> str:
        row = agents_state_fn().get(short_id) or {}
        return str(row.get("live_status") or "unknown")

    before = _state_of()
    after = before
    last_err = ""
    # Already Working or Idle (a stale-registry race, the operator resuming
    # the wrong name, or a live session that isn't actually blocked): don't
    # inject anything into a session mid-turn. Skip straight to reporting;
    # the loop below never runs. This check is only AT loop entry, not
    # re-read immediately before each wake_fn call: if the session
    # transitions out of a skip-eligible state during _default_wake_fn's
    # ~7s pre-clear sleep, the keystrokes still land. Narrowing that window
    # needs the wake subprocess itself to poll and abort mid-sleep, which
    # would change the verified wake.sh recipe's timing; accepted as a
    # residual few-second race rather than risk that.
    #
    # Computed once here rather than re-derived at each of its three uses
    # below (loop guard, exit-16 condition, emit guard): a future edit to
    # the skip condition that touches only one of three call sites would
    # silently reintroduce the exact misreport bug the surrounding comments
    # already describe as fixed once.
    skipped = before.lower() in _WAKE_SKIP_STATUSES_LOWER
    if not skipped:
        for _attempt in range(_WAKE_ATTEMPTS):
            try:
                wake_fn(short_id, message=message, route_env=route_env, cwd=cwd)
            except subprocess.TimeoutExpired:
                last_err = "wake attempt timed out"
            except OSError as exc:
                last_err = f"wake attempt failed: {exc}"
            # Read state even after a caught exception: `script`/`claude
            # attach` only returns when the attach TUI exits, which need not
            # happen the instant the piped stdin hits EOF, so a message that
            # actually landed can still be mid-flight when the subprocess
            # timeout fires. Skipping this read on the exception path would
            # score a successful wake as failed.
            after = _state_of()
            if after.lower() == _WAKE_TARGET_STATUS.lower():
                break

    # A skipped row (before already Working/Idle) reports success on its
    # `before` state even when that state isn't the wake target: nothing was
    # attempted, so "did it reach Working" is the wrong question to ask.
    #
    # Check the outcome BEFORE emitting: the event is named "agent_resumed",
    # so emitting it unconditionally would misreport a wake that never
    # reached Working as a success, the same pre-fix shape a sigma review
    # already caught below for the exec-based harnesses' chdir failure.
    if not skipped and after.lower() != _WAKE_TARGET_STATUS.lower():
        return ResumeResult(
            exit_code=16,
            stderr=(
                f"fno agents resume: {name!r} ({short_id}) did not reach "
                f"{_WAKE_TARGET_STATUS!r} after {_WAKE_ATTEMPTS} wake "
                f"attempt(s): before={before!r} after={after!r}"
                + (f" ({last_err})" if last_err else "")
                + ".\n"
            ),
        )

    # A skipped row never entered the wake loop -- emitting "agent_resumed"
    # for it would claim a resume happened when nothing was attempted, the
    # same misreport the exit-16 check above already refuses for a failed
    # wake. The failure branch already returned above, so the only path left
    # here besides a skip is a genuine wake that reached Working.
    if not skipped:
        try:
            emit_event(
                "agent_resumed",
                name=name,
                provider=harness,
                session_id=session_id,
                cwd=cwd,
                before=before,
                after=after,
            )
        except OSError:  # best-effort telemetry; never mask the resume outcome.
            pass

    # No exec_argv/exec_cwd: this path never execs (unlike every other
    # harness's resume arm), so leaving those fields set to a
    # ["claude", "attach", short_id] this call never runs would misdescribe
    # what just happened to a future reader of the result.
    return ResumeResult(
        exit_code=0,
        output=f"{name} ({short_id}): {before} -> {after}\n",
    )


def _shell_quote(s: str) -> str:
    """POSIX shell quoting for --print-command output.

    Delegates to ``shlex.quote`` (stdlib) rather than a hand-rolled
    trigger set — the stdlib handles the long tail of POSIX-special
    characters including newline, tilde, ``#``, and ``=`` that an
    ad-hoc allowlist would miss.
    """
    return shlex.quote(s)


def resume_logic(
    *,
    name: str,
    print_command: bool = False,
    message: str = _DEFAULT_WAKE_MESSAGE,
    cwd_override: Optional[str] = None,
    registry_loader: Optional[Any] = None,
    path_checker: Optional[Any] = None,
    cwd_checker: Optional[Any] = None,
    emit_event: Optional[Any] = None,
    execvp: Optional[Any] = None,
    wake_fn: Optional[Any] = None,
    agents_state_fn: Optional[Any] = None,
) -> ResumeResult:
    """Pure-function resume pipeline; Typer command wraps this.

    Args:
        name: Registered agent name.
        print_command: When True, return the shell snippet and exit 0
            instead of resuming.
        message: Text to inject once the claude session is woken.
            Ignored for every other harness, which resume via exec instead.
        cwd_override: Use this cwd instead of the registry's recorded one.
            The Rust binary resolves a claude row's EnterWorktree-moved
            transcript dir before delegating here (`resolve_resume_cwd`);
            without this override this fallback would silently re-derive
            the stale pre-EnterWorktree cwd from the registry instead.
        registry_loader: Optional callable returning the registry list
            (defaults to ``fno.agents.registry.load_registry``).
        path_checker: Optional callable ``(bin) -> bool`` for PATH check
            (defaults to shutil.which).
        cwd_checker: Optional callable ``(cwd) -> bool`` for the
            resume-time cwd-reachability check (defaults to os.path.isdir).
        emit_event: Optional ``(kind, **data) -> None`` for the
            ``agent_resumed`` event (defaults to events.emit).
        execvp: Optional ``(file, args) -> None`` for the final exec
            call (defaults to os.execvp). Tests provide a no-op. Used
            only for non-claude harnesses; the claude path never execs.
        wake_fn: Optional callable performing one claude wake attempt
            (defaults to :func:`_default_wake_fn`). Test seam.
        agents_state_fn: Optional callable returning the short_id ->
            live-status map used to verify a claude wake (defaults to
            :func:`_default_agents_state_fn`). Test seam.

    Returns:
        :class:`ResumeResult`: for --print-command, output carries the
        shell one-liner; for a claude resume, output carries the
        before -> after state transition; for direct resume of every
        other harness, exec_argv/exec_cwd carry what os.execvp was
        (about to be) called with.
    """
    # Lazy-load registry to avoid import-time cost on cold trace runs.
    if registry_loader is None:
        from fno.agents.registry import load_registry
        registry_loader = load_registry

    try:
        entries = registry_loader()
    except Exception as exc:
        return ResumeResult(
            exit_code=13,
            stderr=f"fno agents resume: registry read failed: {exc}\n",
        )

    # Resolve by any of the three address forms (x-1b1e): name, full session id,
    # or 8-hex short. The shared core keeps Rust `find_agent_entry` in parity.
    from fno.agents.registry import (
        AgentResolutionError,
        resolve_agent_across_sources,
    )

    try:
        entry = resolve_agent_across_sources(entries, name).entry
    except AgentResolutionError as exc:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: {exc}. "
                f"Use `fno agents list` to see registered agents, "
                f"or pass a full session id to resume an orphaned session.\n"
            ),
        )

    # Identity is one axis (x-8dfc): resume keys on harness (provider fallback
    # for a not-yet-backfilled row); harness == provider on every current row.
    harness = getattr(entry, "harness", None)
    cwd = cwd_override or getattr(entry, "cwd", None)
    session_id = _session_id_for(entry)

    if not cwd:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} has no recorded cwd. "
                f"Run `fno agents rm {name}` to clean up.\n"
            ),
        )

    # A claude pane row carries no attach short_id (empty by design). The
    # default Rust runtime relaunches it with --resume plus the recorded route;
    # this Python fallback does not restore the route, so it refuses rather than
    # resume on the default (wrong) account. The id still resolved above
    # (_session_id_for fell back to harness_session_id) for the parity contract.
    if harness == "claude" and not (getattr(entry, "short_id", "") or ""):
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} is a claude pane row with "
                f"no attach short_id; its recorded route is restored by the "
                f"smart resume path. Unset FNO_AGENTS_RUNTIME=python to use it.\n"
            ),
        )

    # Check harness support BEFORE session_id so an unknown harness
    # surfaces the right error ("not supported") rather than a
    # misleading "no recorded session_id" (which is true for unknown
    # harnesses because _session_id_for returns None for them). Both
    # are exit 13 — module contract reserves 14 for "CLI not on PATH"
    # to keep wrapper diagnostics unambiguous. Codex P2 round 2.
    argv = _build_resume_argv(harness or "?", session_id or "", cwd)
    if argv is None:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: harness {harness!r} resume not supported "
                f"by this fno version.\n"
            ),
        )

    if not session_id:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} has no recorded session_id "
                f"for harness {harness!r}.\n"
            ),
        )

    # PATH check (defaults to shutil.which).
    if path_checker is None:
        def path_checker(b: str) -> bool:
            return shutil.which(b) is not None
    if not path_checker(argv[0]):
        return ResumeResult(
            exit_code=14,
            stderr=f"fno agents resume: {argv[0]} CLI not on PATH\n",
        )

    if print_command:
        # Single-line shell snippet — no banner, paste-ready.
        argv_q = " ".join(_shell_quote(a) for a in argv)
        snippet = f"cd {_shell_quote(cwd)} && exec {argv_q}\n"
        return ResumeResult(
            exit_code=0,
            output=snippet,
            exec_argv=argv,
            exec_cwd=cwd,
        )

    # Validate before actually resuming (mirrors Rust's `run_resume`, which
    # checks this before claiming, delegating, or launching). The claude
    # branch below never reaches the non-claude os.chdir check further down,
    # so without this a deleted cwd burned a full wake attempt (~19s) before
    # surfacing as a confusing "did not reach Working" instead of the
    # immediate, actionable rm hint every other harness gets.
    if cwd_checker is None:
        cwd_checker = os.path.isdir
    if not cwd_checker(cwd):
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: cwd {cwd!r} for agent {name!r} is no "
                f"longer reachable. Run `fno agents rm {name}` to clean up.\n"
            ),
        )

    if harness == "claude":
        # `fno agents attach` owns the interactive hand-off; a claude
        # resume wakes the session headlessly and verifies the state moved,
        # rather than exec'ing into an attach that (run non-interactively)
        # would print "Attaching..." and exit having done nothing.
        if emit_event is None:
            from fno.agents import events as events_mod
            emit_event = events_mod.emit
        return _resume_claude_wake(
            name=name,
            short_id=session_id or "",
            session_id=session_id,
            cwd=cwd,
            harness=harness,
            route_settings_path=getattr(entry, "route_settings_path", None),
            message=message,
            emit_event=emit_event,
            wake_fn=wake_fn if wake_fn is not None else _default_wake_fn,
            agents_state_fn=(
                agents_state_fn if agents_state_fn is not None else _default_agents_state_fn
            ),
        )

    # chdir BEFORE emit so a stale cwd surfaces as "agent_resume_failed"
    # rather than a misleading "agent_resumed" followed by a traceback.
    # (Pre-fix shape emitted success then crashed in os.chdir; sigma
    # review caught it.)
    if execvp is None:
        try:
            os.chdir(cwd)
        except OSError as exc:
            return ResumeResult(
                exit_code=13,
                stderr=(
                    f"fno agents resume: cwd {cwd!r} for agent {name!r} "
                    f"is no longer reachable: {exc}. Run "
                    f"`fno agents rm {name}` to clean up.\n"
                ),
            )

    # Emit the resume event AFTER chdir succeeds but BEFORE execvp
    # (the execvp call replaces the process; nothing in this interpreter
    # runs after it).
    if emit_event is None:
        from fno.agents import events as events_mod
        emit_event = events_mod.emit
    try:
        emit_event(
            "agent_resumed",
            name=name,
            provider=harness,
            session_id=session_id,
            cwd=cwd,
        )
    except OSError:  # best-effort: telemetry write failure (disk full,
        # EACCES) must not block an irreversible exec. Narrower than
        # bare `except Exception` so a TypeError / AttributeError from a
        # signature regression surfaces loud.
        pass

    if execvp is None:
        os.execvp(argv[0], argv)
        # Unreachable; execvp replaces the process.
        return ResumeResult(exit_code=0, exec_argv=argv, exec_cwd=cwd)
    else:
        execvp(argv[0], argv)
        return ResumeResult(exit_code=0, exec_argv=argv, exec_cwd=cwd)


def cmd_resume(
    name: str = typer.Argument(..., help="Registered agent name."),
    print_command: bool = typer.Option(
        False, "--print-command",
        help="Emit a shell-pasteable resume command and exit (no exec).",
    ),
    message: str = typer.Option(
        _DEFAULT_WAKE_MESSAGE, "--message", "-m",
        help=(
            "Text to inject once a claude session is woken. Ignored for "
            "every other harness, which resume via exec instead."
        ),
    ),
    cwd: Optional[str] = typer.Option(
        None, "--cwd",
        help=(
            "Use this cwd instead of the registry's recorded one. Internal: "
            "the Rust binary passes this when delegating a claude row whose "
            "transcript moved under EnterWorktree."
        ),
    ),
) -> None:
    """Resume an agent in its recorded cwd via the provider's resume CLI.

    A claude agent is woken headlessly: allocate a pty, restore its route,
    inject `message`, and verify the live state moved to Working (exit 16
    if it did not). Every other harness execs into the provider's own
    resume CLI in the recorded cwd, handing over the terminal.
    """
    result = resume_logic(
        name=name, print_command=print_command, message=message, cwd_override=cwd
    )
    if result.stderr:
        sys.stderr.write(result.stderr)
    if result.output:
        sys.stdout.write(result.output)
        sys.stdout.flush()
    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)
