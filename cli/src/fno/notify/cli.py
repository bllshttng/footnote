"""fno notify CLI - in-package OS notification helper.

Exposes:
    fno notify TITLE MESSAGE

Formerly a thin wrapper that sourced ``scripts/lib/notify.sh``; the dispatch is
now internalized in :mod:`fno.notify._impl` so the verb runs from the installed
wheel with no repo-root dependency (US2). macOS uses ``osascript``, Linux uses
``notify-send``; with neither available it degrades loudly (non-zero + one-line
message) rather than silently no-opping (AC2-FR).
"""
from __future__ import annotations

import typer

from fno.notify._impl import send_notification


notify_app = typer.Typer(
    name="notify",
    help="OS notification helper (in-package; macOS osascript / Linux notify-send)",
    invoke_without_command=True,
    add_completion=False,
)


@notify_app.callback(invoke_without_command=True)
def send(
    title: str = typer.Argument(..., help="Notification title"),
    message: str = typer.Argument(..., help="Notification message body"),
) -> None:
    """Send an OS notification.

    Dispatches via the in-package helper: macOS (osascript), Linux
    (notify-send), or a loud non-zero degrade when neither tool is available.
    """
    code, err = send_notification(title, message)
    if err:
        typer.echo(err, err=True)
    raise typer.Exit(code=code)
