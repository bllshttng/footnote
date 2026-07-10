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

import re
from pathlib import Path
from typing import Mapping, Optional

from fno.harness_identity import canonical_handle, resolve_harness_identity

# The model appears on every assistant line of a claude transcript and in the
# codex rollout's turn_context; ``\s*`` admits both the compact (`"model":"x"`)
# and spaced (`"model": "x"`) renderings.
_MODEL_RE = re.compile(r'"model"\s*:\s*"([^"]+)"')
# Bounded tail read: the last 256 KiB always holds a fresh model line on any
# non-empty transcript, so we never read a multi-MB rollout whole.
_TAIL_BYTES = 256 * 1024


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


def _last_model(path: Path) -> Optional[str]:
    try:
        with open(path, "rb") as fh:
            try:
                fh.seek(-_TAIL_BYTES, 2)
            except OSError:
                fh.seek(0)
            text = fh.read().decode("utf-8", "replace")
    except OSError:
        return None
    matches = _MODEL_RE.findall(text)
    return matches[-1] if matches else None


def _claude_model(session_id: str) -> Optional[str]:
    from fno.agents.discover import default_projects_dir

    # The transcript is named <session_id>.jsonl under a cwd-encoded dir; glob by
    # id so this is cwd-encoding-agnostic. FNO_CLAUDE_PROJECTS_DIR seams the dir.
    for path in default_projects_dir().glob(f"*/{session_id}.jsonl"):
        model = _last_model(path)
        if model:
            return model
    return None


def _codex_model(session_id: str) -> Optional[str]:
    from fno.agents.discover import default_codex_sessions_dir

    # The rollout filename embeds the session id; FNO_CODEX_SESSIONS_DIR seams it.
    for path in default_codex_sessions_dir().rglob(f"*{session_id}*.jsonl"):
        model = _last_model(path)
        if model:
            return model
    return None
