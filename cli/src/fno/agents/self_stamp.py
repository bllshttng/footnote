"""Auto-stamp the invoking session's own identity + model into a2a envelopes (x-605c).

The a2a reply protocol is: an agent reads ``<fno_mail from=H ...>`` and runs
``fno mail send H``. For that return leg to resolve, the OUTBOUND envelope must
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

from fno.harness_identity import canonical_handle, resolve_harness_identity

_TAIL_BYTES = 256 * 1024
_EXPANDED_TAIL_BYTES = 2 * 1024 * 1024


def stamp_from(from_name: Optional[str]) -> str:
    """Resolve the outbound envelope ``from``.

    An explicit ``--from-name`` (any value, including the literal ``"fno"``) wins
    verbatim. Unset (``None``) auto-stamps the invoking session's canonical
    handle; with no ambient harness identity (cron, CI, bare shell) it floors to
    ``"fno"``.
    """
    if from_name is not None:
        return from_name
    ident = resolve_harness_identity()
    if ident.session_id and ident.harness:
        return canonical_handle(ident.harness, ident.session_id)
    return "fno"


def resolve_self_model(env: Optional[Mapping[str, str]] = None) -> str:
    """The invoking harness's own model string, or ``"unknown"``.

    claude greps the tail of its own transcript jsonl; codex greps its rollout's
    ``turn_context``. Any miss (no ambient identity, unreadable store, no match)
    floors to ``"unknown"`` so a send is never blocked on model resolution.
    """
    ident = resolve_harness_identity(env)
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


def _complete_lines(path: Path, max_bytes: Optional[int]) -> Optional[list[bytes]]:
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

    if data and not data.endswith(b"\n"):
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


def _claude_model(session_id: str) -> Optional[str]:
    from fno.agents.discover import default_projects_dir

    # The transcript is named <session_id>.jsonl under a cwd-encoded dir; glob by
    # id so this is cwd-encoding-agnostic. FNO_CLAUDE_PROJECTS_DIR seams the dir.
    for path in default_projects_dir().glob(f"*/{session_id}.jsonl"):
        model = _last_model(path, _claude_record_model)
        if model:
            return model
    return None


def _codex_model(session_id: str) -> Optional[str]:
    from fno.agents.discover import default_codex_sessions_dir

    # The rollout filename embeds the session id; FNO_CODEX_SESSIONS_DIR seams it.
    for path in default_codex_sessions_dir().rglob(f"*{session_id}*.jsonl"):
        model = _last_model(path, _codex_record_model)
        if model:
            return model
    return None
