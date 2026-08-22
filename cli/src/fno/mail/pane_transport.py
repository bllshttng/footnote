"""Prepare text for the keystroke (mux pane) transport.

The ONE place that decides whether a pane drive carries an ``<fno_mail>``
envelope, and the ONE gate that refuses to type into a pane showing an option
prompt.

Two callers share it. The Python mail lane
(:func:`fno.agents.dispatch._mux_pane_send`) imports :func:`prepare` directly.
The Rust ``fno mux pane send`` verb shells to the ``fno mail pane-prepare``
command that wraps it, and FAILS CLOSED when that call cannot run -- there is no
bare-paste fallback, because a silent fallback rebuilds the unattributed send
this module exists to close, and does so exactly when something is already wrong.

Neither caller renders an envelope of its own: node x-1904 deleted the Rust
mirror of :mod:`fno.mail.envelope` as dead code and this module does not
reintroduce one.

Why the envelope is the DEFAULT and ``--raw`` the opt-out: an opt-in flag would
leave every existing caller unattributed and fix nothing. A genuine keystroke
case exists and stays reachable -- answering an option prompt with a digit,
sending a bare control key, typing a shell command, clearing a modal. An
envelope around the character ``1`` is nonsense.
"""
from __future__ import annotations

import subprocess
from typing import Callable, Optional

#: How many lines of the pane frame the prompt gate reads. Matches the spawn
#: seed's own read (`_submit_spawn_seed`), so both look at the same window.
GATE_FRAME_LINES = 40


class PaneSendRefused(Exception):
    """The pane send must not proceed; ``str(exc)`` names why."""


def _already_wrapped(text: str) -> bool:
    """True when ``text`` already carries an attribution container.

    Both the ``<fno_mail>`` a2a envelope and the ``<cross-session-message>``
    peer-follow-up container mark their sender, so re-wrapping either would
    nest one attribution inside another (and ``wrap_fno_mail`` refuses a body
    holding an ``<fno_mail>`` tag anyway).
    """
    return text.lstrip().startswith(("<fno_mail", "<cross-session-message"))


def resolve_pane_harness(session: str, pane_id: int) -> Optional[str]:
    """The harness hosting ``session:pane_id``, or None when no agent row claims it.

    None is not an error here, it is the discriminator: a pane with no registry
    row is a shell pane or an unregistered process, and typing an agent-to-agent
    envelope at one is meaningless. :func:`prepare` refuses it and names ``--raw``.
    """
    from fno.agents.registry import load_registry

    try:
        entries = load_registry()
    except Exception:  # noqa: BLE001 - an unreadable registry is "unknown", not fatal
        return None
    for entry in entries:
        mux = getattr(entry, "mux", None) or {}
        if str(mux.get("session")) == str(session) and str(mux.get("pane_id")) == str(pane_id):
            return getattr(entry, "harness", None) or None
    return None


