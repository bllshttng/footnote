"""Auto-stamp the invoking session's own identity + model into a2a envelopes (x-605c).

The a2a reply protocol is: an agent reads ``<fno_mail from=H ...>`` and runs
``fno agents mail send H``. For that return leg to resolve, the OUTBOUND envelope must
carry a truthful ``from`` (the sender's canonical handle) and ``model`` (its real
model), not the historical ``from="fno" model="unknown"`` placeholders. Both are
resolved from the invoking process's ambient harness identity and its own
transcript store. Every read is lenient: an unresolvable model floors to
``"unknown"`` and the send always proceeds.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Callable, Mapping, Optional

from fno.claims.self_identity import resolve_self_identity
from fno.harness_identity import canonical_handle

_TAIL_BYTES = 256 * 1024
_EXPANDED_TAIL_BYTES = 2 * 1024 * 1024


class IdentityAmbiguousError(RuntimeError):
    """The ambient markers disagree and no process-tree proof is available."""


def identity_ambiguity_message(identity) -> str:
    """Render the single refusal sentence for an unproven mixed environment.

    The strip lines are the self-rescue: the session cannot prove which
    harness it is (that is the ambiguity), but the operator knows, and
    stripping the foreign family's markers - every one the scrub knows about,
    not just the two the resolver consults - restores self-resolution. Built
    from :func:`fno.harness_identity.ambient_identity_strip_flags`, which reads
    the same list the scrub reads, so the text cannot drift from behavior
    (x-b57a: a poisoned claude session stripped two codex names and nothing
    changed; all seven restored it).
    """
    from fno.harness_identity import ambient_identity_strip_flags

    markers = ", ".join(marker for marker, _harness, _value in identity.markers_present)
    lookup_id = next(
        (value for _marker, _harness, value in identity.markers_present), "<session-id>"
    )
    families = []
    for _marker, harness, _value in identity.markers_present:
        if harness not in families:
            families.append(harness)
    strip_lines = ""
    for family in families:
        flags = ambient_identity_strip_flags(family)
        if flags:
            strip_lines += (
                f"  if this is a {family} session: env {' '.join(flags)}"
                " <command>\n"
            )
    strip_section = (
        # A single-family ambiguous disposition (a proven harness with two
        # distinct ids, or a contradicted only marker) has no foreign family
        # to strip, so the section is omitted entirely rather than printing
        # the header with no command under it.
        f"or strip the foreign family's markers and retry:\n{strip_lines}"
        "keeping your own harness's markers; each harness re-mints "
        "its own for a child\n"
        if strip_lines
        else ""
    )
    return (
        "cannot decide which session is 'self': multiple harness markers present "
        "(inherited env?)\n"
        f"markers: {markers}\n"
        "resolve with: find ~/.codex/sessions ~/.claude/projects -name "
        f"'*{lookup_id}*'\n" + strip_section
    ).rstrip("\n")


def require_self_identity(env: Optional[Mapping[str, str]] = None):
    """Return owned identity or refuse to let a mixed environment guess."""
    identity = resolve_self_identity() if env is None else resolve_self_identity(env)
    if identity.disposition == "ambiguous":
        raise IdentityAmbiguousError(identity_ambiguity_message(identity))
    return identity


def stamp_from(from_name: Optional[str]) -> str:
    """Resolve the outbound envelope ``from``.

    An explicit ``--from-name`` (any value, including the literal ``"fno"``) wins
    verbatim. Unset (``None``) auto-stamps the invoking session's canonical
    handle; with no ambient harness identity (cron, CI, bare shell) it floors to
    ``"fno"``.
    """
    if from_name is not None:
        return from_name
    # Owned resolution with the process-tree prover: a session that inherited a
    # foreign marker is contradicted and floors to "fno" rather than address the
    # return leg to a stranger. A single marker (the dominant case, and any
    # worker after the spawn-time scrub) resolves to its own handle.
    return resolve_self_handle() or "fno"


def resolve_self_handle(
    env: Optional[Mapping[str, str]] = None,
) -> Optional[str]:
    """Return this process's proven harness handle, or omit it when ambiguous."""
    ident = resolve_self_identity(env)
    if ident.session_id and ident.harness:
        return canonical_handle(ident.session_id)
    return None


def resolve_self_model(env: Optional[Mapping[str, str]] = None) -> str:
    """The invoking harness's own model string, or ``"unknown"``.

    claude greps the tail of its own transcript jsonl; codex greps its rollout's
    ``turn_context``. Any miss (no ambient identity, unreadable store, no match)
    floors to ``"unknown"`` so a send is never blocked on model resolution.
    """
    ident = resolve_self_identity(env)
    if not ident.session_id or not ident.harness:
        return "unknown"
    try:
        if ident.harness == "claude":
            return _claude_model(ident.session_id) or "unknown"
        if ident.harness == "codex":
            return _codex_model(ident.session_id) or "unknown"
    except OSError:
        return "unknown"
    return "unknown"


