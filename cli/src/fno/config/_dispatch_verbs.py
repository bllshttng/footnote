"""The dispatch verb registry model: an outside skill as a descriptor, not a
bare string. Mirrors ``ReviewerDescriptor``; lookup drops shipped spellings."""
from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Literal, Mapping, Optional, Sequence, Tuple


@dataclass(frozen=True)
class DispatchVerbDescriptor:
    # The unknown-harness default spelling, exactly as the worker must receive
    # it. Rendered verbatim; never fno-namespaced.
    invocation: str
    # Present: this verb exists ONLY on these harnesses. Absent: `invocation`
    # is correct everywhere. Same field, same meaning, as ReviewerDescriptor.
    invocations: Optional[Mapping[str, str]] = None
    # `skill` is probed against the harness's skill roots at resolve time.
    requires: Literal["none", "skill"] = "none"
    # Whether the resolver appends the node id.
    takes_node_id: bool = True
    # What a completion proves, weakest last.
    asserts: Literal["pr", "doc", "invocation"] = "invocation"
    # The sessions-row phase stamped for a worker dispatched on this verb.
    # Shipped verbs infer theirs from the spawn_phase table; an outside verb
    # is unknown to that table, so it declares its phase here. Absent, a
    # --node spawn is refused rather than launched unbound.
    session_phase: Optional[str] = None


#: The built-in dispatch verbs (harness_map mirrors this tuple; cycle-free).
DEFAULT_DISPATCH_VERBS = ("/target", "/think", "/blueprint")

_VERB_BODY_RE = re.compile(r"^[a-z0-9][a-z0-9_-]*$")


def parse_verb_token(tok: str) -> Optional[Tuple[str, bool]]:
    """Parse one verb-seed token: `(verb, namespaced)` or None.

    The shape rule every reader shares: a leading ``/`` or ``$`` sigil, no
    second ``/`` inside the token (an absolute path never matches), an
    optional ``fno:`` namespace, and a lowercase-word remainder. Both sigils
    parse - ``/fno:target``, ``$fno:target``, ``/target`` and ``$target``
    all yield ``("target", ...)``."""
    if len(tok) < 2 or tok[0] not in "/$":
        return None
    body = tok[1:]
    if "/" in body:
        return None
    namespaced = body.startswith("fno:")
    if namespaced:
        body = body[len("fno:"):]
    if not _VERB_BODY_RE.match(body):
        return None
    return body, namespaced


def is_verb_seed(seed: Optional[str]) -> bool:
    """Whether ``seed``'s FIRST token is a verb-shaped command token.

    Index 0 is load-bearing: only a position-0 command may be rewritten or
    run unattended. A verb inside prose must not pass."""
    if not seed:
        return False
    parts = seed.split()
    if not parts:
        return False
    return parse_verb_token(parts[0]) is not None


def canonical_verb_key(key: str) -> str:
    """Leading `/`, `/fno:x`, `$fno:x` and `$x` -> `/x`: the resolver's
    canonical spelling. Keys the parser rejects keep the legacy strip."""
    if parsed := parse_verb_token(key.strip()):
        return "/" + parsed[0]
    k = key.strip().removeprefix("/")
    k = k.removeprefix("fno:")
    return "/" + k if k else k


def resolvable_verbs(
    registry: Optional[Mapping[str, DispatchVerbDescriptor]] = None,
    allowed: Optional[Sequence[str]] = None,
) -> dict[str, DispatchVerbDescriptor]:
    """The registry minus shipped spellings: a key canonicalizing to a built-in
    or allowlisted verb is dropped, so a shipped verb cannot be redefined
    into a weaker descriptor."""
    allowed_canon = {
        canonical_verb_key(v)
        for v in (tuple(allowed) if allowed is not None else DEFAULT_DISPATCH_VERBS)
        if isinstance(v, str) and v.strip()
    }
    return {
        canon: desc
        for key, desc in (registry or {}).items()
        if (canon := canonical_verb_key(str(key))) not in allowed_canon
    }
