"""fno.agents.whoami — "what is MY registered mesh name?" (read-only).

A mesh-spawned worker has no clean way to learn its OWN registered name —
the derived-name peers use to address it via ``fno mail send <name>``. The
spawn path injects ``FNO_AGENT_SELF`` / ``FNO_AGENT_PROVIDER`` (and, on
follow-up paths, ``FNO_AGENT_SESSION``) into every spawned agent's env
(see :mod:`fno.agents.context`), but nothing surfaces that identity back.

This module is the pure-logic half of ``fno agents whoami`` (the plural
mesh namespace, NOT the retired singular ``fno agent``). The CLI wrapper
in :mod:`fno.agents.cli` wires real env + registry + best-effort enrichers
into :func:`resolve_self`; everything here takes plain inputs so it is
unit-testable without a live mesh.

Read-only by construction: no registry write, no event emit, no state-file
mutation (the CLI tests assert paired-state md5 invariance, mirroring the
``fno whoami`` / ``fno status`` proof).
"""
from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

from fno.agents.registry import AgentEntry

# Exit code for "ran fine, but you are not a registered mesh agent". Distinct
# from Typer's 2 (usage/arg error) and the conventional 1 so a caller can
# branch: `fno agents whoami && echo "I am $(...)"`.
EXIT_NOT_REGISTERED = 3


@dataclass
class WhoamiResult:
    """Resolved mesh self-identity + best-effort enrichment.

    ``registered`` is the single authority for exit code: ``exit_code`` is 0
    iff a name was resolved, else :data:`EXIT_NOT_REGISTERED`. Every
    enrichment field degrades to ``None`` rather than turning a resolved
    identity into a failure.
    """

    registered: bool
    name: Optional[str]
    provider: Optional[str] = None
    session: Optional[str] = None
    short_id: Optional[str] = None
    status: Optional[str] = None
    live_status: Optional[str] = None
    node: Optional[str] = None
    resolved_via: Optional[str] = None  # "env" | "session-fallback" | None
    warnings: list[str] = field(default_factory=list)
    exit_code: int = 0


def _nonempty(value: Optional[str]) -> Optional[str]:
    """Trim and coerce empty-string env values to None.

    A spawn path that exports ``FNO_AGENT_SELF=""`` (or a stray whitespace
    value) must read as "unset", not as a zero-length name (Boundaries).
    """
    if value is None:
        return None
    trimmed = value.strip()
    return trimmed or None


def _find_by_name(registry: list[AgentEntry], name: str) -> Optional[AgentEntry]:
    for entry in registry:
        if entry.name == name:
            return entry
    return None


def _find_by_session(registry: list[AgentEntry], session_uuid: str) -> Optional[AgentEntry]:
    """Match a registry row whose recorded session id equals ``session_uuid``.

    Two passes, most-specific first. ``session_uuid`` here is the full
    ``CLAUDE_CODE_SESSION_ID`` (a UUID), always a non-empty real string, so a
    ``None`` stored field never spuriously matches.

    1. Exact match against the canonical ``harness_session_id`` (x-ec59: a codex
       or gemini worker resolves its own row here too, not just claude), the full
       claude session UUID, or the cc session id.
    2. Prefix match against the 8-hex ``claude_short_id`` - the jobId is a
       32-bit prefix of the session UUID (``claude attach`` / the jobs-dir use
       it). An older / partially-captured claude row may carry ONLY the short
       id, so an exact full-UUID check would miss it and the worker would get a
       false "not registered" (exit 3) precisely on this fallback path the verb
       adds. The exact pass runs first across every row so a real full-id match
       always wins over a shared-prefix coincidence.
    """
    for entry in registry:
        if session_uuid in (
            getattr(entry, "harness_session_id", None),
            entry.claude_session_uuid,
            entry.cc_session_id,
        ):
            return entry
    norm = session_uuid.replace("-", "").lower()
    for entry in registry:
        short_id = entry.claude_short_id
        if short_id and norm.startswith(short_id.lower()):
            return entry
    return None