def _complete_lines(
    path: Path,
    max_bytes: Optional[int],
    *,
    drop_unterminated_tail: bool = True,
) -> Optional[list[bytes]]:
    try:
        with open(path, "rb") as fh:
            fh.seek(0, 2)
            end = fh.tell()
            start = max(0, end - max_bytes) if max_bytes is not None else 0
            read_start = start - 1 if start else 0
            fh.seek(read_start)
            expected = end - read_start
            data = fh.read(expected)
    except OSError:
        return None
    if len(data) != expected:
        return None

    if start:
        starts_at_boundary = data[:1] == b"\n"
        data = data[1:]
        if not starts_at_boundary:
            boundary = data.find(b"\n")
            data = data[boundary + 1 :] if boundary >= 0 else b""

    # A trailing record without a terminal \n is dropped by default: model
    # resolution treats it as an un-committed write (codex rollout's "wait for
    # newline" contract). The context probe passes drop_unterminated_tail=False -
    # dropping a complete-but-unterminated assistant record would make it report
    # the second-to-last turn's usage (silently stale; in arm-handoff that
    # suppresses a needed handoff). There, each record is json.loads-validated,
    # so a genuinely partial trailing line is skipped by the parser, not here.
    if drop_unterminated_tail and data and not data.endswith(b"\n"):
        boundary = data.rfind(b"\n")
        data = data[: boundary + 1] if boundary >= 0 else b""
    return data.splitlines()


def _last_model(path: Path, extract_model: Callable[[object], Optional[str]]) -> Optional[str]:
    try:
        file_size = path.stat().st_size
    except OSError:
        return None
    for max_bytes in (_TAIL_BYTES, _EXPANDED_TAIL_BYTES, None):
        lines = _complete_lines(path, max_bytes)
        if lines is None:
            return None
        for line in reversed(lines):
            try:
                record = json.loads(line)
            except (UnicodeDecodeError, ValueError):
                continue
            model = extract_model(record)
            if model:
                return model
        # This window already spanned the whole file; a larger one re-reads
        # identical bytes for an identical result, so stop escalating.
        if max_bytes is None or file_size <= max_bytes:
            break
    return None


def _claude_record_model(record: object) -> Optional[str]:
    if not isinstance(record, dict) or record.get("type") != "assistant":
        return None
    if record.get("isSidechain") is True:
        return None
    message = record.get("message")
    if not isinstance(message, dict):
        return None
    model = message.get("model")
    return model if isinstance(model, str) and model else None


def _codex_record_model(record: object) -> Optional[str]:
    if not isinstance(record, dict) or record.get("type") != "turn_context":
        return None
    payload = record.get("payload")
    if not isinstance(payload, dict):
        return None
    model = payload.get("model")
    return model if isinstance(model, str) and model else None


def resolve_own_transcript(session_id: str, harness: str) -> Optional[Path]:
    """This session's own transcript jsonl, harness-aware, or None.

    The single locator shared by the model resolver (`resolve_self_model`) and
    the context probe (`fno.context_probe`), so the FNO_CLAUDE_PROJECTS_DIR /
    FNO_CODEX_SESSIONS_DIR seams live in one place rather than being
    reimplemented per caller. A session owns one transcript, so the first match
    wins. None - when the id/harness is blank or no transcript is found - is a
    floor every caller treats as "unknown" / "unreadable", never a fault.
    """
    if not session_id or not harness:
        return None
    if harness == "claude":
        from fno.agents.discover import default_projects_dir

        # Named <session_id>.jsonl under a cwd-encoded dir; glob by id so this
        # is cwd-encoding-agnostic. FNO_CLAUDE_PROJECTS_DIR seams the dir.
        for path in default_projects_dir().glob(f"*/{session_id}.jsonl"):
            return path
        return None
    if harness == "codex":
        from fno.agents.discover import default_codex_sessions_dir

        # The rollout filename embeds the session id; FNO_CODEX_SESSIONS_DIR seams it.
        for path in default_codex_sessions_dir().rglob(f"*{session_id}*.jsonl"):
            return path
        return None
    return None


def _claude_model(session_id: str) -> Optional[str]:
    path = resolve_own_transcript(session_id, "claude")
    return _last_model(path, _claude_record_model) if path else None


def _codex_model(session_id: str) -> Optional[str]:
    path = resolve_own_transcript(session_id, "codex")
    return _last_model(path, _codex_record_model) if path else None
