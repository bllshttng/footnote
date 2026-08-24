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

import json
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


def _pane_entry(session: str, pane_id: int):
    """The registry row occupying ``session:pane_id``, or None."""
    from fno.agents.registry import load_registry

    try:
        entries = load_registry()
    except Exception:  # noqa: BLE001 - an unreadable registry is "unknown", not fatal
        return None
    matches = []
    for entry in entries:
        mux = getattr(entry, "mux", None) or {}
        if str(mux.get("session")) == str(session) and str(mux.get("pane_id")) == str(pane_id):
            matches.append(entry)
    return matches[0] if len(matches) == 1 else None


def resolve_pane_harness(session: str, pane_id: int) -> Optional[str]:
    """The harness hosting ``session:pane_id``, or None when no agent row claims it.

    None is not an error here, it is the discriminator: a pane with no registry
    row is a shell pane or an unregistered process, and typing an agent-to-agent
    envelope at one is meaningless. :func:`prepare` refuses it and names ``--raw``.
    """
    entry = _pane_entry(session, pane_id)
    return (getattr(entry, "harness", None) or None) if entry is not None else None


def resolve_pane_recipient(session: str, pane_id: int) -> Optional[str]:
    """The mail handle of the pane's occupant, for the envelope's ``to``.

    Resolved HERE rather than at each caller, because both of them have the same
    session and pane and neither reliably has the row. Without a ``to`` the
    envelope renders without one, and ``_addressed_here`` answers True for any
    tag lacking it, which turns the receipt check for this lane into a mention
    check. That matters most on the bare pane drive, which writes no bus row and
    has nothing else to resolve against.
    """
    from fno.harness_identity import canonical_handle

    entry = _pane_entry(session, pane_id)
    if entry is None:
        return None
    session_id = getattr(entry, "harness_session_id", None) or getattr(
        entry, "session_id", None
    )
    return canonical_handle(session_id) if session_id else None


def _identity_receipt_refusal(
    *,
    session: str,
    pane_id: int,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    check_identity: bool = True,
    expected_name: Optional[str] = None,
    expected_fno_id: Optional[str] = None,
) -> tuple[Optional[str], Optional[str]]:
    """Read the target frame with its identity receipt when a row is known."""
    from fno.agents.mux_spawn import DispatchAskError, _run_mux

    if not check_identity:
        return None, None
    if expected_name is None or expected_fno_id is None:
        entry = _pane_entry(session, pane_id)
        if entry is None:
            return None, None
        expected_name = getattr(entry, "name", None)
        expected_fno_id = (
            getattr(entry, "fno_id", None)
            or getattr(entry, "harness_session_id", None)
            or getattr(entry, "session_id", None)
        )
    # Legacy pane rows can carry the durable worker name before a canonical
    # fno_id is recorded. Keep their existing frame gate; the identity receipt
    # requires the full address and is enforced for that addressable shape.
    if not expected_name or not expected_fno_id:
        return None, None
    try:
        receipt = _run_mux(
            [
                "mux",
                "pane",
                "read",
                "--session",
                str(session),
                str(pane_id),
                "--lines",
                str(GATE_FRAME_LINES),
                "--json",
            ],
            runner,
        )
    except (DispatchAskError, OSError) as exc:
        return f"pane {pane_id} identity receipt unreadable ({exc})", None
    if receipt.returncode != 0:
        detail = (receipt.stderr or receipt.stdout or "").strip() or "no detail"
        return f"pane {pane_id} identity receipt failed ({detail})", None
    try:
        payload = json.loads(receipt.stdout or "")
    except (TypeError, ValueError) as exc:
        return f"pane {pane_id} identity receipt is invalid JSON ({exc})", None
    if not isinstance(payload, dict) or payload.get("pane_id") != pane_id:
        return f"pane {pane_id} identity receipt names the wrong pane", None
    actual_name = payload.get("pane_name")
    actual_fno_id = payload.get("registry_fno_id")
    if actual_name != expected_name or actual_fno_id != expected_fno_id:
        return (
            f"pane {pane_id} identity mismatch: addressed {expected_name} "
            f"({expected_fno_id}) pane hosts {actual_name or '<unknown>'} "
            f"({actual_fno_id or '<unknown>'})",
            None,
        )
    frame_text = payload.get("text")
    if not isinstance(frame_text, str):
        return f"pane {pane_id} identity receipt has invalid text", None
    return None, frame_text


