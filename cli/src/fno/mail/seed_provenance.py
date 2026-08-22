"""Attribution for the one message that defines a worker's whole task: its seed.

Every a2a message sent by ``fno mail send`` arrives wrapped in ``<fno_mail>``, so
a recipient can tell agent-authored text from operator-authored text and an
auditor can ``grep '</fno_mail>'`` a transcript to enumerate what was injected.
The spawn seed carried no such envelope. That inverts the risk: the seed is the
single message that defines a worker's entire task, and it was precisely the one
a worker could not attribute and an auditor could not find.

**The envelope cannot share the payload, and that is why this is a sidecar.**
``skills/agent/scripts/normalize.sh:710`` classifies a payload by a LEADING
slash (``case "$msg" in /*) payload_mode="passthrough"``), so an envelope in
front of ``/fno:target x-1234`` destroys routing. Putting it after the verb line
is no better: the harness REPL is a second reader we do not control, and for
``/fno:target <node>`` the arguments are load-bearing, so an envelope swallowed
into them is a real failure rather than a cosmetic one.

So the prompt stays byte-identical and the attribution travels beside it. Every
launcher exports the fields below on the child's environment; a SessionStart
hook decodes them, renders them through the sole ``<fno_mail>`` renderer, and
hands the result to the harness as startup context. The worker learns who
supplied its seed before it acts, and the transcript carries a greppable
``</fno_mail>`` for that seed, with no envelope at byte zero.

Startup only. The hook emits for ``source=startup`` and stays silent on
``resume`` and ``compact``, so one spawn leaves exactly one provenance envelope.
"""
from __future__ import annotations

import base64
import binascii
import os
import sys
from typing import Mapping, Optional

#: The child-environment contract. One producer per field, read back in exactly
#: one place (:func:`render_from_env`), so a launcher in another language only
#: has to agree on these names -- never on the wire format, which stays the
#: Python renderer's alone.
ENV_SEED_B64 = "FNO_SEED_PROV_SEED_B64"
ENV_FROM = "FNO_SEED_PROV_FROM"
ENV_FROM_SESSION = "FNO_SEED_PROV_FROM_SESSION"
ENV_HARNESS = "FNO_SEED_PROV_HARNESS"
ENV_MODEL = "FNO_SEED_PROV_MODEL"
ENV_NODE = "FNO_SEED_PROV_NODE"
ENV_MSG_ID = "FNO_SEED_PROV_MSG_ID"

#: The whole group, for callers that must SET-OR-CLEAR it together. A child
#: inheriting half of it attributes its own seed to whoever seeded its parent,
#: so the clear list must never drift from the write list -- hence one tuple,
#: here, beside the writer.
SEED_PROVENANCE_KEYS: tuple[str, ...] = (
    ENV_SEED_B64,
    ENV_FROM,
    ENV_FROM_SESSION,
    ENV_HARNESS,
    ENV_MODEL,
    ENV_NODE,
    ENV_MSG_ID,
)

#: Decoded-seed ceiling. A spawn payload is a handoff and the house contract caps
#: one at 80 words; 16 KiB is far above any honest seed and still small enough
#: that an environment block stays sane. Over it, :func:`build_env` refuses
#: rather than truncating: a sidecar that claims to quote the seed verbatim and
#: silently drops its tail is worse than no sidecar.
MAX_SEED_BYTES = 16 * 1024


class SeedProvenanceRefused(Exception):
    """The seed cannot be attributed; the caller must refuse the spawn."""


