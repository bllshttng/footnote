"""CLI surface for path introspection: fno config paths (forwarder leaf).

All four verbs - emit-shell, shell-stub, verify and handoff - answer natively
(crates/fno-agents/src/paths_cli.rs); this front keeps a forwarding leaf for
handoff so `fno-py` and the test harnesses that wrap it reach the native lane
through the one Rust classify.
"""
from __future__ import annotations

import typer

app = typer.Typer(
    name="paths",
    help="Path introspection and codegen for scripts/lib/paths.sh.",
    no_args_is_help=True,
)


def _forward_native(verb: str, ctx: typer.Context) -> None:
    """Exec the Rust front with the same argv, propagating its exit code.

    The Rust front owns the classify (crates/fno/src/paths_route.rs) and the
    worker_binary resolution; this front never re-states the native verb list.
    """
    from fno.cli import _run_rust_front

    _run_rust_front(["config", "paths", verb, *ctx.args])


@app.command(
    "handoff",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    hidden=True,
)
def handoff(ctx: typer.Context) -> None:
    """Forward to the native handoff verb (crates/fno-agents paths_cli.rs)."""
    _forward_native("handoff", ctx)
