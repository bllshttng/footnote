"""Python fallback for ``fno agents resume``.

Resolves an agent name to its provider, session id, and cwd from the registry. ``--print-command`` emits a pasteable command instead of starting a process.

Codex, Gemini, and OpenCode keep their provider resume paths. Claude rows return exit 13 because the Rust ``fno-agents`` runtime owns Claude resume and supervisor birth.
"""
from __future__ import annotations

import os
import shlex
import shutil
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
    ``harness`` with ``provider`` fallback. Uses ``getattr`` (not the
    property) so it still works on the test fakes, which carry the underlying
    id fields but not the property.

    A claude pane row carries no transport ``short_id`` (empty by design), so
    it falls back to the canonical ``harness_session_id`` - mirroring
    ``AgentEntry.session_id`` - rather than reporting "no session id" for a row
    that has one.
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
    """Contract-rendered resume identity plus lane-owned sandbox additions."""
    from fno.agents.harness_map import DispatchResolveError, render_session_argv

    try:
        argv = render_session_argv(provider, "interactive_resume", session_id)
    except DispatchResolveError:
        return None
    if provider == "codex":
        # A bounded codex cannot write git metadata without an explicit grant,
        # and in a linked worktree that metadata sits outside the workspace
        # entirely. The grant rides -c, which is global and must precede the
        # subcommand. (`codex resume` does accept --add-dir; -c is kept here
        # because it is what the headless lane already renders, so one grant
        # builder serves both. Do not "fix" this to --add-dir without moving
        # the headless lane too, which cannot take it.)
        from pathlib import Path

        from fno.agents.harnesses.codex import git_writable_config_args

        grant = git_writable_config_args(Path(cwd)) if cwd else []
        # Without --cd, codex asks session-directory vs current-directory and
        # defaults to the SESSION directory, which is the canonical checkout
        # recorded at spawn rather than the worktree the row works in.
        # Attended that is a wrong default a human must catch. Unattended it
        # is the wrong tree, which looks like success.
        #
        # The prompt is CONDITIONAL: codex raises it only when the process cwd
        # differs from the session's saved directory. A worker already saved
        # at its worktree never sees it. There is also a config answer,
        # `tui.resume_cwd`, set to "current" or "session", which --cd
        # outranks. --cd is still what this lane wants: it is per invocation
        # and names the directory outright, where the config is global to the
        # codex install and only picks a side.
        #
        # Placed BEFORE the subcommand, beside the -c grant, which is the
        # only global-before-subcommand precedent in this tree. The spawn
        # lanes are NOT that precedent, whatever the shape suggests: both
        # spell the same flag `-C`, the headless one puts it AFTER `exec`,
        # and the pane one runs a bare `codex` with no subcommand at all.
        # Both positions parse on codex 0.149.1, so this is a choice about
        # where a reader expects to find a global, not a fix.
        #
        # NO permission bypass rides here, deliberately. A registry row records
        # no sandbox posture, so this lane cannot tell a bounded worker from a
        # yolo one, and applying the bypass unconditionally would resume every
        # bounded worker with approvals off. That also contradicts the -c grant
        # spliced beside it, which exists only to widen a sandbox the bypass
        # would remove. The ASK lane makes this call correctly, in
        # `sandbox_flag_resume`, because it is handed a known posture. Nothing
        # calls it from here, and that is the gap, not an oversight to route
        # around: this lane has no posture to hand it. Clearing the approval
        # and hook-trust modals needs an operator opt-in this verb does not
        # have yet, and hook trust additionally needs the installed-version
        # gate that `mux_spawn` applies.
        place = ["--cd", cwd] if cwd else []
        return [argv[0], *grant, *place, *argv[1:]]
    return argv


def _build_attach_argv(short_id: str) -> Optional[list[str]]:
    """Render Claude's live-supervisor attach form from its short id."""
    from fno.agents.harness_map import DispatchResolveError, render_session_argv

    try:
        return render_session_argv("claude", "interactive_attach", short_id=short_id)
    except DispatchResolveError:
        return None