def prompt_refusal(
    *,
    session: str,
    pane_id: int,
    harness: str,
    runner: Optional[Callable[..., "subprocess.CompletedProcess[str]"]] = None,
    check_identity: bool = True,
    expected_name: Optional[str] = None,
    expected_fno_id: Optional[str] = None,
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

    identity_refusal, receipt_text = _identity_receipt_refusal(
        session=session,
        pane_id=pane_id,
        runner=runner,
        check_identity=check_identity,
        expected_name=expected_name,
        expected_fno_id=expected_fno_id,
    )
    if identity_refusal:
        return identity_refusal
    if receipt_text is None:
        try:
            frame = _run_mux(
                [
                    "mux", "pane", "read", "--session", str(session), str(pane_id),
                    "--lines", str(GATE_FRAME_LINES),
                ],
                runner,
            )
        except (DispatchAskError, OSError) as exc:
            # OSError too: `_run_mux` only translates FileNotFoundError and
            # TimeoutExpired, so a PermissionError on a bad FNO_BIN (or ENOEXEC,
            # or EMFILE) escapes it raw. The one caller catches PaneSendRefused
            # alone, so an untranslated OSError killed the send with a traceback
            # where an unreadable frame demotes to the durable bus.
            return f"pane {pane_id} frame unreadable ({exc}); refusing to type blind"
        if frame.returncode != 0:
            detail = (frame.stderr or frame.stdout or "").strip() or "no detail"
            return f"pane {pane_id} read failed ({detail}); refusing to type blind"
        receipt_text = frame.stdout or ""

    # allow_dev_binary: this caller only ever REFUSES on the verdict, so reading
    # a cargo dev build here can cost nothing worse than an accurate refusal.
    # Spawn readiness ACTS on its verdict and deliberately does not pass this.
    verdict = _evaluate_manifest_screen(
        harness, receipt_text, runner, allow_dev_binary=True
    )
    error = verdict.get("error")
    if error:
        return (
            f"prompt detector unavailable ({error}); refusing to type blind. "
            f"An instrument that never ran is not an idle pane. The message "
            f"still reaches the durable bus; --raw types it at the pane "
            f"unattributed. A source checkout hits this because the detector "
            f"resolves an INSTALLED fno-agents and skips the cargo dev target."
        )
    # BLOCKED is the contract, and `answerable` is only the subset of it that
    # carries a parsed answer grammar. `evaluate_answerable` returns nothing for
    # a matched blocked rule with no grammar, or one whose options failed to
    # parse -- a codex auth wall, a trust prompt whose menu did not parse. Those
    # are the panes most in need of the refusal, and gating on `answerable`
    # alone let every one of them through to a paste and a CR. Spawn readiness
    # already tests `state == "blocked"`, and the doc states that same contract.
    answerable = verdict.get("answerable") or {}
    blocked = verdict.get("state") == "blocked"
    if answerable or blocked:
        rule = verdict.get("rule_id") or "unnamed rule"
        what = "an option prompt" if answerable else "a blocking prompt"
        return (
            f"pane {pane_id} is showing {what} ({rule}); a submit would "
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
    from fno.inbox.store import generate_msg_id
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
            # And an id to quote. Without one the recipient has a sender but no
            # `reply --to`, so three of the four defects the envelope closes came
            # back and the fourth did not. A bare pane drive writes no bus row,
            # but it does not need to: `resolve_live_sender` recovers a sender
            # off the transcript for an id the bus never saw, and it reads
            # `from_session` first, so the recovered address is the collision-safe
            # one. The mail lane never reaches here -- it mints its own id and
            # hands this function an already-wrapped body.
            id=generate_msg_id(),
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
    wrap_body: bool = True,
    runner: Optional[Callable[..., "subprocess.CompletedProcess[str]"]] = None,
    check_identity: bool = True,
    expected_name: Optional[str] = None,
    expected_fno_id: Optional[str] = None,
) -> str:
    """Gate, then wrap. Returns the bytes an enveloped pane send should type.

    The two are SEPARATE decisions and a caller can want either without the
    other. An operational payload (a ritual command, a busy-hold digest) is not
    mail and must land verbatim, so it takes ``wrap_body=False`` -- but a submit
    into a showing prompt discards it exactly as it discards mail, so it still
    wants the gate. Only a keystroke ANSWERING a prompt turns the gate off.

    Raises :class:`PaneSendRefused` when the pane is showing an option prompt,
    when the pane hosts no known agent, or when the body cannot be attributed.
    """
    resolved = harness or resolve_pane_harness(session, pane_id)
    if to is None:
        to = resolve_pane_recipient(session, pane_id)
    if not resolved:
        raise PaneSendRefused(
            f"pane {pane_id} hosts no registered agent, so there is no peer to "
            f"attribute this to. Use --raw to type keystrokes at it."
        )
    if gate:
        refusal = prompt_refusal(
            session=session,
            pane_id=pane_id,
            harness=resolved,
            runner=runner,
            check_identity=check_identity,
            expected_name=expected_name,
            expected_fno_id=expected_fno_id,
        )
        if refusal:
            raise PaneSendRefused(refusal)
    if not wrap_body:
        return text
    return wrap(text, sender=sender, to=to)
