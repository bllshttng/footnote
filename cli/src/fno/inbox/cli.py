"""``fno inbox`` - what is waiting on a human.

Mints the inbox root (x-afa6, unit 2 of the command reorg x-9d6c). approvals,
notify, and outstanding fold in whole - each app is registered here exactly
as it was registered at the top level, so `fno inbox approvals ls` etc. reach
the same commands `fno approvals ls` did. The king board LEAF joins them too
(ruling d-8c62113b: a leaf places by its own meaning); the king root itself
stays put here and folds into agents in a later unit, so `board_cmd` is
registered under both `king` and `inbox` rather than moved.
"""

from __future__ import annotations

import typer

from fno.approvals.cli import approvals_app
from fno.king.cli import board_cmd
from fno.notify.cli import notify_app
from fno.outstanding.cli import outstanding_app

inbox_app = typer.Typer(
    name="inbox",
    help="What is waiting on a human: approvals, notifications, outstanding "
    "carve-outs and questions, and the king board.",
    no_args_is_help=True,
)

inbox_app.add_typer(approvals_app, name="approvals")
inbox_app.add_typer(notify_app, name="notify")
inbox_app.add_typer(outstanding_app, name="outstanding")
inbox_app.command("board")(board_cmd)