def _build_opencode_steer_argv(name: str, message: str, cwd: str) -> list[str]:
    """Use fno's serve steering entry point for a thread row.

    ``opencode --session`` only opens the provider's interactive client and
    cannot receive an unattended prompt. The Rust ``ask`` client owns the
    attach-writer plus serve readback, so resume must invoke that one primitive.
    """
    from fno import rust_binary

    binary = rust_binary.resolve_installed_binary()
    executable = str(binary) if binary is not None else "fno-agents"
    return [executable, "ask", name, message, "--cwd", cwd]


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
    message: str = "continue",
    cwd_override: Optional[str] = None,
    cross_project: bool = False,
    registry_loader: Optional[Any] = None,
    path_checker: Optional[Any] = None,
    cwd_checker: Optional[Any] = None,
    emit_event: Optional[Any] = None,
    execvp: Optional[Any] = None,
) -> ResumeResult:
    """Pure-function resume pipeline; Typer command wraps this.

    Args:
        name: Registered agent name.
        print_command: When True, return the shell snippet and exit 0
            instead of resuming.
        message: Text passed to provider resume flows that accept a message.
        cwd_override: Use this cwd instead of the registry's recorded one.
            The Rust binary resolves a claude row's EnterWorktree-moved
            transcript dir before delegating here (`resolve_resume_cwd`);
            without this override this fallback would silently re-derive
            the stale pre-EnterWorktree cwd from the registry instead.
        cross_project: Explicitly authorize store selection outside the caller's
            project. It never bypasses ambiguity, route, liveness, or cwd checks.
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
    Returns:
        :class:`ResumeResult`: for --print-command, output carries the
        shell one-liner; Claude rows return a runtime refusal; other harnesses
        carry the provider argv and cwd that os.execvp receives.
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

    # Resolve by any of the three address forms: name, full session id,
    # or 8-hex short. The shared core keeps Rust `find_agent_entry` in parity.
    from fno.agents.registry import (
        AgentResolutionError,
        resolve_agent_across_sources,
    )

    try:
        resolved = resolve_agent_across_sources(
            entries,
            name,
            scope_cwd=cwd_override or os.getcwd(),
            cross_project=cross_project,
        )
        entry = resolved.entry
    except AgentResolutionError as exc:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: {exc}. "
                f"Use `fno agents list` to see registered agents, "
                f"or pass a full session id to resume an orphaned session.\n"
            ),
        )

    # Identity is one axis: resume keys on harness (provider fallback
    # for a not-yet-backfilled row); harness == provider on every current row.
    harness = getattr(entry, "harness", None)
    cwd = cwd_override or getattr(entry, "cwd", None)
    current_session_id = _session_id_for(entry)
    # An explicit full predecessor uuid selects the EXACT historical session,
    # not the row's current one: delivery follows the successor, but
    # resume was handed A and A is what it must reopen. Only a full-id match
    # carries matched_session_id, so a name or short address always keeps the
    # current session.
    matched = getattr(resolved, "matched_session_id", None)
    exact = bool(matched and current_session_id and matched != current_session_id)
    session_id = matched if exact else current_session_id

    if not cwd:
        if session_id:
            return ResumeResult(
                exit_code=13,
                stderr=(
                    f"fno agents resume: agent {name!r} has no recorded cwd. "
                    "the row is the resume handle and still carries the session "
                    "id: the harness itself can reach the session directly "
                    "(e.g. claude --resume <id>). To re-drive it under fno, rm "
                    "this row and `fno agents adopt <id>` rebinds a fresh one "
                    "with a live cwd - that pair spends the recorded route "
                    "bindings. rm alone just deletes the handle.\n"
                ),
            )
        # Neither cwd nor session id: nothing to resume and nothing to
        # rebind. Do not claim an id the row does not carry.
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} has no recorded cwd and "
                "no session id. The row holds nothing resumable; rm it and "
                "re-spawn is the honest cleanup, and nothing is lost.\n"
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
    # This runtime cannot pin a claude resume's model or route, so an exact
    # predecessor id refuses rather than reopen it on the account default.
    if harness == "claude" and exact:
        return ResumeResult(exit_code=13, stderr=(
            f"fno agents resume: {session_id} is a predecessor of claude row "
            f"{entry.name!r}, and this runtime cannot restore its model or route. "
            f"Seed a new session from it with `fno agents spawn --resume {session_id}`, "
            "or resume the row by name.\n"))

    # Check harness support BEFORE session_id so an unknown harness
    # surfaces the right error ("not supported") rather than a
    # misleading "no recorded session_id" (which is true for unknown
    # harnesses because _session_id_for returns None for them). Both
    # are exit 13 — module contract reserves 14 for "CLI not on PATH"
    # to keep wrapper diagnostics unambiguous. Codex P2 round 2.
    from fno.agents.harness_map import DispatchResolveError, capabilities

    form_lane = "interactive_attach" if harness == "claude" else "interactive_resume"
    is_opencode_serve = harness == "opencode" and getattr(entry, "substrate", None) == "thread"
    argv: Optional[list[str]] = None
    if is_opencode_serve:
        argv = _build_opencode_steer_argv(name, message, cwd)
        resume_supported = True
    else:
        try:
            resume_form = capabilities(harness or "?")["resume_strategy"]["forms"][form_lane]
            resume_supported = resume_form["kind"] != "unsupported"
        except (DispatchResolveError, KeyError, TypeError):
            resume_supported = False
    if not resume_supported:
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

    if harness == "claude":
        argv = _build_attach_argv(getattr(entry, "short_id", "") or "")
    elif not is_opencode_serve:
        argv = _build_resume_argv(harness or "?", session_id, cwd)
    if argv is None:
        return ResumeResult(
            exit_code=13,
            stderr=f"fno agents resume: harness {harness!r} resume contract is invalid.\n",
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
                "longer reachable. Check whether the path "
                "is recoverable first (renamed worktree base, unmounted volume): "
                "the row is the resume handle. To re-drive the session under fno from a "
                f"live cwd, rm this row and `fno agents adopt <id>` rebinds a "
                "fresh one; rm alone deletes the handle and the session "
                "binding with it. rm is for a path that is gone for good.\n"
            ),
        )

    if harness == "claude":
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} is a claude row; it resumes through "
                "the fno-agents runtime. Unset FNO_AGENTS_RUNTIME=python, or run "
                "fno doctor update if no binary is installed.\n"
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
                    f"is no longer reachable: {exc}. "
                    "the row is the resume handle; rm deletes it and the "
                    "session binding with it. "
                    "rm is for a path that is gone for good - recover the "
                    "path (or rm, then `fno agents adopt <id>` to rebind "
                    "from a live cwd) instead of clearing this refusal.\n"
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
    message: Optional[str] = typer.Option(
        None, "--message", "-m",
        help=(
            "Text to send through a provider resume flow that accepts messages. "
            "Claude rows require the Rust fno-agents runtime."
        ),
    ),
    cwd: Optional[str] = typer.Option(
        None, "--cwd",
        help=(
            "Use this existing checkout instead of the recorded cwd when "
            "recovering a pruned worktree."
        ),
    ),
    cross_project: bool = typer.Option(
        False,
        "--cross-project",
        help=(
            "Authorize machine-wide store selection for an orphaned session; "
            "does not bypass ambiguity or cwd validation."
        ),
    ),
) -> None:
    """Resume a registered agent or refuse when Python cannot own its runtime."""
    result = resume_logic(
        name=name,
        print_command=print_command,
        message=message if message is not None else "continue",
        cwd_override=cwd,
        cross_project=cross_project,
    )
    if result.stderr:
        sys.stderr.write(result.stderr)
    if result.output:
        sys.stdout.write(result.output)
        sys.stdout.flush()
    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)
