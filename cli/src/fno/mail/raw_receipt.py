"""The ``mail send --raw`` unconfirmed receipt: what a not-confirmed inject
tells its sender, including the portal route an idle thread row needs (x-8f6d)."""

_UNCONFIRMED = (
    "unconfirmed (not confirmed: either the confirm budget expired on a "
    "payload that landed, or the transport refused and nothing was sent - "
    "check the recipient before assuming either; never re-queue)"
)

_UNCONFIRMED_REVIEW = (
    "unconfirmed (review request was not positively classified; do not retry blindly)"
)


def unconfirmed_review_line() -> str:
    """The receipt for a review request the lane could not positively classify."""
    return _UNCONFIRMED_REVIEW


def unconfirmed_lines(entry) -> list[str]:
    """The unconfirmed receipt lines for this row.

    A claude thread row (substrate ``thread``, no pane) rides the control.sock
    lane, whose content confirm cannot prove a landing and whose paste
    measurably fails to land at all (x-8f6d), so its receipt names the
    portal route that reaches an idle thread. Every other row keeps the
    single generic line.
    """
    lines = [_UNCONFIRMED]
    if getattr(entry, "substrate", None) == "thread":
        lines += [
            "idle thread row: this control.sock paste cannot prove landing. The measured route:",
            f"  fno mux thread {entry.name} --portal new          # opens the portal pane",
            "  fno mux pane ls                                   # read the portal pane's <session>:<pane-id>",
            "  fno mux pane send <session>:<pane-id> --text <payload> --submit --raw",
            "  fno mux pane send <session>:<pane-id> --raw --submit   # bare Enter (Continue a dialog)",
        ]
    return lines
