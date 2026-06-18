"""fno.agents.resume_cli — ``fno agents resume`` subcommand.

Task 3.4 from 2026-05-22-fno-agents-observability.md.

Resolves an agent name to its provider + session id + cwd from the
registry and replaces the current process (``os.execvp``) with the
provider's resume CLI in the recorded cwd. ``--print-command`` dumps a
shell-pasteable one-liner instead, useful inside Claude Code (which
can't host an interactive TUI from inside a subprocess).

Provider resume substrates (Locked Decision #6):

- ``codex`` → ``codex resume <codex_session_id>`` (bypasses the
  exec-source picker filter via direct UUID argument).
- ``claude`` → ``claude attach <claude_short_id>`` (reuses the existing
  attach surface).
- ``gemini`` → ``gemini --session <gemini_session_id>``  (TBD at
  implementation time; if gemini lacks a native resume verb, exit 14
  with a stderr "not supported by this CLI version" message).

Exit codes:
- 0   — success (only reachable via ``--print-command``; on direct
  resume, ``os.execvp`` replaces the process and the Python interpreter
  is gone).
- 13  — name not in registry / missing cwd / missing session_id /
  unsupported provider.
- 14  — provider CLI not on ``$PATH``.
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
    """Pick the provider-specific session id from an AgentEntry.

    Reads the provider -> field mapping from the shared
    :data:`fno.agents.registry.PROVIDER_SESSION_ID_FIELDS` so this
    duck-typed resolver and ``AgentEntry.session_id`` stay in sync. Uses
    ``getattr`` (not the property) so it still works on the test fakes,
    which carry the underlying id fields but not the property.
    """
    from fno.agents.registry import PROVIDER_SESSION_ID_FIELDS

    field_name = PROVIDER_SESSION_ID_FIELDS.get(getattr(entry, "provider", None))
    return getattr(entry, field_name, None) if field_name else None


def _build_resume_argv(provider: str, session_id: str) -> Optional[list[str]]:
    """Provider-specific resume argv. Returns None for unsupported providers."""
    if provider == "codex":
        return ["codex", "resume", session_id]
    if provider == "claude":
        # Spec: reuse fno's attach surface. claude's attach is
        # `claude attach <short_id>`.
        return ["claude", "attach", session_id]
    if provider == "gemini":
        # Match providers/gemini.resume() which uses --resume <uuid>
        # (per the gemini CLI's actual resume flag — confirmed against
        # gemini.resume's own argv in this repo). Codex review caught
        # the earlier `--session` form was wrong.
        return ["gemini", "--resume", session_id]
    return None


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
    registry_loader: Optional[Any] = None,
    path_checker: Optional[Any] = None,
    emit_event: Optional[Any] = None,
    execvp: Optional[Any] = None,
) -> ResumeResult:
    """Pure-function resume pipeline; Typer command wraps this.

    Args:
        name: Registered agent name.
        print_command: When True, return the shell snippet and exit 0
            instead of os.execvp'ing.
        registry_loader: Optional callable returning the registry list
            (defaults to ``fno.agents.registry.load_registry``).
        path_checker: Optional callable ``(bin) -> bool`` for PATH check
            (defaults to shutil.which).
        emit_event: Optional ``(kind, **data) -> None`` for the
            ``agent_resumed`` event (defaults to events.emit).
        execvp: Optional ``(file, args) -> None`` for the final exec
            call (defaults to os.execvp). Tests provide a no-op.

    Returns:
        :class:`ResumeResult` — for --print-command, output carries the
        shell one-liner; for direct resume, exec_argv/exec_cwd carry
        what os.execvp was (about to be) called with.
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

    entry = next((e for e in entries if getattr(e, "name", None) == name), None)
    if entry is None:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} not found in registry. "
                f"Use `fno agents list` to see registered agents.\n"
            ),
        )

    provider = getattr(entry, "provider", None)
    cwd = getattr(entry, "cwd", None)
    session_id = _session_id_for(entry)

    if not cwd:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} has no recorded cwd. "
                f"Run `fno agents rm {name}` to clean up.\n"
            ),
        )

    # Check provider support BEFORE session_id so an unknown provider
    # surfaces the right error ("not supported") rather than a
    # misleading "no recorded session_id" (which is true for unknown
    # providers because _session_id_for returns None for them). Both
    # are exit 13 — module contract reserves 14 for "CLI not on PATH"
    # to keep wrapper diagnostics unambiguous. Codex P2 round 2.
    argv = _build_resume_argv(provider or "?", session_id or "")
    if argv is None:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: provider {provider!r} resume not supported "
                f"by this fno version.\n"
            ),
        )

    if not session_id:
        return ResumeResult(
            exit_code=13,
            stderr=(
                f"fno agents resume: agent {name!r} has no recorded session_id "
                f"for provider {provider!r}.\n"
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
            provider=provider,
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
) -> None:
    """Resume an agent in its recorded cwd via the provider's resume CLI."""
    result = resume_logic(name=name, print_command=print_command)
    if result.stderr:
        sys.stderr.write(result.stderr)
    if result.output:
        sys.stdout.write(result.output)
        sys.stdout.flush()
    if result.exit_code != 0:
        raise typer.Exit(code=result.exit_code)