def prompt_refusal(
    *,
    session: str,
    pane_id: int,
    harness: str,
    runner: Optional[Callable[..., "subprocess.CompletedProcess[str]"]] = None,
) -> Optional[str]:
    """Read the pane and return a refusal reason when it is showing an option prompt.

    A ``--submit`` against a showing prompt dismisses the payload and selects the
    highlighted default. Measured specimen: a king's option-3 ruling was typed,
    discarded, and the worker took option 1 and filed a node an operator freeze
    forbade. So an enveloped send that skipped this check would gain the ability
    to invert a ruling while every surface reads normal.

    An instrument that never ran is NOT an idle pane. An unreadable frame or an
    unavailable detector refuses as unmeasurable rather than typing blind -- an
    absence of a detected prompt says nothing when nothing looked.
    """
    from fno.agents.mux_spawn import (
        DispatchAskError,
        _evaluate_manifest_screen,
        _run_mux,
    )

    # Resolved at CALL time, not bound as a default argument. A default binds
    # the original function object at import, so a caller (or a test) that
    # replaces ``subprocess.run`` afterwards would be silently ignored and this
    # gate would shell out for real while every other call in the same lane went
    # to the replacement.
    runner = runner if runner is not None else subprocess.run

    try:
        frame = _run_mux(
            [
                "mux", "pane", "read", "--session", str(session), str(pane_id),
                "--lines", str(GATE_FRAME_LINES),
            ],
            runner,
        )
    except DispatchAskError as exc:
        return f"pane {pane_id} frame unreadable ({exc}); refusing to type blind"
    if frame.returncode != 0:
        detail = (frame.stderr or frame.stdout or "").strip() or "no detail"
        return f"pane {pane_id} read failed ({detail}); refusing to type blind"

    verdict = _evaluate_manifest_screen(harness, frame.stdout or "", runner)
    error = verdict.get("error")
    if error:
        return (
            f"prompt detector unavailable ({error}); refusing to type blind. "
            f"An instrument that never ran is not an idle pane."
        )
    answerable = verdict.get("answerable") or {}
    if answerable:
        rule = verdict.get("rule_id") or "unnamed rule"
        return (
            f"pane {pane_id} is showing an option prompt ({rule}); a submit would "
            f"dismiss this payload and select the highlighted default. Answer the "
            f"prompt with --raw, or wait for it to clear."
        )
    return None


def wrap(text: str, *, sender: Optional[str] = None, to: Optional[str] = None) -> str:
    """Wrap ``text`` in the ``<fno_mail>`` envelope, or return it unchanged when
    it already carries one.

    The sender resolves through :func:`fno.agents.self_stamp.stamp_from` with
    ``None``, which routes to the process-tree prover. Never
    ``resolve_harness_identity`` and never ``--from-self``: both stamp the shared
    ambient id.
    """
    if _already_wrapped(text):
        return text
    if not text.strip():
        raise PaneSendRefused(
            "empty payload: there is nothing to attribute. Use --raw for a bare "
            "submit keystroke."
        )
    from fno.agents.self_stamp import (
        resolve_self_model,
        resolve_self_session_id,
        stamp_from,
    )
    from fno.dispatch_flags import infer_invoking_harness
    from fno.mail.envelope import (
        ForgedEnvelopeError,
        harness_for_provider,
        wrap_fno_mail,
    )

    sender_harness = infer_invoking_harness()
    try:
        return wrap_fno_mail(
            text,
            from_=stamp_from(sender),
            # "cli" is the honest no-harness value: harness_for_provider defaults
            # a MISSING provider to claude-code, a guess this path avoids.
            harness=harness_for_provider(sender_harness) if sender_harness else "cli",
            model=resolve_self_model(),
            to=to,
            # The collision-safe reply address rides the typed envelope too: a
            # pane drive is exactly the message a recipient most needs to answer.
            from_session=resolve_self_session_id(),
        )
    except ForgedEnvelopeError as exc:
        raise PaneSendRefused(str(exc)) from exc


def prepare(
    text: str,
    *,
    session: str,
    pane_id: int,
    harness: Optional[str] = None,
    sender: Optional[str] = None,
    to: Optional[str] = None,
    gate: bool = True,
    runner: Optional[Callable[..., "subprocess.CompletedProcess[str]"]] = None,
) -> str:
    """Gate, then wrap. Returns the bytes an enveloped pane send should type.

    Raises :class:`PaneSendRefused` when the pane is showing an option prompt,
    when the pane hosts no known agent, or when the body cannot be attributed.
    """
    resolved = harness or resolve_pane_harness(session, pane_id)
    if not resolved:
        raise PaneSendRefused(
            f"pane {pane_id} hosts no registered agent, so there is no peer to "
            f"attribute this to. Use --raw to type keystrokes at it."
        )
    if gate:
        refusal = prompt_refusal(
            session=session, pane_id=pane_id, harness=resolved, runner=runner
        )
        if refusal:
            raise PaneSendRefused(refusal)
    return wrap(text, sender=sender, to=to)
