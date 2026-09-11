"""``fno agents ask``: the follow-up message lane; registered here so the file-budget gate keeps ``agents/cli.py`` shrinking."""
from __future__ import annotations

import sys
from typing import Optional

import typer

from fno.agents.cli import agents_app


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
        "only at spawn.",
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
    prompt_file: str | None = typer.Option(
        None,
        "--prompt-file",
        help="Read the prompt from a file ('-' = stdin) instead of the positional.",
    ),
) -> None:
    """Send a message to a registered agent (follow-up only).

    ``ask`` requires the agent to already exist. Unknown names exit 16
    with a hint pointing at ``fno agents spawn <name> --harness <harness>``.
    Use ``spawn`` / ``host`` for initial agent creation.

    Project mode (``ask --to-project <X> <message>``) resolves over the
    registry; because ask blocks for a reply it requires exactly one live
    peer (none/ambiguous exit nonzero).

    Prints the recipient's reply verbatim on stdout (no banner, no
    trailing newline added by fno).

    The follow-up itself runs on the Rust runtime (the ask adapters were
    ported; the parity harnesses freeze its behavior). This body keeps
    only the work the binary cannot do: the ``--to-project`` anycast
    resolution, which routes here first and then execs the binary with a
    resolved name.
    """
    from fno import rust_binary
    from fno._flag_aliases import refuse_retired_provider
    from fno.agents.dispatch import (
        AMBIGUOUS_PROJECT_EXIT_CODE,
        UNKNOWN_AGENT_EXIT_CODE,
        DispatchAskError,
        resolve_to_project,
    )
    from fno.agents.rust_runtime import refuse_without_binary, route_to_rust, runtime_mode

    refuse_retired_provider(_provider_tombstone)

    if prompt_file is not None:
        from fno.text_or_file import read_text_arg

        message = read_text_arg(message, prompt_file, what="the prompt")

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
                f"use `fno agents mail send --to-project {to_project} ...` to queue durable.",
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

    binary = rust_binary.resolve_installed_binary()
    if runtime_mode() == "python" or binary is None:
        # There is no Python ask implementation to fall back to: the legs
        # were ported and deleted in the same change that moved this caller.
        refuse_without_binary("ask")

    args = ["ask"]
    if harness:
        args += ["--harness", harness]
    args += ["--cwd", str(workdir)]
    if timeout is not None:
        args += ["--timeout", str(timeout)]
    args += ["--from-name", from_name]
    if yolo:
        args += ["--yolo"]
    # Equal-form keeps the message seed bound to its flag: a leading-dash
    # message is data, not flags (the argv-fence gate enforces this shape).
    args += [f"--message={message}"]
    # The name rides behind a `--` fence as the one positional.
    args += ["--", name]
    # os.execv never returns; the binary prints the reply verbatim itself.
    route_to_rust(args, binary=binary)