def resolve_self(
    env: dict,
    registry: list[AgentEntry],
    registry_error: Optional[str] = None,
    session_uuid: Optional[str] = None,
    live_status_fn: Optional[Callable[[str], Optional[str]]] = None,
    node_fn: Optional[Callable[[], Optional[str]]] = None,
) -> WhoamiResult:
    """Resolve this process's mesh identity from env + registry.

    Tiers (deterministic):

    1. ``env`` — ``FNO_AGENT_SELF`` set -> name is that value. Never depends
       on the registry, so a corrupt registry still yields the name.
    2. ``session-fallback`` — ``FNO_AGENT_SELF`` unset but a registry row
       matches ``session_uuid`` (``CLAUDE_CODE_SESSION_ID``).
    3. none — neither -> not a registered mesh agent (exit 3).

    ``registry_error`` (a stringified ``RegistryVersionError``) means the
    registry could not be read: enrichment is skipped and a WARN is recorded,
    but tier 1 still answers from env. ``live_status_fn`` / ``node_fn`` are
    best-effort enrichers; any exception they raise is swallowed into a WARN
    (live_status) or silently dropped (node), never propagated.
    """
    warnings: list[str] = []
    if registry_error:
        warnings.append(f"registry unreadable, enrichment skipped: {registry_error}")
        registry = []

    self_name = _nonempty(env.get("FNO_AGENT_SELF"))
    env_provider = _nonempty(env.get("FNO_AGENT_PROVIDER"))
    env_session = _nonempty(env.get("FNO_AGENT_SESSION"))

    row: Optional[AgentEntry] = None
    name: Optional[str] = None
    resolved_via: Optional[str] = None

    if self_name:
        name = self_name
        resolved_via = "env"
        row = _find_by_name(registry, self_name)
    elif session_uuid:
        row = _find_by_session(registry, session_uuid)
        if row is not None:
            name = row.name
            resolved_via = "session-fallback"

    if name is None:
        return WhoamiResult(
            registered=False,
            name=None,
            resolved_via=None,
            warnings=warnings,
            exit_code=EXIT_NOT_REGISTERED,
        )

    provider = env_provider or (row.provider if row else None)
    session = env_session or (row.session_id if row else None)
    short_id = (row.claude_short_id or row.short_id or None) if row else None
    status = row.status if row else None

    live_status: Optional[str] = None
    if live_status_fn is not None and provider == "claude" and short_id:
        try:
            live_status = live_status_fn(short_id)
        except Exception as exc:  # noqa: BLE001 — best-effort enrichment
            warnings.append(f"live_status enrichment skipped: {exc}")

    node: Optional[str] = None
    if node_fn is not None:
        try:
            node = node_fn()
        except Exception:  # noqa: BLE001 — best-effort, silent
            node = None

    return WhoamiResult(
        registered=True,
        name=name,
        provider=provider,
        session=session,
        short_id=short_id,
        status=status,
        live_status=live_status,
        node=node,
        resolved_via=resolved_via,
        warnings=warnings,
        exit_code=0,
    )


def _scan_field(text: str, key: str) -> Optional[str]:
    """Return the value of the first ``<key>: <value>`` line in ``text``.

    Strips only a MATCHED surrounding quote pair, so a value with a lone
    leading/trailing quote is preserved rather than mangled. ``None`` if the
    key is absent.
    """
    pattern = re.compile(rf"^{re.escape(key)}:\s*(\S+)")
    for line in text.splitlines():
        match = pattern.match(line)
        if match:
            value = match.group(1).strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
                value = value[1:-1]
            return value
    return None


def find_held_node(cwd: str = ".", session_uuid: Optional[str] = None) -> Optional[str]:
    """The backlog node THIS worker holds, as ``node:<id>`` — or ``None``.

    A dispatched ``/target`` worker records its bound node in the local
    ``.fno/target-state.md`` body as ``graph_node_id: <id>``. But the file
    belongs to whatever session owns that worktree, NOT necessarily the caller:
    a stale manifest, a reused cwd, or an env-only mesh worker booted in someone
    else's worktree would otherwise let us emit a node this worker does not hold.

    So we attribute the node ONLY when ownership is proven: the manifest's
    ``claude_transcript_id`` must equal this process's ``session_uuid``
    (``CLAUDE_CODE_SESSION_ID``). The claim holder is keyed on the target
    *session* id (not the mesh name/session), so there is no registry-side join;
    the transcript id is the one field that ties the manifest to the live
    process. Without ``session_uuid`` (e.g. a codex/gemini worker) or on any
    mismatch we return ``None`` rather than guess. Returns ``None`` too when the
    file is absent, the field is missing, or it is the ``null`` sentinel.
    """
    if not session_uuid:
        return None
    state = Path(cwd) / ".fno" / "target-state.md"
    try:
        if not state.is_file():
            return None
        text = state.read_text(encoding="utf-8")
    except OSError:
        return None
    # Current key is claude_session_id; fall back to the pre-rename
    # claude_transcript_id for one release so in-flight manifests still match.
    manifest_claude_sid = _scan_field(text, "claude_session_id") or _scan_field(
        text, "claude_transcript_id"
    )
    if manifest_claude_sid != session_uuid:
        return None  # manifest is not this worker's session — never guess
    value = _scan_field(text, "graph_node_id")
    if value and value.lower() != "null":
        return f"node:{value}"
    return None


def render_human(result: WhoamiResult) -> str:
    """Render the resolved identity as aligned ``key: value`` lines.

    Returns the empty string for an unregistered result (the CLI writes the
    "not a registered mesh agent" line to stderr instead). Enrichment fields
    that are ``None`` are omitted so a sparse identity stays terse.
    """
    if not result.registered:
        return ""
    lines = [f"name:        {result.name}"]
    if result.provider:
        lines.append(f"provider:    {result.provider}")
    if result.session:
        lines.append(f"session:     {result.session}")
    if result.short_id:
        lines.append(f"short_id:    {result.short_id}")
    if result.status:
        lines.append(f"status:      {result.status}")
    if result.live_status:
        lines.append(f"live_status: {result.live_status}")
    if result.node:
        lines.append(f"node:        {result.node}")
    return "\n".join(lines)


def render_json(result: WhoamiResult) -> str:
    """Render the canonical JSON shape (every key always present).

    Absent enrichments serialize as ``null`` (not a missing key) so a
    consumer can distinguish "not resolvable" from "older shape".
    """
    return json.dumps(
        {
            "registered": result.registered,
            "name": result.name,
            "provider": result.provider,
            "session": result.session,
            "short_id": result.short_id,
            "status": result.status,
            "live_status": result.live_status,
            "node": result.node,
            "resolved_via": result.resolved_via,
        },
        indent=2,
        sort_keys=True,
    )