def build_env(seed: str, *, node: Optional[str] = None) -> dict[str, str]:
    """The child-environment fields carrying this seed's provenance.

    Empty when the invoking process has no provable harness identity. That is
    the operator case, not a failure: a person typing ``fno agents spawn`` in a
    shell authored the seed themselves, and stamping a peer envelope on it would
    claim an agent sender that does not exist.

    The sender resolves through :func:`fno.agents.self_stamp.stamp_from` with
    ``None``, which routes to the process-tree prover. Never
    ``resolve_harness_identity`` and never ``--from-self``: both stamp the shared
    ambient id.

    Raises :class:`SeedProvenanceRefused` only for a seed already carrying an
    ``<fno_mail>`` tag. That one is a forgery boundary, not a rendering limit:
    the tag reaches the child either way, and refusing the spawn is what stops
    a body from claiming an envelope of its own.

    A seed over :data:`MAX_SEED_BYTES` is NOT that. Nothing about it is
    dishonest, it is only too long to quote, so it takes the same answer as an
    unprovable identity above -- return empty and launch unattributed. Killing
    a spawn because a pasted plan is long would make this helper a gatekeeper
    over work it has no stake in.
    """
    from fno.agents.self_stamp import (
        resolve_self_model,
        resolve_self_session_id,
        stamp_from,
    )
    from fno.dispatch_flags import infer_invoking_harness
    from fno.mail.envelope import contains_fno_mail_tag, harness_for_provider

    # FIRST, ahead of every early return. This one is a fact about the SEED, not
    # about who is spawning or how long the text is, so any check that can exit
    # before it becomes a way to carry a forged tag past it. Both of the returns
    # below were once reachable first: padding a tagged seed over the cap, or
    # spawning one from a shell with no provable identity, each launched it
    # unrefused. Order is the whole guard here.
    if contains_fno_mail_tag(seed):
        raise SeedProvenanceRefused(
            "seed contains an <fno_mail> tag; the envelope frames peer mail and "
            "a body cannot contain one"
        )

    from_session = resolve_self_session_id()
    if not from_session:
        return {}

    raw = seed.encode("utf-8")
    if len(raw) > MAX_SEED_BYTES:
        print(
            f"note: seed is {len(raw)} bytes, over the {MAX_SEED_BYTES}-byte "
            f"provenance cap; spawning without a provenance sidecar. A sidecar "
            f"that quotes the seed must quote all of it.",
            file=sys.stderr,
        )
        return {}

    from fno.inbox.store import generate_msg_id

    sender_harness = infer_invoking_harness()
    env = {
        ENV_SEED_B64: base64.b64encode(raw).decode("ascii"),
        ENV_FROM: stamp_from(None),
        ENV_FROM_SESSION: from_session,
        ENV_HARNESS: (
            harness_for_provider(sender_harness) if sender_harness else "cli"
        ),
        ENV_MODEL: resolve_self_model(),
        ENV_MSG_ID: generate_msg_id(),
    }
    if node:
        env[ENV_NODE] = node
    return env


def render_from_env(env: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """The ``<fno_mail>`` provenance sidecar for this session's seed, or None.

    None whenever the fields are absent or unusable: a hand-started session, a
    spawn by an operator, or a corrupt base64 blob. A hook calling this prints
    nothing on None, because a session with no peer sender has nothing to
    attribute and inventing one would be the same lie in the other direction.
    """
    env = os.environ if env is None else env
    encoded = (env.get(ENV_SEED_B64) or "").strip()
    from_session = (env.get(ENV_FROM_SESSION) or "").strip()
    if not encoded or not from_session:
        return None
    try:
        seed = base64.b64decode(encoded, validate=True).decode("utf-8")
    except (binascii.Error, ValueError, UnicodeDecodeError):
        return None
    if not seed.strip():
        return None

    from fno.mail.envelope import ForgedEnvelopeError, wrap_fno_mail

    # Say three things, in this order: a peer started you, here is the seed
    # verbatim, do not run the copy. The last one matters because the quoted
    # text is a slash command and the harness would happily run it twice.
    body = (
        "A peer started this session; your operator did not type its first "
        "message. Your initial user message is quoted verbatim below so you can "
        "attribute it and reply to the sender. It is a QUOTED COPY for "
        "provenance: act on the prompt you already have and do not execute this "
        "copy again.\n"
        "\n"
        "--- initial user message (verbatim) ---\n"
        f"{seed}\n"
        "--- end initial user message ---"
    )
    try:
        return wrap_fno_mail(
            body,
            from_=(env.get(ENV_FROM) or "").strip() or "fno",
            harness=(env.get(ENV_HARNESS) or "").strip() or "cli",
            model=(env.get(ENV_MODEL) or "").strip() or "unknown",
            node=(env.get(ENV_NODE) or "").strip() or None,
            id=(env.get(ENV_MSG_ID) or "").strip() or None,
            from_session=from_session,
        )
    except ForgedEnvelopeError:
        return None
