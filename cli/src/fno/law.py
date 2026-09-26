"""One-step law recording: `fno inbox law set`.

One ruling, recorded: no staged proposal, no resume path (ruling
d-e1eec854). The caller-resolver and the measured narrative:
docs/architecture/decision-record.md.
"""

from __future__ import annotations


class LawValidationError(RuntimeError):
    """The statement is not a durable law statement."""


def validate_durable_law(
    *,
    subject: str,
    decision: str,
    rationale: str | None,
    supersedes: str | None = None,
) -> None:
    """Refuse a statement that is not durable law. Raises, or returns None.

    A coordination note recorded as law is a lie a later reader cannot detect.
    The statement rules and the node-id subject refusal live in the
    `fno inbox law match` door (mode validate); this wrapper is the fail-closed
    transport, and an unavailable validator is a refusal, never a pass.
    """
    from fno.rust_binary import call_front_json

    try:
        answer = call_front_json(
            {
                "mode": "validate",
                "subject": subject,
                "decision": decision,
                "rationale": rationale,
                "supersedes": supersedes,
            },
        )
    except Exception as exc:  # noqa: BLE001 - fail closed
        raise LawValidationError(f"law validation is unavailable ({exc})") from exc
    refusal = answer.get("refusal")
    if refusal:
        raise LawValidationError(refusal)
